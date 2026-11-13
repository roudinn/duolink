use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::json;
use tempfile::{Builder, TempDir};
use uuid::Uuid;

use crate::{
    infra,
    platform::{Process, display_refresh_would_drop, run_tool, system_tool},
    protocol::{Identity, MEDIA_PORT, Mode},
};

const DESKTOP: &str = "DuoLink";

/// At QUIC's 1200-byte floor Quinn allows 1162 bytes with its default CID.
/// Leave 16 for RTP and two for the port tag, and round down for Moonlight FEC.
pub(crate) const PACKET_SIZE: usize = 1136;
pub(crate) const LEGACY_PACKET_SIZE: usize = 1024;
pub(crate) const VIDEO_PACKET_OVERHEAD: usize = 16 + 2;

pub(crate) fn video_packet_size(limit: usize) -> usize {
    if limit >= PACKET_SIZE { PACKET_SIZE } else { LEGACY_PACKET_SIZE }
}

pub const SUNSHINE: Engine = Engine {
    name: "Sunshine",
    directory: "sunshine-2026.914.233613",
    exe: "Sunshine/sunshine.exe",
    url: "https://github.com/SunshineStream/Sunshine/releases/download/v2026.914.233613/Sunshine-Windows-AMD64-lite.zip",
    mirror: "sunshine-2026.914.233613-windows-amd64-lite.zip",
    sha256: "233008e46f4c0e501a586cbfd6c4fd4a4c0d414a0b5fc7f13c070eb92ec3824b",
};

pub const MOONLIGHT: Engine = Engine {
    name: "Moonlight",
    directory: "moonlight-6.2.0",
    exe: "Moonlight.exe",
    url: "https://github.com/moonlight-stream/moonlight-qt/releases/download/v6.2.0/MoonlightPortable-x64-6.2.0.zip",
    mirror: "moonlight-6.2.0-windows-x64-portable.zip",
    sha256: "6f91d268b41ed5ca8066d9d7578dc403b8db190991ff48f00b1700465f8f7ebf",
};

pub struct Engine {
    pub name: &'static str,
    pub directory: &'static str,
    pub exe: &'static str,
    url: &'static str,
    mirror: &'static str,
    sha256: &'static str,
}

#[cfg(test)]
mod download_tests {
    use super::*;

    #[test]
    #[ignore = "downloads the pinned Windows engines and driver installer"]
    fn live_mirror_installs_and_caches_pinned_dependencies() {
        let data = tempfile::tempdir().unwrap();
        let sunshine = SUNSHINE.ensure(data.path(), || Ok(())).unwrap();
        let moonlight = MOONLIGHT.ensure(data.path(), || Ok(())).unwrap();
        let driver = crate::setup::fetch(data.path(), &crate::setup::VIGEM, || Ok(())).unwrap();
        assert!(sunshine.is_file() && moonlight.is_file() && driver.is_file());
        for name in [SUNSHINE.directory, MOONLIGHT.directory, "download-ViGEmBus"] {
            let log = fs::read_to_string(data.path().join("logs").join(format!("{name}.log"))).unwrap();
            assert!(log.contains("sslip.io/downloads/"));
            assert!(!log.contains("GET https://github.com/"));
        }
        assert_eq!(SUNSHINE.ensure(data.path(), || panic!("cached engine")).unwrap(), sunshine);
        assert_eq!(MOONLIGHT.ensure(data.path(), || panic!("cached engine")).unwrap(), moonlight);
        assert_eq!(crate::setup::fetch(data.path(), &crate::setup::VIGEM, || panic!("cached driver")).unwrap(), driver);
    }
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

        infra::fetch(self.name, self.url, Some(self.mirror), self.sha256, &zip, &log, &check)?;
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
     nvenc_realtime_hags = disabled\n\
     nvenc_latency_over_power = enabled\n\
     amd_usage = ultralowlatency\n\
     amd_quality = speed\n\
     amd_preanalysis = disabled\n\
     qsv_preset = veryfast\n\
     qsv_slow_hevc = disabled\n";

/// Forward error correction when nothing was measured.
pub const DEFAULT_FEC: u8 = 15;

/// Redundancy for a measured random loss rate: enough to rebuild a frame
/// without a keyframe round trip, and no more bandwidth than that needs.
pub fn fec_for_loss(loss: f64, local: bool) -> u8 {
    match loss {
        l if l < 0.002 => if local { 5 } else { 10 },
        l if l < 0.01 => 15,
        l if l < 0.03 => 25,
        l if l < 0.06 => 40,
        _ => 50,
    }
}

fn host_config(rel: &str, address: Ipv4Addr, fec: u8, refresh_hz: Option<u32>, broker: bool) -> String {
    let refresh = match refresh_hz {
        Some(hz) => format!("dd_refresh_rate_option = manual\ndd_manual_refresh_rate = {hz}\n"),
        None => "dd_refresh_rate_option = disabled\n".into(),
    };
    // A second pass tightens the per-frame bitrate on links that already lose packets.
    let nvenc_twopass = if fec >= 40 { "quarter_res" } else { "disabled" };
    let gamepad_driver = if broker { "all" } else { "vigembus" };
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
         gamepad_driver = {gamepad_driver}\n\
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
         capture = ddx\n\
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

/// Removes what earlier sessions left in `base`: Sunshine may still hold a file
/// when its folder is deleted, and the folder holds a session key. Only one host
/// session runs per PC, so nothing else here is in use.
fn sweep(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("duolink-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn is_ascii_path(path: &Path) -> bool {
    path.to_str().is_some_and(str::is_ascii)
}

/// Where Sunshine finds this session's files: the folder, the name its config
/// uses for it, and the config file argument.
///
/// Sunshine opens `apps.json` and `state.json` through narrow (ANSI) paths. Under
/// a Windows user name with other than Latin letters (for example "Пользователь")
/// that fails: it starts with no trusted guest and every guest is "not paired".
/// For such paths the session files go to a Latin-only folder instead.
fn session_folder(config: &Path) -> Result<(TempDir, String, String)> {
    if !is_ascii_path(config) {
        let roots = ["ProgramData", "PUBLIC", "SystemDrive"].into_iter().filter_map(|variable| {
            let base = std::env::var_os(variable).map(PathBuf::from)?;
            let base = if variable == "SystemDrive" { PathBuf::from(format!("{}\\", base.display())) } else { base };
            Some(base.join("DuoLink").join("host"))
        });
        for base in roots {
            if !is_ascii_path(&base) || fs::create_dir_all(&base).is_err() {
                continue;
            }
            sweep(&base);
            if let Ok(directory) = Builder::new().prefix("duolink-").tempdir_in(&base) {
                let full = directory.path().to_string_lossy().replace('\\', "/");
                if full.is_ascii() {
                    let conf = format!("{full}/sunshine.conf");
                    return Ok((directory, full, conf));
                }
            }
        }
    }
    sweep(config);
    let directory = Builder::new().prefix("duolink-").tempdir_in(config)?;
    let rel = directory.path().file_name().unwrap().to_string_lossy().into_owned();
    let conf = format!("config/{rel}/sunshine.conf");
    Ok((directory, rel, conf))
}

pub struct HostStream {
    process: Process,
    _directory: TempDir,
    output: PathBuf,
    offset: u64,
    tail: String,
    problem: bool,
}

/// Shown to the host and to guests when Sunshine started without its files.
pub const SUNSHINE_FILES: &str = "Sunshine на ПК хоста не смог прочитать свои файлы. Хосту: пришлите файл host.log из папки журналов DuoLink.";

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
        broker: bool,
    ) -> Result<Self> {
        let root = exe.parent().context("Не найдена папка Sunshine.")?;
        let config = root.join("config");
        fs::create_dir_all(&config)?;
        let (directory, rel, conf_arg) = session_folder(&config)?;
        let rel = rel.as_str();
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
            host_config(rel, address, fec, refresh_hz, broker),
        )?;
        let (output, output_path) = create_log(data, "host")?;
        let process = Process::spawn(
            Command::new(exe)
                .current_dir(root)
                .arg(&conf_arg)
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
            problem: false,
        })
    }

    /// Reads what Sunshine printed while starting: files it could not open mean
    /// no trusted guest and no app, however the rest looks.
    pub fn scan_problem(&mut self) {
        if let Ok(log) = fs::read(&self.output) {
            self.problem = String::from_utf8_lossy(&log).contains("cannot open file");
        }
    }

    pub fn problem(&self) -> Option<&'static str> {
        self.problem.then_some(SUNSHINE_FILES)
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

/// Sunshine advertises codec support after probing the encoder it will use.
fn sunshine_codecs(address: Ipv4Addr) -> Result<u32> {
    let mut socket = TcpStream::connect_timeout(&(address, MEDIA_PORT).into(), Duration::from_secs(1))?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    socket.set_write_timeout(Some(Duration::from_secs(1)))?;
    socket.write_all(b"GET /serverinfo HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut reply = String::new();
    socket.take(16 * 1024).read_to_string(&mut reply)?;
    let (_, field) = reply.split_once("<ServerCodecModeSupport>").context("Missing Sunshine codec flags")?;
    let (value, _) = field.split_once("</ServerCodecModeSupport>").context("Invalid Sunshine codec flags")?;
    Ok(value.parse()?)
}

pub(crate) fn automatic_codec(address: Ipv4Addr, mode: Mode, prefs: Preferences) -> Codec {
    if prefs.codec != Codec::Auto || prefs.yuv444 {
        return prefs.codec;
    }
    let Ok(codecs) = sunshine_codecs(address) else { return Codec::Auto };
    use windows::Win32::Graphics::{
        Direct3D::D3D_DRIVER_TYPE_HARDWARE,
        Direct3D11::{
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0,
            D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10,
            D3D11_SDK_VERSION, D3D11_VIDEO_DECODER_DESC, D3D11CreateDevice, ID3D11VideoDevice,
        },
        Dxgi::{IDXGIAdapter, Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010}},
    };
    use windows::core::Interface;

    let mut device = None;
    if unsafe { D3D11CreateDevice(None::<&IDXGIAdapter>, D3D_DRIVER_TYPE_HARDWARE,
        Default::default(), D3D11_CREATE_DEVICE_VIDEO_SUPPORT, None, D3D11_SDK_VERSION,
        Some(&mut device), None, None) }.is_err()
    {
        return Codec::Auto;
    }
    let Some(video) = device.and_then(|device| device.cast::<ID3D11VideoDevice>().ok()) else {
        return Codec::Auto;
    };
    // Flags from moonlight-common-c. Keep Sunshine's auto advertisement so it
    // can expose HDR profiles and keep its preferred hardware encoder.
    let (av1, hevc, hevc_profile, format) = if prefs.hdr {
        (0x20000, 0x200, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10, DXGI_FORMAT_P010)
    } else {
        (0x10000, 0x100, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, DXGI_FORMAT_NV12)
    };
    for (codec, flag, profile) in [
        (Codec::Av1, av1, D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0),
        (Codec::Hevc, hevc, hevc_profile),
    ] {
        let desc = D3D11_VIDEO_DECODER_DESC {
            Guid: profile,
            SampleWidth: mode.width,
            SampleHeight: mode.height,
            OutputFormat: format,
        };
        if codecs & flag != 0 && unsafe { video.GetVideoDecoderConfigCount(&desc) }.is_ok_and(|count| count > 0) {
            return codec;
        }
    }
    Codec::Auto
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
    pub smooth: bool,
    pub software_decode: bool,
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
    pub smooth: bool,
    pub software_decode: bool,
}

pub const MAX_BITRATE_KBPS: u32 = 500_000;
pub const MIN_AUTO_BITRATE_KBPS: u32 = 500;
/// Ceiling: 1.5× Moonlight's reference. Past it the picture barely improves
/// while every frame grows and takes longer on the wire; FPS and latency win.
const QUALITY_SCALE: f64 = 1.5;
/// Keep headroom for frame bursts, audio and other traffic.
pub const PATH_SHARE: f64 = 0.35;
pub const UNMEASURED_BITRATE_KBPS: u32 = 15_000;

fn path_bitrate_kbps(path_kbps: u32) -> u32 {
    ((path_kbps as f64 * PATH_SHARE) as u32).clamp(MIN_AUTO_BITRATE_KBPS, MAX_BITRATE_KBPS)
}

pub fn stream_wire_kbps(bitrate: u32, audio_kbps: u32) -> u32 {
    // Sunshine includes FEC in Moonlight's requested rate. Allow 10% for
    // QUIC/IP/RTP and 650 kbps for its stereo audio/control at low video rates,
    // where Sunshine caps the audio deduction. DuoLink audio is additional.
    bitrate.saturating_add(bitrate.div_ceil(10)).saturating_add(650).saturating_add(audio_kbps)
}

pub fn lobby_wire_budget(capacity: Option<u32>) -> u32 {
    capacity.map_or(UNMEASURED_BITRATE_KBPS, |kbps| (u64::from(kbps) * 60 / 100) as u32)
}

/// Zero means no room: never round an exhausted budget up to a playable rate.
pub fn lobby_bitrate_limit(capacity: Option<u32>, reserved: u32, audio_kbps: u32) -> u32 {
    let remaining = lobby_wire_budget(capacity).saturating_sub(reserved);
    let minimum = stream_wire_kbps(MIN_AUTO_BITRATE_KBPS, audio_kbps);
    if remaining < minimum {
        return 0;
    }
    let share = if reserved == 0 && capacity.is_some() { remaining } else { (remaining / 2).max(minimum) };
    let bitrate = (u64::from(share.saturating_sub(audio_kbps + 650)) * 100 / 110) as u32;
    let ceiling = if reserved == 0 { capacity.map_or(UNMEASURED_BITRATE_KBPS, path_bitrate_kbps) }
        else { UNMEASURED_BITRATE_KBPS };
    bitrate.min(ceiling)
}

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
            smooth: form.smooth,
            software_decode: form.software_decode,
        })
    }

    /// Each automatic setting follows the host's stream mode.
    pub fn mode(self, host: Mode) -> Mode {
        let (width, height) = self.resolution.unwrap_or((host.width, host.height));
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
        (kbps.round() as u32).clamp(MIN_AUTO_BITRATE_KBPS, MAX_BITRATE_KBPS)
    }

    /// Choose the automatic rate within the measured path and lobby budgets.
    pub fn auto_bitrate(
        self,
        mode: Mode,
        path_kbps: Option<u32>,
        lobby_limit: Option<u32>,
    ) -> (u32, bool) {
        let ceiling = self.bitrate_for(mode);
        if self.bitrate.is_some() {
            return (ceiling, false);
        }
        let path_limit = path_kbps.map_or(UNMEASURED_BITRATE_KBPS, path_bitrate_kbps);
        let lobby_limit = lobby_limit
            .map_or(MAX_BITRATE_KBPS, |limit| limit.clamp(MIN_AUTO_BITRATE_KBPS, MAX_BITRATE_KBPS));
        let bitrate = ceiling.min(path_limit).min(lobby_limit);
        (bitrate, bitrate < ceiling && (path_kbps.is_some() || bitrate < path_limit))
    }
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
        packet_size: usize,
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
                "[General]\nrichpresence=false\nuniqueid={}\ncertificate={}\nkey={}\n\n\
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
            .args(guest_args(same_machine, mode, keep_desktop, preferences, packet_size))
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
    packet_size: usize,
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
        if preferences.software_decode { "auto" } else { "hardware" },
        "--display-mode",
        display,
        "--audio-config",
        "stereo",
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
    args.push(toggle(preferences.smooth, "vsync"));
    args.push(toggle(preferences.smooth, "frame-pacing"));
    // An explicit size also keeps Moonlight in its LAN mode (5 ms audio packets).
    args.extend(["--packet-size".into(), packet_size.to_string()]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sunshine_files_avoid_non_latin_paths() {
        let base = tempfile::tempdir().unwrap();
        let config = base.path().join("Пользователь").join("config");
        fs::create_dir_all(&config).unwrap();
        let (directory, rel, conf) = session_folder(&config).unwrap();
        assert!(rel.is_ascii() && conf.is_ascii(), "{rel} {conf}");
        assert!(directory.path().is_dir());
        // A Latin path keeps the short form Sunshine resolves under its config folder.
        let plain = base.path().join("plain").join("config");
        fs::create_dir_all(&plain).unwrap();
        let (_, rel, conf) = session_folder(&plain).unwrap();
        assert!(!rel.contains('/') && conf == format!("config/{rel}/sunshine.conf"));
    }

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
        assert_eq!(Preferences::parse(&form("", "", "", "")).unwrap().mode(mode), mode);
        let custom = Preferences::parse(&form("7680", "4320", "240", "85")).unwrap();
        assert_eq!(
            custom.mode(mode),
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
        assert_eq!(prefs.auto_bitrate(fhd, None, None), (15_000, false));
        assert_eq!(prefs.auto_bitrate(fhd, Some(300_000), None), (30_000, false));
        assert_eq!(prefs.auto_bitrate(fhd, Some(40_000), None), (14_000, true));
        assert_eq!(prefs.auto_bitrate(fhd, Some(2_000), None), (700, true));
        assert_eq!(prefs.auto_bitrate(fhd, None, Some(1_042)), (1_042, true));
        assert_eq!(prefs.auto_bitrate(fhd, Some(2_000), Some(1_042)), (700, true));
        let manual = Preferences { bitrate: Some(80_000), ..Preferences::default() };
        assert_eq!(manual.auto_bitrate(fhd, Some(40_000), Some(1_042)), (80_000, false));
    }

    #[test]
    fn late_guests_share_the_available_upload() {
        let capacity = Some(12_000);
        let audio = 200;
        let first = lobby_bitrate_limit(capacity, 0, audio);
        assert_eq!(first, 4_200);
        let reserved = stream_wire_kbps(first, audio);
        let second = lobby_bitrate_limit(capacity, reserved, audio);
        assert_eq!(second, 500);
        let reserved = reserved + stream_wire_kbps(second, audio);
        assert_eq!(lobby_bitrate_limit(capacity, reserved, audio), 0);
        assert!(lobby_bitrate_limit(capacity, stream_wire_kbps(second, audio), audio) > second);
        assert_eq!(lobby_bitrate_limit(None, 0, audio), 6_045);
        assert_eq!(lobby_bitrate_limit(capacity, u32::MAX, audio), 0);
    }

    #[test]
    fn exhausted_upload_must_not_admit_another_stream() {
        let limit = lobby_bitrate_limit(Some(12_000), 7_200, 200);
        assert_eq!(limit, 0, "a full uplink must refuse a new stream, not add 500 kbps");
    }

    #[test]
    fn reservations_never_exceed_shared_budget() {
        for capacity in [None, Some(500), Some(2_000), Some(12_000), Some(100_000), Some(300_000)] {
            for audio in [0, 200, 1_700] {
                let mut reserved = 0;
                for _ in 0..50 {
                    let rate = lobby_bitrate_limit(capacity, reserved, audio);
                    if rate == 0 { break; }
                    assert!(rate >= MIN_AUTO_BITRATE_KBPS);
                    reserved += stream_wire_kbps(rate, audio);
                    assert!(reserved <= lobby_wire_budget(capacity));
                }
            }
        }
    }

    #[test]
    fn auto_bitrate_saturates_for_large_custom_modes() {
        let preferences = Preferences::parse(&form("2000000000", "2000000000", "1000", "")).unwrap();
        let host = Mode { width: 1920, height: 1080, fps: 60 };
        let mode = preferences.mode(host);
        assert_eq!(mode.fps, 1000);
        assert_eq!(preferences.bitrate_for(mode), MAX_BITRATE_KBPS);
    }

    #[test]
    fn fec_tracks_measured_loss() {
        assert_eq!(fec_for_loss(0.0, true), 5);
        assert_eq!(fec_for_loss(0.0, false), 10);
        assert_eq!(fec_for_loss(0.005, false), 15);
        assert_eq!(fec_for_loss(0.02, false), 25);
        assert_eq!(fec_for_loss(0.05, false), 40);
        assert_eq!(fec_for_loss(0.2, false), 50);
        let config = host_config("x", Ipv4Addr::LOCALHOST, 90, None, true);
        assert!(config.contains("fec_percentage = 60"));
        assert!(config.contains("nvenc_twopass = quarter_res"));
        assert!(config.contains("gamepad_driver = all"));
    }

    #[test]
    fn engines_get_low_latency_flags() {
        let mode = Mode { width: 2560, height: 1440, fps: 165 };
        let args = guest_args(false, mode, false, Preferences::default(), PACKET_SIZE);
        let has = |flag: &str| args.iter().any(|arg| arg == flag);
        for flag in ["--no-vsync", "--no-frame-pacing", "fullscreen", "--background-gamepad", "--multi-controller", "1136", "hardware"] {
            assert!(has(flag), "missing {flag}");
        }
        assert!(guest_args(true, mode, false, Preferences::default(), PACKET_SIZE).iter().any(|arg| arg == "windowed"));
        let smooth = guest_args(false, mode, false, Preferences { smooth: true, ..Preferences::default() }, PACKET_SIZE);
        assert!(smooth.iter().any(|arg| arg == "--vsync"));
        assert!(smooth.iter().any(|arg| arg == "--frame-pacing"));
        let config = host_config("x", Ipv4Addr::LOCALHOST, DEFAULT_FEC, None, false);
        assert!(config.contains("nvenc_twopass = disabled"));
        assert!(config.contains("nvenc_split_encode = enabled"));
        assert!(config.contains("amd_usage = ultralowlatency"));
        assert!(config.contains("packetsize = 1136"));
        assert!(config.contains("capture = ddx"));
        assert!(config.contains("gamepad_driver = vigembus"));
        assert!(config.contains("bind_address = 127.0.0.1"));
    }

    #[test]
    fn automatic_host_mode_reaches_moonlight_with_a_smaller_desktop() {
        let host = Mode { width: 3840, height: 2160, fps: 240 };
        let desktop = Mode { width: 1920, height: 1080, fps: 60 };
        let preferences = Preferences::default();
        let mode = preferences.mode(host);
        let args = guest_args(false, mode, display_refresh_would_drop(desktop, mode.fps), preferences, PACKET_SIZE);
        assert_eq!(mode, host);
        assert!(args.windows(2).any(|pair| pair == ["--resolution", "3840x2160"]));
        assert!(args.windows(2).any(|pair| pair == ["--fps", "240"]));
        assert_eq!(Preferences::parse(&form("", "", "120", "")).unwrap().mode(host),
            Mode { fps: 120, ..host });
        assert_eq!(Preferences::parse(&form("1920", "1080", "", "")).unwrap().mode(host),
            Mode { width: 1920, height: 1080, ..host });
    }

    #[test]
    fn monitor_refresh_is_independent_of_stream_fps() {
        let config = host_config("x", Ipv4Addr::LOCALHOST, DEFAULT_FEC, Some(165), false);
        assert!(config.contains("dd_refresh_rate_option = manual\ndd_manual_refresh_rate = 165\n"));
        let config = host_config("x", Ipv4Addr::LOCALHOST, DEFAULT_FEC, None, false);
        assert!(config.contains("dd_refresh_rate_option = disabled\n"));

        let stream = Mode { width: 1920, height: 1080, fps: 60 };
        let args = guest_args(false, stream, true, Preferences::default(), PACKET_SIZE);
        assert!(args.iter().any(|arg| arg == "borderless"));
    }
}
