use std::{
    ffi::OsStr,
    io,
    mem::{size_of, zeroed},
    net::{IpAddr, Ipv4Addr},
    os::windows::{ffi::OsStrExt, io::AsRawHandle, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, ExitStatus},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use windows_sys::{
    Win32::{
        Devices::DeviceAndDriverInstallation::CM_Get_Device_Interface_List_SizeW,
        Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE},
        Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState},
            Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW},
            Threading::{
                CREATE_NO_WINDOW, CreateMutexW, HIGH_PRIORITY_CLASS,
                PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
                ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
            },
        },
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    },
    core::GUID,
};

use crate::protocol::{Mode, tailscale_address};

pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DUOLINK_DATA_DIR") {
        return Ok(std::path::absolute(path)?);
    }
    Ok(PathBuf::from(
        std::env::var_os("LOCALAPPDATA").context("Windows не указала папку LocalAppData.")?,
    )
    .join("DuoLink"))
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkKind {
    Tailscale,
    Lan,
    Loopback,
}

#[derive(Clone, Serialize)]
pub struct Network {
    pub label: String,
    pub address: Ipv4Addr,
    pub kind: NetworkKind,
}

pub fn networks() -> Vec<Network> {
    let mut entries = Vec::new();
    for (name, address) in local_ip_address::list_afinet_netifas().unwrap_or_default() {
        let IpAddr::V4(address) = address else {
            continue;
        };
        if name.to_ascii_lowercase().contains("tailscale") && tailscale_address(address) {
            entries.insert(
                0,
                Network {
                    label: format!("Tailscale · {address}"),
                    address,
                    kind: NetworkKind::Tailscale,
                },
            );
        } else if address.is_private() {
            entries.push(Network {
                label: format!("Локальная сеть · {address}"),
                address,
                kind: NetworkKind::Lan,
            });
        }
    }
    entries.dedup_by_key(|entry| entry.address);
    entries.push(Network {
        label: "Этот ПК · тест".into(),
        address: Ipv4Addr::LOCALHOST,
        kind: NetworkKind::Loopback,
    });
    entries
}

pub fn display_mode() -> Result<Mode> {
    let mut mode: DEVMODEW = unsafe { zeroed() };
    mode.dmSize = size_of::<DEVMODEW>() as u16;
    ensure!(
        unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut mode) } != 0,
        "Windows не вернула режим основного экрана."
    );
    Ok(Mode {
        width: mode.dmPelsWidth & !1,
        height: mode.dmPelsHeight & !1,
        fps: if mode.dmDisplayFrequency > 1 {
            mode.dmDisplayFrequency
        } else {
            60
        },
    })
}

pub fn vigem_present() -> bool {
    // GUID_DEVINTERFACE_BUSENUM_VIGEM from ViGEmClient's shared driver ABI.
    let guid = GUID::from_u128(0x96e42b22_f5e9_42f8_b043_ed0f932f014f);
    let mut length = 0;
    unsafe {
        CM_Get_Device_Interface_List_SizeW(&mut length, &guid, std::ptr::null(), 0) == 0
            && length > 1
    }
}

pub fn open(target: &OsStr) -> Result<()> {
    let target: Vec<u16> = target.encode_wide().chain(std::iter::once(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    ensure!(
        result as isize > 32,
        "Windows не удалось открыть указанный файл или ссылку."
    );
    Ok(())
}

pub fn system_tool(executable: &str) -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("SystemRoot").context("Windows не указала SystemRoot.")?)
            .join("System32")
            .join(executable),
    )
}

struct Handle(HANDLE);

// These handles are owned and their kernel objects support closing from another thread.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub struct HostLock {
    _handle: Handle,
}

impl HostLock {
    pub fn acquire() -> Result<Self> {
        let name: Vec<u16> = "Local\\DuoLink.Host.v1\0".encode_utf16().collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        let error = unsafe { GetLastError() };
        if handle.is_null() {
            return Err(io::Error::from_raw_os_error(error as i32))
                .context("Не удалось зарезервировать сессию хоста.");
        }
        let handle = Handle(handle);
        ensure!(
            error != ERROR_ALREADY_EXISTS,
            "На этом ПК уже открыта сессия DuoLink."
        );
        Ok(Self { _handle: handle })
    }
}

pub struct Process {
    child: Child,
    _job: Handle,
}

impl Process {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error())
                .context("Не удалось создать группу процессов Windows.");
        }
        let job = Handle(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error())
                .context("Не удалось настроить завершение фонового процесса.");
        }
        let mut child = command
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("Windows не удалось запустить компонент DuoLink.")?;
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) } == 0 {
            let error = io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("Не удалось привязать компонент к сессии DuoLink.");
        }
        Ok(Self { child, _job: job })
    }

    /// Streaming engines run in the background on the host, where Windows 11
    /// EcoQoS would otherwise park them on efficiency cores and coalesce their
    /// timers. Raise the class and opt out of both throttles.
    pub fn boost(&self) {
        let handle = self.child.as_raw_handle();
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        unsafe {
            SetPriorityClass(handle, HIGH_PRIORITY_CLASS);
            SetProcessInformation(
                handle,
                ProcessPowerThrottling,
                (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }

    pub fn status(&mut self) -> Result<Option<ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn run_tool(command: &mut Command, check: impl Fn() -> Result<()>) -> Result<()> {
    let mut process = Process::spawn(command)?;
    loop {
        check()?;
        if let Some(status) = process.status()? {
            if status.success() {
                return Ok(());
            }
            bail!(
                "{} завершился с кодом {status}.",
                command.get_program().to_string_lossy()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
}

/// Keeps the display and system awake while the calling thread holds it.
/// Desktop Duplication stops delivering frames once the host display sleeps.
pub struct KeepAwake;

impl KeepAwake {
    pub fn hold() -> Self {
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED) };
        Self
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

/// Hardware-accelerated GPU scheduling lets NVENC run with realtime priority.
pub fn hags_enabled() -> Option<bool> {
    let key: Vec<u16> = "SYSTEM\\CurrentControlSet\\Control\\GraphicsDrivers\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "HwSchMode\0".encode_utf16().collect();
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (status == 0).then_some(data == 2)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Route {
    pub direct: bool,
    pub via: String,
    pub ms: u32,
}

impl Route {
    pub fn describe(&self) -> String {
        if self.direct {
            format!("Прямой P2P · {} мс", self.ms)
        } else {
            format!("Ретранслятор {} · {} мс", self.via, self.ms)
        }
    }
}

pub fn tailscale_cli() -> Option<PathBuf> {
    [std::env::var_os("ProgramFiles"), std::env::var_os("ProgramW6432")]
        .into_iter()
        .flatten()
        .map(|root| PathBuf::from(root).join("Tailscale").join("tailscale.exe"))
        .find(|path| path.is_file())
}

/// Asks Tailscale for the path to `peer`. `tailscale ping` keeps probing until
/// a direct UDP path is negotiated, so a DERP result here means NAT traversal failed.
pub fn tailscale_route(peer: Ipv4Addr) -> Option<Route> {
    let output = Command::new(tailscale_cli()?)
        .args(["ping", "--c", "8", "--timeout", "3s"])
        .arg(peer.to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().rev().find_map(parse_pong)
}

pub fn parse_pong(line: &str) -> Option<Route> {
    let rest = line.trim().strip_prefix("pong from ")?;
    let (_, rest) = rest.split_once(" via ")?;
    let (via, rest) = rest.rsplit_once(" in ")?;
    let number: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let value: f64 = number.parse().ok()?;
    let unit = &rest[number.len()..];
    let ms = match unit.trim() {
        "ms" => value,
        "s" => value * 1000.0,
        "µs" | "us" => value / 1000.0,
        _ => return None,
    };
    let relay = via.strip_prefix("DERP(").and_then(|name| name.strip_suffix(')'));
    Some(Route {
        direct: relay.is_none(),
        via: relay.unwrap_or(via).to_string(),
        ms: ms.round().max(0.0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pong_lines_classify_direct_and_relay() {
        assert_eq!(
            parse_pong("pong from host (100.101.1.2) via 203.0.113.5:41641 in 23ms"),
            Some(Route { direct: true, via: "203.0.113.5:41641".into(), ms: 23 })
        );
        assert_eq!(
            parse_pong("pong from host (100.101.1.2) via DERP(fra) in 61ms"),
            Some(Route { direct: false, via: "fra".into(), ms: 61 })
        );
        assert_eq!(
            parse_pong("pong from h (100.64.0.1) via [2001:db8::1]:41641 in 1.4ms").map(|r| r.ms),
            Some(1)
        );
        assert!(parse_pong("direct connection not established").is_none());
    }
}
