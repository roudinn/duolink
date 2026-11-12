use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempDir};
use uuid::Uuid;

use crate::{
    platform::{Process, display_refresh_would_drop, run_tool, system_tool},
    protocol::{Identity, MEDIA_PORT, Mode},
};

const DESKTOP: &str = "DuoLink";

/// Video packet size. With RTP, Sunshine's encryption and the 2-byte tunnel
/// header it still fits one QUIC datagram on a 1280-byte path, so a frame is
/// never fragmented at the IP layer anywhere on the internet.
const PACKET_SIZE: u32 = 1024;

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

/// The one encoder setup: the fastest preset of every vendor. Sunshine always
/// uses single-frame VBV, low-delay rate control and no lookahead, so no frame
/// waits for another.
const ENCODER: &str = "nvenc_preset = 1\n\
     nvenc_split_encode = enabled\n\
     nvenc_realtime_hags = enabled\n\
     nvenc_latency_over_power = enabled\n\
     amd_usage = ultralowlatency\n\
     amd_quality = speed\n\
     amd_preanalysis = disabled\n\
     qsv_preset = veryfast\n\
     qsv_slow_hevc = disabled\n";

/// Forward error correction when nothing was measured.
pub const DEFAULT_FEC: u8 = 20;

/// Redundancy for a measured random loss rate: enough to rebuild a frame
/// without a keyframe round trip, and no more bandwidth than that needs.
pub fn fec_for_loss(loss: f64) -> u8 {
    match loss {
        l if l < 0.002 => 15,
        l if l < 0.01 => 20,
        l if l < 0.03 => 30,
        l if l < 0.06 => 40,
        _ => 50,
    }
}

fn host_config(rel: &str, address: Ipv4Addr, fec: u8, refresh_hz: Option<u32>) -> String {
    let refresh = match refresh_hz {
        Some(hz) => format!("dd_refresh_rate_option = manual\ndd_manual_refresh_rate = {hz}\n"),
        None => "dd_refresh_rate_option = disabled\n".into(),
    };
    // A second pass tightens the per-frame bitrate on links that already lose packets.
    let nvenc_twopass = if fec > DEFAULT_FEC { "quarter_res" } else { "disabled" };
    format!(
        "sunshine_name = {DESKTOP}\n\
         port = {MEDIA_PORT}\n\
         address_family = ipv4\n\
         bind_address = {address}\n\
         origin_web_ui_allowed = pc\n\
         upnp = disabled\n\
         system_tray = disabled\n\
         notify_pre_releases = disabled\n\
         lan_encryption_mode = 0\n\
         wan_encryption_mode = 0\n\
         controller = enabled\n\
         gamepad_driver = vigembus\n\
         gamepad = x360\n\
         back_button_timeout = -1\n\
         keyboard = disabled\n\
         mouse = disabled\n\
         native_pen_touch = disabled\n\
         stream_audio = enabled\n\
         install_steam_audio_drivers = disabled\n\
         dd_configuration_option = verify_only\n\
         dd_resolution_option = disabled\n\
         {refresh}\
         dd_hdr_option = disabled\n\
         dd_config_revert_on_disconnect = enabled\n\
         max_bitrate = 0\n\
         ping_timeout = 5000\n\
         hevc_mode = 0\n\
         av1_mode = 0\n\
         packetsize = {PACKET_SIZE}\n\
         fec_percentage = {fec}\n\
         nvenc_twopass = {nvenc_twopass}\n\
         {ENCODER}\
         file_apps = {rel}/apps.json\n\
         file_state = {rel}/state.json\n\
         pkey = {rel}/server.key\n\
         cert = {rel}/server.pem\n\
         log_path = {rel}/sunshine.log\n",
        fec = fec.clamp(5, 60),
    )
}

pub struct HostStream {
    process: Process,
    _directory: TempDir,
    output: PathBuf,
    offset: u64,
    tail: String,
}

impl HostStream {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        exe: &Path,
        data: &Path,
        address: Ipv4Addr,
        identity: &Identity,
        host: Uuid,
        peer: &str,
        peer_cert: &str,
        fec: u8,
        refresh_hz: Option<u32>,
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
            host_config(&rel, address, fec, refresh_hz),
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
        process.boost();
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

    /// How many clients connected and disconnected since the last call.
    pub fn events(&mut self) -> Result<(usize, usize)> {
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
            return Ok((0, 0));
        };
        let lines = &self.tail[..end];
        let connected = lines.matches("CLIENT CONNECTED").count();
        let disconnected = lines.matches("CLIENT DISCONNECTED").count();
        self.tail.drain(..=end);
        Ok((connected, disconnected))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
}

impl Codec {
    fn moonlight(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::Av1 => "AV1",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Авто",
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::Av1 => "AV1",
        }
    }
}

/// Guest form exactly as typed in the interface.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Form {
    pub width: String,
    pub height: String,
    pub fps: String,
    pub bitrate: String,
    pub codec: Codec,
    pub yuv444: bool,
    pub hdr: bool,
    pub windowed: bool,
    pub stats: bool,
}

#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct Preferences {
    pub resolution: Option<(u32, u32)>,
    pub fps: Option<u32>,
    pub bitrate: Option<u32>,
    pub codec: Codec,
    pub yuv444: bool,
    pub hdr: bool,
    pub windowed: bool,
    pub stats: bool,
}

pub const MAX_BITRATE_KBPS: u32 = 500_000;
/// Ceiling: 1.5× Moonlight's reference. Past it the picture barely improves
/// while every frame grows and takes longer on the wire; FPS and latency win.
const QUALITY_SCALE: f64 = 1.5;
/// Share of the measured path the video may take. With single-frame VBV a
/// frame is at most bitrate/fps, so at 35 % of the path it crosses the link in
/// about a third of a frame interval, and the router never builds a queue.
pub const PATH_SHARE: f64 = 0.35;
const UNMEASURED_BITRATE_KBPS: u32 = 15_000;

impl Preferences {
    pub fn parse(form: &Form) -> Result<Self> {
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
        let width = parse(&form.width, "Значение ширины")?;
        let height = parse(&form.height, "Значение высоты")?;
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
        let fps = parse(&form.fps, "Значение FPS")?;
        let bitrate = parse(&form.bitrate, "Значение битрейта")?
            .map(|mbps| {
                mbps.checked_mul(1000)
                    .filter(|kbps| *kbps <= MAX_BITRATE_KBPS)
                    .context("Битрейт не может превышать 500 Мбит/с.")
            })
            .transpose()?;
        Ok(Self {
            resolution: width.zip(height),
            fps,
            bitrate,
            codec: form.codec,
            yuv444: form.yuv444,
            hdr: form.hdr,
            windowed: form.windowed,
            stats: form.stats,
        })
    }

    /// Stream mode: explicit values win; otherwise the host's desktop, scaled
    /// down (keeping its aspect) when the guest screen is smaller. Pixels the
    /// guest cannot show only cost encode, network and decode time.
    pub fn mode(self, host: Mode, guest: Option<Mode>) -> Mode {
        let (width, height) = self.resolution.unwrap_or_else(|| fit(host, guest));
        Mode {
            width,
            height,
            fps: self.fps.unwrap_or(host.fps),
        }
    }

    /// Kbps for `mode`: the explicit value, or the quality ceiling.
    pub fn bitrate_for(self, mode: Mode) -> u32 {
        if let Some(bitrate) = self.bitrate {
            return bitrate;
        }
        let kbps = reference_bitrate(mode, self.yuv444) as f64 * QUALITY_SCALE;
        (kbps.round() as u32).clamp(500, MAX_BITRATE_KBPS)
    }

    /// The automatic bitrate: the quality ceiling, unless the measured path
    /// cannot carry a frame in half a frame interval. Returns (kbps, capped).
    pub fn auto_bitrate(self, mode: Mode, path_mbps: Option<f64>) -> (u32, bool) {
        let ceiling = self.bitrate_for(mode);
        if self.bitrate.is_some() {
            return (ceiling, false);
        }
        match path_mbps {
            Some(mbps) => {
                let budget = ((mbps * PATH_SHARE * 1000.0) as u32).max(500);
                if budget < ceiling { (budget, true) } else { (ceiling, false) }
            }
            None => (ceiling.min(UNMEASURED_BITRATE_KBPS), false),
        }
    }
}

fn fit(host: Mode, guest: Option<Mode>) -> (u32, u32) {
    let Some(guest) = guest.filter(|g| g.width > 0 && g.height > 0) else {
        return (host.width, host.height);
    };
    let scale = (guest.width as f64 / host.width as f64)
        .min(guest.height as f64 / host.height as f64);
    if scale >= 1.0 {
        return (host.width, host.height);
    }
    let even = |value: f64| ((value / 2.0).round() as u32 * 2).max(2);
    (even(host.width as f64 * scale), even(host.height as f64 * scale))
}

/// Moonlight's default bitrate table, extrapolated linearly above 4K.
pub fn reference_bitrate(mode: Mode, yuv444: bool) -> u32 {
    const TABLE: [(f64, f64); 6] = [
        (640.0 * 360.0, 1.0),
        (854.0 * 480.0, 2.0),
        (1280.0 * 720.0, 5.0),
        (1920.0 * 1080.0, 10.0),
        (2560.0 * 1440.0, 20.0),
        (3840.0 * 2160.0, 40.0),
    ];
    let pixels = mode.width as f64 * mode.height as f64;
    let mut factor = if pixels <= TABLE[0].0 {
        TABLE[0].1
    } else if pixels >= TABLE[5].0 {
        TABLE[5].1 * pixels / TABLE[5].0
    } else {
        let i = TABLE.iter().position(|(p, _)| pixels <= *p).unwrap();
        let (p0, f0) = TABLE[i - 1];
        let (p1, f1) = TABLE[i];
        f0 + (pixels - p0) / (p1 - p0) * (f1 - f0)
    };
    if yuv444 {
        factor *= 2.0;
    }
    let fps = mode.fps as f64;
    let rate = if fps <= 60.0 { fps } else { (fps / 60.0).sqrt() * 60.0 } / 30.0;
    ((factor * rate).round() * 1000.0) as u32
}

pub struct GuestStream {
    pub process: Process,
    _directory: TempDir,
    _branding: crate::platform::Branding,
}

impl GuestStream {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        exe: &Path,
        data: &Path,
        address: Ipv4Addr,
        same_machine: bool,
        own_audio: bool,
        host: Uuid,
        certificate: &str,
        client: &crate::protocol::ClientKey,
        mode: Mode,
        desktop: Option<Mode>,
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
                qt_bytes(&client.certificate),
                qt_bytes(&client.key),
                qt_bytes(certificate)
            ),
        )?;
        let keep_desktop = !same_machine
            && !preferences.windowed
            && desktop.is_some_and(|desktop| display_refresh_would_drop(desktop, mode.fps));
        let (log, _) = create_log(data, "guest")?;
        let mut command = Command::new(exe);
        command
            .current_dir(directory.path())
            .args(guest_args(same_machine, mode, keep_desktop, preferences))
            .arg(host.to_string())
            .arg(DESKTOP)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if same_machine || own_audio {
            // Still receive and decode Sunshine's audio, but play nothing: either the
            // host's loopback would hear it, or DuoLink plays the cleaner copy.
            command
                .env("SDL_AUDIODRIVER", "dummy")
                .env("SDL_AUDIO_DRIVER", "dummy");
        }
        let process = Process::spawn(&mut command)?;
        process.boost();
        let branding = crate::platform::Branding::start(process.id());
        Ok(Self {
            process,
            _directory: directory,
            _branding: branding,
        })
    }
}

fn guest_args(
    same_machine: bool,
    mode: Mode,
    keep_desktop: bool,
    preferences: Preferences,
) -> Vec<String> {
    let display = if same_machine {
        "windowed"
    } else if preferences.windowed || keep_desktop {
        "borderless"
    } else {
        "fullscreen"
    };
    let toggle = |on: bool, name: &str| format!("--{}{name}", if on { "" } else { "no-" });
    let mut args: Vec<String> = [
        "stream",
        "--resolution",
        &format!("{}x{}", mode.width, mode.height),
        "--fps",
        &mode.fps.to_string(),
        "--bitrate",
        &preferences.bitrate_for(mode).to_string(),
        "--video-codec",
        preferences.codec.moonlight(),
        "--video-decoder",
        "auto",
        "--display-mode",
        display,
        "--audio-config",
        "stereo",
        "--no-vsync",
        "--no-frame-pacing",
        "--audio-on-host",
        "--no-mute-on-focus-loss",
        // In single-controller mode Moonlight keeps one virtual pad connected
        // even when there is no physical pad. Use actual attach/detach events.
        "--multi-controller",
        "--background-gamepad",
        "--keep-awake",
        "--no-game-optimization",
        "--no-quit-after",
        "--capture-system-keys",
        "never",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    args.push(toggle(preferences.yuv444, "yuv444"));
    args.push(toggle(preferences.hdr, "hdr"));
    args.push(toggle(preferences.stats, "performance-overlay"));
    // An explicit size also keeps Moonlight in its LAN mode (5 ms audio packets).
    args.extend(["--packet-size".into(), PACKET_SIZE.to_string()]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(width: &str, height: &str, fps: &str, bitrate: &str) -> Form {
        Form {
            width: width.into(),
            height: height.into(),
            fps: fps.into(),
            bitrate: bitrate.into(),
            ..Form::default()
        }
    }

    #[test]
    fn preferences_keep_custom_modes_and_validate_dimensions() {
        let mode = Mode {
            width: 1920,
            height: 1080,
            fps: 144,
        };
        assert_eq!(Preferences::parse(&form("", "", "", "")).unwrap().mode(mode, None), mode);
        let auto = Preferences::default();
        let small = Mode { width: 1280, height: 800, fps: 60 };
        assert_eq!(auto.mode(mode, Some(small)), Mode { width: 1280, height: 720, fps: 144 });
        let big = Mode { width: 3840, height: 2160, fps: 60 };
        assert_eq!(auto.mode(mode, Some(big)), mode);
        let custom = Preferences::parse(&form("7680", "4320", "240", "85")).unwrap();
        assert_eq!(
            custom.mode(mode, Some(Mode { width: 1280, height: 720, fps: 60 })),
            Mode {
                width: 7680,
                height: 4320,
                fps: 240
            }
        );
        assert_eq!(custom.bitrate, Some(85_000));
        assert_eq!(custom.bitrate_for(mode), 85_000);
        assert!(Preferences::parse(&form("1919", "1080", "60", "")).is_err());
        assert!(Preferences::parse(&form("1920", "", "60", "")).is_err());
        assert!(Preferences::parse(&form("", "", "-1", "")).is_err());
        assert!(Preferences::parse(&form("", "", "", "501")).is_err());
    }

    #[test]
    fn bitrate_is_quality_ceiling_bounded_by_half_the_path() {
        let m = |width, height, fps| Mode { width, height, fps };
        assert_eq!(reference_bitrate(m(1920, 1080, 60), false), 20_000);
        assert_eq!(reference_bitrate(m(3840, 2160, 60), false), 80_000);
        assert_eq!(reference_bitrate(m(3840, 2160, 60), true), 160_000);
        assert_eq!(reference_bitrate(m(7680, 4320, 60), false), 320_000);
        let prefs = Preferences::default();
        let fhd = m(1920, 1080, 60);
        assert_eq!(prefs.bitrate_for(fhd), 30_000);
        assert_eq!(prefs.bitrate_for(m(7680, 4320, 240)), MAX_BITRATE_KBPS);
        assert_eq!(prefs.auto_bitrate(fhd, None), (15_000, false));
        assert_eq!(prefs.auto_bitrate(fhd, Some(300.0)), (30_000, false));
        assert_eq!(prefs.auto_bitrate(fhd, Some(40.0)), (14_000, true));
        assert_eq!(prefs.auto_bitrate(fhd, Some(2.0)), (700, true));
        let manual = Preferences { bitrate: Some(80_000), ..Preferences::default() };
        assert_eq!(manual.auto_bitrate(fhd, Some(40.0)), (80_000, false));
    }

    #[test]
    fn auto_bitrate_saturates_for_large_custom_modes() {
        let preferences = Preferences::parse(&form("2000000000", "2000000000", "1000", "")).unwrap();
        let host = Mode { width: 1920, height: 1080, fps: 60 };
        let mode = preferences.mode(host, None);
        assert_eq!(mode.fps, 1000);
        assert_eq!(preferences.bitrate_for(mode), MAX_BITRATE_KBPS);
    }

    #[test]
    fn fec_tracks_measured_loss() {
        assert_eq!(fec_for_loss(0.0), 15);
        assert_eq!(fec_for_loss(0.005), 20);
        assert_eq!(fec_for_loss(0.02), 30);
        assert_eq!(fec_for_loss(0.05), 40);
        assert_eq!(fec_for_loss(0.2), 50);
        let config = host_config("x", Ipv4Addr::LOCALHOST, 90, None);
        assert!(config.contains("fec_percentage = 60"));
        assert!(config.contains("nvenc_twopass = quarter_res"));
    }

    #[test]
    fn engines_get_low_latency_flags() {
        let mode = Mode { width: 2560, height: 1440, fps: 165 };
        let args = guest_args(false, mode, false, Preferences::default());
        let has = |flag: &str| args.iter().any(|arg| arg == flag);
        for flag in ["--no-vsync", "--no-frame-pacing", "fullscreen", "--background-gamepad", "--multi-controller", "1024"] {
            assert!(has(flag), "missing {flag}");
        }
        assert!(guest_args(true, mode, false, Preferences::default()).iter().any(|arg| arg == "windowed"));
        let config = host_config("x", Ipv4Addr::LOCALHOST, DEFAULT_FEC, None);
        assert!(config.contains("nvenc_twopass = disabled"));
        assert!(config.contains("nvenc_split_encode = enabled"));
        assert!(config.contains("amd_usage = ultralowlatency"));
        assert!(config.contains("packetsize = 1024"));
        assert!(config.contains("bind_address = 127.0.0.1"));
    }

    #[test]
    fn monitor_refresh_is_independent_of_stream_fps() {
        let config = host_config("x", Ipv4Addr::LOCALHOST, DEFAULT_FEC, Some(165));
        assert!(config.contains("dd_refresh_rate_option = manual\ndd_manual_refresh_rate = 165\n"));
        let config = host_config("x", Ipv4Addr::LOCALHOST, DEFAULT_FEC, None);
        assert!(config.contains("dd_refresh_rate_option = disabled\n"));

        let stream = Mode { width: 1920, height: 1080, fps: 60 };
        let args = guest_args(false, stream, true, Preferences::default());
        assert!(args.iter().any(|arg| arg == "borderless"));
    }
}
