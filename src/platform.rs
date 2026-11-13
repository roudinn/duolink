use std::{
    ffi::OsStr,
    io,
    mem::{size_of, zeroed},

    os::windows::{ffi::OsStrExt, io::AsRawHandle, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, ExitStatus},
};

use anyhow::{Context, Result, bail, ensure};
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
            Registry::{RRF_RT_REG_DWORD, RegGetValueW},
            Threading::{
                ABOVE_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW, CreateMutexW, GetCurrentProcess,
                GetCurrentThread, GetPriorityClass, HIGH_PRIORITY_CLASS, PROCESS_CREATION_FLAGS,
                PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
                ProcessPowerThrottling, SetPriorityClass, SetProcessInformation, SetThreadPriority,
                THREAD_PRIORITY_HIGHEST,
            },
        },
        UI::{
            Shell::ShellExecuteW,
            WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS, SW_SHOWNORMAL},
        },
    },
    core::GUID,
};

use crate::protocol::Mode;

pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DUOLINK_DATA_DIR") {
        return Ok(std::path::absolute(path)?);
    }
    Ok(PathBuf::from(
        std::env::var_os("LOCALAPPDATA").context("Windows не указала папку LocalAppData.")?,
    )
    .join("DuoLink"))
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

/// The highest matching refresh rate at the primary display's current resolution.
pub fn display_max_refresh(current: Mode, accepts: impl Fn(u32) -> bool) -> Option<u32> {
    let mut best = accepts(current.fps).then_some(current.fps);
    let mut index = 0;
    loop {
        let mut mode: DEVMODEW = unsafe { zeroed() };
        mode.dmSize = size_of::<DEVMODEW>() as u16;
        if unsafe { EnumDisplaySettingsW(std::ptr::null(), index, &mut mode) } == 0 {
            break;
        }
        if mode.dmPelsWidth & !1 == current.width
            && mode.dmPelsHeight & !1 == current.height
            && mode.dmDisplayFrequency > 1
            && accepts(mode.dmDisplayFrequency)
        {
            best = best.max(Some(mode.dmDisplayFrequency));
        }
        index += 1;
    }
    best
}

/// Moonlight's exclusive mode looks for a refresh rate divisible by the stream
/// FPS. On a single display we know which desktop mode it starts on.
pub fn display_refresh_would_drop(desktop: Mode, stream_fps: u32) -> bool {
    stream_fps > 0
        && desktop.fps > stream_fps
        && !desktop.fps.is_multiple_of(stream_fps)
        && unsafe { GetSystemMetrics(SM_CMONITORS) == 1 }
        && display_max_refresh(desktop, |hz| hz.is_multiple_of(stream_fps))
            .is_some_and(|matching| matching < desktop.fps)
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

pub struct ProcessLock {
    _handle: Handle,
}

impl ProcessLock {
    pub fn try_acquire(name: &str) -> Result<Option<Self>> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        let error = unsafe { GetLastError() };
        if handle.is_null() {
            return Err(io::Error::from_raw_os_error(error as i32).into());
        }
        let handle = Handle(handle);
        Ok((error != ERROR_ALREADY_EXISTS).then_some(Self { _handle: handle }))
    }
}

/// Raises a process's priority class and opts it out of EcoQoS, which on
/// Windows 11 parks background processes on efficiency cores and coalesces
/// their timers.
fn prioritize(handle: HANDLE, class: PROCESS_CREATION_FLAGS) {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    unsafe {
        SetPriorityClass(handle, class);
        SetProcessInformation(
            handle,
            ProcessPowerThrottling,
            (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

/// DuoLink itself relays every packet of the stream: keep it responsive while
/// a game saturates the CPU and its window sits in the background.
pub fn prioritize_self() {
    prioritize(unsafe { GetCurrentProcess() }, ABOVE_NORMAL_PRIORITY_CLASS);
}

/// Hold the timer request and boost DuoLink only while a tunnel is active.
/// Tests or tools can host and join in the same process, so restore the
/// original priority after the last session exits.
pub(crate) struct StreamingTiming {
    timer: bool,
}

static STREAM_TIMING: std::sync::Mutex<(usize, u32)> = std::sync::Mutex::new((0, 0));

impl StreamingTiming {
    pub fn hold() -> Self {
        let mut state = STREAM_TIMING.lock().unwrap();
        let handle = unsafe { GetCurrentProcess() };
        if state.0 == 0 {
            state.1 = unsafe { GetPriorityClass(handle) };
            prioritize(handle, HIGH_PRIORITY_CLASS);
        }
        state.0 += 1;
        Self { timer: unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) } == 0 }
    }
}

impl Drop for StreamingTiming {
    fn drop(&mut self) {
        if self.timer {
            unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
        }
        let mut state = STREAM_TIMING.lock().unwrap();
        state.0 -= 1;
        if state.0 == 0 && state.1 != 0 {
            unsafe { SetPriorityClass(GetCurrentProcess(), state.1) };
        }
    }
}

pub(crate) fn virtualhid_running() -> bool {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
        SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
    };
    let name: Vec<u16> = "libvirtualhid_broker\0".encode_utf16().collect();
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return false;
        }
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_QUERY_STATUS);
        let mut status = SERVICE_STATUS::default();
        let running = !service.is_null() && QueryServiceStatus(service, &mut status) != 0
            && status.dwCurrentState == SERVICE_RUNNING;
        if !service.is_null() {
            CloseServiceHandle(service);
        }
        CloseServiceHandle(manager);
        running
    }
}

/// For the threads that move stream packets.
pub fn prioritize_thread() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
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
        prioritize(self.child.as_raw_handle(), HIGH_PRIORITY_CLASS);
    }

    pub fn id(&self) -> u32 {
        self.child.id()
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

/// Request a normal exit so Moonlight closes its GameStream connection.
pub fn close_windows(pid: u32) -> bool {
    use windows_sys::{
        Win32::{
            Foundation::{HWND, LPARAM},
            UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE},
        },
        core::BOOL,
    };

    struct Request {
        pid: u32,
        posted: bool,
    }

    unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> BOOL {
        let request = unsafe { &mut *(data as *mut Request) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        if pid == request.pid
            && unsafe { IsWindowVisible(window) } != 0
            && unsafe { PostMessageW(window, WM_CLOSE, 0, 0) } != 0
        {
            request.posted = true;
        }
        1
    }

    let mut request = Request { pid, posted: false };
    unsafe { EnumWindows(Some(visit), (&mut request as *mut Request) as LPARAM) };
    request.posted
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

fn registry_dword(root: windows_sys::Win32::System::Registry::HKEY, key: &str, value: &str) -> Option<u32> {
    let key: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (status == 0).then_some(data)
}

/// Xbox Game Bar's "record what happened" keeps the GPU encoder busy in the
/// background, competing with Sunshine for every frame.
pub fn background_recording() -> bool {
    registry_dword(
        windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
        r"Software\Microsoft\Windows\CurrentVersion\GameDVR",
        "HistoricalCaptureEnabled",
    ) == Some(1)
}

/// Quiets every connected Wi-Fi adapter for the length of a session.
///
/// Windows periodically leaves the channel to scan for other networks, which
/// stalls traffic for 50–200 ms roughly once a minute: the classic periodic
/// streaming hitch. Background scanning is turned off and media-streaming mode
/// on. Both settings belong to this handle; Windows restores them when it is
/// closed, including if DuoLink crashes.
pub struct WifiQuiet {
    handle: HANDLE,
}

// The WLAN client handle may be closed from any thread.
unsafe impl Send for WifiQuiet {}

impl WifiQuiet {
    pub fn hold() -> Option<Self> {
        use windows_sys::Win32::NetworkManagement::WiFi::{
            WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory,
            WlanOpenHandle, WlanSetInterface, wlan_interface_state_connected,
            wlan_intf_opcode_background_scan_enabled, wlan_intf_opcode_media_streaming_mode,
        };
        let mut version = 0u32;
        let mut handle: HANDLE = std::ptr::null_mut();
        if unsafe { WlanOpenHandle(2, std::ptr::null(), &mut version, &mut handle) } != 0 {
            return None;
        }
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if unsafe { WlanEnumInterfaces(handle, std::ptr::null(), &mut list) } != 0 || list.is_null() {
            unsafe { WlanCloseHandle(handle, std::ptr::null()) };
            return None;
        }
        let mut adapters = 0;
        unsafe {
            let count = (*list).dwNumberOfItems as usize;
            let first = std::ptr::addr_of!((*list).InterfaceInfo).cast::<
                windows_sys::Win32::NetworkManagement::WiFi::WLAN_INTERFACE_INFO,
            >();
            for index in 0..count {
                let info = &*first.add(index);
                if info.isState != wlan_interface_state_connected {
                    continue;
                }
                let off: u32 = 0;
                let on: u32 = 1;
                WlanSetInterface(
                    handle,
                    &info.InterfaceGuid,
                    wlan_intf_opcode_background_scan_enabled,
                    size_of::<u32>() as u32,
                    (&off as *const u32).cast(),
                    std::ptr::null(),
                );
                WlanSetInterface(
                    handle,
                    &info.InterfaceGuid,
                    wlan_intf_opcode_media_streaming_mode,
                    size_of::<u32>() as u32,
                    (&on as *const u32).cast(),
                    std::ptr::null(),
                );
                adapters += 1;
            }
            WlanFreeMemory(list.cast());
        }
        if adapters == 0 {
            unsafe { WlanCloseHandle(handle, std::ptr::null()) };
            return None;
        }
        Some(Self { handle })
    }
}

impl Drop for WifiQuiet {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::NetworkManagement::WiFi::WlanCloseHandle(self.handle, std::ptr::null())
        };
    }
}

/// Shows the stream window as DuoLink: our title and icon on Moonlight's
/// windows (title bar, taskbar, Alt+Tab). Moonlight itself is unchanged.
pub struct Branding {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Branding {
    pub fn start(pid: u32) -> Self {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while !flag.load(Ordering::Relaxed) {
                brand_windows(pid);
                // Windows appear while the stream opens; later only fullscreen toggles.
                let pause = if started.elapsed().as_secs() < 30 { 100 } else { 1000 };
                std::thread::sleep(std::time::Duration::from_millis(pause));
            }
        });
        Self { stop }
    }
}

impl Drop for Branding {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn brand_windows(pid: u32) {
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            EnumWindows, GetSystemMetrics, GetWindowTextW, GetWindowThreadProcessId, ICON_BIG, ICON_SMALL,
            IMAGE_ICON, IsWindowVisible, LR_DEFAULTCOLOR, LoadImageW, SM_CXICON, SM_CXSMICON, SendMessageW,
            SetWindowTextW, WM_SETICON,
        },
    };
    struct Icons {
        big: isize,
        small: isize,
    }
    static ICONS: std::sync::OnceLock<Icons> = std::sync::OnceLock::new();
    let icons = ICONS.get_or_init(|| unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let load = |metric| {
            let size = GetSystemMetrics(metric);
            LoadImageW(module, 1 as _, IMAGE_ICON, size, size, LR_DEFAULTCOLOR) as isize
        };
        Icons { big: load(SM_CXICON), small: load(SM_CXSMICON) }
    });
    const TITLE: &str = "DuoLink";

    unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> i32 {
        unsafe {
            let (pid, icons) = &*(data as *const (u32, &Icons));
            let mut owner = 0u32;
            GetWindowThreadProcessId(window, &mut owner);
            if owner != *pid || IsWindowVisible(window) == 0 {
                return 1;
            }
            let mut buffer = [0u16; 64];
            let length = GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32).max(0) as usize;
            if String::from_utf16_lossy(&buffer[..length]) != TITLE {
                let title: Vec<u16> = TITLE.encode_utf16().chain(Some(0)).collect();
                SetWindowTextW(window, title.as_ptr());
                if icons.big != 0 {
                    SendMessageW(window, WM_SETICON, ICON_BIG as usize, icons.big);
                }
                if icons.small != 0 {
                    SendMessageW(window, WM_SETICON, ICON_SMALL as usize, icons.small);
                }
            }
            1
        }
    }
    let data = (pid, icons);
    unsafe {
        EnumWindows(Some(visit), &data as *const _ as LPARAM);
    }
}


const INSTANCE: &str = r"Local\DuoLink.App.v1";
const SHOW: &str = r"Local\DuoLink.Show.v1";

/// One DuoLink per user. A second launch (shortcut, Start menu) brings the
/// running window forward instead. After an update the new copy waits for
/// the old one to finish exiting.
pub fn single_instance(after_update: bool) -> Option<ProcessLock> {
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
    let by = std::time::Instant::now() + std::time::Duration::from_secs(if after_update { 8 } else { 0 });
    loop {
        if let Ok(Some(lock)) = ProcessLock::try_acquire(INSTANCE) {
            return Some(lock);
        }
        if std::time::Instant::now() >= by {
            let name: Vec<u16> = SHOW.encode_utf16().chain(Some(0)).collect();
            unsafe {
                let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
                if !event.is_null() {
                    SetEvent(event);
                    CloseHandle(event);
                }
            }
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Calls `show` whenever another launch asks this instance to come forward.
pub fn on_show_request(show: impl Fn() + Send + 'static) {
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    let name: Vec<u16> = SHOW.encode_utf16().chain(Some(0)).collect();
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
    if event.is_null() {
        return;
    }
    let event = Handle(event);
    std::thread::spawn(move || {
        let event = event;
        loop {
            if unsafe { WaitForSingleObject(event.0, INFINITE) } != 0 {
                return;
            }
            show();
        }
    });
}


const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "DuoLink";

/// Whether Windows starts DuoLink at sign-in (the per-user Run key).
pub fn autostart_enabled() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let key: Vec<u16> = RUN_KEY.encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = RUN_VALUE.encode_utf16().chain(Some(0)).collect();
    let mut size = 0u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) == 0
    }
}

/// Turns the sign-in start on or off. Only this user's Run key changes.
pub fn set_autostart(on: bool) -> Result<()> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    let key: Vec<u16> = RUN_KEY.encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = RUN_VALUE.encode_utf16().chain(Some(0)).collect();
    let status = if on {
        let exe = std::env::current_exe()?;
        let command: Vec<u16> = format!("\"{}\" --tray", exe.display()).encode_utf16().chain(Some(0)).collect();
        unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), REG_SZ, command.as_ptr().cast(), (command.len() * 2) as u32)
        }
    } else {
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) };
        // Already absent is as good as removed.
        if status == 2 { 0 } else { status }
    };
    ensure!(status == 0, "Windows отказала в записи (код {status}).");
    Ok(())
}
