use std::{
    borrow::Cow,
    ffi::OsStr,
    fs,
    path::PathBuf,
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
    friends::{self, Book, INVITE_LIFETIME, Inbox, Me, Message, Status},
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
    AddFriend { code: String },
    AcceptFriend { id: String },
    RemoveFriend { id: String },
    CopyFriendCode,
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
    status: Status,
}

/// A friend's invite waiting for an answer.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Invitation {
    id: String,
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
#[serde(rename_all = "camelCase")]
struct HostFor {
    id: String,
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
            self.set(Phase::Stopping, "Закрываем соединение…");
        }
    }

    fn decide(&mut self, accept: bool) {
        if self.phase == Phase::Approval
            && let Some(session) = &self.session
        {
            session.decide(accept);
            self.set(
                Phase::Working,
                if accept { "Запускаем поток…" } else { "Отклоняем запрос…" },
            );
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
                Event::Fact(key, value) => {
                    match self.facts.iter_mut().find(|(name, _)| name == key) {
                        Some(entry) => entry.1 = value.clone(),
                        None => self.facts.push((key, value.clone())),
                    }
                }
                Event::Invite(code) => {
                    self.set(Phase::Invite, "Отправьте код другу. Перед началом вы подтвердите его запрос.");
                    self.code = code.clone();
                }
                Event::Approval(peer) => {
                    self.set(Phase::Approval, "Друг получит изображение основного экрана, звук и свой геймпад.");
                    self.peer = peer.clone();
                }
                Event::Streaming => self.set(
                    Phase::Live,
                    if host {
                        "Добавьте второго игрока в локальном режиме игры."
                    } else {
                        "Окно игры открыто. Возьмите геймпад."
                    },
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
    nickname: String,
    nickname_chosen: bool,
    friend_code: String,
    friends: Vec<FriendView>,
    invitation: Option<Invitation>,
    host_for: Option<HostFor>,
    #[serde(skip)]
    me: Me,
    #[serde(skip)]
    book: Book,
    #[serde(skip)]
    inbox: Option<Inbox>,
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
    fn new(data: PathBuf, wake: impl Fn() + Send + 'static) -> Result<Self> {
        let me = Me::load_or_create(&data)?;
        let inbox = Inbox::listen(&me.public, wake);
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
            nickname: String::new(),
            nickname_chosen: false,
            friend_code: String::new(),
            friends: Vec::new(),
            invitation: None,
            host_for: None,
            me,
            book,
            inbox: Some(inbox),
            attention: false,
            updater: Updater::start(),
            quit: false,
            data,
        };
        // Streaming engines need no permission: fetch them right away.
        if !app.components.engines() {
            app.setup
                .begin(setup::install(app.data.clone(), false), "Загружаем компоненты…");
        }
        app.sync();
        Ok(app)
    }

    /// Mirrors identity and friends into the serialized state.
    fn sync(&mut self) {
        self.nickname = self.me.name.clone();
        self.nickname_chosen = self.me.chosen;
        self.friend_code = self.me.code();
        self.friends = self
            .book
            .entries
            .iter()
            .map(|f| FriendView { id: f.id(), name: f.name.clone(), status: f.status.clone() })
            .collect();
    }

    fn tell(&self, to: &[u8; 32], message: &Message) {
        friends::send(self.me.seal(to, message), to);
    }

    fn friend(&self, id: &str) -> Option<(friends::Friend, [u8; 32])> {
        let friend = self.book.by_id(id)?;
        let key = friend.public()?;
        Some((friend, key))
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

    fn handle_message(&mut self, view: &WebView, envelope: &str) -> Result<()> {
        let Some((from, message)) = self.me.open(envelope) else {
            return Ok(());
        };
        let known = self.book.find(&from).cloned();
        let is_friend = known.as_ref().is_some_and(|f| f.status == Status::Friend);
        match message {
            Message::FriendRequest { name } => match known.map(|f| f.status) {
                Some(Status::Friend) | Some(Status::Outgoing) => {
                    // Mutual or repeated request: settle as friends.
                    self.book.set(&from, &name, Status::Friend);
                    self.tell(&from, &Message::FriendAccept { name: self.me.name.clone() });
                }
                _ => {
                    self.book.set(&from, &name, Status::Incoming);
                    self.attention = true;
                    toast(view, &format!("{name} хочет добавить вас в друзья"))?;
                }
            },
            Message::FriendAccept { name } => {
                if known.is_some_and(|f| f.status != Status::Incoming) {
                    self.book.set(&from, &name, Status::Friend);
                    toast(view, &format!("{name} теперь ваш друг"))?;
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
                    self.invitation = Some(Invitation {
                        id: self.book.find(&from).map(friends::Friend::id).unwrap_or_default(),
                        name,
                        key: from,
                        code,
                        at,
                    });
                    self.attention = true;
                }
            }
            Message::InviteDecline { .. } => {
                if let Some(target) = &self.host_for
                    && target.key == from
                    && !target.joined
                {
                    let name = target.name.clone();
                    self.host_for = None;
                    self.host.stop();
                    toast(view, &format!("{name} отклонил приглашение"))?;
                }
            }
            Message::InviteCancel { at } => {
                if self.invitation.as_ref().is_some_and(|i| i.key == from && i.at <= at) {
                    self.invitation = None;
                }
            }
        }
        self.sync();
        Ok(())
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
        self.setup
            .begin(setup::install(self.data.clone(), true), "Готовим установку…");
    }

    fn start_host(&mut self) {
        let trusted = self.host_for.is_some();
        self.host
            .begin(Session::host(self.data.clone(), trusted), "Готовим приглашение…");
    }

    fn update(&self, view: &WebView) -> Result<()> {
        view.evaluate_script(&format!(
            "window.duo.applyState({});",
            serde_json::to_string(self)?
        ))?;
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
            Action::SetName { name } => match self.me.rename(&name) {
                Ok(()) => {
                    for friend in self.book.entries.iter().filter(|f| f.status != Status::Incoming) {
                        if let Some(key) = friend.public() {
                            friends::send(self.me.seal(&key, &Message::Rename { name: self.me.name.clone() }), &key);
                        }
                    }
                    self.sync();
                }
                Err(error) => toast(view, &error.to_string())?,
            },
            Action::AddFriend { code } => match friends::parse_code(&code) {
                Ok((key, _)) if key == self.me.public => toast(view, "Это ваш собственный код.")?,
                Ok((key, name)) => {
                    match self.book.find(&key).map(|f| f.status.clone()) {
                        Some(Status::Friend) => toast(view, &format!("{name} уже у вас в друзьях."))?,
                        Some(Status::Incoming) => {
                            self.book.set(&key, &name, Status::Friend);
                            self.tell(&key, &Message::FriendAccept { name: self.me.name.clone() });
                            toast(view, &format!("{name} теперь ваш друг"))?;
                        }
                        _ => {
                            self.book.set(&key, &name, Status::Outgoing);
                            self.tell(&key, &Message::FriendRequest { name: self.me.name.clone() });
                            toast(view, &format!("Запрос отправлен. {name} увидит его в DuoLink."))?;
                        }
                    }
                    view.evaluate_script("window.duo.friendAdded();")?;
                    self.sync();
                }
                Err(error) => toast(view, &error.to_string())?,
            },
            Action::AcceptFriend { id } => {
                if let Some((friend, key)) = self.friend(&id) {
                    self.book.set(&key, &friend.name, Status::Friend);
                    self.tell(&key, &Message::FriendAccept { name: self.me.name.clone() });
                    self.sync();
                }
            }
            Action::RemoveFriend { id } => {
                if let Some((_, key)) = self.friend(&id) {
                    self.book.remove(&key);
                    self.tell(&key, &Message::FriendRemove);
                    self.sync();
                }
            }
            Action::CopyFriendCode => match clipboard_win::set_clipboard_string(&self.me.code()) {
                Ok(()) => toast(view, "Код друга скопирован — отправьте его другу один раз")?,
                Err(_) => toast(view, "Не удалось скопировать код.")?,
            },
            Action::InviteFriend { id } if self.host.session.is_none() => {
                if let Some((friend, key)) = self.friend(&id).filter(|(f, _)| f.status == Status::Friend) {
                    self.host_for = Some(HostFor {
                        id,
                        name: friend.name,
                        key,
                        sent_at: None,
                        joined: false,
                    });
                    self.begin_hosting();
                }
            }
            Action::AcceptInvite { options } => {
                if let Some(invitation) = self.invitation.take() {
                    if self.guest.session.is_some() {
                        toast(view, "Сначала завершите текущее подключение.")?;
                        self.invitation = Some(invitation);
                    } else {
                        match (Invite::decode(&invitation.code), Preferences::parse(&options)) {
                            (Ok(invite), Ok(preferences)) => self.guest.begin(
                                Session::guest(self.data.clone(), invite, preferences, self.me.name.clone()),
                                "Подключаемся к другу…",
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
            Action::HostStop => self.host.stop(),
            Action::Allow => self.host.decide(true),
            Action::Deny => self.host.decide(false),
            Action::Copy if self.host.phase == Phase::Invite => {
                match clipboard_win::set_clipboard_string(&self.host.code) {
                    Ok(()) => view.evaluate_script("window.duo.copied();")?,
                    Err(_) => toast(view, "Не удалось скопировать. Выделите код и нажмите Ctrl+C.")?,
                }
            }
            Action::GuestStart { code, options } if self.guest.session.is_none() => {
                match Invite::decode(&code) {
                    Ok(invite) => match Preferences::parse(&options) {
                        Ok(preferences) => self.guest.begin(
                            Session::guest(self.data.clone(), invite, preferences, self.me.name.clone()),
                            "Готовим подключение…",
                        ),
                        Err(error) => self.guest.fail(error.to_string()),
                    },
                    Err(error) => {
                        self.guest.fail(if code.trim().is_empty() {
                            "Вставьте код приглашения от друга.".into()
                        } else {
                            error.to_string()
                        });
                        self.guest.code_error = true;
                    }
                }
            }
            Action::GuestStop => self.guest.stop(),
            Action::Paste => match clipboard_win::get_clipboard_string() {
                Ok(text) => {
                    let code: String = text.trim().chars().take(256).collect();
                    view.evaluate_script(&format!(
                        "window.duo.pasted({});",
                        serde_json::to_string(&code)?
                    ))?;
                    self.guest.set(Phase::Idle, "");
                }
                Err(_) => toast(view, "В буфере обмена нет текста.")?,
            },
            Action::Install if !self.full_setup => self.start_full_setup(),
            Action::CheckUpdate => match &self.updater {
                Some(updater) => {
                    updater.check_now();
                    self.update = update::Status::Checking;
                }
                None => toast(view, "Обновления отключены для этой сборки.")?,
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
            _ => {}
        }
        self.update(view)
    }

    fn poll(&mut self, view: &WebView) {
        let (host, host_done) = self.host.poll(true);
        let (guest, _) = self.guest.poll(false);
        let (setup, setup_done) = self.setup.poll(false);
        let mut changed = host || guest || setup;
        // Friend invite: hand the session code to the friend once it exists.
        if let Some(target) = &mut self.host_for {
            if target.sent_at.is_none() && self.host.phase == Phase::Invite && !self.host.code.is_empty() {
                let at = friends::now();
                let message = Message::Invite { name: self.me.name.clone(), code: self.host.code.clone(), at };
                friends::send(self.me.seal(&target.key, &message), &target.key);
                target.sent_at = Some(at);
                self.host.message = format!("Приглашение отправлено {}. Ждём ответа…", target.name);
                changed = true;
            }
            if self.host.phase == Phase::Live {
                target.joined = true;
            }
        }
        if host_done && let Some(target) = self.host_for.take() {
            if let (Some(at), false) = (target.sent_at, target.joined) {
                let cancel = self.me.seal(&target.key, &Message::InviteCancel { at });
                friends::send(cancel, &target.key);
            }
            changed = true;
        }
        if self.invitation.as_ref().is_some_and(|i| friends::now().saturating_sub(i.at) >= INVITE_LIFETIME) {
            self.invitation = None;
            changed = true;
        }
        let envelopes: Vec<String> = self.inbox.as_ref().map(|i| i.messages.try_iter().collect()).unwrap_or_default();
        for envelope in envelopes {
            let _ = self.handle_message(view, &envelope);
            changed = true;
        }
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
                    self.host.fail("Без драйвера геймпада друг не сможет играть. Попробуйте ещё раз.");
                } else if self.host.session.is_none() {
                    self.start_host();
                }
            }
        }
        if changed {
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

    fn shutdown(&self) {
        let sessions = [
            self.host.session.as_ref(),
            self.guest.session.as_ref(),
            self.setup.session.as_ref(),
        ];
        for session in sessions.into_iter().flatten() {
            session.cancel();
        }
        let by = Instant::now() + Duration::from_secs(2);
        while Instant::now() < by
            && sessions
                .into_iter()
                .flatten()
                .any(|session| !session.finished())
        {
            thread::sleep(Duration::from_millis(30));
        }
    }
}

fn toast(view: &WebView, message: &str) -> Result<()> {
    view.evaluate_script(&format!(
        "window.duo.toast({});",
        serde_json::to_string(message)?
    ))?;
    Ok(())
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
        _ => {
            return Response::builder()
                .status(404)
                .body(Cow::Borrowed(&[][..]))
                .unwrap();
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
        .with_inner_size(LogicalSize::new(560.0, 700.0))
        .with_min_inner_size(LogicalSize::new(440.0, 560.0))
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
        .with_background_color((12, 12, 13, 255))
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
        .context("Не удалось открыть интерфейс DuoLink. Проверьте Microsoft Edge WebView2 Runtime.")?;
    let waker = event_loop.create_proxy();
    let mut app = App::new(data, move || {
        let _ = waker.send_event(Action::Wake);
    })?;
    window.set_visible(true);

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
            }
            WindowEvent::WindowEvent {
                event: NativeEvent::CloseRequested,
                ..
            } => {
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
            ControlFlow::Wait
        };
    });
}
