//! Friends without accounts. The server stores public keys and sealed messages.
//!
//! Every install owns an X25519 key pair. A user is `nickname#tag`, where the
//! four-digit tag is derived from the public key. Two kinds of
//! topics, all derived by hashing, carry everything:
//!
//! * directory — `nickname#tag` → public key, so friends can be added by name;
//! * inbox — one per key; messages are sealed with AES-256-GCM under the
//!   X25519 shared secret, which hides them and authenticates the sender.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc::{self, Receiver}},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{infra::{self, Subscription}, platform::user_protected_data};

const CODE_PREFIX: &str = "DLF-";
const PROTECTED_SECRET: &str = "dpapi:";
const DEFAULT_NAME: &str = "player";
/// Invites older than this are stale: the host's session code has expired.
pub const INVITE_LIFETIME: u64 = 15 * 60;
const MAIL_WINDOW: u64 = 12 * 3600;
/// Refresh the directory entry periodically while DuoLink runs.
pub const DIRECTORY_REFRESH: Duration = Duration::from_secs(4 * 3600);
pub const DIRECTORY_RETRY: Duration = Duration::from_secs(30);

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest(label: &[u8], data: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(label);
    hash.update(data);
    hash.finalize().into()
}

pub fn short_id(key: &[u8; 32]) -> String {
    hex(&key[..6])
}

/* ------------------------------------------------------------- nicknames */

/// Latin letters, digits, `_`, `.` and `-`; 3 to 16 characters.
pub fn valid_nickname(text: &str) -> Result<String> {
    let name = text.trim();
    ensure!(
        (3..=16).contains(&name.chars().count()),
        "Ник — от 3 до 16 символов."
    );
    ensure!(
        name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
        "Только латинские буквы, цифры и _ . -"
    );
    Ok(name.to_string())
}

/// The four digits after `#`, fixed by the public key.
pub fn tag(key: &[u8; 32]) -> u16 {
    let hash = digest(b"duolink/tag/v1", key);
    (u32::from_be_bytes(hash[..4].try_into().unwrap()) % 10_000) as u16
}

/// Creator nicknames get a fixed, memorable tag instead of the key-derived one.
/// Everyone else keeps the tag derived from their public key.
const CREATORS: [(&str, &str); 2] = [("roud", "1"), ("maxudgorade", "05")];

fn creator_tag(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    CREATORS.iter().find(|(nick, _)| *nick == lower).map(|(_, tag)| *tag)
}

/// The numeric tag used in the directory: fixed for creators, key-derived otherwise.
pub fn tag_for(name: &str, key: &[u8; 32]) -> u16 {
    match creator_tag(name) {
        Some(fixed) => fixed.parse().unwrap_or_else(|_| tag(key)),
        None => tag(key),
    }
}

/// The tag as shown after `#`: creators keep their short form, others use 4 digits.
pub fn tag_label(name: &str, key: &[u8; 32]) -> String {
    match creator_tag(name) {
        Some(fixed) => fixed.to_string(),
        None => format!("{:04}", tag(key)),
    }
}

pub fn handle(name: &str, key: &[u8; 32]) -> String {
    format!("{name}#{}", tag_label(name, key))
}

/// Parses `nickname#1234` (creators may use a short `#1` / `#05`).
pub fn parse_handle(text: &str) -> Result<(String, u16)> {
    let (name, number) = text.trim().rsplit_once('#').context("Нужен ник с тегом: name#1234")?;
    let name = valid_nickname(name)?;
    ensure!(
        (1..=4).contains(&number.len()) && number.chars().all(|c| c.is_ascii_digit()),
        "После # — до четырёх цифр."
    );
    Ok((name, number.parse()?))
}

/* -------------------------------------------------------------- identity */

#[derive(Serialize, Deserialize)]
struct StoredMe {
    secret: String,
    name: String,
    #[serde(default)]
    chosen: bool,
}

pub struct Me {
    secret: StaticSecret,
    pub public: [u8; 32],
    pub name: String,
    /// The user chose a public nickname for the friends directory.
    pub chosen: bool,
    path: PathBuf,
}

impl Me {
    pub fn load_or_create(data: &Path) -> Result<Self> {
        let path = data.join("me.json");
        let stored = match fs::read_to_string(&path) {
            Ok(text) => Some(serde_json::from_str::<StoredMe>(&text).context("Файл профиля DuoLink повреждён.")?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("Не удалось прочитать профиль DuoLink."),
        };
        if let Some(stored) = stored {
            let protected = stored.secret.strip_prefix(PROTECTED_SECRET);
            let mut bytes = URL_SAFE_NO_PAD.decode(protected.unwrap_or(&stored.secret))
                .context("Ключ профиля DuoLink повреждён.")?;
            if protected.is_some() {
                bytes = user_protected_data(&bytes, false)
                    .context("Не удалось открыть ключ профиля DuoLink под этой учётной записью Windows.")?;
            }
            let bytes = <[u8; 32]>::try_from(bytes.as_slice())
                .map_err(|_| anyhow::anyhow!("Ключ профиля DuoLink повреждён."))?;
            let secret = StaticSecret::from(bytes);
            // Nicknames from older versions may not be Latin: ask again.
            let valid = valid_nickname(&stored.name).ok();
            let me = Self {
                public: PublicKey::from(&secret).to_bytes(),
                secret,
                chosen: stored.chosen && valid.is_some(),
                name: valid.unwrap_or_else(|| DEFAULT_NAME.into()),
                path,
            };
            if protected.is_none() {
                me.save(&me.name, me.chosen)?;
            }
            return Ok(me);
        }
        let secret = StaticSecret::random_from_rng(OsRng);
        let me = Self {
            public: PublicKey::from(&secret).to_bytes(),
            secret,
            name: DEFAULT_NAME.into(),
            chosen: false,
            path,
        };
        me.save(&me.name, false)?;
        Ok(me)
    }

    fn save(&self, name: &str, chosen: bool) -> Result<()> {
        let protected = user_protected_data(&self.secret.to_bytes(), true)
            .context("Не удалось защитить ключ профиля DuoLink.")?;
        let stored = StoredMe {
            secret: format!("{PROTECTED_SECRET}{}", URL_SAFE_NO_PAD.encode(protected)),
            name: name.to_owned(),
            chosen,
        };
        let parent = self.path.parent().unwrap();
        fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(file.as_file_mut(), &stored)?;
        file.persist(&self.path)?;
        Ok(())
    }

    pub fn rename(&mut self, name: &str) -> Result<()> {
        let name = valid_nickname(name)?;
        self.save(&name, true).context("Не удалось сохранить ник.")?;
        self.name = name;
        self.chosen = true;
        Ok(())
    }

    pub fn handle(&self) -> String {
        handle(&self.name, &self.public)
    }

    /// Fallback for adding a friend when the directory cannot be reached.
    pub fn code(&self) -> String {
        let mut bytes = self.public.to_vec();
        bytes.extend_from_slice(self.name.as_bytes());
        format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
    }

    fn cipher_with(&self, peer: &[u8; 32]) -> Option<LessSafeKey> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*peer));
        if !shared.was_contributory() {
            return None;
        }
        let key = digest(b"duolink/friend/v1", shared.as_bytes());
        Some(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).unwrap()))
    }

    /// Envelope: sender public key ‖ nonce ‖ AES-GCM(message).
    pub fn seal(&self, to: &[u8; 32], message: &Message) -> Result<String> {
        let key = self.cipher_with(to).context("Ключ друга недействителен.")?;
        let mut body = serde_json::to_vec(message).unwrap();
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(to), &mut body)
            .unwrap();
        let mut envelope = self.public.to_vec();
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&body);
        Ok(URL_SAFE_NO_PAD.encode(envelope))
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
            .cipher_with(&sender)?
            .open_in_place(nonce, Aad::from(&self.public), &mut bytes[44..])
            .ok()?;
        Some((sender, serde_json::from_slice(plain).ok()?))
    }
}

/// Reads a `DLF-` friend code.
pub fn parse_code(code: &str) -> Result<([u8; 32], String)> {
    let payload = code.trim().strip_prefix(CODE_PREFIX).context("Это не код друга.")?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok().context("Код друга повреждён.")?;
    ensure!(bytes.len() >= 32 && bytes.len() <= 32 + 64, "Код друга повреждён.");
    let key: [u8; 32] = bytes[..32].try_into()?;
    let name = valid_nickname(&String::from_utf8_lossy(&bytes[32..])).unwrap_or_else(|_| "friend".into());
    Ok((key, name))
}

/* --------------------------------------------------------------- friends */

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
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

    pub fn handle(&self) -> String {
        self.public().map(|key| handle(&self.name, &key)).unwrap_or_else(|| self.name.clone())
    }
}

/// Names from the wire: kept only if they are valid nicknames.
fn received_name(name: &str) -> String {
    valid_nickname(name).unwrap_or_else(|_| "friend".into())
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
        let name = received_name(name);
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

/* -------------------------------------------------------------- messages */

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    FriendRequest { name: String },
    FriendAccept { name: String },
    FriendRemove,
    Rename { name: String },
    /// A session invite; `code` is the one-time DL2 code.
    Invite { name: String, code: String, at: u64 },
    InviteDecline { at: u64 },
    InviteCancel { at: u64 },
}

fn topic(kind: &str, data: &[u8]) -> String {
    let hash = digest(format!("duolink/{kind}/v1").as_bytes(), data);
    format!("duolink-{}-{}", &kind[..1], hex(&hash[..16]))
}

pub fn inbox_topic(key: &[u8; 32]) -> String {
    topic("inbox", key)
}

fn directory_topic(name: &str, tag: u16) -> String {
    topic("directory", format!("{}#{tag:04}", name.to_ascii_lowercase()).as_bytes())
}

fn post(topic: &str, body: &str) -> Result<()> {
    for attempt in 0..3 {
        if attempt > 0 {
            thread::sleep(Duration::from_secs(2 * attempt));
        }
        match infra::post(topic, body, "10").unwrap_or(0) {
            200..=299 => return Ok(()),
            429 if attempt == 2 => bail!("Лимит отправки. Попробуйте позже."),
            429 => {}
            400..=499 => bail!("Сервис отклонил сообщение."),
            _ => {}
        }
    }
    bail!("Не удалось отправить сообщение.")
}

fn post_background(topic: String, body: String) {
    thread::spawn(move || {
        let _ = post(&topic, &body);
    });
}

/// Sends a sealed message to a friend's inbox.
pub fn send(envelope: String, to: &[u8; 32]) {
    post_background(inbox_topic(to), envelope);
}

/// Waits for the server to accept a message. Call off the UI thread.
pub fn deliver(envelope: &str, to: &[u8; 32]) -> Result<()> {
    post(&inbox_topic(to), envelope)
}

/// Lists us in the directory under `name#tag`. Call off the UI thread.
pub fn announce(name: &str, public: &[u8; 32]) -> Result<()> {
    post(&directory_topic(name, tag_for(name, public)), &URL_SAFE_NO_PAD.encode(public))
}

/// The directory commits a successful POST before replying.
pub fn publish_nickname(name: &str, public: &[u8; 32]) -> Result<()> {
    let topic = directory_topic(name, tag_for(name, public));
    let envelope = URL_SAFE_NO_PAD.encode(public);
    let mut last_status = 0u16;
    for attempt in 0..3 {
        if attempt > 0 {
            thread::sleep(Duration::from_secs(2 * attempt));
        }
        match infra::post(&topic, &envelope, "5") {
            Ok(status) if (200..=299).contains(&status) => return Ok(()),
            Ok(status) if (500..=599).contains(&status) || status == 429 => last_status = status,
            Ok(0) => {
                if attempt == 2 {
                    bail!("Connection or DNS lookup timed out");
                }
            }
            Ok(status) => bail!("HTTP {status}"),
            Err(error) if attempt == 2 => return Err(error),
            Err(_) => {}
        }
    }
    if last_status == 429 {
        bail!("Лимит отправки. Попробуйте позже.");
    }
    bail!("Сервис временно недоступен. Попробуйте позже.")
}

/// Finds the public key behind `name#tag`.
pub fn lookup(name: &str, wanted: u16) -> Result<[u8; 32]> {
    let topic = directory_topic(name, wanted);
    let mut keys: HashSet<[u8; 32]> = HashSet::new();
    for attempt in 0..3 {
        if attempt > 0 {
            thread::sleep(Duration::from_secs(2 * attempt));
        }
        match infra::history(&topic, "10") {
            Ok(events) => {
                keys = events
                    .into_iter()
                    .filter(|event| event.topic == topic)
                    .filter_map(|event| URL_SAFE_NO_PAD.decode(event.message.trim()).ok())
                    .filter_map(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
                    // The tag is a function of the key: an entry that does not match is forged.
                    .filter(|key| tag_for(name, key) == wanted)
                    .collect();
                break;
            }
            Err(error) if attempt == 2 => return Err(error),
            Err(_) => {}
        }
    }
    match keys.len() {
        1 => Ok(keys.into_iter().next().unwrap()),
        0 => bail!("Ник не найден. Попросите друга открыть DuoLink или прислать код друга из настроек."),
        _ => bail!("Ник занят несколькими людьми. Попросите код друга."),
    }
}

/* ----------------------------------------------------------------- inbox */

pub struct Incoming {
    pub time: u64,
    pub text: String,
}

/// One long-lived subscription to our inbox.
pub struct Inbox {
    pub messages: Receiver<Incoming>,
    _subscription: Subscription,
}

impl Inbox {
    /// `since`: latest mail time from the saved history.
    pub fn listen(me: &[u8; 32], since: u64, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (sender, messages) = mpsc::channel();
        let subscription = Subscription::start(inbox_topic(me), since, move |event| {
            if sender.send(Incoming { time: event.time, text: event.message }).is_ok() {
                wake();
            }
        });
        Self { messages, _subscription: subscription }
    }
}

/// Track processed envelopes across reconnects and app restarts.
#[derive(Default, Deserialize, Serialize)]
pub struct MailHistory {
    legacy_through: u64,
    received: HashMap<String, u64>,
    #[serde(default)]
    cancelled_invites: HashMap<String, u64>,
}

impl MailHistory {
    pub fn load(data: &Path) -> Self {
        let Ok(bytes) = fs::read(data.join("inbox.json")) else { return Self::default() };
        serde_json::from_slice(&bytes).unwrap_or_else(|_| Self {
            legacy_through: serde_json::from_slice(&bytes).unwrap_or(0),
            ..Self::default()
        })
    }

    pub fn latest(&self) -> u64 {
        self.received.values().copied().max().unwrap_or(0).max(self.legacy_through)
    }

    pub fn remember(&mut self, time: u64, text: &str) -> bool {
        // Keep IDs through the replay window and the maximum POST retry time.
        // The server already limits the replay window. Use its timestamps
        // here as well so an incorrect Windows clock doesn't hide requests.
        let floor = self.latest().max(time).saturating_sub(MAIL_WINDOW + 1);
        self.received.retain(|_, at| *at >= floor.saturating_sub(60));
        self.cancelled_invites.retain(|_, at| *at >= floor);
        if time < floor || time <= self.legacy_through {
            return false;
        }
        let id = hex(&digest(b"duolink/mail/v1", text.trim().as_bytes()));
        match self.received.entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(time);
                true
            }
        }
    }

    pub fn cancel_invite(&mut self, id: &str, time: u64) {
        self.cancelled_invites.insert(id.to_owned(), time);
    }

    pub fn invite_cancelled(&self, id: &str) -> bool {
        self.cancelled_invites.contains_key(id)
    }

    pub fn save(&self, data: &Path) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(data)?;
        serde_json::to_writer(file.as_file_mut(), self)?;
        file.persist(data.join("inbox.json"))?;
        Ok(())
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
    fn mail_history_keeps_distinct_messages_in_the_same_second() {
        let dir = tempfile::tempdir().unwrap();
        let time = now();
        let mut history = MailHistory::default();
        assert!(history.remember(time, "first"));
        history.save(dir.path()).unwrap();

        let mut history = MailHistory::load(dir.path());
        assert_eq!(history.latest(), time);
        assert!(history.remember(time, "second"));
        assert!(!history.remember(time + 1, "first"));
        assert!(history.remember(time - 1, "late"));
    }

    #[test]
    fn mail_history_keeps_cancelled_invites_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let time = now();
        let mut history = MailHistory::default();
        history.cancel_invite("alice-10", time);
        history.save(dir.path()).unwrap();
        let history = MailHistory::load(dir.path());
        assert!(history.invite_cancelled("alice-10"));
        assert!(!history.invite_cancelled("alice-11"));
        assert!(!history.invite_cancelled("bob-10"));
    }

    #[test]
    fn mail_history_loads_the_old_numeric_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let time = now();
        fs::write(dir.path().join("inbox.json"), time.to_string()).unwrap();
        let mut history = MailHistory::load(dir.path());
        assert!(!history.remember(time, "old"));
        assert!(history.remember(time + 1, "new"));
        history.save(dir.path()).unwrap();
        let mut history = MailHistory::load(dir.path());
        assert!(!history.remember(time + 1, "new"));
        assert!(history.remember(time + 1, "another"));
    }

    #[test]
    fn nicknames_are_latin_and_handles_round_trip() {
        assert!(valid_nickname("roud").is_ok());
        assert!(valid_nickname("ro").is_err());
        assert!(valid_nickname("Артём").is_err());
        assert!(valid_nickname("a b c").is_err());
        assert!(valid_nickname("player_one.2-x").is_ok());
        assert!(valid_nickname("seventeen_chars_x").is_err());
        let key = [3u8; 32];
        let text = handle("bob", &key);
        assert_eq!(parse_handle(&text).unwrap(), ("bob".into(), tag(&key)));
        assert!(parse_handle("bob#12345").is_err());
        assert!(parse_handle("bob").is_err());
        // Creators keep their short, fixed tags.
        assert_eq!(handle("roud", &key), "roud#1");
        assert_eq!(parse_handle("roud#1").unwrap(), ("roud".into(), 1));
        assert_eq!(handle("maxudgorade", &key), "maxudgorade#05");
        assert_eq!(parse_handle("maxudgorade#05").unwrap(), ("maxudgorade".into(), 5));
        // Different keys almost always get different tags.
        let tags: HashSet<u16> = (0u8..50).map(|i| tag(&[i; 32])).collect();
        assert!(tags.len() > 45);
    }

    #[test]
    fn identity_persists_and_old_non_latin_names_are_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let first = me(dir.path(), "roud");
        let again = Me::load_or_create(dir.path()).unwrap();
        assert_eq!((first.public, again.name.as_str(), again.chosen), (again.public, "roud", true));
        let (key, name) = parse_code(&first.code()).unwrap();
        assert_eq!((key, name.as_str()), (first.public, "roud"));

        let stored = serde_json::json!({ "secret": URL_SAFE_NO_PAD.encode([1u8; 32]), "name": "Артём", "chosen": true });
        fs::write(dir.path().join("me.json"), stored.to_string()).unwrap();
        let legacy = Me::load_or_create(dir.path()).unwrap();
        assert!(!legacy.chosen);
        assert!(valid_nickname(&legacy.name).is_ok());
    }

    #[test]
    fn only_the_recipient_opens_and_the_sender_is_authenticated() {
        let (a, b, c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (alice, bob, carol) = (me(a.path(), "alice"), me(b.path(), "bob"), me(c.path(), "carol"));
        let message = Message::Invite { name: "alice".into(), code: "DL2-xyz".into(), at: 42 };
        let sealed = alice.seal(&bob.public, &message).unwrap();
        assert_eq!(bob.open(&sealed), Some((alice.public, message)));
        assert_eq!(carol.open(&sealed), None);
        assert!(alice.seal(&[0; 32], &Message::FriendRemove).is_err());
        assert_eq!(alice.open(&sealed), None);
        let mut forged = URL_SAFE_NO_PAD.decode(carol.seal(&bob.public, &Message::FriendRemove).unwrap()).unwrap();
        forged[..32].copy_from_slice(&alice.public);
        assert_eq!(bob.open(&URL_SAFE_NO_PAD.encode(forged)), None);
    }

    #[test]
    fn legacy_identity_is_protected_without_changing_its_public_key() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("me.json");
        let original = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&original).to_bytes();
        let legacy = URL_SAFE_NO_PAD.encode(original.to_bytes());
        let profile = serde_json::json!({ "secret": legacy, "name": "mike", "chosen": true });
        fs::write(&path, profile.to_string()).unwrap();

        let loaded = Me::load_or_create(directory.path()).unwrap();
        assert_eq!(loaded.handle(), handle("mike", &public));
        assert!(loaded.chosen);
        let now = fs::read_to_string(&path).unwrap();
        assert!(now.contains(PROTECTED_SECRET));
        assert!(!now.contains(&legacy));
        assert_eq!(Me::load_or_create(directory.path()).unwrap().public, public);

        fs::write(&path, profile.to_string().replace(&legacy, "dpapi:invalid")).unwrap();
        let damaged = fs::read(&path).unwrap();
        assert!(Me::load_or_create(directory.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), damaged);
    }

    #[test]
    fn book_tracks_status_and_sanitises_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut book = Book::load(dir.path());
        let key = [9u8; 32];
        book.set(&key, "mike", Status::Incoming);
        book.set(&key, "mike", Status::Friend);
        let reloaded = Book::load(dir.path());
        assert_eq!(reloaded.entries[0].status, Status::Friend);
        assert_eq!(reloaded.entries[0].public(), Some(key));
        book.set(&[8u8; 32], "<script>", Status::Incoming);
        assert_eq!(Book::load(dir.path()).find(&[8u8; 32]).unwrap().name, "friend");
        book.remove(&key);
        assert_eq!(Book::load(dir.path()).entries.len(), 1);
    }

    /// Two identities: directory lookup and friend requests through the VPS.
    #[test]
    #[ignore = "uses the DuoLink signaling service over the internet"]
    fn live_directory_and_mail() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let suffix = &hex(&rand::random::<[u8; 3]>())[..5];
        let (alice, bob) = (me(a.path(), &format!("alice{suffix}")), me(b.path(), &format!("bob{suffix}")));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let alice_inbox = Inbox::listen(&alice.public, 0, wake.clone());
        let bob_inbox = Inbox::listen(&bob.public, 0, wake);
        publish_nickname(&alice.name, &alice.public).unwrap();

        let (name, number) = parse_handle(&alice.handle()).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(lookup(&name, number).unwrap(), alice.public);
        eprintln!("lookup {} ms", started.elapsed().as_millis());
        assert!(lookup(&name, (number + 1) % 10_000).is_err());

        let mail = |inbox: &Inbox, who: &Me| {
            let message = inbox.messages.recv_timeout(Duration::from_secs(15)).expect("nothing arrived");
            who.open(&message.text).expect("cannot open")
        };
        send(bob.seal(&alice.public, &Message::FriendRequest { name: bob.name.clone() }).unwrap(), &alice.public);
        assert_eq!(mail(&alice_inbox, &alice).0, bob.public);
        send(alice.seal(&bob.public, &Message::FriendAccept { name: alice.name.clone() }).unwrap(), &bob.public);
        assert!(matches!(mail(&bob_inbox, &bob).1, Message::FriendAccept { .. }));
    }
}
