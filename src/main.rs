#![cfg_attr(not(test), windows_subsystem = "windows")]

mod gui;
mod media;
mod platform;
mod protocol;
mod session;

fn main() {
    if let Err(error) = gui::run() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let title: Vec<u16> = "DuoLink\0".encode_utf16().collect();
        let message: Vec<u16> = format!("{error:#}\0").encode_utf16().collect();
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}
