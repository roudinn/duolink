//! Discord status ("Rich Presence") over Discord's local pipe.
//!
//! It shows only what the lobby is doing and how many people are in it: no
//! nicknames, no session code. Nothing is sent anywhere but to the Discord
//! app on this PC. Without Discord running it quietly does nothing.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

/// DuoLink's application ID from discord.com/developers. It is public (every
/// player's Discord shows it), not a secret. Empty switches the status off.
pub const APP_ID: &str = "1570522216171446383";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The player switched the status off: it is cleared.
    Off,
    /// DuoLink is open and nothing is going on.
    Idle,
    /// A lobby the player hosts, `players` of `max` including the host.
    Lobby { players: usize, max: usize },
    /// Playing in someone else's lobby.
    Playing,
}

pub struct Presence {
    tx: Sender<Status>,
}

impl Presence {
    pub fn start() -> Option<Self> {
        if APP_ID.is_empty() {
            return None;
        }
        let (tx, rx) = mpsc::channel::<Status>();
        thread::Builder::new()
            .name("duolink-presence".into())
            .spawn(move || {
                let started = unix_now();
                let mut wanted = Status::Idle;
                let mut shown: Option<Status> = None;
                let mut pipe: Option<File> = None;
                loop {
                    match rx.recv_timeout(Duration::from_secs(10)) {
                        Ok(status) => wanted = status,
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    if shown.as_ref() == Some(&wanted) {
                        continue;
                    }
                    // Discord may start later or restart: connect again on the next pass.
                    if pipe.is_none() {
                        pipe = connect();
                    }
                    let Some(file) = pipe.as_mut() else { continue };
                    if send(file, &activity(&wanted, started)).is_ok() {
                        shown = Some(wanted.clone());
                    } else {
                        pipe = None;
                        shown = None;
                    }
                }
            })
            .ok()?;
        Some(Self { tx })
    }

    pub fn set(&self, status: Status) {
        let _ = self.tx.send(status);
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn frame(opcode: u32, payload: &Value) -> Vec<u8> {
    let body = payload.to_string().into_bytes();
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&opcode.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Reads one frame from Discord and returns its JSON text.
fn read_frame(file: &mut File) -> std::io::Result<String> {
    let mut header = [0u8; 8];
    file.read_exact(&mut header)?;
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if length > 64 * 1024 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0u8; length];
    file.read_exact(&mut body)?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn connect() -> Option<File> {
    (0..10).find_map(|index| {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!(r"\\.\pipe\discord-ipc-{index}"))
            .ok()?;
        file.write_all(&frame(0, &json!({ "v": 1, "client_id": APP_ID }))).ok()?;
        read_frame(&mut file).ok()?;
        Some(file)
    })
}

fn activity(status: &Status, started: u64) -> Value {
    let body = match status {
        Status::Off => None,
        Status::Idle => Some(json!({
            "details": "В приложении DuoLink",
            "state": "Игра с друзьями по сети",
            "timestamps": { "start": started },
        })),
        Status::Lobby { players, max } => Some(json!({
            "details": "Лобби DuoLink",
            "state": if *players > 1 { "Играют вместе" } else { "Ждёт игроков" },
            "timestamps": { "start": started },
            "party": { "id": "duolink-lobby", "size": [players, max] },
        })),
        Status::Playing => Some(json!({
            "details": "Играет вместе по DuoLink",
            "timestamps": { "start": started },
        })),
    };
    json!({
        "cmd": "SET_ACTIVITY",
        "args": { "pid": std::process::id(), "activity": body },
        "nonce": format!("{started}-{}", unix_now()),
    })
}

fn send(file: &mut File, message: &Value) -> std::io::Result<String> {
    file.write_all(&frame(1, message))?;
    let reply = read_frame(file)?;
    // Discord answers a rejected status with an ERROR event.
    if reply.contains("\"evt\":\"ERROR\"") {
        return Err(std::io::Error::other(reply));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs the Discord desktop app running"]
    fn discord_accepts_the_status() {
        let mut file = connect().expect("Discord is not running or refused DuoLink's application ID");
        let reply = send(&mut file, &activity(&Status::Lobby { players: 2, max: 4 }, unix_now())).unwrap();
        eprintln!("Discord replied: {reply}");
        assert!(reply.contains("SET_ACTIVITY"));
        std::thread::sleep(Duration::from_secs(4));
        send(&mut file, &activity(&Status::Off, 0)).unwrap();
    }

    #[test]
    fn frames_are_length_prefixed() {
        let bytes = frame(1, &json!({ "a": 1 }));
        assert_eq!(&bytes[..4], &1u32.to_le_bytes());
        assert_eq!(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize, bytes.len() - 8);
    }

    #[test]
    fn status_shows_counts_and_never_names() {
        let lobby = activity(&Status::Lobby { players: 3, max: 5 }, 100).to_string();
        assert!(lobby.contains("[3,5]") && lobby.contains("Играют вместе"));
        let alone = activity(&Status::Lobby { players: 1, max: 5 }, 100).to_string();
        assert!(alone.contains("Ждёт игроков"));
        assert!(activity(&Status::Off, 100)["args"]["activity"].is_null());
        assert!(activity(&Status::Idle, 100).to_string().contains("В приложении DuoLink"));
        assert!(activity(&Status::Playing, 100).to_string().contains("Играет вместе"));
    }
}
