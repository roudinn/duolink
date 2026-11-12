use std::{
    borrow::Cow,
    collections::HashSet,
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
        self, Book, DIRECTORY_REFRESH, INVITE_LIFETIME, Inbox, Incoming, MailHistory, Me, Message,
        Status,
    },
    media::{Form, Preferences},
    platform::{data_dir, open},
    protocol::Invite,
    session::{Event, Player, PlayerState, Session},
    setup,
    update::{self, Updater},
};

const VIEW: &str = "duolink://localhost/index.html";
const WINDOWS_VIEW: &str = "https://duolink.localhost/index.html";
const GRAPHICS: &str = "ms-settings:display-advancedgraphics";
/// An update found this soon after launch installs and restarts on its own.
const AUTO_RESTART: Duration = Duration::from_secs(120);
/// WebView2 without what a local two-screen UI never uses: less memory and no
/// background traffic. The GPU stays on: software drawing made scrolling choppy.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,Translate,\
OptimizationHints,AutofillServerCommunication,MediaRouter,SpareRendererForSitePerProcess \
--disable-background-networking --disable-component-update --disable-default-apps --disable-sync \
--no-first-run --renderer-process-limit=1";

/// Lets WebView2 drop caches while the window is in the background.
fn memory_level(view: &WebView, low: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL, ICoreWebView2_19,
    };
    use windows::core::Interface;
    use wry::WebViewExtWindows;
    let level = if low { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW } else { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL };
    unsafe {
        if let Ok(core) = view.controller().CoreWebView2()
            && let Ok(core) = core.cast::<ICoreWebView2_19>()
        {
            let _ = core.SetMemoryUsageTargetLevel(level);
        }
    }
}

/// The tray icon: click opens the window, the right-click menu opens or exits.
fn tray(proxy: tao::event_loop::EventLoopProxy<Action>) -> Result<tray_icon::TrayIcon> {
    use tray_icon::{
        MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };
    let open = MenuItem::new("Открыть DuoLink", true, None);
    let quit = MenuItem::new("Выйти", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit])?;
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
    let clicks = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
        | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = event
        {
            let _ = clicks.send_event(Action::Show);
        }
    }));
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = if event.id == quit_id {
            Action::Quit
        } else if event.id == open_id {
            Action::Show
        } else {
            return;
        };
        let _ = proxy.send_event(action);
    }));
    Ok(TrayIconBuilder::new()
        .with_icon(tray_icon::Icon::from_resource(1, None)?)
        .with_tooltip("DuoLink")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?)
}

/// Downloads Sunshine and Moonlight in the background on first launch, so
/// the first session starts the game instead of a download.
fn prefetch_engines(data: PathBuf) {
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(4));
        for engine in [&crate::media::MOONLIGHT, &crate::media::SUNSHINE] {
            if !engine.path(&data).is_file() {
                let _ = engine.ensure(&data, || Ok(()));
            }
        }
    });
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    Ready,
    HostStart,
    HostStop,
    Allow { id: u32 },
    Deny { id: u32 },
    Kick { id: u32 },
    Copy,
    GuestStart { code: String, options: Form },
    GuestStop,
    ClearCodeError,
    Paste,
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
    AcceptInvite { id: String, options: Form },
    DeclineInvite { id: String },
    /// Sent by background threads to wake the event loop.
    #[serde(skip_deserializing)]
    Wake,
    /// Tray click, tray menu or a second launch: bring the window back.
    #[serde(skip_deserializing)]
    Show,
    /// Tray menu: really exit.
    #[serde(skip_deserializing)]
    Quit,
}

#[derive(Serialize)]
struct FriendView {
    id: String,
    name: String,
    tag: String,
    status: Status,
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
    id: String,
    name: String,
    #[serde(skip)]
    key: [u8; 32],
    #[serde(skip)]
    code: String,
    #[serde(skip)]
    at: u64,
    #[serde(skip)]
    expires: u64,
}

impl Invitation {
    fn id(key: &[u8; 32], at: u64) -> String {
        format!("{}-{at}", friends::short_id(key))
    }
}

/// A friend the running lobby was sent to.
#[derive(Clone, Serialize)]
struct HostFor {
    name: String,
    #[serde(skip)]
    key: [u8; 32],
    #[serde(skip)]
    sent_at: Option<u64>,
    #[serde(skip)]
    joined: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Idle,
    Working,
    Invite,
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
    video_error: bool,
    facts: Vec<(&'static str, String)>,
    /// The host's lobby.
    players: Vec<Player>,
    /// Round trip in milliseconds while connected.
    latency: Option<f64>,
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
        self.video_error = false;
    }

    fn begin(&mut self, session: Session, message: &str) {
        self.set(Phase::Working, message);
        self.facts.clear();
        self.players.clear();
        self.latency = None;
        self.session = Some(session);
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.set(Phase::Idle, message);
        self.error = true;
    }

    fn fail_video(&mut self, error: anyhow::Error) {
        self.fail(error.to_string());
        self.video_error = true;
    }

    fn stop(&mut self) {
        if let Some(session) = &self.session {
            session.cancel();
            self.set(Phase::Stopping, "Завершаем…");
        }
    }

    fn decide(&mut self, id: u32, accept: bool) {
        if let Some(session) = &self.session
            && let Some(player) = self.players.iter_mut().find(|p| p.id == id && p.state == PlayerState::Waiting)
        {
            session.decide(id, accept);
            player.state = PlayerState::Starting;
        }
    }

    fn kick(&mut self, id: u32) {
        if let Some(session) = &self.session {
            session.kick(id);
            self.players.retain(|p| p.id != id);
        }
    }

    fn waiting(&self) -> usize {
        self.players.iter().filter(|p| p.state == PlayerState::Waiting).count()
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
                    self.set(Phase::Invite, "Отправьте код друзьям.");
                    self.code = code.clone();
                }
                Event::Players(players) => self.players = players.clone(),
                Event::Latency(rtt) => self.latency = Some(rtt.as_secs_f64() * 1000.0),
                Event::Streaming if host => {
                    // The lobby stays open: the code still lets more friends in.
                    self.phase = Phase::Live;
                    self.message = "Добавьте игроков в игре.".into();
                }
                Event::Streaming => self.set(Phase::Live, "Окно игры открыто."),
                Event::Finished { message, failed } => {
                    self.set(Phase::Idle, message.clone());
                    self.error = *failed;
                    self.latency = None;
                    self.players.clear();
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
    FriendAccepted { key: [u8; 32], result: Result<()> },
    InviteSent { key: [u8; 32], at: u64, result: Result<()> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct App {
    host: Pane,
    guest: Pane,
    setup: Pane,
    version: &'static str,
    update: update::Status,
    me: MeView,
    friends: Vec<FriendView>,
    invitations: Vec<Invitation>,
    host_for: Vec<HostFor>,
    looking_up: bool,
    #[serde(skip)]
    identity: Me,
    #[serde(skip)]
    book: Book,
    #[serde(skip)]
    inbox: Option<Inbox>,
    #[serde(skip)]
    mail_history: MailHistory,
    #[serde(skip)]
    sending_requests: HashSet<[u8; 32]>,
    #[serde(skip)]
    jobs: (Sender<Job>, Receiver<Job>),
    #[serde(skip)]
    wake: Arc<dyn Fn() + Send + Sync>,
    #[serde(skip)]
    attention: bool,
    #[serde(skip)]
    updater: Option<Updater>,
    #[serde(skip)]
    quit: bool,
    #[serde(skip)]
    launched: Instant,
    /// The user stopped hosting while the driver/firewall check was running.
    #[serde(skip)]
    setup_cancelled: bool,
    #[serde(skip)]
    data: PathBuf,
}

impl App {
    fn new(data: PathBuf, wake: Arc<dyn Fn() + Send + Sync>, updater: Option<Updater>) -> Result<Self> {
        let identity = Me::load_or_create(&data)?;
        let book = Book::load(&data);
        let mut app = Self {
            host: Pane::default(),
            guest: Pane::default(),
            setup: Pane::default(),
            version: env!("CARGO_PKG_VERSION"),
            update: update::Status::Idle,
            me: MeView { name: String::new(), tag: String::new(), chosen: false },
            friends: Vec::new(),
            invitations: Vec::new(),
            host_for: Vec::new(),
            looking_up: false,
            mail_history: MailHistory::load(&data),
            sending_requests: HashSet::new(),
            identity,
            book,
            inbox: None,
            jobs: mpsc::channel(),
            wake,
            attention: false,
            updater,
            quit: false,
            launched: Instant::now(),
            setup_cancelled: false,
            data,
        };
        if app.identity.chosen {
            app.listen();
            friends::announce(&app.identity);
        }
        app.sync();
        Ok(app)
    }

    fn listen(&mut self) {
        self.inbox = Some(Inbox::listen(
            &self.identity.public,
            self.mail_history.latest(),
            self.wake.clone(),
        ));
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
            .filter_map(|f| {
                let key = f.public()?;
                Some(FriendView {
                    id: f.id(),
                    name: f.name.clone(),
                    tag: format!("{:04}", friends::tag(&key)),
                    status: f.status.clone(),
                    sending: self.sending_requests.contains(&key),
                })
            })
            .collect();
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

    fn session_idle(&self) -> bool {
        self.guest.session.is_none() && self.setup.session.is_none() && self.host.session.is_none()
    }

    fn start_host(&mut self) {
        self.host.begin(Session::host(self.data.clone()), "Создаём сессию…");
    }

    fn begin_hosting(&mut self) {
        self.host.set(Phase::Idle, "");
        self.setup.begin(setup::prepare(self.data.clone()), "Проверяем драйвер и брандмауэр…");
    }

    fn start_guest(&mut self, invite: Invite, preferences: Preferences) {
        let name = if self.identity.chosen { self.identity.name.clone() } else { String::new() };
        self.guest.begin(
            Session::guest(self.data.clone(), invite, preferences, name),
            "Подключаемся…",
        );
    }

    fn accept_friend(&mut self, key: [u8; 32]) {
        if self.sending_requests.insert(key) {
            self.send(
                key,
                &Message::FriendAccept { name: self.identity.name.clone() },
                move |result| Job::FriendAccepted { key, result },
            );
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
                self.accept_friend(key);
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

    fn handle_mail(&mut self, view: &WebView, from: [u8; 32], message: Message, time: u64) -> Result<()> {
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
                if known.is_some_and(|f| f.status == Status::Outgoing) {
                    self.book.set(&from, &name, Status::Friend);
                    toast(view, &format!("{} теперь в друзьях.", friends::handle(&name, &from)))?;
                }
            }
            Message::FriendRemove => {
                self.book.remove(&from);
                self.invitations.retain(|invite| invite.key != from);
            }
            Message::Rename { name } => {
                if let Some(friend) = known {
                    self.book.set(&from, &name, friend.status);
                }
            }
            Message::Invite { name, code, at } => {
                let id = Invitation::id(&from, at);
                let now = friends::now();
                let expires = time.min(now).saturating_add(INVITE_LIFETIME);
                let accepting_friend = known.is_some_and(|f| f.status == Status::Incoming)
                    && self.sending_requests.contains(&from);
                if (is_friend || accepting_friend)
                    && expires > now
                    && !self.mail_history.invite_cancelled(&id)
                    && !self.invitations.iter().any(|invite| invite.key == from && invite.at >= at)
                {
                    self.book.set(&from, &name, Status::Friend);
                    self.invitations.retain(|invite| invite.key != from);
                    self.invitations.push(Invitation {
                        id,
                        name: self.book.find(&from).unwrap().handle(),
                        key: from,
                        code,
                        at,
                        expires,
                    });
                    self.attention = true;
                    crate::audio::play_chime();
                }
            }
            Message::InviteDecline { at } => {
                if let Some(index) = self
                    .host_for
                    .iter()
                    .position(|t| t.key == from && t.sent_at == Some(at) && !t.joined)
                {
                    let target = self.host_for.remove(index);
                    toast(view, &format!("{} отказался.", target.name))?;
                }
            }
            Message::InviteCancel { at } if is_friend || self.sending_requests.contains(&from) => {
                let id = Invitation::id(&from, at);
                self.mail_history.cancel_invite(&id, time);
                self.invitations.retain(|invite| invite.id != id);
            }
            Message::InviteCancel { .. } => {}
        }
        Ok(())
    }

    fn update(&self, view: &WebView) -> Result<()> {
        view.evaluate_script(&format!("window.duo.applyState({});", serde_json::to_string(self)?))?;
        Ok(())
    }

    fn action(&mut self, action: Action, view: &WebView) -> Result<()> {
        match action {
            Action::Ready | Action::Wake | Action::Show => {}
            Action::Quit => self.quit = true,
            Action::HostStart if self.session_idle() => {
                self.host_for.clear();
                self.begin_hosting();
            }
            Action::HostStop => {
                // Hosting may still be in its driver/firewall check.
                if self.setup.session.is_some() {
                    self.setup_cancelled = true;
                    self.setup.stop();
                }
                self.host.stop();
            }
            Action::Allow { id } => self.host.decide(id, true),
            Action::Deny { id } => self.host.decide(id, false),
            Action::Kick { id } => self.host.kick(id),
            Action::Copy if !self.host.code.is_empty() => copy(view, &self.host.code, "Код скопирован")?,
            Action::GuestStart { code, options } if self.session_idle() => match Invite::decode(&code) {
                Ok(invite) => match Preferences::parse(&options) {
                    Ok(preferences) => self.start_guest(invite, preferences),
                    Err(error) => self.guest.fail_video(error),
                },
                Err(error) => {
                    self.guest.fail(if code.trim().is_empty() { "Вставьте код.".into() } else { error.to_string() });
                    self.guest.code_error = true;
                }
            },
            Action::GuestStop => self.guest.stop(),
            Action::ClearCodeError if self.guest.code_error => self.guest.set(Phase::Idle, ""),
            Action::Paste if self.session_idle() => match clipboard_win::get_clipboard_string() {
                Ok(text) if !text.trim().is_empty() => {
                    let code: String = text.trim().chars().take(256).collect();
                    view.evaluate_script(&format!("window.duo.pasted({});", serde_json::to_string(&code)?))?;
                    self.guest.set(Phase::Idle, "");
                }
                _ => toast(view, "В буфере обмена нет текста.")?,
            },
            Action::CheckUpdate if matches!(self.update, update::Status::Idle | update::Status::Current | update::Status::Failed(_)) => match &self.updater {
                Some(updater) => {
                    updater.check_now();
                    self.update = update::Status::Checking;
                }
                None => toast(view, "В этой сборке обновления выключены.")?,
            },
            Action::Restart if matches!(self.update, update::Status::Ready(_)) => {
                if !self.session_idle() {
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
            Action::SetName { name } if !self.identity.chosen || self.identity.name != name.trim() => {
                let first = !self.identity.chosen;
                match self.identity.rename(&name) {
                    Ok(()) => {
                        friends::announce(&self.identity);
                        if first {
                            self.listen();
                        } else {
                            for friend in self.book.entries.iter().filter(|f| f.status != Status::Incoming) {
                                if let Some(key) = friend.public() {
                                    self.tell(&key, &Message::Rename { name: self.identity.name.clone() });
                                }
                            }
                        }
                        self.sync();
                        toast(view, "Ник сохранён.")?;
                    }
                    Err(error) => toast(view, &error.to_string())?,
                }
            }
            Action::AddFriend { query } if self.identity.chosen => {
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
            Action::AcceptFriend { id } if self.identity.chosen => {
                if let Some((_, key)) = self.friend(&id).filter(|(f, _)| f.status == Status::Incoming) {
                    self.accept_friend(key);
                    self.sync();
                }
            }
            Action::RemoveFriend { id } => {
                if let Some((_, key)) = self.friend(&id).filter(|(_, key)| !self.sending_requests.contains(key)) {
                    self.book.remove(&key);
                    self.invitations.retain(|invite| invite.key != key);
                    self.tell(&key, &Message::FriendRemove);
                    self.sync();
                }
            }
            Action::CopyHandle if self.identity.chosen => copy(view, &self.identity.handle(), "Ник скопирован")?,
            Action::CopyCode if self.identity.chosen => copy(view, &self.identity.code(), "Код друга скопирован")?,
            Action::InviteFriend { id } if self.identity.chosen => {
                if let Some((friend, key)) = self.friend(&id).filter(|(f, _)| f.status == Status::Friend) {
                    let lobby_open = self.host.session.is_some() || self.setup.session.is_some();
                    if self.session_idle() {
                        self.host_for.clear();
                        self.host_for.push(HostFor { name: friend.handle(), key, sent_at: None, joined: false });
                        self.begin_hosting();
                    } else if lobby_open && !self.host_for.iter().any(|t| t.key == key) {
                        // More friends into the running lobby.
                        self.host_for.push(HostFor { name: friend.handle(), key, sent_at: None, joined: false });
                    }
                }
            }
            Action::AcceptInvite { id, options } if self.session_idle() => {
                if let Some(index) = self.invitations.iter().position(|invite| invite.id == id) {
                    if self.invitations[index].expires <= friends::now() {
                        self.invitations.remove(index);
                        self.guest.fail("Приглашение истекло.");
                    } else {
                        match Invite::decode(&self.invitations[index].code) {
                            Ok(invite) => match Preferences::parse(&options) {
                                Ok(preferences) => {
                                    self.invitations.remove(index);
                                    self.start_guest(invite, preferences);
                                }
                                Err(error) => self.guest.fail_video(error),
                            },
                            Err(error) => {
                                self.invitations.remove(index);
                                self.guest.fail(error.to_string());
                            }
                        }
                    }
                }
            }
            Action::DeclineInvite { id } => {
                if let Some(index) = self.invitations.iter().position(|invite| invite.id == id) {
                    let invitation = self.invitations.remove(index);
                    self.tell(&invitation.key, &Message::InviteDecline { at: invitation.at });
                }
            }
            _ => {}
        }
        self.update(view)
    }

    fn poll(&mut self, view: &WebView) {
        let waiting = self.host.waiting();
        let (host, host_done) = self.host.poll(true);
        // Someone new knocked: get the host's attention.
        if self.host.waiting() > waiting {
            self.attention = true;
            crate::audio::play_chime();
        }
        let (guest, _) = self.guest.poll(false);
        let (setup, setup_done) = self.setup.poll(false);
        let mut changed = host || guest || setup;

        if let Some(updater) = &self.updater {
            for status in updater.statuses.try_iter() {
                self.update = status;
                changed = true;
            }
            // Found at launch and nothing started yet: switch to it straight away.
            if matches!(self.update, update::Status::Ready(_))
                && self.launched.elapsed() < AUTO_RESTART
                && self.session_idle()
                && updater.relaunch().is_ok()
            {
                self.quit = true;
            }
        }
        if setup_done {
            if std::mem::take(&mut self.setup_cancelled) {
                self.host_for.clear();
                self.setup.set(Phase::Idle, "");
                self.host.set(Phase::Idle, "");
            } else if self.setup.error {
                self.host_for.clear();
                self.host.fail(self.setup.message.clone());
            } else {
                self.start_host();
            }
        }

        // Friend invite: hand the session code to the friend once it exists.
        let mut invites_to_send = Vec::new();
        let lobby_ready = matches!(self.host.phase, Phase::Invite | Phase::Live) && !self.host.code.is_empty();
        for target in &mut self.host_for {
            if target.sent_at.is_none() && lobby_ready {
                let at = friends::now();
                target.sent_at = Some(at);
                invites_to_send.push((target.key, at));
            }
            let nick = target.name.split('#').next().unwrap_or_default();
            target.joined |= self.host.players.iter().any(|p| p.name == nick);
        }
        for (key, at) in invites_to_send {
            let message = Message::Invite { name: self.identity.name.clone(), code: self.host.code.clone(), at };
            self.send(key, &message, move |result| Job::InviteSent { key, at, result });
            changed = true;
        }
        if host_done {
            for target in std::mem::take(&mut self.host_for) {
                if let (Some(at), false) = (target.sent_at, target.joined) {
                    self.tell(&target.key, &Message::InviteCancel { at });
                }
            }
            changed = true;
        }
        let count = self.invitations.len();
        let now = friends::now();
        self.invitations.retain(|invite| invite.expires > now);
        changed |= self.invitations.len() != count;

        let incoming: Vec<Incoming> = self.inbox.as_ref().map(|i| i.messages.try_iter().collect()).unwrap_or_default();
        let mut mail_changed = false;
        for Incoming { time, text } in incoming {
            if let Some((from, message)) = self.identity.open(&text)
                && self.mail_history.remember(time, &text)
            {
                let _ = self.handle_mail(view, from, message, time);
                mail_changed = true;
            }
        }
        changed |= mail_changed;
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
                Job::FriendAccepted { key, result } => {
                    self.sending_requests.remove(&key);
                    if let Some(friend) = self.book.find(&key).filter(|entry| entry.status == Status::Incoming) {
                        match result {
                            Ok(()) => {
                                let name = friend.name.clone();
                                self.book.set(&key, &name, Status::Friend);
                            }
                            Err(error) => {
                                let _ = toast(view, &error.to_string());
                            }
                        }
                    }
                }
                Job::InviteSent { key, at, result } => {
                    if let Some(index) = self.host_for.iter().position(|t| t.key == key && t.sent_at == Some(at)) {
                        let name = self.host_for[index].name.clone();
                        match result {
                            Ok(()) => { let _ = toast(view, &format!("Приглашение для {name} отправлено.")); }
                            Err(error) => {
                                self.host_for.remove(index);
                                let _ = toast(view, &format!("{error} Отправьте {name} код сессии."));
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

    fn next_invite_expiry(&self) -> Option<Instant> {
        let expires = self.invitations.iter().map(|invite| invite.expires).min()?;
        Some(Instant::now() + Duration::from_secs(expires.saturating_sub(friends::now())))
    }

    fn active(&self) -> bool {
        self.host.session.is_some()
            || self.guest.session.is_some()
            || self.setup.session.is_some()
    }

    fn shutdown(&mut self) {
        if let Some(updater) = &self.updater {
            updater.stop();
        }
        let sessions = [self.host.session.as_ref(), self.guest.session.as_ref(), self.setup.session.as_ref()];
        for session in sessions.into_iter().flatten() {
            session.cancel();
        }
        for target in &self.host_for {
            if let (Some(at), false) = (target.sent_at, target.joined) {
                friends::send(self.identity.seal(&target.key, &Message::InviteCancel { at }), &target.key);
            }
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
        "/i18n.js" => ("text/javascript; charset=utf-8", include_bytes!("ui/i18n.js")),
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
        .with_inner_size(LogicalSize::new(880.0, 580.0))
        .with_min_inner_size(LogicalSize::new(420.0, 520.0))
        .with_visible(false)
        .build(&event_loop)?;
    if let Some(monitor) = window.current_monitor() {
        let screen = monitor.size();
        let origin = monitor.position();
        // Small laptop screens at 125–150% scaling are narrower than 880×580 logical.
        let scale = monitor.scale_factor();
        let fit_width = (screen.width as f64 / scale * 0.9).max(420.0);
        let fit_height = (screen.height as f64 / scale * 0.85).max(520.0);
        if fit_width < 880.0 || fit_height < 580.0 {
            window.set_inner_size(LogicalSize::new(fit_width.min(880.0), fit_height.min(580.0)));
        }
        let size = window.outer_size();
        let x = screen.width.saturating_sub(size.width) / 2;
        let y = screen.height.saturating_sub(size.height) / 2;
        window.set_outer_position(PhysicalPosition::new(origin.x + x as i32, origin.y + y as i32));
    }
    let proxy = event_loop.create_proxy();
    let waker = event_loop.create_proxy();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = waker.send_event(Action::Wake);
    });
    // The update check runs while the window and WebView2 start.
    let updater = Updater::start(wake.clone());
    let view = WebViewBuilder::new_with_web_context(&mut context)
        .with_additional_browser_args(BROWSER_ARGS)
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
    prefetch_engines(data.clone());
    // Closing the window keeps DuoLink in the tray, so invites still arrive.
    let _tray = tray(event_loop.create_proxy())?;
    let shower = event_loop.create_proxy();
    crate::platform::on_show_request(move || {
        let _ = shower.send_event(Action::Show);
    });
    let mut app = App::new(data, wake, updater)?;
    window.set_visible(true);
    let mut next_announce = Instant::now() + DIRECTORY_REFRESH;

    event_loop.run(move |event, _, control| {
        match event {
            WindowEvent::UserEvent(Action::Show) => {
                window.set_visible(true);
                window.set_minimized(false);
                window.set_focus();
            }
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
                if app.quit {
                    window.set_visible(false);
                    app.shutdown();
                    *control = ControlFlow::Exit;
                    return;
                }
                if std::mem::take(&mut app.attention) {
                    // An invite or a knock while in the tray: show the window.
                    if !window.is_visible() {
                        window.set_visible(true);
                    }
                    window.request_user_attention(Some(UserAttentionType::Critical));
                }
                let now = Instant::now();
                if now >= next_announce {
                    if app.identity.chosen {
                        friends::announce(&app.identity);
                    }
                    next_announce = now + DIRECTORY_REFRESH;
                }
            }
            WindowEvent::WindowEvent { event: NativeEvent::Focused(focused), .. } => {
                memory_level(&view, !focused);
            }
            WindowEvent::WindowEvent { event: NativeEvent::CloseRequested, .. } => {
                // The cross hides; "Выйти" in the tray menu exits.
                window.set_visible(false);
                memory_level(&view, true);
            }
            _ => {}
        }
        *control = if app.active() {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(80))
        } else {
            let deadline = app.next_invite_expiry().map_or(next_announce, |expires| expires.min(next_announce));
            ControlFlow::WaitUntil(deadline)
        };
    });
}
