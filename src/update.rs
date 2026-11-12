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
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::platform::{ProcessLock, system_tool};

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
    exe.with_extension("old.exe")
}

fn update_lock(exe: &Path) -> Result<Option<ProcessLock>> {
    let directory = exe.parent().context("Нет папки программы.")?.canonicalize()?;
    let file_name = exe.file_name().context("Нет имени файла программы.")?;
    let path = directory.join(file_name).to_string_lossy().to_lowercase();
    let key = Sha256::digest(path.as_bytes());
    ProcessLock::try_acquire(&format!(r"Local\DuoLink.Update.{key:x}"))
}

/// Removes what a previous update left behind. Called on every start.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe()
        && updatable(&exe)
        && let Ok(Some(_lock)) = update_lock(&exe)
    {
        let _ = fs::remove_file(old_path(&exe));
    }
}

/// Development builds under `target\` never replace themselves.
fn updatable(exe: &Path) -> bool {
    std::env::var_os("DUOLINK_NO_UPDATE").is_none()
        && !exe
            .ancestors()
            .any(|dir| dir.file_name().is_some_and(|name| name == "target"))
}

fn curl() -> Result<Command> {
    let mut command = Command::new(system_tool("curl.exe")?);
    command.args(["-q", "--fail", "--location", "--silent", "--show-error", "--proto", "=https"])
        .args(["--proto-redir", "=https", "--connect-timeout", "10", "--user-agent", "DuoLink-updater"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null());
    Ok(command)
}

struct Release {
    version: String,
    url: String,
    sha256: String,
    size: u64,
}

/// Parallel ranges: GitHub's CDN sometimes hands one connection a few hundred
/// KB/s while the next gets megabytes, so several connections finish in about
/// the time of the fastest.
const PARTS: u64 = 4;

fn latest() -> Result<Release> {
    let output = curl()?.args([
        "--max-time",
        "20",
        "--header",
        "Accept: application/vnd.github+json",
        &format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"),
    ]).output()?;
    ensure!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr).trim());
    let release: serde_json::Value = serde_json::from_slice(&output.stdout)?;
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
        size: asset["size"].as_u64().unwrap_or(0),
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

/// Downloads `release` into `target` over several connections at once.
/// A connection slower than 100 KB/s for 4 s is dropped and opened again.
fn download(release: &Release, target: &Path, progress: &dyn Fn(u8), stopping: &Mutex<bool>) -> Result<()> {
    let cancelled = || -> Result<()> {
        ensure!(!*stopping.lock().unwrap(), "Обновление отменено.");
        Ok(())
    };
    let parts = if release.size >= 1 << 20 { PARTS } else { 1 };
    let chunk = release.size.div_ceil(parts).max(1);
    let files: Vec<PathBuf> = (0..parts).map(|i| target.with_extension(format!("part{i}"))).collect();
    let mut running = Vec::new();
    for (i, file) in files.iter().enumerate() {
        let mut command = curl()?;
        command
            .args(["--max-time", "300", "--retry", "5", "--retry-all-errors", "--retry-delay", "0"])
            .args(["--speed-limit", "100000", "--speed-time", "4"]);
        if parts > 1 {
            let start = i as u64 * chunk;
            let end = (start + chunk).min(release.size) - 1;
            command.args(["--range", &format!("{start}-{end}")]);
        }
        let child = command
            .arg("--output")
            .arg(file)
            .arg(&release.url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Не удалось запустить загрузку.")?;
        running.push(child);
    }
    let result = (|| -> Result<()> {
        loop {
            cancelled()?;
            let mut done = true;
            for child in &mut running {
                match child.try_wait()? {
                    Some(status) => ensure!(status.success(), "Не удалось скачать обновление."),
                    None => done = false,
                }
            }
            if release.size > 0 {
                let have: u64 = files.iter().filter_map(|f| fs::metadata(f).ok()).map(|m| m.len()).sum();
                progress((have * 100 / release.size).min(99) as u8);
            }
            if done {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(150));
        }
    })();
    if result.is_err() {
        for child in &mut running {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    result?;
    let mut whole = File::create(target)?;
    for file in &files {
        std::io::copy(&mut File::open(file)?, &mut whole)?;
        let _ = fs::remove_file(file);
    }
    Ok(())
}

fn install(exe: &Path, release: &Release, progress: &dyn Fn(u8), stopping: &Mutex<bool>) -> Result<()> {
    let _lock = update_lock(exe)?.context("Обновление уже выполняется в другом окне.")?;
    if sha256(exe)? == release.sha256 {
        return Ok(());
    }
    let parent = exe.parent().context("Нет папки программы.")?;
    let stage = tempfile::Builder::new().prefix(".update-").tempdir_in(parent)?;
    let staged = stage.path().join(ASSET);
    download(release, &staged, progress, stopping).context("Не удалось скачать обновление.")?;
    ensure!(sha256(&staged)? == release.sha256, "Контрольная сумма обновления не совпала.");
    let mut header = [0u8; 2];
    File::open(&staged)?.read_exact(&mut header)?;
    ensure!(header == *b"MZ", "Файл обновления не является программой.");

    // Closing the window waits for the two renames, even if it skips a download.
    let stopping = stopping.lock().unwrap();
    ensure!(!*stopping, "Обновление отменено.");
    let old = old_path(exe);
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old).context("Не удалось освободить место для новой версии.")?;
    if let Err(error) = fs::rename(&staged, exe) {
        if let Err(rollback) = fs::rename(&old, exe) {
            bail!("Не удалось установить обновление ({error}) и вернуть старую версию ({rollback}).");
        }
        return Err(error).context("Не удалось установить новую версию.");
    }
    Ok(())
}

fn check(exe: &Path, report: impl Fn(Status), stopping: &Mutex<bool>) -> Status {
    report(Status::Checking);
    let release = match latest() {
        Ok(release) => release,
        Err(error) => return Status::Failed(format!("{error:#}")),
    };
    if !newer(&release.version, CURRENT) {
        return Status::Current;
    }
    report(Status::Downloading(release.version.clone()));
    let shown = Mutex::new(0u8);
    let progress = |percent: u8| {
        // Every 5 %, so the window is not redrawn for every block.
        let mut last = shown.lock().unwrap();
        if percent >= *last + 5 {
            *last = percent;
            report(Status::Downloading(format!("{} · {percent} %", release.version)));
        }
    };
    match install(exe, &release, &progress, stopping) {
        Ok(()) => Status::Ready(release.version),
        Err(error) => Status::Failed(format!("{error:#}")),
    }
}

pub struct Updater {
    pub statuses: Receiver<Status>,
    trigger: Sender<()>,
    stopping: Arc<Mutex<bool>>,
    /// Path of the exe as launched; after a swap it holds the new version.
    pub exe: PathBuf,
}

impl Updater {
    /// Checks shortly after start and then every few hours, or on `check_now`.
    pub fn start(wake: Arc<dyn Fn() + Send + Sync>) -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        if !updatable(&exe) {
            return None;
        }
        let (report, statuses) = mpsc::channel();
        let (trigger, triggers) = mpsc::channel::<()>();
        let path = exe.clone();
        let stopping = Arc::new(Mutex::new(false));
        let worker_stopping = stopping.clone();
        thread::spawn(move || {
            // Right away: an update found at launch installs before anything starts.
            let mut wait = Duration::ZERO;
            loop {
                match triggers.recv_timeout(wait) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                let status = check(
                    &path,
                    |status| {
                        let _ = report.send(status);
                        wake();
                    },
                    &worker_stopping,
                );
                let ready = matches!(status, Status::Ready(_));
                while triggers.try_recv().is_ok() {}
                if report.send(status).is_err() {
                    return;
                }
                wake();
                if ready {
                    return;
                }
                wait = RECHECK;
            }
        });
        Some(Self { statuses, trigger, stopping, exe })
    }

    pub fn check_now(&self) {
        let _ = self.trigger.send(());
    }

    pub fn stop(&self) {
        *self.stopping.lock().unwrap() = true;
    }

    /// Starts the freshly installed exe. The caller then exits.
    pub fn relaunch(&self) -> Result<()> {
        Command::new(&self.exe)
            .arg("--after-update")
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
    fn update_lock_is_exclusive_per_file() {
        let folder = tempfile::tempdir().unwrap();
        let exe = folder.path().join("DuoLink.exe");
        let lock = update_lock(&exe).unwrap().unwrap();
        assert!(update_lock(&exe).unwrap().is_none());
        assert!(update_lock(&exe.with_file_name("Other.exe")).unwrap().is_some());
        drop(lock);
        assert!(update_lock(&exe).unwrap().is_some());
    }

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
        let stopping = Mutex::new(false);
        let started = std::time::Instant::now();
        install(&exe, &release, &|p| eprintln!("{p} %"), &stopping).unwrap();
        eprintln!("downloaded in {:?}", started.elapsed());
        assert_eq!(sha256(&exe).unwrap(), release.sha256);
        assert_eq!(fs::read(old_path(&exe)).unwrap(), b"MZ old build");
        assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 2);

        // A tampered digest must leave the installed exe untouched.
        let installed = sha256(&exe).unwrap();
        let bad = Release { sha256: "0".repeat(64), ..release };
        assert!(install(&exe, &bad, &|_| {}, &stopping).is_err());
        assert_eq!(sha256(&exe).unwrap(), installed);
        assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 2);
    }
}
