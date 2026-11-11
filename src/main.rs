#![cfg_attr(not(test), windows_subsystem = "windows")]

mod friends;
mod gui;
mod media;
mod platform;
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
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION};
    if std::env::args().any(|arg| arg == "--uninstall") {
        match setup::uninstall() {
            Ok(()) => message("DuoLink удалён.", MB_ICONINFORMATION),
            Err(error) => message(&format!("{error:#}"), MB_ICONERROR),
        }
        return;
    }
    match setup::self_install() {
        Ok(setup::Launch::Handoff) => return,
        Ok(setup::Launch::Run) => {}
        // Installation is a convenience: run portable if it fails.
        Err(_) => {}
    }
    update::cleanup();
    platform::prioritize_self();
    if let Err(error) = gui::run() {
        message(&format!("{error:#}"), MB_ICONERROR);
    }
}
