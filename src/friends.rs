//! Friends without accounts or servers of our own.
//!
//! Every install owns an X25519 key pair. The public key plus a display name
//! form a friend code, shared once. Each user has an inbox: an ntfy.sh topic
//! derived from their public key. A message to a friend is sealed with
//! AES-256-GCM under the X25519 shared secret of the two keys, so only the
//! recipient can read it and only the holder of the sender's secret key can
//! have written it. The relay sees opaque bytes.

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{
    session::clean_name,
    tunnel::{SIGNAL_SERVER, curl},
};

const CODE_PREFIX: &str = "DLF-";
/// Invites older than this are stale: the host's invite code has expired.
pub const INVITE_LIFETIME: u64 = 15 * 60;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn short_id(key: &[u8; 32]) -> String {
    key[..6].iter().map(|b| format!("{b:02x}")).collect()
}

/* ------------------------------------------------------------- identity */

#[derive(Serialize, Deserialize)]
struct StoredMe {
    secret: String,
    name: String,
    /// The user picked this nickname (rather than the computer name default).
    #[serde(default)]
    chosen: bool,
}

pub struct Me {
    secret: StaticSecret,
    pub public: [u8; 32],
    /// Nickname shown to friends and hosts.
    pub name: String,
    pub chosen: bool,
    path: PathBuf,
}

impl Me {
    pub fn load_or_create(data: &Path) -> Result<Self> {
        let path = data.join("me.json");
        if let Ok(text) = fs::read_to_string(&path)
            && let Ok(stored) = serde_json::from_str::<StoredMe>(&text)
            && let Ok(bytes) = URL_SAFE_NO_PAD.decode(&stored.secret)
            && let Ok(bytes) = <[u8; 32]>::try_from(bytes.as_slice())
        {
            let secret = StaticSecret::from(bytes);
            return Ok(Self {
                public: PublicKey::from(&secret).to_bytes(),
                secret,
                name: clean_name(&stored.name),
                chosen: stored.chosen,
                path,
            });
        }
        let secret = StaticSecret::random_from_rng(OsRng);
        let me = Self {
            public: PublicKey::from(&secret).to_bytes(),
            secret,
            name: clean_name(&std::env::var("COMPUTERNAME").unwrap_or_default()),
            chosen: false,
            path,
        };
        me.save()?;
        Ok(me)
    }

    fn save(&self) -> Result<()> {
        let stored = StoredMe {
            secret: URL_SAFE_NO_PAD.encode(self.secret.to_bytes()),
            name: self.name.clone(),
            chosen: self.chosen,
        };
        fs::create_dir_all(self.path.parent().unwrap())?;
        fs::write(&self.path, serde_json::to_vec_pretty(&stored)?)?;
        Ok(())
    }

    pub fn rename(&mut self, name: &str) -> Result<()> {
        let name = clean_name(name);
        ensure!(name.chars().count() >= 2, "Ник должен быть не короче двух символов.");
        self.name = name;
        self.chosen = true;
        self.save()
    }

    /// What a friend pastes to add us.
    pub fn code(&self) -> String {
        let mut bytes = self.public.to_vec();
        bytes.extend_from_slice(self.name.as_bytes());
        format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
    }

    fn cipher_with(&self, peer: &[u8; 32]) -> LessSafeKey {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*peer));
        let mut hash = Sha256::new();
        hash.update(b"duolink/friend/v1");
        hash.update(shared.as_bytes());
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &hash.finalize()).unwrap())
    }

    /// Envelope: sender public key ‖ nonce ‖ AES-GCM(message).
    pub fn seal(&self, to: &[u8; 32], message: &Message) -> String {
        let mut body = serde_json::to_vec(message).unwrap();
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        self.cipher_with(to)
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(to), &mut body)
            .unwrap();
        let mut envelope = self.public.to_vec();
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&body);
        URL_SAFE_NO_PAD.encode(envelope)
    }

    /// Opens an envelope addressed to us; returns the authenticated sender.
    pub fn open(&self, text: &str) -> Option<([u8; 32], Message)> {
        let mut bytes = URL_SAFE_NO_PAD.decode(text.trim()).ok()?;
        if bytes.len() < 32 + 12 + 16 {
            return None;
        }
        let sender: [u8; 32] = bytes[..32].try_into().ok()?;
        if sender == self.public {
            return None;
        }
        let nonce = Nonce::try_assume_unique_for_key(&bytes[32..44]).ok()?;
        let plain = self
            .cipher_with(&sender)
            .open_in_place(nonce, Aad::from(&self.public), &mut bytes[44..])
            .ok()?;
        Some((sender, serde_json::from_slice(plain).ok()?))
    }
}

pub fn parse_code(code: &str) -> Result<([u8; 32], String)> {
    let payload = code
        .trim()
        .strip_prefix(CODE_PREFIX)
        .context("Код друга начинается с DLF-. Код сессии (DL2-) вставляется в «Подключиться к другу».")?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok().context("Повреждённый код друга.")?;
    ensure!(bytes.len() >= 32 && bytes.len() <= 32 + 120, "Повреждённый код друга.");
    let key: [u8; 32] = bytes[..32].try_into()?;
    let name = clean_name(&String::from_utf8_lossy(&bytes[32..]));
    Ok((key, name))
}

/* -------------------------------------------------------------- friends */

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Mutual friends.
    Friend,
    /// They asked; we have not answered.
    Incoming,
    /// We asked; waiting for them.
    Outgoing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Friend {
    pub key: String,
    pub name: String,
    pub status: Status,
}

impl Friend {
    pub fn public(&self) -> Option<[u8; 32]> {
        URL_SAFE_NO_PAD.decode(&self.key).ok()?.try_into().ok()
    }

    pub fn id(&self) -> String {
        self.public().map(|key| short_id(&key)).unwrap_or_default()
    }
}

#[derive(Default)]
pub struct Book {
    path: PathBuf,
    pub entries: Vec<Friend>,
}

impl Book {
    pub fn load(data: &Path) -> Self {
        let path = data.join("friends.json");
        let entries = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { path, entries }
    }

    fn save(&self) {
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.entries) {
            let _ = fs::write(&self.path, bytes);
        }
    }

    pub fn find(&self, key: &[u8; 32]) -> Option<&Friend> {
        self.entries.iter().find(|f| f.public().as_ref() == Some(key))
    }

    pub fn by_id(&self, id: &str) -> Option<Friend> {
        self.entries.iter().find(|f| f.id() == id).cloned()
    }

    pub fn set(&mut self, key: &[u8; 32], name: &str, status: Status) {
        let name = clean_name(name);
        match self.entries.iter_mut().find(|f| f.public().as_ref() == Some(key)) {
            Some(entry) => {
                entry.name = name;
                entry.status = status;
            }
            None => self.entries.push(Friend {
                key: URL_SAFE_NO_PAD.encode(key),
                name,
                status,
            }),
        }
        self.save();
    }

    pub fn remove(&mut self, key: &[u8; 32]) {
        self.entries.retain(|f| f.public().as_ref() != Some(key));
        self.save();
    }
}

/* ------------------------------------------------------------- messages */

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    FriendRequest { name: String },
    FriendAccept { name: String },
    FriendRemove,
    /// The sender changed their nickname.
    Rename { name: String },
    /// A session invite; `code` is the one-time DL2 code.
    Invite { name: String, code: String, at: u64 },
    InviteDecline { at: u64 },
    InviteCancel { at: u64 },
}

pub fn inbox_topic(key: &[u8; 32]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"duolink/inbox/v1");
    hash.update(key);
    let digest = hash.finalize();
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("duolink-u-{hex}")
}

/// Posts a sealed message to a friend's inbox in the background.
pub fn send(envelope: String, to: &[u8; 32]) {
    let url = format!("{SIGNAL_SERVER}/{}", inbox_topic(to));
    thread::spawn(move || {
        let Ok(mut command) = curl() else { return };
        let Ok(mut child) = command
            .args(["--max-time", "15", "--data-binary", "@-"])
            .arg(url)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
        else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(envelope.as_bytes());
        }
        let _ = child.wait();
    });
}

/// Listens to our inbox for as long as it lives, reconnecting as needed.
/// Raw envelopes come out of `messages`; the GUI opens them with `Me`.
pub struct Inbox {
    pub messages: Receiver<String>,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Inbox {
    pub fn listen(me: &[u8; 32], wake: impl Fn() + Send + 'static) -> Self {
        let url = format!("{SIGNAL_SERVER}/{}/json", inbox_topic(me));
        let (sender, messages): (Sender<String>, _) = mpsc::channel();
        let child = Arc::new(Mutex::new(None::<Child>));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let child = child.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut seen = HashSet::new();
                // On start, pick up what arrived while DuoLink was closed.
                let mut since = format!("{}m", INVITE_LIFETIME / 60);
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let spawned = curl().ok().and_then(|mut command| {
                        command
                            .args(["--no-buffer", "--max-time", "900"])
                            .arg(format!("{url}?since={since}"))
                            .stdin(Stdio::null())
                            .stdout(Stdio::piped())
                            .spawn()
                            .ok()
                    });
                    let Some(mut process) = spawned else {
                        thread::sleep(Duration::from_secs(5));
                        continue;
                    };
                    let stdout = process.stdout.take();
                    *child.lock().unwrap() = Some(process);
                    if let Some(stdout) = stdout {
                        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                                continue;
                            };
                            if event["event"] != "message" {
                                continue;
                            }
                            let id = event["id"].as_str().unwrap_or_default().to_string();
                            if !id.is_empty() {
                                since = id.clone();
                            }
                            if !seen.insert(id) {
                                continue;
                            }
                            if let Some(text) = event["message"].as_str() {
                                if sender.send(text.to_string()).is_err() {
                                    return;
                                }
                                wake();
                            }
                        }
                    }
                    if let Some(mut process) = child.lock().unwrap().take() {
                        let _ = process.kill();
                        let _ = process.wait();
                    }
                    thread::sleep(Duration::from_secs(2));
                }
            });
        }
        Self { messages, child, stop }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(mut process) = self.child.lock().unwrap().take() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(dir: &Path, name: &str) -> Me {
        let mut me = Me::load_or_create(dir).unwrap();
        me.rename(name).unwrap();
        me
    }

    #[test]
    fn identity_persists_and_codes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let first = me(dir.path(), "Артём");
        let again = Me::load_or_create(dir.path()).unwrap();
        assert_eq!(first.public, again.public);
        assert_eq!(again.name, "Артём");
        let (key, name) = parse_code(&first.code()).unwrap();
        assert_eq!(key, first.public);
        assert_eq!(name, "Артём");
        assert!(parse_code("DL2-abc").is_err());
    }

    #[test]
    fn only_the_recipient_opens_and_the_sender_is_authenticated() {
        let (a_dir, b_dir, c_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (alice, bob, carol) = (me(a_dir.path(), "Alice"), me(b_dir.path(), "Bob"), me(c_dir.path(), "Carol"));
        let message = Message::Invite { name: "Alice".into(), code: "DL2-xyz".into(), at: 42 };
        let sealed = alice.seal(&bob.public, &message);
        assert_eq!(bob.open(&sealed), Some((alice.public, message)));
        assert_eq!(carol.open(&sealed), None);
        assert_eq!(alice.open(&sealed), None);

        // Claiming to be Alice without her key fails authentication.
        let mut forged = URL_SAFE_NO_PAD.decode(carol.seal(&bob.public, &Message::FriendRemove)).unwrap();
        forged[..32].copy_from_slice(&alice.public);
        assert_eq!(bob.open(&URL_SAFE_NO_PAD.encode(forged)), None);
    }

    /// Two identities talk through their real ntfy.sh inboxes.
    #[test]
    #[ignore = "uses ntfy.sh over the internet"]
    fn live_request_accept_invite() {
        let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (alice, bob) = (me(a_dir.path(), "Alice"), me(b_dir.path(), "Bob"));
        let alice_inbox = Inbox::listen(&alice.public, || {});
        let bob_inbox = Inbox::listen(&bob.public, || {});
        thread::sleep(Duration::from_secs(2));

        let receive = |inbox: &Inbox, who: &Me| {
            let started = std::time::Instant::now();
            let envelope = inbox.messages.recv_timeout(Duration::from_secs(15)).expect("no message");
            eprintln!("delivered in {} ms", started.elapsed().as_millis());
            who.open(&envelope).expect("cannot open")
        };

        // Bob pastes Alice's friend code and sends a request.
        let (alice_key, alice_name) = parse_code(&alice.code()).unwrap();
        assert_eq!(alice_name, "Alice");
        send(bob.seal(&alice_key, &Message::FriendRequest { name: bob.name.clone() }), &alice_key);
        let (from, message) = receive(&alice_inbox, &alice);
        assert_eq!(from, bob.public);
        assert_eq!(message, Message::FriendRequest { name: "Bob".into() });

        send(alice.seal(&bob.public, &Message::FriendAccept { name: alice.name.clone() }), &bob.public);
        assert_eq!(receive(&bob_inbox, &bob).1, Message::FriendAccept { name: "Alice".into() });

        let invite = Message::Invite { name: "Alice".into(), code: "DL2-test".into(), at: now() };
        send(alice.seal(&bob.public, &invite), &bob.public);
        assert_eq!(receive(&bob_inbox, &bob), (alice.public, invite));
    }

    #[test]
    fn book_tracks_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut book = Book::load(dir.path());
        let key = [9u8; 32];
        book.set(&key, "Друг", Status::Incoming);
        book.set(&key, "Друг", Status::Friend);
        let reloaded = Book::load(dir.path());
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries[0].status, Status::Friend);
        assert_eq!(reloaded.by_id(&short_id(&key)).unwrap().name, "Друг");
        book.remove(&key);
        assert!(Book::load(dir.path()).entries.is_empty());
    }
}
