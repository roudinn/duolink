use std::{
    borrow::Cow,
    ffi::OsStr,
    fs,
    net::Ipv4Addr,
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
    window::{Icon, Theme, WindowBuilder},
};
use wry::{
    NewWindowResponse, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows,
    http::Response,
};

use crate::{
    media::{Form, Preferences},
    platform::{Network, NetworkKind, data_dir, networks, open},
    protocol::Invite,
    session::{Event, Session},
    setup::{self, Components},
};

const VIEW: &str = "duolink://localhost/index.html";
const WINDOWS_VIEW: &str = "https://duolink.localhost/index.html";
const GRAPHICS: &str = "ms-settings:display-advancedgraphics";
const SHARING: &str = "https://login.tailscale.com/admin/machines";
const LOGIN_WATCH: Duration = Duration::from_secs(180);

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    Ready,
    Refresh,
    HostStart { address: Ipv4Addr },
    HostStop,
    Allow,
    Deny,
    Copy,
    GuestStart { code: String, options: Form },
    GuestStop,
    Paste,
    Install,
    TailscaleLogin,
    Sharing,
    Graphics,
    Logs,
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
    networks: Vec<Network>,
    components: Components,
    /// The running setup includes the elevated step (driver, Tailscale, firewall).
    full_setup: bool,
    version: &'static str,
    #[serde(skip)]
    data: PathBuf,
    #[serde(skip)]
    login_watch: Option<(Instant, Instant)>,
}

impl App {
    fn new(data: PathBuf) -> Self {
        let mut app = Self {
            host: Pane::default(),
            guest: Pane::default(),
            setup: Pane::default(),
            networks: networks(),
            components: Components::detect(&data),
            full_setup: false,
            version: env!("CARGO_PKG_VERSION"),
            data,
            login_watch: None,
        };
        // Streaming engines need no permission: fetch them right away.
        if !app.components.engines() {
            app.setup
                .begin(setup::install(app.data.clone(), false), "Загружаем компоненты…");
        }
        app
    }

    fn refresh(&mut self) {
        self.networks = networks();
        self.components = Components::detect(&self.data);
    }

    fn tailscale_up(&self) -> bool {
        self.networks
            .iter()
            .any(|network| matches!(network.kind, NetworkKind::Tailscale))
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
            Action::Ready => {}
            Action::Refresh => self.refresh(),
            Action::HostStart { address } if self.host.session.is_none() => {
                if self.networks.iter().any(|network| network.address == address) {
                    self.host
                        .begin(Session::host(self.data.clone(), address), "Готовим приглашение…");
                } else {
                    self.networks = networks();
                    self.host.fail("Эта сеть больше недоступна. Выберите другую.");
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
                            Session::guest(self.data.clone(), invite, preferences),
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
            Action::Install if !self.full_setup => {
                // Supersede a background engine prefetch: the full setup fetches them too.
                if let Some(prefetch) = self.setup.session.take() {
                    prefetch.cancel();
                }
                self.full_setup = true;
                self.setup
                    .begin(setup::install(self.data.clone(), true), "Готовим установку…");
            }
            Action::TailscaleLogin => match setup::tailscale_login() {
                Ok(()) => {
                    let now = Instant::now();
                    self.login_watch = Some((now + LOGIN_WATCH, now));
                    toast(view, "Войдите в Tailscale в открывшемся браузере.")?;
                }
                Err(error) => toast(view, &format!("{error:#}"))?,
            },
            Action::Sharing => open_or_toast(view, OsStr::new(SHARING))?,
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
        let (host, _) = self.host.poll(true);
        let (guest, _) = self.guest.poll(false);
        let (setup, setup_done) = self.setup.poll(false);
        let mut changed = host || guest || setup;
        if setup_done {
            self.full_setup = false;
            self.refresh();
        }
        // After «Войти в Tailscale», watch for the tailnet address to appear.
        if let Some((until, last)) = self.login_watch {
            let now = Instant::now();
            if self.tailscale_up() || now >= until {
                self.login_watch = None;
            } else if now.duration_since(last) >= Duration::from_secs(2) {
                self.login_watch = Some((until, now));
                let had = self.tailscale_up();
                self.networks = networks();
                changed |= self.tailscale_up() != had;
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
            || self.login_watch.is_some()
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
    let mut app = App::new(data);
    window.set_visible(true);

    event_loop.run(move |event, _, control| {
        match event {
            WindowEvent::UserEvent(action) => {
                if let Err(error) = app.action(action, &view) {
                    let _ = toast(&view, &error.to_string());
                }
            }
            WindowEvent::MainEventsCleared => app.poll(&view),
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
