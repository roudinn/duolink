use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempDir};
use uuid::Uuid;

use crate::{
    platform::{Process, run_tool, system_tool},
    protocol::{Identity, MEDIA_PORT, Mode},
};

const DESKTOP: &str = "DuoLink";

pub const SUNSHINE: Engine = Engine {
    name: "Sunshine",
    directory: "sunshine-2026.914.233613",
    exe: "Sunshine/sunshine.exe",
    url: "https://github.com/SunshineStream/Sunshine/releases/download/v2026.914.233613/Sunshine-Windows-AMD64-lite.zip",
    sha256: "233008e46f4c0e501a586cbfd6c4fd4a4c0d414a0b5fc7f13c070eb92ec3824b",
};

pub const MOONLIGHT: Engine = Engine {
    name: "Moonlight",
    directory: "moonlight-6.2.0",
    exe: "Moonlight.exe",
    url: "https://github.com/moonlight-stream/moonlight-qt/releases/download/v6.2.0/MoonlightPortable-x64-6.2.0.zip",
    sha256: "6f91d268b41ed5ca8066d9d7578dc403b8db190991ff48f00b1700465f8f7ebf",
};

pub struct Engine {
    pub name: &'static str,
    pub directory: &'static str,
    pub exe: &'static str,
    url: &'static str,
    sha256: &'static str,
}

impl Engine {
    pub fn path(&self, data: &Path) -> PathBuf {
        data.join("engines").join(self.directory).join(self.exe)
    }

    pub fn ensure(&self, data: &Path, check: impl Fn() -> Result<()>) -> Result<PathBuf> {
        let executable = self.path(data);
        if executable.is_file() {
            return Ok(executable);
        }

        let engines = data.join("engines");
        fs::create_dir_all(&engines)?;
        let stage = Builder::new().prefix(".install-").tempdir_in(&engines)?;
        let zip = stage.path().join("engine.zip");
        let unpack = stage.path().join("bundle");
        fs::create_dir(&unpack)?;
        let (log, _) = create_log(data, self.directory)?;

        run_tool(
            Command::new(system_tool("curl.exe")?)
                .args([
                    "-q",
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--proto",
                    "=https",
                    "--proto-redir",
                    "=https",
                    "--connect-timeout",
                    "15",
                    "--max-time",
                    "180",
                    "--output",
                ])
                .arg(&zip)
                .arg(self.url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log.try_clone()?),
            &check,
        )
        .with_context(|| format!("Не удалось скачать {} с GitHub. См. журнал.", self.name))?;

        let mut sha = Sha256::new();
        std::io::copy(&mut File::open(&zip)?, &mut sha)?;
        ensure!(
            format!("{:x}", sha.finalize()) == self.sha256,
            "Контрольная сумма {} не совпала.",
            self.name
        );
        run_tool(
            Command::new(system_tool("tar.exe")?)
                .arg("-xf")
                .arg(&zip)
                .arg("-C")
                .arg(&unpack)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log),
            &check,
        )
        .with_context(|| format!("Не удалось распаковать {}.", self.name))?;
        ensure!(
            unpack.join(self.exe).is_file(),
            "В архиве {} отсутствует исполняемый файл.",
            self.name
        );
        check()?;
        match fs::rename(&unpack, engines.join(self.directory)) {
            Ok(()) => Ok(executable),
            Err(_) if executable.is_file() => Ok(executable),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "Не удалось установить {}. Проверьте папку engines.",
                    self.name
                )
            }),
        }
    }
}

fn create_log(data: &Path, name: &str) -> Result<(File, PathBuf)> {
    let dir = data.join("logs");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.log"));
    let file = File::create(&path)
        .with_context(|| format!("Не удалось открыть журнал {}.", path.display()))?;
    Ok((file, path))
}

pub struct HostStream {
    process: Process,
    _directory: TempDir,
    output: PathBuf,
    offset: u64,
    tail: String,
}

impl HostStream {
    pub fn start(
        exe: &Path,
        data: &Path,
        address: Ipv4Addr,
        identity: &Identity,
        host: Uuid,
        peer: &str,
        peer_cert: &str,
    ) -> Result<Self> {
        let root = exe.parent().context("Не найдена папка Sunshine.")?;
        let config = root.join("config");
        fs::create_dir_all(&config)?;
        let directory = Builder::new().prefix("duolink-").tempdir_in(&config)?;
        let rel = directory.path().file_name().unwrap().to_string_lossy();
        fs::write(directory.path().join("server.pem"), &identity.cert_pem)?;
        fs::write(directory.path().join("server.key"), &identity.key_pem)?;
        fs::write(
            directory.path().join("apps.json"),
            serde_json::to_vec(&json!({
                "env": {},
                "apps": [{ "name": DESKTOP, "image-path": "desktop.png" }]
            }))?,
        )?;
        fs::write(
            directory.path().join("state.json"),
            serde_json::to_vec(&json!({
                "root": {
                    "uniqueid": host,
                    "named_devices": [{ "name": peer, "uuid": Uuid::new_v4(), "cert": peer_cert, "enabled": true }]
                }
            }))?,
        )?;
        fs::write(
            directory.path().join("sunshine.conf"),
            format!(
                "sunshine_name = {DESKTOP}\n\
             port = {MEDIA_PORT}\n\
             address_family = ipv4\n\
             bind_address = {address}\n\
             origin_web_ui_allowed = pc\n\
             upnp = disabled\n\
             system_tray = disabled\n\
             notify_pre_releases = disabled\n\
             lan_encryption_mode = 2\n\
             wan_encryption_mode = 2\n\
             controller = enabled\n\
             gamepad_driver = vigembus\n\
             gamepad = x360\n\
             back_button_timeout = -1\n\
             keyboard = disabled\n\
             mouse = disabled\n\
             native_pen_touch = disabled\n\
             stream_audio = enabled\n\
             install_steam_audio_drivers = disabled\n\
             dd_configuration_option = disabled\n\
             max_bitrate = 0\n\
             file_apps = {rel}/apps.json\n\
             file_state = {rel}/state.json\n\
             pkey = {rel}/server.key\n\
             cert = {rel}/server.pem\n\
             log_path = {rel}/sunshine.log\n"
            ),
        )?;
        let (output, output_path) = create_log(data, "host")?;
        let process = Process::spawn(
            Command::new(exe)
                .current_dir(root)
                .arg(format!("config/{rel}/sunshine.conf"))
                .stdin(Stdio::null())
                .stdout(output.try_clone()?)
                .stderr(output),
        )?;
        Ok(Self {
            process,
            _directory: directory,
            output: output_path,
            offset: 0,
            tail: String::new(),
        })
    }

    pub fn ready(&mut self, address: Ipv4Addr) -> Result<bool> {
        self.check_running()?;
        let socket = SocketAddr::from((address, MEDIA_PORT));
        Ok(TcpStream::connect_timeout(&socket, Duration::from_millis(120)).is_ok())
    }

    pub fn check_running(&mut self) -> Result<()> {
        if let Some(code) = self.process.status()? {
            bail!("Sunshine завершился ({code}). Откройте host.log.");
        }
        Ok(())
    }

    pub fn events(&mut self) -> Result<(bool, bool)> {
        self.check_running()?;
        let mut file = File::open(&self.output)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut block = [0; 16 * 1024];
        let count = file.read(&mut block)?;
        self.offset += count as u64;
        self.tail
            .push_str(&String::from_utf8_lossy(&block[..count]));
        let Some(end) = self.tail.rfind('\n') else {
            if self.tail.len() > 32 * 1024 {
                self.tail.clear();
            }
            return Ok((false, false));
        };
        let lines = &self.tail[..end];
        let connected = lines.contains("CLIENT CONNECTED");
        let disconnected = lines.contains("CLIENT DISCONNECTED");
        self.tail.drain(..=end);
        Ok((connected, disconnected))
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Preferences {
    pub resolution: Option<(u32, u32)>,
    pub fps: Option<u32>,
    pub bitrate: Option<u32>,
    pub stats: bool,
}

impl Preferences {
    pub fn parse(width: &str, height: &str, fps: &str, bitrate: &str, stats: bool) -> Result<Self> {
        let parse = |value: &str, name| -> Result<Option<u32>> {
            if value.trim().is_empty() {
                return Ok(None);
            }
            let parsed = value
                .trim()
                .parse::<i32>()
                .ok()
                .filter(|value| *value > 0)
                .with_context(|| format!("{name} должно быть целым числом больше нуля."))?;
            Ok(Some(parsed as u32))
        };
        let width = parse(width, "Значение ширины")?;
        let height = parse(height, "Значение высоты")?;
        ensure!(
            width.is_some() == height.is_some(),
            "Укажите ширину и высоту или оставьте оба поля пустыми."
        );
        if let Some((width, height)) = width.zip(height) {
            ensure!(
                width % 2 == 0 && height % 2 == 0,
                "Ширина и высота должны быть чётными для видеокодека."
            );
        }
        let fps = parse(fps, "Значение FPS")?;
        let bitrate = parse(bitrate, "Значение битрейта")?
            .map(|mbps| {
                mbps.checked_mul(1000)
                    .filter(|kbps| *kbps <= i32::MAX as u32)
                    .context("Слишком большое значение битрейта.")
            })
            .transpose()?;
        Ok(Self {
            resolution: width.zip(height),
            fps,
            bitrate,
            stats,
        })
    }

    pub fn mode(self, host: Mode) -> Mode {
        let (width, height) = self.resolution.unwrap_or((host.width, host.height));
        Mode {
            width,
            height,
            fps: self.fps.unwrap_or(host.fps),
        }
    }
}

pub struct GuestStream {
    pub process: Process,
    _directory: TempDir,
}

impl GuestStream {
    pub fn start(
        exe: &Path,
        data: &Path,
        address: Ipv4Addr,
        host: Uuid,
        certificate: &str,
        identity: &Identity,
        mode: Mode,
        preferences: Preferences,
    ) -> Result<Self> {
        let sessions = data.join("clients");
        fs::create_dir_all(&sessions)?;
        let directory = Builder::new().prefix("duolink-").tempdir_in(sessions)?;
        fs::write(directory.path().join("portable.dat"), b"")?;
        let settings = directory.path().join("Moonlight Game Streaming Project");
        fs::create_dir(&settings)?;
        let qt_bytes =
            |pem: &str| format!("@ByteArray({})", pem.replace('\r', "").replace('\n', "\\n"));
        let uid = Uuid::new_v4().simple().to_string();
        fs::write(
            settings.join("Moonlight.ini"),
            format!(
                "[General]\nuniqueid={}\ncertificate={}\nkey={}\n\n\
             [hosts]\nsize=1\n1\\hostname={DESKTOP}\n1\\uuid={host}\n1\\manualaddress={address}\n\
             1\\manualport={MEDIA_PORT}\n1\\srvcert={}\n1\\nvidiasw=false\n",
                &uid[..16],
                qt_bytes(&identity.cert_pem),
                qt_bytes(&identity.key_pem),
                qt_bytes(certificate)
            ),
        )?;
        let (log, _) = create_log(data, "guest")?;
        let mut command = Command::new(exe);
        command
            .current_dir(directory.path())
            .args([
                "stream",
                "--resolution",
                &format!("{}x{}", mode.width, mode.height),
                "--fps",
                &mode.fps.to_string(),
            ])
            .args([
                "--display-mode",
                if address.is_loopback() {
                    "windowed"
                } else {
                    "borderless"
                },
                "--no-vsync",
                "--no-frame-pacing",
                "--audio-on-host",
                "--no-mute-on-focus-loss",
                "--no-multi-controller",
                "--no-game-optimization",
                "--no-quit-after",
                "--capture-system-keys",
                "never",
            ])
            .arg(if preferences.stats {
                "--performance-overlay"
            } else {
                "--no-performance-overlay"
            })
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if let Some(bitrate) = preferences.bitrate {
            command.args(["--bitrate", &bitrate.to_string()]);
        }
        if address.is_loopback() {
            // Still receive and decode audio, with no feedback into host WASAPI loopback.
            command
                .env("SDL_AUDIODRIVER", "dummy")
                .env("SDL_AUDIO_DRIVER", "dummy");
        }
        command.args([&host.to_string(), DESKTOP]);
        let process = Process::spawn(&mut command)?;
        Ok(Self {
            process,
            _directory: directory,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_keep_custom_modes_and_validate_dimensions() {
        let mode = Mode {
            width: 1920,
            height: 1080,
            fps: 144,
        };
        assert_eq!(
            Preferences::parse("", "", "", "", false)
                .unwrap()
                .mode(mode),
            mode
        );
        let custom = Preferences::parse("7680", "4320", "240", "85", true).unwrap();
        assert_eq!(
            custom.mode(mode),
            Mode {
                width: 7680,
                height: 4320,
                fps: 240
            }
        );
        assert_eq!(custom.bitrate, Some(85_000));
        assert!(Preferences::parse("1919", "1080", "60", "", false).is_err());
        assert!(Preferences::parse("1920", "", "60", "", false).is_err());
        assert!(Preferences::parse("", "", "-1", "", false).is_err());
    }
}
