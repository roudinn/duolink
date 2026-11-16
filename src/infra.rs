use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    net::SocketAddrV4,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::os::windows::process::CommandExt;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::platform::{run_tool, system_tool};

pub(crate) fn relay_url() -> String {
    // No relay server is deployed; direct P2P is the only path. Return an
    // empty string so callers skip the relay entirely. A custom relay can
    // still be injected via DUOLINK_RELAY_URL.
    if let Ok(url) = std::env::var("DUOLINK_RELAY_URL")
        && url.starts_with("wss://")
    {
        return url;
    }
    String::new()
}

// Signalling runs on ntfy.sh, a public publish/subscribe service reachable
// without our own server. Messages are already sealed (AES-256-GCM), so a
// public topic leaks nothing. Every topic is a 32-hex hash, so it is hard to
// guess and effectively private.
const ORIGIN: &str = "https://ntfy.sh";
static USE_FIXED_ADDRESS: AtomicBool = AtomicBool::new(false);
// STUN uses public servers only (Google, Cloudflare); no private VPS.
pub(crate) const STUN: [SocketAddrV4; 2] = [
    SocketAddrV4::new(std::net::Ipv4Addr::new(142, 250, 74, 14), 19302),
    SocketAddrV4::new(std::net::Ipv4Addr::new(162, 159, 192, 9), 3478),
];

fn curl() -> Result<Command> {
    let mut command = Command::new(system_tool("curl.exe")?);
    command.args(["-q", "--silent", "--proto", "=https"])
        .creation_flags(CREATE_NO_WINDOW);
    Ok(command)
}

fn signal(topic: &str) -> String {
    format!("{ORIGIN}/{topic}")
}

fn signal_curl(fixed: bool) -> Result<Command> {
    let mut command = curl()?;
    command.args(["--connect-timeout", "3"]);
    let _ = fixed;
    Ok(command)
}

fn connection_failed(code: Option<i32>) -> bool {
    matches!(code, Some(6 | 7 | 28))
}

fn with_signal_route<T>(request: impl Fn(bool) -> Result<(T, bool)>) -> Result<T> {
    let fixed = USE_FIXED_ADDRESS.load(Ordering::Relaxed);
    let (value, retry) = request(fixed)?;
    if !retry {
        return Ok(value);
    }
    let (value, failed) = request(!fixed)?;
    if !failed {
        USE_FIXED_ADDRESS.store(!fixed, Ordering::Relaxed);
    }
    Ok(value)
}

#[derive(Deserialize)]
pub(crate) struct SignalEvent {
    pub topic: String,
    pub time: u64,
    #[serde(default)]
    pub message: String,
}

pub(crate) fn post(topic: &str, body: &str, timeout: &str) -> Result<u16> {
    with_signal_route(|fixed| {
        let mut child = signal_curl(fixed)?
            .args(["--max-time", timeout, "--data-binary", "@-",
                "--output", "NUL", "--write-out", "%{http_code}"])
            .arg(signal(topic))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let written = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(body.as_bytes()).is_ok());
        let output = child.wait_with_output()?;
        let status = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0);
        if status == 0 && connection_failed(output.status.code()) {
            return Ok((0, true));
        }
        // curl can receive a success status before the response finishes.
        if (!written || !output.status.success()) && (200..300).contains(&status) {
            return Ok((0, false));
        }
        if status == 0 {
            anyhow::bail!("curl {}", output.status.code().unwrap_or(-1));
        }
        Ok((status, false))
    })
}

pub(crate) fn history(topic: &str, timeout: &str) -> Result<Vec<SignalEvent>> {
    let request = || with_signal_route(|fixed| {
        let output = signal_curl(fixed)?
            .args(["--fail", "--max-time", timeout, "--write-out", "\n%{http_code}"])
            .arg(format!("{}/json?poll=1&since=all", signal(topic)))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("Нет связи.")?;
        let retry = connection_failed(output.status.code());
        Ok((output, retry))
    });
    let mut output = request()?;
    let (_, status) = history_response(&output.stdout);
    if matches!(status, b"429" | b"503") {
        thread::sleep(Duration::from_millis(400 + u64::from(rand::random::<u8>())));
        output = request()?;
    }
    ensure!(output.status.success(), "Нет связи.");
    let (body, _) = history_response(&output.stdout);
    Ok(String::from_utf8_lossy(body).lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|event: &SignalEvent| !event.message.is_empty())
        .collect())
}

fn history_response(output: &[u8]) -> (&[u8], &[u8]) {
    match output.iter().rposition(|&byte| byte == b'\n') {
        Some(end) => (&output[..end], &output[end + 1..]),
        None => (output, b""),
    }
}

/// A reconnecting NDJSON subscription. Read the boundary second again after a
/// reconnect so two posts in the same second cannot skip one another.
pub(crate) struct Subscription {
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
}

impl Subscription {
    pub fn start(topic: String, since: u64, mut receive: impl FnMut(SignalEvent) + Send + 'static) -> Self {
        let child = Arc::new(Mutex::new(None::<Child>));
        let stop = Arc::new(AtomicBool::new(false));
        let task_child = child.clone();
        let task_stop = stop.clone();
        thread::spawn(move || {
            let mut cursor = since;
            let mut last_time = 0;
            let mut retry = 1;
            let mut fixed = USE_FIXED_ADDRESS.load(Ordering::Relaxed);
            let mut deliver = |event: SignalEvent| {
                if event.topic == topic && !event.message.is_empty() && event.time >= last_time {
                    cursor = cursor.max(event.time);
                    last_time = event.time;
                    receive(event);
                }
                cursor
            };
            let mut since = since.saturating_sub(1);
            while !task_stop.load(Ordering::Relaxed) {
                let spawned = signal_curl(fixed).and_then(|mut command| Ok(command
                    .args(["--fail", "--no-buffer", "--max-time", "1800"])
                    .arg(format!("{}/json?since={since}", signal(&topic)))
                    .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?));
                if let Ok(mut process) = spawned {
                    let stdout = process.stdout.take();
                    {
                        let mut slot = task_child.lock().unwrap();
                        if task_stop.load(Ordering::Relaxed) {
                            let _ = process.kill();
                            let _ = process.wait();
                            break;
                        }
                        *slot = Some(process);
                    }
                    let mut connected = false;
                    for line in stdout.into_iter().flat_map(|out| BufReader::new(out).lines().map_while(Result::ok)) {
                        connected = true;
                        retry = 1;
                        if let Ok(event) = serde_json::from_str::<SignalEvent>(&line) {
                            since = deliver(event).saturating_sub(1);
                        }
                    }
                    let process = task_child.lock().unwrap().take();
                    let status = process.and_then(|mut process| process.wait().ok());
                    if !connected && !task_stop.load(Ordering::Relaxed) {
                        if status.is_some_and(|status| connection_failed(status.code())) {
                            fixed = !fixed;
                        } else if let Ok(events) = history(&topic, "4") {
                            // When the server has no free streaming slot, short
                            // HTTPS reads can still receive invitations and mail.
                            for event in events {
                                since = deliver(event).saturating_sub(1);
                            }
                            retry = 15;
                        }
                    }
                }
                let jitter = rand::random::<u8>() % 30;
                for _ in 0..retry * 10 + u64::from(jitter) {
                    if task_stop.load(Ordering::Relaxed) { return }
                    thread::sleep(Duration::from_millis(100));
                }
                retry = (retry * 2).min(15);
            }
        });
        Self { child, stop }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut process) = self.child.lock().unwrap().take() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

pub(crate) fn sha256(path: &Path) -> Result<String> {
    let mut sha = Sha256::new();
    std::io::copy(&mut File::open(path)?, &mut sha)?;
    Ok(format!("{:x}", sha.finalize()))
}

pub(crate) fn fetch(
    name: &str,
    origin: &str,
    _mirror: Option<&str>,
    expected: &str,
    target: &Path,
    log: &File,
    check: &impl Fn() -> Result<()>,
) -> Result<()> {
    // No mirror server is deployed: download straight from the source (GitHub),
    // which is reachable without our own infrastructure.
    let _ = fs::remove_file(target);
    let _ = writeln!(&*log, "GET {origin}");
    let outcome = run_tool(
        curl()?.args(["--fail", "--location", "--show-error", "--proto-redir", "=https",
            "--connect-timeout", "15", "--max-time", "180", "--output"])
            .arg(target).arg(origin)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(log.try_clone()?),
        check,
    );
    check()?;
    if outcome.is_err() {
        anyhow::bail!("Не удалось скачать {name}. См. журнал.");
    }
    if sha256(target)? == expected {
        return Ok(());
    }
    let _ = writeln!(&*log, "SHA-256 mismatch");
    let _ = fs::remove_file(target);
    anyhow::bail!("Контрольная сумма {name} не совпала.");
}
