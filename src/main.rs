#![cfg_attr(not(test), windows_subsystem = "windows")]

mod audio;
mod bitrate;
mod friends;
mod gui;
mod media;
mod platform;
mod presence;
mod protocol;
mod session;
mod setup;
mod tunnel;
mod update;

fn message(text: &str, icon: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_OK, MessageBoxW};
    let title: Vec<u16> = "DuoLink\0".encode_utf16().collect();
    let body: Vec<u16> = format!("{text}\0").encode_utf16().collect();
    unsafe { MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), MB_OK | icon) };
}

fn main() {
    use windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR;
    if std::env::args().any(|arg| arg == "--uninstall") {
        if let Err(error) = setup::uninstall() {
            message(&format!("{error:#}"), MB_ICONERROR);
        }
        return;
    }
    match setup::self_install() {
        Ok(setup::Launch::Handoff) => return,
        Ok(setup::Launch::Run) => {}
        // Installation is a convenience: run portable if it fails.
        Err(_) => {}
    }
    let after_update = std::env::args().any(|arg| arg == "--after-update");
    let Some(_instance) = platform::single_instance(after_update) else {
        return;
    };
    update::cleanup();
    platform::prioritize_self();
    // Started by Windows at sign-in: stay in the tray.
    let hidden = std::env::args().any(|arg| arg == "--tray");
    if let Err(error) = gui::run(hidden) {
        message(&format!("{error:#}"), MB_ICONERROR);
    }
}
