//! Self-installation, shortcuts and one-click setup of everything DuoLink needs.

use std::{
    fs::{self, File},
    mem::{size_of, zeroed},
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError, WAIT_OBJECT_0},
    System::Threading::{CREATE_NO_WINDOW, GetExitCodeProcess, WaitForSingleObject},
    UI::{
        Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::SW_HIDE,
    },
};

use crate::{
    infra,
    platform::{system_tool, vigem_present, virtualhid_running},
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
    mirror: Option<&'static str>,
}

pub const VIGEM: Download = Download {
    name: "ViGEmBus",
    url: "https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe",
    sha256: "89220a7865076b342892f98865f3499fb7c4cfd673159e89d352c360fd014c6a",
    file: "ViGEmBus_1.22.0_x64_x86_arm64.exe",
    mirror: Some("ViGEmBus_1.22.0_x64_x86_arm64.exe"),
};

const VIRTUALHID: Download = Download {
    name: "Virtual HID Broker",
    url: "https://github.com/LizardByte/libvirtualhid/releases/download/v2026.914.1218.10/libvirtualhid-Windows-AMD64-driver-installer.msi",
    sha256: "bc31539a41f71939c13decb171ccbd4306fae1fed63273434a4f5ceccaf3e71c",
    file: "libvirtualhid-2026.914.1218.10-amd64.msi",
    mirror: None,
};

/// Downloads `item` into the cache once and verifies it before every use.
pub fn fetch(data: &Path, item: &Download, check: impl Fn() -> Result<()>) -> Result<PathBuf> {
    let folder = data.join("downloads");
    fs::create_dir_all(&folder)?;
    let target = folder.join(item.file);
    if target.is_file() && infra::sha256(&target)? == item.sha256 {
        return Ok(target);
    }
    let partial = tempfile::Builder::new().prefix(".download-").tempfile_in(&folder)?.into_temp_path();
    fs::create_dir_all(data.join("logs"))?;
    let log = File::create(data.join("logs").join(format!("download-{}.log", item.name)))?;
    infra::fetch(item.name, item.url, item.mirror, item.sha256, &partial, &log, &check)?;
    partial.persist(&target)?;
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

pub fn install_virtualhid(data: PathBuf) -> Session {
    Session::start(move |worker, _| {
        let powershell = system_tool("WindowsPowerShell\\v1.0\\powershell.exe")?;
        if !virtualhid_running() {
            worker.status("Скачиваем драйвер геймпада…");
            let msi = fetch(&data, &VIRTUALHID, || worker.check())?;
            worker.status("Подтвердите в окне Windows…");
            let script = data.join("setup").join("install.ps1");
            write_script(&script, INSTALL_SCRIPT)?;
            let params = format!(
                "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{}\" -VirtualHid \"{}\" -VirtualHidSha {} -Log \"{}\"",
                script.display(), msi.display(), VIRTUALHID.sha256, data.join("logs/setup-virtualhid.log").display(),
            );
            let code = run_elevated(&powershell, &params, || worker.check())?;
            ensure!(matches!(code, 0 | 20), "Не удалось установить Virtual HID Broker. Подробности в setup-virtualhid.log.");
            ensure!(code != 20, "Перезагрузите ПК, чтобы закончить установку драйвера геймпада.");
        }
        ensure!(virtualhid_running(), "Windows не запустила Virtual HID Broker.");
        open_virtualhid()?;
        Ok("Активируйте лицензию Virtual HID Broker в открывшемся окне.".into())
    })
}

fn open_virtualhid() -> Result<()> {
    let path = PathBuf::from(std::env::var_os("ProgramFiles").context("Папка Program Files недоступна.")?)
        .join(r"libvirtualhid\tools\windows\virtualhid_control.exe");
    ensure!(path.is_file(), "Не найдено приложение Virtual HID Broker.");
    crate::platform::open(path.as_os_str())
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
