use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tao::{
    dpi::{LogicalSize, PhysicalPosition},
    event::{Event as WindowEvent, WindowEvent as NativeEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::windows::IconExtWindows,
    window::{Icon, Theme, UserAttentionType, WindowBuilder},
};
use wry::{
    NewWindowResponse, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows,
    http::Response,
};

use crate::{
    friends::{
        self, Book, DIRECTORY_REFRESH, HEARTBEAT, INVITE_LIFETIME, Inbox, Incoming, MailHistory, Me,
        Message, ONLINE_WINDOW, Status,
    },
    media::{Form, Preferences},
    platform::{data_dir, open},
    protocol::Invite,
    session::{Event, Session},
    setup::{self, Components},
    update::{self, Updater},
};

const VIEW: &str = "duolink://localhost/index.html";
const WINDOWS_VIEW: &str = "https://duolink.localhost/index.html";
const GRAPHICS: &str = "ms-settings:display-advancedgraphics";

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    Ready,
    Refresh,
    HostStart,
    HostStop,
    Allow,
    Deny,
    Copy,
    GuestStart { code: String, options: Form },
    GuestStop,
    Paste,
    Install,
    Graphics,
    CheckUpdate,
    Restart,
    Logs,
    SetName { name: String },
    AddFriend { query: String },
    AcceptFriend { id: String },
    RemoveFriend { id: String },
    CopyHandle,
    CopyCode,
    InviteFriend { id: String },
    AcceptInvite { options: Form },
    DeclineInvite,
    /// Sent by background threads to wake the event loop.
    #[serde(skip_deserializing)]
    Wake,
}

#[derive(Serialize)]
struct FriendView {
    id: String,
    name: String,
    tag: String,
    status: Status,
    online: bool,
    sending: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MeView {
    name: String,
    tag: String,
    chosen: bool,
}

/// A friend's invite waiting for an answer.
#[derive(Clone, Serialize)]
struct Invitation {
    name: String,
    #[serde(skip)]
    key: [u8; 32],
    #[serde(skip)]
    code: String,
    #[serde(skip)]
    at: u64,
}

/// The friend a running host session was created for.
#[derive(Clone, Serialize)]
struct HostFor {
    name: String,
    #[serde(skip)]
    key: [u8; 32],
    #[serde(skip)]
    sent_at: Option<u64>,
    #[serde(skip)]
    joined: bool,
    #[serde(skip)]
    send_error: Option<String>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Idle,
    Working,
    Invite,
    Approval,
    Live,
    Stopping,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Pane {
    phase: Phase,
    message: String,
    code: String,
    peer: String,
    error: bool,
    code_error: bool,
    facts: Vec<(&'static str, String)>,
    #[serde(skip)]
    session: Option<Session>,
}

impl Pane {
    fn set(&mut self, phase: Phase, message: impl Into<String>) {
        self.phase = phase;
        self.message = message.into();
        self.code.clear();
        self.peer.clear();
        self.error = false;
        self.code_error = false;
    }

    fn begin(&mut self, session: Session, message: &str) {
        self.set(Phase::Working, message);
        self.facts.clear();
        self.session = Some(session);
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.set(Phase::Idle, message);
        self.error = true;
    }

    fn stop(&mut self) {
        if let Some(session) = &self.session {
            session.cancel();
            self.set(Phase::Stopping, "Завершаем…");
        }
    }

    fn decide(&mut self, accept: bool) {
        if self.phase == Phase::Approval
            && let Some(session) = &self.session
        {
            session.decide(accept);
            self.set(Phase::Working, if accept { "Запускаем…" } else { "Отклоняем…" });
        }
    }

    /// Applies queued session events. Returns (changed, finished).
    fn poll(&mut self, host: bool) -> (bool, bool) {
        let Some(session) = &self.session else {
            return (false, false);
        };
        let events: Vec<Event> = session.events.try_iter().collect();
        let mut finished = false;
        for event in &events {
            if self.phase == Phase::Stopping && !matches!(event, Event::Finished { .. }) {
                continue;
            }
            match event {
                Event::Status(message) => self.message = message.clone(),
                Event::Fact(key, value) => match self.facts.iter_mut().find(|(name, _)| name == key) {
                    Some(entry) => entry.1 = value.clone(),
                    None => self.facts.push((key, value.clone())),
                },
                Event::Invite(code) => {
                    self.set(Phase::Invite, "Отправьте код другу.");
                    self.code = code.clone();
                }
                Event::Approval(peer) => {
                    self.set(Phase::Approval, "");
                    self.peer = peer.clone();
                }
                Event::Streaming => self.set(
                    Phase::Live,
                    if host { "Добавьте второго игрока в игре." } else { "Окно игры открыто." },
                ),
                Event::Finished { message, failed } => {
                    self.set(Phase::Idle, message.clone());
                    self.error = *failed;
                    finished = true;
                }
            }
        }
        if finished {
            self.session.take();
        }
        (!events.is_empty(), finished)
    }
}

/// Results of background work, delivered back to the UI thread.
enum Job {
    Lookup { query: String, result: Result<[u8; 32]> },
    FriendRequest { key: [u8; 32], new: bool, result: Result<()> },
    InviteSent { key: [u8; 32], at: u64, result: Result<()> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct App {
    host: Pane,
    guest: Pane,
    setup: Pane,
    components: Components,
    /// The running setup includes the elevated step (gamepad driver, firewall).
    full_setup: bool,
    version: &'static str,
    /// Start hosting as soon as the running setup succeeds.
    host_after_setup: bool,
    update: update::Status,
    me: MeView,
    friends: Vec<FriendView>,
    invitation: Option<Invitation>,
    host_for: Option<HostFor>,
    looking_up: bool,
    #[serde(skip)]
    identity: Me,
    #[serde(skip)]
    book: Book,
    #[serde(skip)]
    inbox: Option<Inbox>,
    #[serde(skip)]
    subscribed: Vec<[u8; 32]>,
    #[serde(skip)]
    presence: HashMap<[u8; 32], (u64, bool)>,
    #[serde(skip)]
    mail_history: MailHistory,
    #[serde(skip)]
    sending_requests: HashSet<[u8; 32]>,
    #[serde(skip)]
    jobs: (Sender<Job>, Receiver<Job>),
    #[serde(skip)]
    wake: Arc<dyn Fn() + Send + Sync>,
    #[serde(skip)]
    next_presence: Option<Instant>,
    #[serde(skip)]
    attention: bool,
    #[serde(skip)]
    updater: Option<Updater>,
    #[serde(skip)]
    quit: bool,
    #[serde(skip)]
    data: PathBuf,
}

impl App {
    fn new(data: PathBuf, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        let identity = Me::load_or_create(&data)?;
        let book = Book::load(&data);
        let mut app = Self {
            host: Pane::default(),
            guest: Pane::default(),
            setup: Pane::default(),
            components: Components::detect(&data),
            full_setup: false,
            version: env!("CARGO_PKG_VERSION"),
            host_after_setup: false,
            update: update::Status::Idle,
            me: MeView { name: String::new(), tag: String::new(), chosen: false },
            friends: Vec::new(),
            invitation: None,
            host_for: None,
            looking_up: false,
            mail_history: MailHistory::load(&data),
            sending_requests: HashSet::new(),
            identity,
            book,
            inbox: None,
            subscribed: Vec::new(),
            presence: HashMap::new(),
            jobs: mpsc::channel(),
            wake,
            next_presence: None,
            attention: false,
            updater: Updater::start(),
            quit: false,
            data,
        };
        if !app.components.engines() {
            app.setup.begin(setup::install(app.data.clone(), false), "Загружаем компоненты…");
        }
        app.resubscribe();
        if app.identity.chosen {
            friends::announce(&app.identity);
        }
        app.sync();
        Ok(app)
    }

    fn refresh_presence(&mut self, now: Instant) {
        if self.identity.chosen && !self.subscribed.is_empty() {
            if self.next_presence.is_none_or(|next| now >= next) {
                friends::heartbeat(&self.identity.public, true);
                self.next_presence = Some(now + HEARTBEAT);
            }
        } else if self.next_presence.take().is_some() {
            friends::heartbeat(&self.identity.public, false);
        }
    }

    /// Follows the presence of the current friend list.
    fn resubscribe(&mut self) {
        let keys = self.book.friend_keys();
        if self.inbox.is_some() && keys == self.subscribed {
            return;
        }
        self.inbox = Some(Inbox::listen(&self.identity.public, &keys, self.mail_history.latest(), self.wake.clone()));
        self.subscribed = keys;
    }

    fn online(&self, key: &[u8; 32]) -> bool {
        self.presence.get(key).is_some_and(|(seen, online)| {
            *online && friends::now().saturating_sub(*seen) < ONLINE_WINDOW
        })
    }

    /// Mirrors identity and friends into the serialized state.
    fn sync(&mut self) {
        self.me = MeView {
            name: self.identity.name.clone(),
            tag: format!("{:04}", friends::tag(&self.identity.public)),
            chosen: self.identity.chosen,
        };
        self.friends = self
            .book
            .entries
            .iter()
            .map(|f| {
                let key = f.public().unwrap_or_default();
                FriendView {
                    id: f.id(),
                    name: f.name.clone(),
                    tag: format!("{:04}", friends::tag(&key)),
                    status: f.status.clone(),
                    online: f.status == Status::Friend && self.online(&key),
                    sending: self.sending_requests.contains(&key),
                }
            })
            .collect();
        self.resubscribe();
    }

    fn tell(&self, to: &[u8; 32], message: &Message) {
        friends::send(self.identity.seal(to, message), to);
    }

    fn send(&self, to: [u8; 32], message: &Message, done: impl FnOnce(Result<()>) -> Job + Send + 'static) {
        let envelope = self.identity.seal(&to, message);
        let sender = self.jobs.0.clone();
        let wake = self.wake.clone();
        thread::spawn(move || {
            let _ = sender.send(done(friends::deliver(&envelope, &to)));
            wake();
        });
    }

    fn friend(&self, id: &str) -> Option<(friends::Friend, [u8; 32])> {
        let friend = self.book.by_id(id)?;
        let key = friend.public()?;
        Some((friend, key))
    }

    fn refresh(&mut self) {
        self.components = Components::detect(&self.data);
    }

    fn start_full_setup(&mut self) {
        // Supersede a background engine prefetch: the full setup fetches them too.
        if let Some(prefetch) = self.setup.session.take() {
            prefetch.cancel();
        }
        self.full_setup = true;
        self.setup.begin(setup::install(self.data.clone(), true), "Готовим установку…");
    }

    fn start_host(&mut self) {
        let trusted = self.host_for.is_some();
        self.host.begin(Session::host(self.data.clone(), trusted), "Создаём сессию…");
    }

    /// Hosting needs the driver and firewall once; set them up first if missing.
    fn begin_hosting(&mut self) {
        self.refresh();
        if self.components.host_ready() {
            self.start_host();
        } else {
            self.host_after_setup = true;
            self.host.set(Phase::Idle, "");
            if !self.full_setup {
                self.start_full_setup();
            }
        }
    }

    /// Adds `key` as a friend, or completes a pending request.
    fn befriend(&mut self, view: &WebView, key: [u8; 32], name: &str) -> Result<()> {
        if key == self.identity.public {
            return toast(view, "Это вы.");
        }
        let handle = friends::handle(name, &key);
        match self.book.find(&key).map(|f| f.status.clone()) {
            Some(Status::Friend) => toast(view, &format!("{handle} уже в друзьях."))?,
            Some(Status::Incoming) => {
                self.book.set(&key, name, Status::Friend);
                self.tell(&key, &Message::FriendAccept { name: self.identity.name.clone() });
                toast(view, &format!("{handle} теперь в друзьях."))?;
            }
            status => {
                if !self.sending_requests.insert(key) {
                    return Ok(());
                }
                self.book.set(&key, name, Status::Outgoing);
                let new = status.is_none();
                self.send(
                    key,
                    &Message::FriendRequest { name: self.identity.name.clone() },
                    move |result| Job::FriendRequest { key, new, result },
                );
                toast(view, "Отправляем запрос…")?;
            }
        }
        view.evaluate_script("window.duo.friendAdded();")?;
        self.sync();
        Ok(())
    }

    fn handle_mail(&mut self, view: &WebView, from: [u8; 32], message: Message) -> Result<()> {
        let known = self.book.find(&from).cloned();
        let is_friend = known.as_ref().is_some_and(|f| f.status == Status::Friend);
        match message {
            Message::FriendRequest { name } => match known.map(|f| f.status) {
                Some(Status::Friend) | Some(Status::Outgoing) => {
                    // Mutual or repeated request: settle as friends.
                    self.book.set(&from, &name, Status::Friend);
                    self.tell(&from, &Message::FriendAccept { name: self.identity.name.clone() });
                }
                _ => {
                    self.book.set(&from, &name, Status::Incoming);
                    self.attention = true;
                    toast(view, &format!("{} хочет дружить", friends::handle(&name, &from)))?;
                }
            },
            Message::FriendAccept { name } => {
                if known.is_some_and(|f| f.status != Status::Incoming) {
                    self.book.set(&from, &name, Status::Friend);
                    toast(view, &format!("{} теперь в друзьях.", friends::handle(&name, &from)))?;
                }
            }
            Message::FriendRemove => self.book.remove(&from),
            Message::Rename { name } => {
                if let Some(friend) = known {
                    self.book.set(&from, &name, friend.status);
                }
            }
            Message::Invite { name, code, at } => {
                if is_friend && friends::now().saturating_sub(at) < INVITE_LIFETIME {
                    self.book.set(&from, &name, Status::Friend);
                    self.presence.insert(from, (friends::now(), true));
                    self.invitation = Some(Invitation {
                        name: friends::handle(&name, &from),
                        key: from,
                        code,
                        at,
                    });
                    self.attention = true;
                }
            }
            Message::InviteDecline { at } => {
                if let Some(target) = &self.host_for
                    && target.key == from
                    && target.sent_at == Some(at)
                    && !target.joined
                {
                    let name = target.name.clone();
                    self.host_for = None;
                    self.host.stop();
                    toast(view, &format!("{name} отказался."))?;
                }
            }
            Message::InviteCancel { at } => {
                if self.invitation.as_ref().is_some_and(|i| i.key == from && i.at == at) {
                    self.invitation = None;
                }
            }
        }
        self.sync();
        Ok(())
    }

    fn update(&self, view: &WebView) -> Result<()> {
        view.evaluate_script(&format!("window.duo.applyState({});", serde_json::to_string(self)?))?;
        Ok(())
    }

    fn action(&mut self, action: Action, view: &WebView) -> Result<()> {
        match action {
            Action::Ready | Action::Wake => {}
            Action::Refresh => self.refresh(),
            Action::HostStart if self.host.session.is_none() => {
                self.host_for = None;
                self.begin_hosting();
            }
            Action::HostStop => self.host.stop(),
            Action::Allow => self.host.decide(true),
            Action::Deny => self.host.decide(false),
            Action::Copy if self.host.phase == Phase::Invite => copy(view, &self.host.code, "Код скопирован")?,
            Action::GuestStart { code, options } if self.guest.session.is_none() => match Invite::decode(&code) {
                Ok(invite) => match Preferences::parse(&options) {
                    Ok(preferences) => self.guest.begin(
                        Session::guest(self.data.clone(), invite, preferences, self.identity.name.clone()),
                        "Подключаемся…",
                    ),
                    Err(error) => self.guest.fail(error.to_string()),
                },
                Err(error) => {
                    self.guest.fail(if code.trim().is_empty() { "Вставьте код.".into() } else { error.to_string() });
                    self.guest.code_error = true;
                }
            },
            Action::GuestStop => self.guest.stop(),
            Action::Paste => match clipboard_win::get_clipboard_string() {
                Ok(text) => {
                    let code: String = text.trim().chars().take(256).collect();
                    view.evaluate_script(&format!("window.duo.pasted({});", serde_json::to_string(&code)?))?;
                    self.guest.set(Phase::Idle, "");
                }
                Err(_) => toast(view, "Буфер обмена пуст.")?,
            },
            Action::Install if !self.full_setup => self.start_full_setup(),
            Action::CheckUpdate => match &self.updater {
                Some(updater) => {
                    updater.check_now();
                    self.update = update::Status::Checking;
                }
                None => toast(view, "В этой сборке обновления выключены.")?,
            },
            Action::Restart if matches!(self.update, update::Status::Ready(_)) => {
                if self.host.session.is_some() || self.guest.session.is_some() {
                    toast(view, "Сначала завершите сессию.")?;
                } else if let Some(updater) = &self.updater {
                    match updater.relaunch() {
                        Ok(()) => self.quit = true,
                        Err(error) => toast(view, &format!("{error:#}"))?,
                    }
                }
            }
            Action::Graphics => open_or_toast(view, OsStr::new(GRAPHICS))?,
            Action::Logs => {
                let directory = self.data.join("logs");
                let _ = fs::create_dir_all(&directory);
                open_or_toast(view, directory.as_os_str())?;
            }
            Action::SetName { name } => {
                let first = !self.identity.chosen;
                match self.identity.rename(&name) {
                    Ok(()) => {
                        friends::announce(&self.identity);
                        if !first {
                            for friend in self.book.entries.iter().filter(|f| f.status != Status::Incoming) {
                                if let Some(key) = friend.public() {
                                    self.tell(&key, &Message::Rename { name: self.identity.name.clone() });
                                }
                            }
                        }
                        self.sync();
                    }
                    Err(error) => toast(view, &error.to_string())?,
                }
            }
            Action::AddFriend { query } => {
                let query = query.trim().to_string();
                if query.starts_with("DLF-") {
                    match friends::parse_code(&query) {
                        Ok((key, name)) => self.befriend(view, key, &name)?,
                        Err(error) => toast(view, &error.to_string())?,
                    }
                } else if !self.looking_up {
                    match friends::parse_handle(&query) {
                        Ok((name, number)) => {
                            self.looking_up = true;
                            let sender = self.jobs.0.clone();
                            let wake = self.wake.clone();
                            thread::spawn(move || {
                                let result = friends::lookup(&name, number);
                                let _ = sender.send(Job::Lookup { query: name, result });
                                wake();
                            });
                        }
                        Err(error) => toast(view, &error.to_string())?,
                    }
                }
            }
            Action::AcceptFriend { id } => {
                if let Some((friend, key)) = self.friend(&id) {
                    self.book.set(&key, &friend.name, Status::Friend);
                    self.tell(&key, &Message::FriendAccept { name: self.identity.name.clone() });
                    self.sync();
                }
            }
            Action::RemoveFriend { id } => {
                if let Some((_, key)) = self.friend(&id) {
                    self.book.remove(&key);
                    self.presence.remove(&key);
                    self.tell(&key, &Message::FriendRemove);
                    self.sync();
                }
            }
            Action::CopyHandle => copy(view, &self.identity.handle(), "Ник скопирован")?,
            Action::CopyCode => copy(view, &self.identity.code(), "Код друга скопирован")?,
            Action::InviteFriend { id } if self.host.session.is_none() => {
                if let Some((friend, key)) = self.friend(&id).filter(|(f, _)| f.status == Status::Friend) {
                    self.host_for = Some(HostFor {
                        name: friend.handle(), key, sent_at: None, joined: false, send_error: None,
                    });
                    self.begin_hosting();
                }
            }
            Action::AcceptInvite { options } => {
                if let Some(invitation) = self.invitation.take() {
                    if self.guest.session.is_some() {
                        toast(view, "Сначала отключитесь.")?;
                        self.invitation = Some(invitation);
                    } else {
                        match (Invite::decode(&invitation.code), Preferences::parse(&options)) {
                            (Ok(invite), Ok(preferences)) => self.guest.begin(
                                Session::guest(self.data.clone(), invite, preferences, self.identity.name.clone()),
                                "Подключаемся…",
                            ),
                            (Err(error), _) | (_, Err(error)) => self.guest.fail(error.to_string()),
                        }
                    }
                }
            }
            Action::DeclineInvite => {
                if let Some(invitation) = self.invitation.take() {
                    self.tell(&invitation.key, &Message::InviteDecline { at: invitation.at });
                }
            }
            _ => {}
        }
        self.update(view)
    }

    fn poll(&mut self, view: &WebView) {
        let (host, host_done) = self.host.poll(true);
        let (guest, _) = self.guest.poll(false);
        let (setup, setup_done) = self.setup.poll(false);
        let mut changed = host || guest || setup;

        if let Some(updater) = &self.updater {
            for status in updater.statuses.try_iter() {
                self.update = status;
                changed = true;
            }
        }
        if setup_done {
            let full = std::mem::take(&mut self.full_setup);
            self.refresh();
            if full && std::mem::take(&mut self.host_after_setup) {
                if self.setup.error {
                    self.host_for = None;
                    self.host.fail("Без драйвера геймпада друг не сможет играть.");
                } else if self.host.session.is_none() {
                    self.start_host();
                }
            }
        }

        // Friend invite: hand the session code to the friend once it exists.
        let mut invite_to_send = None;
        if let Some(target) = &mut self.host_for {
            if target.sent_at.is_none() && self.host.phase == Phase::Invite && !self.host.code.is_empty() {
                let at = friends::now();
                target.sent_at = Some(at);
                invite_to_send = Some((target.key, at));
            }
            target.joined |= self.host.phase == Phase::Live;
        }
        if let Some((key, at)) = invite_to_send {
            let message = Message::Invite { name: self.identity.name.clone(), code: self.host.code.clone(), at };
            self.send(key, &message, move |result| Job::InviteSent { key, at, result });
            self.host.message = "Отправляем приглашение…".into();
            changed = true;
        }
        if host_done && let Some(target) = self.host_for.take() {
            if let (Some(at), false) = (target.sent_at, target.joined) {
                self.tell(&target.key, &Message::InviteCancel { at });
            }
            if let Some(error) = target.send_error {
                self.host.fail(error);
            }
            changed = true;
        }
        if self.invitation.as_ref().is_some_and(|i| friends::now().saturating_sub(i.at) >= INVITE_LIFETIME) {
            self.invitation = None;
            changed = true;
        }

        let incoming: Vec<Incoming> = self.inbox.as_ref().map(|i| i.messages.try_iter().collect()).unwrap_or_default();
        let mut mail_changed = false;
        for item in incoming {
            match item {
                Incoming::Mail { time, text } => {
                    if let Some((from, message)) = self.identity.open(&text)
                        && self.mail_history.remember(time, &text)
                    {
                        let _ = self.handle_mail(view, from, message);
                        mail_changed = true;
                    }
                }
                Incoming::Presence { key, online, time } => {
                    let current = self.presence.entry(key).or_insert((0, false));
                    if time >= current.0 {
                        *current = (time, online);
                    }
                }
            }
            changed = true;
        }
        if mail_changed && self.mail_history.save(&self.data).is_err() {
            let _ = toast(view, "Не удалось сохранить историю сообщений.");
        }

        for job in self.jobs.1.try_iter().collect::<Vec<_>>() {
            match job {
                Job::Lookup { query, result } => {
                    self.looking_up = false;
                    match result {
                        Ok(key) => {
                            let _ = self.befriend(view, key, &query);
                        }
                        Err(error) => {
                            let _ = toast(view, &error.to_string());
                        }
                    }
                }
                Job::FriendRequest { key, new, result } => {
                    self.sending_requests.remove(&key);
                    if self.book.find(&key).is_some_and(|entry| entry.status == Status::Outgoing) {
                        match result {
                            Ok(()) => { let _ = toast(view, "Запрос отправлен."); }
                            Err(error) => {
                                if new {
                                    self.book.remove(&key);
                                }
                                let _ = toast(view, &error.to_string());
                            }
                        }
                    }
                }
                Job::InviteSent { key, at, result } => {
                    if let Some(target) = self.host_for.as_mut().filter(|target| target.key == key && target.sent_at == Some(at)) {
                        if self.host.phase == Phase::Invite {
                            match result {
                                Ok(()) => self.host.message = "Приглашение отправлено.".into(),
                                Err(error) => {
                                    target.send_error = Some(format!("{error} Попробуйте по коду."));
                                    self.host.stop();
                                }
                            }
                        }
                    } else if result.is_ok() {
                        // It may have arrived after the user closed the session.
                        self.tell(&key, &Message::InviteCancel { at });
                    }
                }
            }
            changed = true;
        }

        if changed {
            self.sync();
            let _ = self.update(view);
        }
    }

    /// Presence expires with time; refresh the list now and then.
    fn tick(&mut self, view: &WebView) {
        let before: Vec<bool> = self.friends.iter().map(|f| f.online).collect();
        self.sync();
        if self.friends.iter().map(|f| f.online).collect::<Vec<_>>() != before {
            let _ = self.update(view);
        }
    }

    fn active(&self) -> bool {
        self.host.session.is_some()
            || self.guest.session.is_some()
            || self.setup.session.is_some()
            || matches!(
                self.update,
                update::Status::Idle | update::Status::Checking | update::Status::Downloading(_)
            ) && self.updater.is_some()
    }

    fn shutdown(&mut self) {
        let sessions = [self.host.session.as_ref(), self.guest.session.as_ref(), self.setup.session.as_ref()];
        for session in sessions.into_iter().flatten() {
            session.cancel();
        }
        if let Some(target) = &self.host_for
            && let (Some(at), false) = (target.sent_at, target.joined)
        {
            friends::send(self.identity.seal(&target.key, &Message::InviteCancel { at }), &target.key);
        }
        if self.next_presence.is_some() {
            friends::sign_off(&self.identity.public);
        }
        self.inbox = None;
        let by = Instant::now() + Duration::from_secs(2);
        while Instant::now() < by && sessions.into_iter().flatten().any(|session| !session.finished()) {
            thread::sleep(Duration::from_millis(30));
        }
    }
}

fn toast(view: &WebView, message: &str) -> Result<()> {
    view.evaluate_script(&format!("window.duo.toast({});", serde_json::to_string(message)?))?;
    Ok(())
}

fn copy(view: &WebView, text: &str, done: &str) -> Result<()> {
    match clipboard_win::set_clipboard_string(text) {
        Ok(()) => toast(view, done),
        Err(_) => toast(view, "Не удалось скопировать."),
    }
}

fn open_or_toast(view: &WebView, target: &OsStr) -> Result<()> {
    if let Err(error) = open(target) {
        toast(view, &error.to_string())?;
    }
    Ok(())
}

fn asset(path: &str) -> Response<Cow<'static, [u8]>> {
    let (content_type, body): (&str, &'static [u8]) = match path {
        "/index.html" => ("text/html; charset=utf-8", include_bytes!("ui/index.html")),
        "/style.css" => ("text/css; charset=utf-8", include_bytes!("ui/style.css")),
        "/app.js" => ("text/javascript; charset=utf-8", include_bytes!("ui/app.js")),
        "/inter.woff2" => ("font/woff2", include_bytes!("ui/inter.woff2")),
        _ => {
            return Response::builder().status(404).body(Cow::Borrowed(&[][..])).unwrap();
        }
    };
    Response::builder()
        .header("Content-Type", content_type)
        .header("X-Content-Type-Options", "nosniff")
        .body(Cow::Borrowed(body))
        .unwrap()
}

pub fn run() -> Result<()> {
    let data = data_dir()?;
    fs::create_dir_all(&data)?;
    let mut context = WebContext::new(Some(data.join("interface")));
    let event_loop = EventLoopBuilder::<Action>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("DuoLink")
        .with_theme(Some(Theme::Dark))
        .with_window_icon(Icon::from_resource(1, None).ok())
        .with_inner_size(LogicalSize::new(480.0, 720.0))
        .with_min_inner_size(LogicalSize::new(400.0, 560.0))
        .with_visible(false)
        .build(&event_loop)?;
    if let Some(monitor) = window.current_monitor() {
        let screen = monitor.size();
        let origin = monitor.position();
        let size = window.outer_size();
        let x = screen.width.saturating_sub(size.width) / 2;
        let y = screen.height.saturating_sub(size.height) / 2;
        window.set_outer_position(PhysicalPosition::new(origin.x + x as i32, origin.y + y as i32));
    }
    let proxy = event_loop.create_proxy();
    let view = WebViewBuilder::new_with_web_context(&mut context)
        .with_background_color((11, 11, 11, 255))
        .with_theme(wry::Theme::Dark)
        .with_https_scheme(true)
        .with_default_context_menus(false)
        .with_browser_accelerator_keys(false)
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .with_drag_drop_handler(|_| true)
        .with_navigation_handler(|url| url == VIEW || url == WINDOWS_VIEW)
        .with_custom_protocol("duolink".into(), |_, request| asset(request.uri().path()))
        .with_ipc_handler(move |request| {
            if matches!(request.uri().to_string().as_str(), VIEW | WINDOWS_VIEW)
                && request.body().len() < 4096
                && let Ok(action) = serde_json::from_str(request.body())
            {
                let _ = proxy.send_event(action);
            }
        })
        .with_url(VIEW)
        .build(&window)
        .context("Не удалось открыть окно. Нужен Microsoft Edge WebView2 Runtime.")?;
    let waker = event_loop.create_proxy();
    let mut app = App::new(
        data,
        Arc::new(move || {
            let _ = waker.send_event(Action::Wake);
        }),
    )?;
    window.set_visible(true);
    let mut next_tick = Instant::now() + Duration::from_secs(20);
    let mut next_announce = Instant::now() + DIRECTORY_REFRESH;

    event_loop.run(move |event, _, control| {
        match event {
            WindowEvent::UserEvent(action) => {
                if let Err(error) = app.action(action, &view) {
                    let _ = toast(&view, &error.to_string());
                }
                if app.quit {
                    window.set_visible(false);
                    app.shutdown();
                    *control = ControlFlow::Exit;
                    return;
                }
            }
            WindowEvent::MainEventsCleared => {
                app.poll(&view);
                if std::mem::take(&mut app.attention) {
                    window.request_user_attention(Some(UserAttentionType::Critical));
                }
                let now = Instant::now();
                app.refresh_presence(now);
                if now >= next_tick {
                    app.tick(&view);
                    next_tick = now + Duration::from_secs(20);
                }
                if now >= next_announce {
                    if app.identity.chosen {
                        friends::announce(&app.identity);
                    }
                    next_announce = now + DIRECTORY_REFRESH;
                }
            }
            WindowEvent::WindowEvent { event: NativeEvent::CloseRequested, .. } => {
                window.set_visible(false);
                app.shutdown();
                *control = ControlFlow::Exit;
                return;
            }
            _ => {}
        }
        *control = if app.active() {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(80))
        } else {
            // Wake up now and then so presence can expire on screen.
            ControlFlow::WaitUntil(next_tick.min(next_announce))
        };
    });
}
