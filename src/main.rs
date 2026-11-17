#![cfg_attr(not(test), windows_subsystem = "windows")]

mod audio;
mod bitrate;
mod datagram;
mod friends;
mod gui;
mod infra;
mod media;
mod pairing;
mod platform;
mod presence;
mod protocol;
mod relay;
mod session;
mod setup;
mod tunnel;
mod update;

fn message(text: &str, style: u32) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW;
    let title: Vec<u16> = "DuoLink\0".encode_utf16().collect();
    let body: Vec<u16> = format!("{text}\0").encode_utf16().collect();
    unsafe { MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), style) }
}

fn main() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONERROR, MB_OK, MB_YESNO};
    if std::env::args().any(|arg| arg == "--uninstall") {
        if let Err(error) = setup::uninstall() {
            message(&format!("{error:#}"), MB_OK | MB_ICONERROR);
        }
        return;
    }
    let installed = match setup::self_install() {
        Ok(setup::Launch::Handoff) => return,
        Ok(setup::Launch::Installed(exe)) => Some(exe),
        // Installation is a convenience: run portable if it fails.
        Ok(setup::Launch::Run) | Err(_) => None,
    };
    let after_update = std::env::args().any(|arg| arg == "--after-update");
    let Some(_instance) = platform::single_instance(after_update) else {
        return;
    };
    if let Some(exe) = installed {
        setup::register_installed(&exe);
    }
    update::cleanup();
    platform::prioritize_self();
    // Started by Windows at sign-in: stay in the tray.
    let hidden = std::env::args().any(|arg| arg == "--tray");
    if let Err(error) = gui::run(hidden) {
        if gui::webview_missing() {
            let answer = message(
                "Для запуска DuoLink нужен Microsoft Edge WebView2. Открыть страницу загрузки Microsoft?",
                MB_YESNO | MB_ICONERROR,
            );
            let download = std::ffi::OsStr::new("https://developer.microsoft.com/microsoft-edge/webview2/#download");
            if answer == IDYES
                && let Err(error) = platform::open(download)
            {
                message(&error.to_string(), MB_OK | MB_ICONERROR);
            }
        } else {
            message(&format!("{error:#}"), MB_OK | MB_ICONERROR);
        }
    }
}
