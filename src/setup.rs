//! Self-installation, shortcuts and one-click setup of everything DuoLink needs.

use std::{
    fs::{self, File},
    io::Read,
    mem::{size_of, zeroed},
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError, WAIT_OBJECT_0},
    System::Threading::{CREATE_NO_WINDOW, GetExitCodeProcess, WaitForSingleObject},
    UI::{
        Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::SW_HIDE,
    },
};

use crate::{
    platform::{run_tool, system_tool, vigem_present},
    session::{Session, Worker},
    tunnel::TUNNEL_PORT,
};

const INSTALL_SCRIPT: &str = include_str!("../scripts/install.ps1");
const REGISTER_SCRIPT: &str = include_str!("../scripts/register.ps1");
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Download {
    pub name: &'static str,
    url: &'static str,
    sha256: &'static str,
    file: &'static str,
}

pub const VIGEM: Download = Download {
    name: "ViGEmBus",
    url: "https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe",
    sha256: "89220a7865076b342892f98865f3499fb7c4cfd673159e89d352c360fd014c6a",
    file: "ViGEmBus_1.22.0_x64_x86_arm64.exe",
};

fn sha256(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path)?;
    let mut block = vec![0; 1 << 16];
    loop {
        let count = file.read(&mut block)?;
        if count == 0 {
            break;
        }
        hasher.update(&block[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Downloads `item` into the cache once and verifies it before every use.
pub fn fetch(data: &Path, item: &Download, check: impl Fn() -> Result<()>) -> Result<PathBuf> {
    let folder = data.join("downloads");
    fs::create_dir_all(&folder)?;
    let target = folder.join(item.file);
    if target.is_file() && sha256(&target)? == item.sha256 {
        return Ok(target);
    }
    let partial = folder.join(format!("{}.part", item.file));
    let log = File::create(data.join("logs").join(format!("download-{}.log", item.name)))?;
    run_tool(
        Command::new(system_tool("curl.exe")?)
            .args([
                "-q", "--fail", "--location", "--silent", "--show-error", "--proto", "=https",
                "--proto-redir", "=https", "--connect-timeout", "15", "--max-time", "600",
                "--output",
            ])
            .arg(&partial)
            .arg(item.url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log),
        &check,
    )
    .with_context(|| format!("Не удалось скачать {}. Проверьте интернет.", item.name))?;
    ensure!(
        sha256(&partial)? == item.sha256,
        "{} скачался с ошибкой. Попробуйте ещё раз.",
        item.name
    );
    let _ = fs::remove_file(&target);
    fs::rename(&partial, &target)?;
    Ok(target)
}

fn firewall_ready(powershell: &Path, program: &Path) -> Result<bool> {
    let status = Command::new(powershell)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(include_str!("../scripts/check-firewall.ps1"))
        .env("DUOLINK_CHECK_PROGRAM", program)
        .env("DUOLINK_CHECK_PORT", TUNNEL_PORT.to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Не удалось проверить брандмауэр Windows.")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("Не удалось проверить брандмауэр Windows."),
    }
}

/// Checks the host's requirements and installs missing components.
pub fn prepare(data: PathBuf) -> Session {
    Session::start(move |worker, _| run_install(worker, &data))
}

fn run_install(worker: &Worker, data: &Path) -> Result<String> {
    let powershell = system_tool("WindowsPowerShell\\v1.0\\powershell.exe")?;
    let program = std::env::current_exe()?;
    let firewall = firewall_ready(&powershell, &program)?;
    let have_driver = vigem_present();
    if have_driver && firewall {
        return Ok(String::new());
    }
    worker.check()?;
    fs::create_dir_all(data.join("logs"))?;
    let check = || worker.check();
    let vigem = if have_driver {
        None
    } else {
        worker.status("Скачиваем драйвер геймпада…");
        Some(fetch(data, &VIGEM, check)?)
    };
    worker.status("Подтвердите в окне Windows…");
    let script = data.join("setup").join("install.ps1");
    write_script(&script, INSTALL_SCRIPT)?;
    let mut params = format!(
        "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{}\" -Log \"{}\"",
        script.display(),
        data.join("logs").join("setup.log").display()
    );
    if let Some(path) = &vigem {
        params += &format!(" -ViGEm \"{}\" -ViGEmSha {}", path.display(), VIGEM.sha256);
    }
    if !firewall {
        params += &format!(
            " -Firewall -Port {TUNNEL_PORT} -Program \"{}\"",
            program.display()
        );
    }
    let code = run_elevated(&powershell, &params, || worker.check())?;
    ensure!(matches!(code, 0 | 20), "Установка не удалась. Подробности в setup.log.");
    if vigem.is_some() && !vigem_present() {
        ensure!(code != 20, "Перезагрузите ПК, чтобы закончить установку драйвера геймпада.");
        for _ in 0..20 {
            worker.check()?;
            std::thread::sleep(std::time::Duration::from_millis(250));
            if vigem_present() {
                break;
            }
        }
        ensure!(vigem_present(), "Windows не запустила драйвер геймпада. Подробности в setup.log.");
    }
    ensure!(firewall || firewall_ready(&powershell, &program)?,
        "Правила DuoLink не действуют. Проверьте политику брандмауэра Windows.");
    Ok(String::new())
}

/// Windows PowerShell 5.1 reads BOM-less scripts in the ANSI code page.
fn write_script(path: &Path, body: &str) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    // Normalise first: a CRLF checkout must not turn into CR CR LF.
    bytes.extend_from_slice(body.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
    fs::write(path, bytes)?;
    Ok(())
}

fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}

fn run_elevated(program: &Path, params: &str, tick: impl Fn() -> Result<()>) -> Result<u32> {
    let verb = wide("runas");
    let file = wide(program);
    let params = wide(params);
    let mut info: SHELLEXECUTEINFOW = unsafe { zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        if unsafe { GetLastError() } == ERROR_CANCELLED {
            bail!("Установка отменена.");
        }
        bail!("Windows не запустила установку: {}", std::io::Error::last_os_error());
    }
    ensure!(!info.hProcess.is_null(), "Windows не вернула процесс установки.");
    let process = info.hProcess;
    let result = (|| loop {
        tick()?;
        if unsafe { WaitForSingleObject(process, 250) } == WAIT_OBJECT_0 {
            let mut code = 1;
            unsafe { GetExitCodeProcess(process, &mut code) };
            return Ok(code);
        }
    })();
    unsafe { CloseHandle(process) };
    result
}

fn installed_exe() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("LOCALAPPDATA").context("Windows не указала папку LocalAppData.")?,
    )
    .join("Programs")
    .join("DuoLink")
    .join("DuoLink.exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires the DuoLink firewall rules on this Windows installation"]
    fn finds_active_firewall_rules_by_internal_name() {
        let powershell = system_tool("WindowsPowerShell\\v1.0\\powershell.exe").unwrap();
        let program = installed_exe().unwrap();
        assert!(firewall_ready(&powershell, &program).unwrap());
        assert!(!firewall_ready(&powershell, &program.with_file_name("other.exe")).unwrap());
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let normalize = |p: &Path| {
        fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .to_lowercase()
    };
    normalize(a) == normalize(b)
}

fn register(exe: &Path) -> Result<()> {
    let data = crate::platform::data_dir()?;
    let script = data.join("setup").join("register.ps1");
    write_script(&script, REGISTER_SCRIPT)?;
    let mut command = Command::new(system_tool("WindowsPowerShell\\v1.0\\powershell.exe")?);
    command
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-Exe")
        .arg(exe)
        .args(["-Version", VERSION])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ensure!(command.status()?.success(), "Не удалось создать ярлыки DuoLink.");
    Ok(())
}

pub enum Launch {
    /// Keep running this process.
    Run,
    /// The installed copy was started; this process should exit.
    Handoff,
}

/// Copies a downloaded DuoLink.exe into `%LOCALAPPDATA%\Programs\DuoLink`,
/// creates Desktop and Start menu shortcuts and hands off to the installed copy.
/// A `portable.txt` next to the exe, or `DUOLINK_PORTABLE=1`, skips this.
pub fn self_install() -> Result<Launch> {
    let current = std::env::current_exe()?;
    if std::env::var_os("DUOLINK_PORTABLE").is_some()
        || current.with_file_name("portable.txt").exists()
    {
        return Ok(Launch::Run);
    }
    let target = installed_exe()?;
    if same_path(&current, &target) {
        // Repair shortcuts and the uninstall entry once per version.
        let marker = target.with_file_name(format!(".registered-{VERSION}"));
        if !marker.exists() && register(&target).is_ok() {
            let _ = fs::write(marker, b"");
        }
        return Ok(Launch::Run);
    }
    fs::create_dir_all(target.parent().unwrap())?;
    let staged = target.with_extension("new");
    fs::copy(&current, &staged).context("Не удалось скопировать DuoLink в папку программ.")?;
    if fs::rename(&staged, &target).is_err() {
        let _ = fs::remove_file(&staged);
        // The installed copy is running: use this one for now.
        return Ok(Launch::Run);
    }
    register(&target)?;
    let _ = fs::write(target.with_file_name(format!(".registered-{VERSION}")), b"");
    Command::new(&target)
        .current_dir(target.parent().unwrap())
        .spawn()
        .context("Не удалось запустить установленный DuoLink.")?;
    Ok(Launch::Handoff)
}

/// `DuoLink.exe --uninstall`, started from Apps & Features.
pub fn uninstall() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_literal = exe.to_string_lossy().replace('\'', "''");
    // The helper waits for this process and reports the result of the cleanup.
    let script = format!(
        "& {{\n{REGISTER_SCRIPT}\n}} -Exe '{exe_literal}' -Remove -ParentId {}",
        std::process::id()
    );
    let encoded = STANDARD.encode(script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>());
    Command::new(system_tool("WindowsPowerShell\\v1.0\\powershell.exe")?)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand"])
        .arg(encoded)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
