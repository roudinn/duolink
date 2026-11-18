//! Self-installation, shortcuts and one-click setup of everything DuoLink needs.

use std::{
    fs::{self, File},
    mem::{size_of, zeroed},
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError, WAIT_OBJECT_0},
    Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW},
    System::Threading::{CREATE_NO_WINDOW, GetExitCodeProcess, WaitForSingleObject},
    UI::{
        Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::SW_HIDE,
    },
};

use crate::{
    infra,
    platform::{system_tool, vigem_present},
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

/// The state of DuoLink's Windows Firewall rules.
enum Firewall {
    /// The rules are present and enabled; nothing to do.
    Ready,
    /// The rules are missing; add them (elevated) before hosting.
    Missing,
    /// The firewall service is down, denied, or the check failed. With the
    /// service off the ports are not filtered, so hosting still works; the
    /// caller should just note a warning and continue.
    Unavailable,
}

fn firewall_state(powershell: &Path, program: &Path) -> Firewall {
    let status = Command::new(powershell)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(include_str!("../scripts/check-firewall.ps1"))
        .env("DUOLINK_CHECK_PROGRAM", program)
        .env("DUOLINK_CHECK_PORT", TUNNEL_PORT.to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status.ok().and_then(|status| status.code()) {
        Some(0) => Firewall::Ready,
        Some(1) => Firewall::Missing,
        // Any failure to run or a firewall-service error: never block hosting.
        _ => Firewall::Unavailable,
    }
}

/// Checks the host's requirements and installs missing components.
pub fn prepare(data: PathBuf) -> Session {
    Session::start(move |worker, _| run_install(worker, &data))
}

fn run_install(worker: &Worker, data: &Path) -> Result<String> {
    let powershell = system_tool("WindowsPowerShell\\v1.0\\powershell.exe")?;
    let program = std::env::current_exe()?;
    let firewall = firewall_state(&powershell, &program);
    // A failed or denied firewall check must never stop hosting: with the
    // firewall service off the ports are not filtered, so we just log it.
    if let Firewall::Unavailable = firewall {
        let _ = log_host_warning(
            data,
            "Проверка брандмауэра Windows недоступна (служба отключена или нет прав). \
             Порты не блокируются, сессия продолжается.",
        );
    }
    let have_driver = vigem_present();
    if have_driver && matches!(firewall, Firewall::Ready | Firewall::Unavailable) {
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
    if matches!(firewall, Firewall::Missing) {
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
    // Firewall rules are best-effort: if the check now fails, the service is
    // down (ports open) — host anyway. If it reports missing, we already asked
    // to add them above; do not block the session on it either way.
    Ok(String::new())
}

/// Appends a warning to the host log without failing if the log cannot be written.
fn log_host_warning(data: &Path, message: &str) -> Result<()> {
    use std::io::Write;
    let logs = data.join("logs");
    fs::create_dir_all(&logs)?;
    let mut file = fs::OpenOptions::new().create(true).append(true).open(logs.join("host.log"))?;
    writeln!(file, "[DuoLink] {message}")?;
    Ok(())
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
    use std::io::Write;

    #[test]
    fn installer_recovers_a_missing_copy_and_keeps_an_equal_version() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Programs/DuoLink/DuoLink.exe");
        let source = std::env::current_exe().unwrap();
        let version = file_version(&source).expect("test executable has a Windows version resource");
        prepare_installed_copy(&source, &target).unwrap();
        assert_eq!(file_version(&target), Some(version));

        let original = fs::metadata(&target).unwrap().len();
        fs::OpenOptions::new().append(true).open(&target).unwrap().write_all(b"trailer").unwrap();
        prepare_installed_copy(&source, &target).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().len(), original + 7);

        fs::write(&target, b"incomplete").unwrap();
        prepare_installed_copy(&source, &target).unwrap();
        assert_eq!(file_version(&target), Some(version));
        assert_eq!(fs::metadata(&target).unwrap().len(), original);
    }

    #[test]
    #[ignore = "requires the DuoLink firewall rules on this Windows installation"]
    fn finds_active_firewall_rules_by_internal_name() {
        let powershell = system_tool("WindowsPowerShell\\v1.0\\powershell.exe").unwrap();
        let program = installed_exe().unwrap();
        assert!(matches!(firewall_state(&powershell, &program), Firewall::Ready));
        assert!(matches!(firewall_state(&powershell, &program.with_file_name("other.exe")), Firewall::Missing));
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

pub fn register_installed(exe: &Path) {
    let marker = exe.with_file_name(format!(".registered-{VERSION}"));
    if !marker.exists() && register(exe).is_ok() {
        let _ = fs::write(marker, b"");
    }
}

pub enum Launch {
    /// Keep running this process.
    Run,
    /// The installed process registers once it holds the instance lock.
    Installed(PathBuf),
    /// The installed copy was started; this process should exit.
    Handoff,
}

fn file_version(path: &Path) -> Option<(u32, u32)> {
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let length = unsafe { GetFileVersionInfoSizeW(name.as_ptr(), std::ptr::null_mut()) };
    if length == 0 {
        return None;
    }
    let mut buffer = vec![0_u8; length as usize];
    if unsafe { GetFileVersionInfoW(name.as_ptr(), 0, length, buffer.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let mut fixed = std::ptr::null_mut();
    let mut size = 0;
    let root = [u16::from(b'\\'), 0];
    let found = unsafe { VerQueryValueW(buffer.as_ptr().cast(), root.as_ptr(), &mut fixed, &mut size) };
    if found == 0 || fixed.is_null() || (size as usize) < size_of::<VS_FIXEDFILEINFO>() {
        return None;
    }
    // The version resource is returned as a byte buffer and may be unaligned.
    let fixed = unsafe { fixed.cast::<VS_FIXEDFILEINFO>().read_unaligned() };
    Some((fixed.dwFileVersionMS, fixed.dwFileVersionLS))
}

fn prepare_installed_copy(current: &Path, target: &Path) -> Result<()> {
    let folder = target.parent().context("Не найдена папка установки DuoLink.")?;
    fs::create_dir_all(folder)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let _lock = loop {
        if let Some(lock) = crate::update::update_lock(target)? {
            break lock;
        }
        if target.is_file() {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "Установка DuoLink уже выполняется.");
        std::thread::sleep(Duration::from_millis(50));
    };
    let installed = file_version(target);
    if installed.zip(file_version(current)).is_some_and(|(old, new)| old >= new) {
        return Ok(());
    }

    let staging = tempfile::tempdir_in(folder)?;
    let staged = staging.path().join("DuoLink.exe");
    fs::copy(current, &staged).context("Не удалось скопировать DuoLink в папку программ.")?;
    // Windows does not allow replacing an executable that is still running.
    if let Err(error) = fs::rename(staged, target)
        && installed.is_none()
    {
        return Err(error).context("Не удалось установить DuoLink.");
    }
    Ok(())
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
        return Ok(Launch::Installed(target));
    }
    prepare_installed_copy(&current, &target)?;
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
