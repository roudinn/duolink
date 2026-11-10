//! Self-update from GitHub releases.
//!
//! The newest release's `DuoLink.exe` is downloaded next to the running exe,
//! verified against the SHA-256 digest GitHub publishes for the asset, and
//! swapped in: Windows lets a running image be renamed, so the old file moves
//! aside and the new one takes its name. The next start runs the new version
//! and deletes the leftover.

use std::{
    fs::{self, File},
    io::Read,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::platform::system_tool;

const REPOSITORY: &str = "roudinn/duolink";
const ASSET: &str = "DuoLink.exe";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const RECHECK: Duration = Duration::from_secs(6 * 3600);

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "version")]
pub enum Status {
    #[default]
    Idle,
    Checking,
    Current,
    Downloading(String),
    /// Installed on disk; takes effect after a restart.
    Ready(String),
    Failed(String),
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    let mut next = || parts.next()?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok();
    Some((next()?, next()?, next()?))
}

pub fn newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

fn old_path(exe: &Path) -> PathBuf {
    exe.with_file_name("DuoLink.old.exe")
}

fn staged_path(exe: &Path) -> PathBuf {
    exe.with_file_name("DuoLink.new.exe")
}

/// Removes what a previous update left behind. Called on every start.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(old_path(&exe));
        let _ = fs::remove_file(staged_path(&exe));
    }
}

/// Development builds under `target\` never replace themselves.
fn updatable(exe: &Path) -> bool {
    std::env::var_os("DUOLINK_NO_UPDATE").is_none()
        && !exe
            .ancestors()
            .any(|dir| dir.file_name().is_some_and(|name| name == "target"))
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new(system_tool("curl.exe")?)
        .args(["-q", "--fail", "--location", "--silent", "--show-error", "--proto", "=https"])
        .args(["--proto-redir", "=https", "--connect-timeout", "10", "--user-agent", "DuoLink-updater"])
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()?;
    ensure!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr).trim());
    Ok(output.stdout)
}

struct Release {
    version: String,
    url: String,
    sha256: String,
}

fn latest() -> Result<Release> {
    let body = curl(&[
        "--max-time",
        "20",
        "--header",
        "Accept: application/vnd.github+json",
        &format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"),
    ])?;
    let release: serde_json::Value = serde_json::from_slice(&body)?;
    let version = release["tag_name"].as_str().context("Нет версии в релизе.")?.to_string();
    let asset = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|asset| asset["name"] == ASSET)
        .context("В релизе нет DuoLink.exe.")?;
    let url = asset["browser_download_url"].as_str().context("Нет ссылки на файл.")?;
    ensure!(
        url.starts_with(&format!("https://github.com/{REPOSITORY}/releases/download/")),
        "Неожиданный адрес файла обновления."
    );
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|hex| hex.len() == 64)
        .context("GitHub не прислал контрольную сумму — обновление пропущено.")?;
    Ok(Release {
        version: version.trim_start_matches('v').to_string(),
        url: url.to_string(),
        sha256: sha256.to_ascii_lowercase(),
    })
}

fn sha256(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut file = File::open(path)?;
    let mut block = vec![0; 1 << 16];
    loop {
        let count = file.read(&mut block)?;
        if count == 0 {
            break;
        }
        hash.update(&block[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn install(exe: &Path, release: &Release) -> Result<()> {
    let staged = staged_path(exe);
    let _ = fs::remove_file(&staged);
    curl(&["--max-time", "300", "--output", &staged.to_string_lossy(), &release.url])?;
    if sha256(&staged)? != release.sha256 {
        let _ = fs::remove_file(&staged);
        bail!("Контрольная сумма обновления не совпала.");
    }
    let mut header = [0u8; 2];
    File::open(&staged)?.read_exact(&mut header)?;
    ensure!(header == *b"MZ", "Файл обновления не является программой.");

    let old = old_path(exe);
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old).context("Не удалось освободить место для новой версии.")?;
    if let Err(error) = fs::rename(&staged, exe) {
        let _ = fs::rename(&old, exe);
        return Err(error).context("Не удалось установить новую версию.");
    }
    Ok(())
}

fn check(exe: &Path, report: &Sender<Status>) -> Status {
    let _ = report.send(Status::Checking);
    let release = match latest() {
        Ok(release) => release,
        Err(error) => return Status::Failed(format!("{error:#}")),
    };
    if !newer(&release.version, CURRENT) {
        return Status::Current;
    }
    let _ = report.send(Status::Downloading(release.version.clone()));
    match install(exe, &release) {
        Ok(()) => Status::Ready(release.version),
        Err(error) => Status::Failed(format!("{error:#}")),
    }
}

pub struct Updater {
    pub statuses: Receiver<Status>,
    trigger: Sender<()>,
    /// Path of the exe as launched; after a swap it holds the new version.
    pub exe: PathBuf,
}

impl Updater {
    /// Checks shortly after start and then every few hours, or on `check_now`.
    pub fn start() -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        if !updatable(&exe) {
            return None;
        }
        let (report, statuses) = mpsc::channel();
        let (trigger, triggers) = mpsc::channel::<()>();
        let path = exe.clone();
        thread::spawn(move || {
            let mut wait = Duration::from_secs(5);
            loop {
                match triggers.recv_timeout(wait) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                let status = check(&path, &report);
                let ready = matches!(status, Status::Ready(_));
                if report.send(status).is_err() || ready {
                    return;
                }
                wait = RECHECK;
            }
        });
        Some(Self { statuses, trigger, exe })
    }

    pub fn check_now(&self) {
        let _ = self.trigger.send(());
    }

    /// Starts the freshly installed exe. The caller then exits.
    pub fn relaunch(&self) -> Result<()> {
        Command::new(&self.exe)
            .current_dir(self.exe.parent().context("Нет папки программы.")?)
            .spawn()
            .context("Не удалось запустить новую версию.")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("v1.10.0", "1.9.9"));
        assert!(newer("1.4.0", "1.3.0"));
        assert!(!newer("v1.3.0", "1.3.0"));
        assert!(!newer("v1.2.9", "1.3.0"));
        assert!(!newer("garbage", "1.3.0"));
    }

    #[test]
    #[ignore = "downloads the latest release from GitHub"]
    fn live_download_verifies_and_swaps() {
        let folder = tempfile::tempdir().unwrap();
        let exe = folder.path().join("DuoLink.exe");
        fs::write(&exe, b"MZ old build").unwrap();
        let release = latest().unwrap();
        eprintln!("latest release {} sha256 {}", release.version, release.sha256);
        install(&exe, &release).unwrap();
        assert_eq!(sha256(&exe).unwrap(), release.sha256);
        assert_eq!(fs::read(old_path(&exe)).unwrap(), b"MZ old build");
        assert!(!staged_path(&exe).exists());

        // A tampered digest must leave the installed exe untouched.
        let installed = sha256(&exe).unwrap();
        let bad = Release { sha256: "0".repeat(64), ..release };
        assert!(install(&exe, &bad).is_err());
        assert_eq!(sha256(&exe).unwrap(), installed);
        assert!(!staged_path(&exe).exists());
    }
}
