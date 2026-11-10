use std::{
    borrow::Cow,
    ffi::OsStr,
    fs,
    net::Ipv4Addr,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tao::{
    dpi::{LogicalSize, PhysicalPosition},
    event::{Event as WindowEvent, WindowEvent as NativeEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::{Theme, WindowBuilder},
};
use wry::{
    NewWindowResponse, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows,
    http::Response,
};

use crate::{
    media::{Form, Preferences},
    platform::{Network, data_dir, hags_enabled, networks, open, vigem_present},
    protocol::Invite,
    session::{Event, Session},
};

const VIEW: &str = "duolink://localhost/index.html";
const WINDOWS_VIEW: &str = "https://duolink.localhost/index.html";
const TAILSCALE: &str = "https://tailscale.com/download/windows";
const VIGEM: &str = "https://github.com/nefarius/ViGEmBus/releases/tag/v1.22.0";
const GRAPHICS: &str = "ms-settings:display-advancedgraphics";

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    Ready,
    Refresh,
    HostStart {
        address: Ipv4Addr,
    },
    HostStop,
    Allow,
    Deny,
    Copy,
    GuestStart {
        code: String,
        options: Form,
    },
    GuestStop,
    Paste,
    Tailscale,
    Driver,
    Graphics,
    Firewall,
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
                if accept {
                    "Разрешаем подключение…"
                } else {
                    "Отклоняем запрос…"
                },
            );
        }
    }

    fn poll(&mut self, host: bool) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        let events: Vec<Event> = session.events.try_iter().collect();
        let changed = !events.is_empty();
        for event in events {
            if self.phase == Phase::Stopping && !matches!(event, Event::Finished { .. }) {
                continue;
            }
            match event {
                Event::Status(message) => self.message = message,
                Event::Fact(key, value) => {
                    match self.facts.iter_mut().find(|(name, _)| *name == key) {
                        Some(entry) => entry.1 = value,
                        None => self.facts.push((key, value)),
                    }
                }
                Event::Invite(code) => {
                    self.set(Phase::Invite, "Отправьте код другу. Вы подтвердите его запрос перед началом.");
                    self.code = code;
                }
                Event::Approval(peer) => {
                    self.set(Phase::Approval, "Гость получит изображение основного экрана, звук и управление с геймпада.");
                    self.peer = peer;
                }
                Event::Streaming => self.set(Phase::Live, if host {
                    "Друг подключён. Добавьте второго игрока в локальном режиме вашей игры."
                } else {
                    "Изображение игры открылось в отдельном окне. Возьмите геймпад и выберите второго игрока."
                }),
                Event::Finished { message, failed } => {
                    self.set(Phase::Idle, message);
                    self.error = failed;
                    self.session.take();
                }
            }
        }
        changed
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct App {
    host: Pane,
    guest: Pane,
    networks: Vec<Network>,
    driver: bool,
    hags: Option<bool>,
    setup_message: String,
    setup_error: bool,
    #[serde(skip)]
    data: PathBuf,
}

impl App {
    fn new(data: PathBuf) -> Self {
        Self {
            host: Pane::default(),
            guest: Pane::default(),
            networks: networks(),
            driver: vigem_present(),
            hags: hags_enabled(),
            setup_message: String::new(),
            setup_error: false,
            data,
        }
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
            Action::Refresh => {
                self.networks = networks();
                self.driver = vigem_present();
                self.hags = hags_enabled();
                self.setup_message.clear();
                self.setup_error = false;
            }
            Action::HostStart { address } if self.host.session.is_none() => {
                if self
                    .networks
                    .iter()
                    .any(|network| network.address == address)
                {
                    self.host
                        .begin(Session::host(self.data.clone(), address), "Готовим приглашение…");
                } else {
                    self.host
                        .set(Phase::Idle, "Эта сеть недоступна. Обновите список сетей.");
                    self.host.error = true;
                }
            }
            Action::HostStop => self.host.stop(),
            Action::Allow => self.host.decide(true),
            Action::Deny => self.host.decide(false),
            Action::Copy if self.host.phase == Phase::Invite => {
                match clipboard_win::set_clipboard_string(&self.host.code) {
                    Ok(()) => view.evaluate_script("window.duo.copied();")?,
                    Err(_) => toast(
                        view,
                        "Не удалось скопировать код. Выделите его и нажмите Ctrl+C.",
                    )?,
                }
            }
            Action::GuestStart { code, options } if self.guest.session.is_none() => match Invite::decode(&code) {
                Ok(invite) => match Preferences::parse(&options) {
                    Ok(preferences) => self.guest.begin(
                        Session::guest(self.data.clone(), invite, preferences),
                        "Готовим подключение…",
                    ),
                    Err(error) => {
                        self.guest.set(Phase::Idle, error.to_string());
                        self.guest.error = true;
                    }
                },
                Err(error) => {
                    self.guest.set(
                        Phase::Idle,
                        if code.trim().is_empty() {
                            "Вставьте код приглашения от хоста.".into()
                        } else {
                            error.to_string()
                        },
                    );
                    self.guest.error = true;
                    self.guest.code_error = true;
                }
            },
            Action::GuestStop => self.guest.stop(),
            Action::Paste => {
                if let Ok(text) = clipboard_win::get_clipboard_string() {
                    let code: String = text.trim().chars().take(256).collect();
                    view.evaluate_script(&format!(
                        "window.duo.pasted({});",
                        serde_json::to_string(&code)?
                    ))?;
                    self.guest.set(Phase::Idle, "");
                } else {
                    toast(view, "В буфере обмена нет текста.")?;
                }
            }
            Action::Tailscale => self.setup_result(open(OsStr::new(TAILSCALE))),
            Action::Driver => self.setup_result(open(OsStr::new(VIGEM))),
            Action::Graphics => self.setup_result(open(OsStr::new(GRAPHICS))),
            Action::Firewall => self.setup_result(firewall()),
            Action::Logs => {
                let directory = self.data.join("logs");
                if let Err(error) = fs::create_dir_all(&directory)
                    .map_err(anyhow::Error::from)
                    .and_then(|_| open(directory.as_os_str()))
                {
                    toast(view, &format!("Не удалось открыть журналы. {error}"))?;
                }
            }
            _ => {}
        }
        self.update(view)
    }

    fn setup_result(&mut self, result: Result<()>) {
        self.setup_error = result.is_err();
        self.setup_message = result
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
    }

    fn poll(&mut self, view: &WebView) {
        let host_changed = self.host.poll(true);
        let guest_changed = self.guest.poll(false);
        if host_changed || guest_changed {
            let _ = self.update(view);
        }
    }

    fn active(&self) -> bool {
        self.host.session.is_some() || self.guest.session.is_some()
    }

    fn shutdown(&self) {
        let sessions = [self.host.session.as_ref(), self.guest.session.as_ref()];
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

fn firewall() -> Result<()> {
    let executable = std::env::current_exe()?;
    let script = executable
        .parent()
        .context("Не найдена папка приложения.")?
        .join("Configure-Host.cmd");
    ensure!(
        script.is_file(),
        "Распакуйте весь архив DuoLink и запустите Configure-Host.cmd из папки приложения."
    );
    open(script.as_os_str())
}

fn asset(path: &str) -> Response<Cow<'static, [u8]>> {
    let (content_type, body): (&str, &'static [u8]) = match path {
        "/index.html" => ("text/html; charset=utf-8", include_bytes!("ui/index.html")),
        "/style.css" => ("text/css; charset=utf-8", include_bytes!("ui/style.css")),
        "/app.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("ui/app.js"),
        ),
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
        .with_inner_size(LogicalSize::new(720.0, 680.0))
        .with_min_inner_size(LogicalSize::new(480.0, 460.0))
        .with_visible(false)
        .build(&event_loop)?;
    if let Some(monitor) = window.current_monitor() {
        let screen = monitor.size();
        let origin = monitor.position();
        let size = window.outer_size();
        let x = screen.width.saturating_sub(size.width) / 2;
        let y = screen.height.saturating_sub(size.height) / 2;
        window.set_outer_position(PhysicalPosition::new(
            origin.x + x as i32,
            origin.y + y as i32,
        ));
    }
    let proxy = event_loop.create_proxy();
    let view = WebViewBuilder::new_with_web_context(&mut context)
        .with_background_color((16, 16, 18, 255))
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
        .context(
            "Не удалось открыть интерфейс DuoLink. Проверьте Microsoft Edge WebView2 Runtime.",
        )?;
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
