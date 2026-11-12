use std::{
    io,
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use rustls::{ClientConnection, ServerConnection, StreamOwned, pki_types::ServerName};
use uuid::Uuid;

use crate::{
    audio::{AUDIO_PORT, GuestAudio, HostAudio, voice_app},
    media::{DEFAULT_FEC, GuestStream, HostStream, MOONLIGHT, PATH_SHARE, Preferences, SUNSHINE, fec_for_loss},
    platform::{
        KeepAwake, ProcessLock, WifiQuiet, background_recording, display_max_refresh, display_mode,
        vigem_present,
    },
    protocol::{
        CONTROL_PORT, ClientKey, Identity, Invite, MEDIA_PORT, Reply, Request, Wire, client_config,
        new_token, valid_pem,
    },
    tunnel::{GuestTunnel, HostTunnel, PROBE_MBPS, describe_peer, dropped},
};

const HEARTBEAT: Duration = Duration::from_secs(1);
const SILENCE: Duration = Duration::from_secs(6);
const DECISION_TIMEOUT: Duration = Duration::from_secs(90);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CODE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MEASURE_TIMEOUT: Duration = Duration::from_secs(9);

#[derive(Debug)]
pub enum Event {
    Status(String),
    Invite(String),
    /// Everyone in the lobby, whenever that changes (and their ping, ten times a second).
    Players(Vec<Player>),
    /// A labelled fact about the live session (mode, bitrate, route…).
    Fact(&'static str, String),
    Streaming,
    /// Round trip to the other PC, ten times a second.
    Latency(Duration),
    Finished { message: String, failed: bool },
}

pub(crate) struct Worker {
    events: Sender<Event>,
    cancelled: Arc<AtomicBool>,
}

impl Worker {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    pub(crate) fn status(&self, message: impl Into<String>) {
        self.emit(Event::Status(message.into()));
    }

    fn fact(&self, key: &'static str, value: impl Into<String>) {
        self.emit(Event::Fact(key, value.into()));
    }

    pub(crate) fn check(&self) -> Result<()> {
        ensure!(!self.cancelled.load(Ordering::Relaxed), "Сессия завершена.");
        Ok(())
    }
}

pub struct Session {
    pub events: Receiver<Event>,
    cancelled: Arc<AtomicBool>,
    commands: Sender<Command>,
    task: Option<thread::JoinHandle<()>>,
}

impl Session {
    pub(crate) fn start(run: impl FnOnce(&Worker, Receiver<Command>) -> Result<String> + Send + 'static) -> Self {
        let (events, receiver) = mpsc::channel();
        let (commands, decisions) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            events,
            cancelled: cancelled.clone(),
        };
        let task = thread::spawn(move || {
            let outcome = run(&worker, decisions);
            let (message, failed) = if worker.cancelled.load(Ordering::Relaxed) {
                ("Сессия завершена.".into(), false)
            } else {
                match outcome {
                    Ok(message) => (message, false),
                    Err(error) => (format!("{error:#}"), true),
                }
            };
            worker.emit(Event::Finished { message, failed });
        });
        Self {
            events: receiver,
            cancelled,
            commands,
            task: Some(task),
        }
    }

    pub fn host(data: PathBuf) -> Self {
        Self::start(move |worker, decision| host(worker, decision, data))
    }

    pub fn guest(data: PathBuf, invite: Invite, preferences: Preferences, nickname: String) -> Self {
        Self::start(move |worker, _| guest(worker, data, invite, preferences, nickname))
    }

    pub fn decide(&self, id: u32, approve: bool) {
        let _ = self.commands.send(Command::Decide(id, approve));
    }

    pub fn kick(&self, id: u32) {
        let _ = self.commands.send(Command::Kick(id));
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn finished(&self) -> bool {
        self.task
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
        if self.finished()
            && let Some(task) = self.task.take()
        {
            let _ = task.join();
        }
    }
}

const LATENCY_EVERY: Duration = Duration::from_millis(100);

type HostWire = Wire<StreamOwned<ServerConnection, TcpStream>>;

fn configure_socket(socket: &TcpStream) -> io::Result<()> {
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    socket.set_write_timeout(Some(Duration::from_secs(3)))
}

fn reserve_check(address: Ipv4Addr) -> Result<()> {
    for port in [MEDIA_PORT - 5, MEDIA_PORT, MEDIA_PORT + 1, MEDIA_PORT + 21] {
        let _socket = TcpListener::bind((address, port))
            .with_context(|| format!("TCP-порт {port} недоступен на {address}."))?;
    }
    for port in [MEDIA_PORT + 9, MEDIA_PORT + 10, MEDIA_PORT + 11, AUDIO_PORT] {
        let _socket = UdpSocket::bind((address, port))
            .with_context(|| format!("UDP-порт {port} недоступен на {address}."))?;
    }
    Ok(())
}

/// One line about how reachable this host is from the internet.
fn reachability(tunnel: &HostTunnel) -> &'static str {
    if tunnel.forwarded {
        "Открыт через UPnP"
    } else if tunnel.reflexive.addr.is_some() && !tunnel.reflexive.symmetric {
        "Через пробивку NAT"
    } else if tunnel.reflexive.symmetric {
        "Строгий NAT"
    } else {
        "Только локальная сеть"
    }
}

fn connection_fact(label: &str, rtt: Duration) -> String {
    let ms = rtt.as_secs_f64() * 1000.0;
    if ms < 10.0 {
        format!("{label} · {ms:.1} мс")
    } else {
        format!("{label} · {ms:.0} мс")
    }
}

/// Silences Wi-Fi background scans for the session and says so.
fn quiet_wifi(worker: &Worker) -> Option<WifiQuiet> {
    let quiet = WifiQuiet::hold()?;
    worker.fact("Wi-Fi", "Без фоновых сканирований");
    Some(quiet)
}

/// Shows tunnel losses only when there are any.
fn report_drops(worker: &Worker, shown: &mut u64) {
    let now = dropped();
    if now != *shown {
        *shown = now;
        worker.fact("Отброшено пакетов", now.to_string());
    }
}

/// Someone in the lobby, as the host's window shows them.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    pub id: u32,
    pub name: String,
    pub state: PlayerState,
    /// Round trip in milliseconds.
    pub latency: Option<f64>,
    pub place: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerState {
    /// Asked to join; the host has not answered yet.
    Waiting,
    /// Approved; queued for its stream or opening it.
    Starting,
    Playing,
}

/// What the host's window asks of a running lobby.
pub enum Command {
    Decide(u32, bool),
    Kick(u32),
}

struct Guest {
    id: u32,
    wire: HostWire,
    name: String,
    /// Local port of the control socket: identifies the guest's tunnel link.
    port: u16,
    fec: Option<u8>,
    state: PlayerState,
    /// Ready has gone out; Sunshine's next "client connected" is this guest.
    sent_ready: bool,
    since: Instant,
    last_ping: Instant,
}

struct Knock {
    wire: HostWire,
    port: u16,
    by: Instant,
}

/// A broken control socket only removes its own guest.
fn reply_to_guest<'a>(guests: &'a mut Vec<Guest>, at: usize, reply: &Reply) -> Option<&'a mut Guest> {
    if guests[at].wire.send(reply).is_err() {
        guests.remove(at);
        return None;
    }
    Some(&mut guests[at])
}

fn host(worker: &Worker, commands: Receiver<Command>, data: PathBuf) -> Result<String> {
    let address = Ipv4Addr::LOCALHOST;
    let _lock = ProcessLock::try_acquire(r"Local\DuoLink.Host.v1")
        .context("Не удалось зарезервировать сессию хоста.")?
        .context("На этом ПК уже открыта сессия DuoLink.")?;
    reserve_check(address)?;
    if !vigem_present() {
        worker.fact("Геймпады", "Нет драйвера");
    }
    if !SUNSHINE.path(&data).is_file() {
        worker.status("Скачиваем Sunshine…");
    }
    let engine = SUNSHINE.ensure(&data, || worker.check())?;
    worker.check()?;

    let identity = Identity::generate("DuoLink Host")?;
    // One client identity for every guest: Sunshine trusts it from the start,
    // so a new guest never waits for a restart.
    let client = Identity::generate("DuoLink Guest")?;
    let host_id = Uuid::new_v4();
    let mut mode = display_mode()?;
    let current_hz = mode.fps;
    mode.fps = display_max_refresh().unwrap_or(current_hz);
    let mut fec = DEFAULT_FEC;
    // Sunshine starts while the code travels to the friends: by the time
    // someone joins, it is already listening.
    let start_engine = |fec: u8| {
        HostStream::start(&engine, &data, address, &identity, host_id, "DuoLink", &client.cert_pem, fec)
    };
    let mut stream = start_engine(fec)?;
    let mut engine_by = Instant::now() + START_TIMEOUT;
    let mut engine_ready = false;

    let token = new_token();
    worker.status("Открываем доступ…");
    let tunnel = HostTunnel::open(&identity, token)?;
    worker.check()?;
    worker.fact("Доступ", reachability(&tunnel));
    worker.fact(
        "Экран",
        if mode.fps > current_hz {
            format!("{}×{} · {current_hz} → {} Гц", mode.width, mode.height, mode.fps)
        } else {
            format!("{}×{} · {} Гц", mode.width, mode.height, mode.fps)
        },
    );
    if background_recording() {
        worker.fact("Xbox Game Bar", "Фоновая запись включена");
    }
    let invite = Invite {
        token,
        fingerprint: identity.fingerprint(),
        candidates: tunnel.candidates.clone(),
    };
    let tls = identity.server_config()?;
    let listener = TcpListener::bind((address, CONTROL_PORT))
        .with_context(|| format!("Не удалось открыть порт {CONTROL_PORT}."))?;
    listener.set_nonblocking(true)?;
    // Game audio without the voice-chat app, so nobody hears themself.
    let audio = match HostAudio::start(address) {
        Ok(audio) => {
            worker.fact(
                "Звук",
                match voice_app() {
                    Some((_, name)) => format!("Без {name}"),
                    None => "Без голосовых чатов".into(),
                },
            );
            Some(audio)
        }
        Err(error) => {
            worker.fact("Звук", format!("{error:#}"));
            None
        }
    };
    let _awake = KeepAwake::hold();
    let mut wifi = None;
    worker.emit(Event::Invite(invite.encode()));

    let expires = Instant::now() + CODE_TIMEOUT;
    let mut joined_once = false;
    let mut knocks: Vec<Knock> = Vec::new();
    let mut guests: Vec<Guest> = Vec::new();
    let mut next_id = 1u32;
    let mut shown: Vec<Player> = Vec::new();
    let mut audio_recipients = Vec::new();
    let mut next_players = Instant::now();
    let mut playing_before = false;
    let mut drops_shown = dropped();
    let mut next_drops = Instant::now();
    let mut next_log = Instant::now();

    let close = |wire: &mut HostWire, reason: &str| {
        let _ = wire.send(&Reply::Closed { reason: reason.into() });
    };
    let reason = 'lobby: loop {
        if worker.check().is_err() {
            break "Сессия завершена.";
        }
        if guests.is_empty() && knocks.is_empty() && !joined_once && Instant::now() >= expires {
            break "Код истёк.";
        }

        // Someone opened the control channel through the tunnel.
        loop {
            match listener.accept() {
                Ok((socket, from)) => {
                    if configure_socket(&socket)
                        .and_then(|()| socket.set_read_timeout(Some(Duration::from_millis(2))))
                        .is_err()
                    {
                        continue;
                    }
                    let wire = Wire::new(StreamOwned::new(ServerConnection::new(tls.clone())?, socket));
                    knocks.push(Knock { wire, port: from.port(), by: Instant::now() + Duration::from_secs(4) });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if matches!(
                    error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                ) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let mut index = 0;
        while index < knocks.len() {
            let knock = &mut knocks[index];
            let request = match knock.wire.poll::<Request>() {
                Ok(Some(request)) => Some(request),
                Ok(None) if Instant::now() < knock.by => {
                    index += 1;
                    continue;
                }
                _ => None,
            };
            let mut knock = knocks.swap_remove(index);
            match request {
                Some(Request::Join { token, name, lobby, controller_hotplug, fec: wanted, .. }) if invite.authorizes(&token) => {
                    if !lobby || !controller_hotplug {
                        close(&mut knock.wire, "Обновите DuoLink: у хоста новая версия.");
                    } else {
                        if knock.wire.send(&Reply::Pending).is_err() {
                            continue;
                        }
                        joined_once = true;
                        guests.push(Guest {
                            id: next_id,
                            wire: knock.wire,
                            name: clean_name(&name),
                            port: knock.port,
                            fec: wanted,
                            state: PlayerState::Waiting,
                            sent_ready: false,
                            since: Instant::now(),
                            last_ping: Instant::now(),
                        });
                        next_id += 1;
                    }
                }
                _ => close(&mut knock.wire, "Неверный код."),
            }
        }

        for command in commands.try_iter() {
            match command {
                Command::Decide(id, approve) => {
                    if let Some(at) = guests.iter().position(|g| g.id == id && g.state == PlayerState::Waiting) {
                        if approve {
                            if let Some(guest) = reply_to_guest(&mut guests, at, &Reply::Preparing) {
                                guest.state = PlayerState::Starting;
                                guest.since = Instant::now();
                            }
                        } else {
                            let mut guest = guests.remove(at);
                            close(&mut guest.wire, "Хост отклонил подключение.");
                        }
                    }
                }
                Command::Kick(id) => {
                    if let Some(at) = guests.iter().position(|g| g.id == id) {
                        let mut guest = guests.remove(at);
                        close(&mut guest.wire, "Хост отключил вас.");
                    }
                }
            }
        }

        // Sunshine: up, still running, and who just connected.
        if engine_ready && Instant::now() >= next_log {
            next_log = Instant::now() + Duration::from_millis(50);
            let (connected, _) = stream.events()?;
            for _ in 0..connected {
                if let Some(at) = guests.iter().position(|g| g.state == PlayerState::Starting && g.sent_ready)
                    && let Some(guest) = reply_to_guest(&mut guests, at, &Reply::Connected)
                {
                    guest.state = PlayerState::Playing;
                }
            }
        } else if !engine_ready && Instant::now() >= next_log {
            next_log = Instant::now() + Duration::from_millis(50);
            if stream.ready(address)? {
                engine_ready = true;
            } else {
                ensure!(Instant::now() < engine_by, "Sunshine не запустился. Подробности в host.log.");
            }
        }

        // One stream opens at a time, so Sunshine's "connected" maps to one guest.
        let opening = guests.iter().any(|g| g.state == PlayerState::Starting && g.sent_ready);
        if engine_ready
            && !opening
            && let Some(at) = guests.iter().position(|g| {
                g.state == PlayerState::Starting
                    && (g.fec.is_some() || g.since.elapsed() >= MEASURE_TIMEOUT)
            })
        {
            // The first guest on a lossy path: restart Sunshine with more
            // redundancy while nobody is watching yet.
            let busy = guests.iter().any(|g| g.state == PlayerState::Playing);
            let wanted_fec = guests[at].fec.unwrap_or(DEFAULT_FEC);
            if !busy && wanted_fec > fec {
                fec = wanted_fec;
                drop(stream);
                stream = start_engine(fec)?;
                engine_ready = false;
                engine_by = Instant::now() + START_TIMEOUT;
            } else {
                tunnel.stop_probes();
                wifi.get_or_insert_with(|| quiet_wifi(worker));
                let reply = Reply::Ready {
                    host: host_id,
                    certificate: identity.cert_pem.clone(),
                    mode,
                    own_audio: audio.is_some(),
                    client: Some(ClientKey { certificate: client.cert_pem.clone(), key: client.key_pem.clone() }),
                };
                if let Some(guest) = reply_to_guest(&mut guests, at, &reply) {
                    guest.sent_ready = true;
                    guest.since = Instant::now();
                }
            }
        }

        // Heartbeats, departures and timeouts.
        let mut index = 0;
        while index < guests.len() {
            let guest = &mut guests[index];
            let timeout = match guest.state {
                PlayerState::Waiting => Some((DECISION_TIMEOUT, "Хост не ответил.")),
                PlayerState::Starting => Some((START_TIMEOUT * 2, "Поток не запустился.")),
                PlayerState::Playing => None,
            };
            let expired = timeout.filter(|(limit, _)| guest.since.elapsed() > *limit);
            let silence = if guest.fec.is_none() { MEASURE_TIMEOUT + SILENCE } else { SILENCE };
            let alive = guest.last_ping.elapsed() <= silence
                && match guest.wire.poll::<Request>() {
                    Ok(Some(Request::Ping)) => {
                        guest.last_ping = Instant::now();
                        guest.wire.send(&Reply::Pong).is_ok()
                    }
                    Ok(Some(Request::Measured { fec })) => {
                        guest.fec = Some(fec);
                        guest.last_ping = Instant::now();
                        true
                    }
                    Ok(None) => true,
                    _ => false,
                };
            if let Some((_, reason)) = expired {
                close(&mut guest.wire, reason);
                guests.remove(index);
            } else if !alive {
                guests.remove(index);
            } else {
                index += 1;
            }
        }

        // The window: who is here, and how far away, ten times a second.
        if Instant::now() >= next_players {
            next_players = Instant::now() + LATENCY_EVERY;
            let mut players = Vec::with_capacity(guests.len());
            audio_recipients.clear();
            for guest in &guests {
                let link = tunnel.link(guest.port);
                if guest.sent_ready
                    && let Some(address) = link.and_then(|link| link.audio)
                    && !audio_recipients.contains(&address)
                {
                    audio_recipients.push(address);
                }
                players.push(Player {
                    id: guest.id,
                    name: guest.name.clone(),
                    state: guest.state,
                    latency: link.map(|link| link.rtt.as_secs_f64() * 1000.0),
                    place: link.map_or("", |link| describe_peer(link.address)),
                });
            }
            if let Some(audio) = &audio {
                audio.set_recipients(&audio_recipients);
            }
            if players != shown {
                shown = players.clone();
                worker.emit(Event::Players(players));
            }
            let playing = guests.iter().any(|g| g.state == PlayerState::Playing);
            if playing && !playing_before {
                worker.emit(Event::Streaming);
            } else if !playing && playing_before {
                worker.emit(Event::Invite(invite.encode()));
            }
            playing_before = playing;
        }
        if Instant::now() >= next_drops {
            next_drops = Instant::now() + Duration::from_secs(2);
            report_drops(worker, &mut drops_shown);
        }
        if guests.iter().all(|g| g.state != PlayerState::Playing) {
            stream.check_running()?;
        }
        thread::sleep(Duration::from_millis(4));
        continue 'lobby;
    };
    // Closing Sunshine removes the virtual controllers before telling the guests.
    drop(stream);
    drop(audio);
    for guest in &mut guests {
        close(&mut guest.wire, "Хост завершил сессию.");
    }
    drop(guests);
    drop(tunnel);
    Ok(reason.into())
}

pub(crate) fn clean_name(name: &str) -> String {
    let result: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(30)
        .collect();
    if result.is_empty() {
        "Гость".into()
    } else {
        result
    }
}

fn guest(
    worker: &Worker,
    data: PathBuf,
    invite: Invite,
    mut preferences: Preferences,
    nickname: String,
) -> Result<String> {
    if !MOONLIGHT.path(&data).is_file() {
        worker.status("Скачиваем Moonlight…");
    }
    let engine = MOONLIGHT.ensure(&data, || worker.check())?;
    worker.check()?;

    worker.status("Ищем хост…");
    let tunnel = GuestTunnel::connect(invite.token, invite.fingerprint, &invite.candidates, || {
        worker.check()
    })?;
    worker.fact("Соединение", connection_fact(tunnel.describe(), tunnel.rtt()));
    let _wifi = quiet_wifi(worker);
    worker.status("Ждём хост…");
    let loopback = tunnel.loopback;
    let endpoint = SocketAddr::from((loopback, CONTROL_PORT));
    let mut socket = TcpStream::connect_timeout(&endpoint, Duration::from_secs(5))
        .context("Хост не ответил.")?;
    socket.set_read_timeout(Some(Duration::from_secs(4)))?;
    socket.set_write_timeout(Some(Duration::from_secs(4)))?;
    let mut tls = ClientConnection::new(
        client_config(&invite),
        ServerName::from(std::net::IpAddr::from(loopback)),
    )?;
    tls.complete_io(&mut socket)
        .context("Не удалось связаться с хостом.")?;
    configure_socket(&socket)?;
    let mut wire = Wire::new(StreamOwned::new(tls, socket));
    // Ask first: the host sees the request at once, and the path is measured
    // while they decide instead of before.
    wire.send(&Request::Join {
        token: invite.token,
        name: clean_name(&nickname),
        certificate: String::new(),
        fec: None,
        lobby: true,
        controller_hotplug: true,
    })
    .context("Не удалось связаться с хостом.")?;
    worker.status("Меряем канал…");
    let channel = tunnel.measure(PROBE_MBPS, Duration::from_millis(700));
    let path_mbps = channel.map(|c| c.mbps);
    if let Some(channel) = channel {
        worker.fact(
            "Канал",
            if channel.limited {
                format!("≈ {:.0} Мбит/с", channel.mbps)
            } else {
                format!("быстрее {:.0} Мбит/с", channel.mbps.floor())
            },
        );
    }
    // Random loss at the rate the stream will actually use (video + FEC +
    // audio) decides how much redundancy Sunshine adds.
    let loss_probe = channel.and_then(|channel| {
        worker.status("Меряем потери…");
        let rate = (channel.mbps * PATH_SHARE * 1.3).clamp(5.0, 150.0) as u32;
        tunnel.measure(rate, Duration::from_millis(1000))
    });
    let fec = match loss_probe {
        Some(probe) => {
            let fec = fec_for_loss(probe.loss);
            worker.fact("Потери", format!("{:.1} % · коррекция {fec} %", probe.loss * 100.0));
            fec
        }
        None => DEFAULT_FEC,
    };
    wire.send(&Request::Measured { fec }).context("Не удалось связаться с хостом.")?;
    worker.status("Ждём, пока хост разрешит…");
    let mut next_rtt = Instant::now() + Duration::from_secs(2);
    let mut next_latency = Instant::now();
    let mut drops_shown = dropped();

    let mut last_pong = Instant::now();
    let mut next_ping = Instant::now();
    let mut by = Instant::now() + DECISION_TIMEOUT + SILENCE;
    let mut stream = None::<GuestStream>;
    // Held for the session; dropping it stops playback.
    let mut _guest_audio = None::<GuestAudio>;
    let mut streaming = false;
    loop {
        if worker.check().is_err() {
            let _ = wire.send(&Request::Leave);
            return Ok("Сессия завершена.".into());
        }
        if let Some(stream) = stream.as_mut()
            && let Some(status) = stream.process.status()?
        {
            let _ = wire.send(&Request::Leave);
            if status.success() && streaming {
                return Ok("Поток закрыт.".into());
            }
            bail!("Moonlight закрылся с ошибкой. Подробности в guest.log.");
        }
        if !tunnel.alive() || last_pong.elapsed() > SILENCE {
            // The host may have said goodbye just before the tunnel closed.
            for _ in 0..5 {
                if let Ok(Some(Reply::Closed { reason })) = wire.poll::<Reply>() {
                    return Ok(reason);
                }
            }
            bail!("Связь с хостом потеряна.");
        }
        if Instant::now() >= next_latency {
            worker.emit(Event::Latency(tunnel.rtt()));
            next_latency = Instant::now() + LATENCY_EVERY;
        }
        if Instant::now() >= next_rtt {
            worker.fact("Соединение", connection_fact(tunnel.describe(), tunnel.rtt()));
            report_drops(worker, &mut drops_shown);
            next_rtt = Instant::now() + Duration::from_secs(2);
        }
        ensure!(
            streaming || Instant::now() < by,
            "Хост не ответил."
        );
        if Instant::now() >= next_ping {
            wire.send(&Request::Ping)?;
            next_ping = Instant::now() + HEARTBEAT;
        }
        match wire.poll::<Reply>().context("Связь с хостом закрыта.")? {
            Some(Reply::Pending) => worker.status("Ждём, пока хост разрешит…"),
            Some(Reply::Preparing) => {
                by = Instant::now() + START_TIMEOUT + SILENCE;
                worker.status("Хост разрешил. Запускаем…");
            }
            Some(Reply::Ready {
                host,
                certificate,
                mode,
                own_audio,
                client: Some(client),
            }) if stream.is_none() && valid_pem(&certificate) && valid_pem(&client.certificate) => {
                let mode = preferences.mode(mode, display_mode().ok());
                ensure!(
                    mode.width > 0 && mode.height > 0 && mode.fps > 0,
                    "Хост прислал неверные данные."
                );
                // Highest visible quality, unless a frame could not cross the
                // measured path in half a frame interval.
                let (bitrate, capped) = preferences.auto_bitrate(mode, path_mbps);
                preferences.bitrate = Some(bitrate);
                worker.fact("Режим", format!("{}×{} · {} FPS", mode.width, mode.height, mode.fps));
                worker.fact(
                    "Битрейт",
                    if capped {
                        format!("{} Мбит/с · по каналу", bitrate.div_ceil(1000))
                    } else {
                        format!("{} Мбит/с", bitrate.div_ceil(1000))
                    },
                );
                worker.fact("Кодек", preferences.codec.label());
                worker.status("Открываем игру…");
                stream = Some(GuestStream::start(
                    &engine,
                    &data,
                    loopback,
                    tunnel.same_machine,
                    own_audio,
                    host,
                    &certificate,
                    &client,
                    mode,
                    preferences,
                )?);
                if own_audio {
                    _guest_audio = Some(GuestAudio::start(
                        SocketAddr::from((loopback, AUDIO_PORT)),
                        tunnel.same_machine,
                    )?);
                }
                by = Instant::now() + START_TIMEOUT + SILENCE;
            }
            Some(Reply::Ready { .. }) => bail!("Хост прислал неверные данные."),
            Some(Reply::Connected) => {
                streaming = true;
                worker.emit(Event::Streaming);
            }
            Some(Reply::Pong) => last_pong = Instant::now(),
            Some(Reply::Closed { reason }) => return Ok(reason),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::data_dir;

    #[test]
    fn a_failed_reply_only_removes_its_guest() {
        let identity = Identity::generate("DuoLink Test").unwrap();
        let config = identity.server_config().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let make_guest = |id, broken| {
            let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let (socket, from) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            if broken {
                socket.shutdown(std::net::Shutdown::Both).unwrap();
            }
            let wire = Wire::new(StreamOwned::new(ServerConnection::new(config.clone()).unwrap(), socket));
            let now = Instant::now();
            (Guest {
                id,
                wire,
                name: format!("Guest {id}"),
                port: from.port(),
                fec: Some(DEFAULT_FEC),
                state: PlayerState::Starting,
                sent_ready: false,
                since: now,
                last_ping: now,
            }, peer)
        };
        let (broken, _broken_peer) = make_guest(1, true);
        let (healthy, peer) = make_guest(2, false);
        let mut guests = vec![broken, healthy];
        assert!(reply_to_guest(&mut guests, 0, &Reply::Preparing).is_none());
        assert_eq!(guests.len(), 1);

        let invite = Invite { token: new_token(), fingerprint: identity.fingerprint(), candidates: Vec::new() };
        let listener = thread::spawn(move || {
            let connection = ClientConnection::new(
                client_config(&invite),
                ServerName::from(std::net::IpAddr::from(Ipv4Addr::LOCALHOST)),
            ).unwrap();
            let mut wire = Wire::new(StreamOwned::new(connection, peer));
            (0..3).find_map(|_| wire.poll::<Reply>().unwrap())
        });
        assert_eq!(reply_to_guest(&mut guests, 0, &Reply::Pong).unwrap().id, 2);
        assert!(matches!(listener.join().unwrap(), Some(Reply::Pong)));
    }

    fn until(
        session: &Session,
        timeout: Duration,
        mut accept: impl FnMut(Event) -> Option<String>,
    ) -> String {
        let by = Instant::now() + timeout;
        loop {
            let event = session
                .events
                .recv_timeout(by.saturating_duration_since(Instant::now()))
                .expect("timed out");
            match &event {
                Event::Invite(_) => eprintln!("Одноразовый код создан"),
                Event::Players(players) => eprintln!("Игроки: {players:?}"),
                Event::Latency(_) => {}
                _ => eprintln!("{event:?}"),
            }
            if let Some(value) = accept(event) {
                return value;
            }
        }
    }

    /// Waits for someone to ask to join and returns their id.
    fn waiting(host: &Session) -> u32 {
        until(host, Duration::from_secs(60), |event| match event {
            Event::Players(players) => players
                .iter()
                .find(|p| p.state == PlayerState::Waiting)
                .map(|p| p.id.to_string()),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        })
        .parse()
        .unwrap()
    }

    /// WASAPI loopback, which Sunshine captures, delivers nothing while no
    /// application renders audio. Keep a silent stream playing for the test.
    struct SilentAudio(std::process::Child);

    impl SilentAudio {
        fn start() -> Self {
            use std::io::Write;
            let path = std::env::temp_dir().join("duolink-silence.wav");
            let samples = 48_000u32 * 4 * 120;
            let mut wav = Vec::with_capacity(44 + samples as usize);
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + samples).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
            wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
            wav.extend_from_slice(&48_000u32.to_le_bytes());
            wav.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
            wav.extend_from_slice(&4u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&samples.to_le_bytes());
            wav.resize(44 + samples as usize, 0);
            std::fs::File::create(&path).unwrap().write_all(&wav).unwrap();
            let script = format!(
                "(New-Object System.Media.SoundPlayer '{}').PlaySync()",
                path.display()
            );
            let child = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-Command", &script])
                .spawn()
                .unwrap();
            thread::sleep(Duration::from_millis(800));
            Self(child)
        }
    }

    impl Drop for SilentAudio {
        fn drop(&mut self) {
            let _ = self.0.kill();
        }
    }

    #[test]
    #[ignore = "needs an audio device"]
    fn live_audio_without_voice_chat() {
        let _audio = SilentAudio::start();
        let host = HostAudio::start(Ipv4Addr::LOCALHOST).unwrap();
        let target = SocketAddr::from((Ipv4Addr::LOCALHOST, AUDIO_PORT));
        let first = GuestAudio::start(target, true).unwrap();
        let second = GuestAudio::start(target, true).unwrap();
        let packets = |guest: &GuestAudio| guest.received.load(Ordering::Relaxed);

        host.set_recipients(&[first.address]);
        thread::sleep(Duration::from_secs(2));
        let first_before = packets(&first);
        assert!(first_before > 150, "audio capture: {first_before}");
        assert_eq!(packets(&second), 0, "unapproved receiver got audio");

        host.set_recipients(&[first.address, second.address]);
        thread::sleep(Duration::from_secs(2));
        let first_delta = packets(&first) - first_before;
        let second_delta = packets(&second);
        eprintln!("audio packets during 2 s: first={first_delta}, second={second_delta}");
        assert!(first_delta > 150 && second_delta > 150);

        host.set_recipients(&[second.address]);
        thread::sleep(Duration::from_millis(150));
        let (first_before, second_before) = (packets(&first), packets(&second));
        thread::sleep(Duration::from_secs(1));
        assert_eq!(packets(&first), first_before, "removed receiver still got audio");
        assert!(packets(&second) - second_before > 100, "remaining receiver lost audio");
    }

    #[test]
    #[ignore = "needs interactive Windows desktop, display/audio and engine downloads"]
    fn live_loopback() {
        let _audio = SilentAudio::start();
        assert!(
            std::env::var_os("DUOLINK_DATA_DIR").is_some(),
            "set DUOLINK_DATA_DIR to an isolated test directory"
        );
        let data = data_dir().unwrap();
        let address = Ipv4Addr::LOCALHOST;
        let host = Session::host(data.clone());
        let code = until(&host, Duration::from_secs(200), |event| match event {
            Event::Invite(code) => Some(code),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        });
        let mut bad_invite = Invite::decode(&code).unwrap();
        bad_invite.token[0] ^= 1;
        let unauthorized = Session::guest(data.clone(), bad_invite, Preferences::default(), "Тест".into());
        let result = until(
            &unauthorized,
            Duration::from_secs(200),
            |event| match event {
                Event::Finished {
                    message: reason, ..
                } => Some(reason),
                _ => None,
            },
        );
        assert!(result.contains("отклонил"), "{result}");
        assert!(host.events.try_recv().is_err());
        let invite = Invite::decode(&code).unwrap();
        let prefs = Preferences::parse(&crate::media::Form {
            width: "1920".into(),
            height: "1080".into(),
            fps: "144".into(),
            bitrate: "30".into(),
            stats: true,
            ..Default::default()
        })
        .unwrap();
        let guest = Session::guest(data.clone(), invite, prefs, "Тест".into());
        let first = waiting(&host);
        host.decide(first, true);
        for session in [&host, &guest] {
            until(session, Duration::from_secs(50), |event| match event {
                Event::Streaming => Some(String::new()),
                Event::Finished { message: error, .. } => panic!("{error}"),
                _ => None,
            });
        }
        let by = Instant::now() + Duration::from_secs(12);
        loop {
            let log = std::fs::read_to_string(data.join("logs/guest.log")).unwrap_or_default();
            if log.contains("Received first video packet")
                && log.contains("Received first audio packet")
                && log.contains("Decoded frame with POC 0/0")
            {
                assert!(
                    log.contains("mode=1920x1080x144"),
                    "the requested stream mode was not forwarded"
                );
                break;
            }
            assert!(
                Instant::now() < by,
                "audio/video packets or decoded stream frame absent from guest.log"
            );
            thread::sleep(Duration::from_millis(250));
        }
        // DUOLINK_TEST_HOLD=<seconds> keeps the stream open for a manual look;
        // closing Moonlight (Ctrl+Alt+Shift+Q) ends it early.
        if let Some(hold) = std::env::var("DUOLINK_TEST_HOLD").ok().and_then(|s| s.parse().ok()) {
            let by = Instant::now() + Duration::from_secs(hold);
            while Instant::now() < by && !guest.finished() {
                thread::sleep(Duration::from_millis(250));
            }
            if guest.finished() {
                drop(guest);
                return;
            }
        }

        // A second guest joins the same lobby with the same code and gets its own stream.
        let second = Session::guest(data.clone(), Invite::decode(&code).unwrap(), prefs, "Второй".into());
        let id = waiting(&host);
        assert_ne!(id, first);
        host.decide(id, true);
        until(&second, Duration::from_secs(50), |event| match event {
            Event::Streaming => Some(String::new()),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        });
        thread::sleep(Duration::from_secs(2));
        let host_log = std::fs::read_to_string(data.join("logs/host.log")).unwrap();
        assert!(host_log.matches("CLIENT CONNECTED").count() >= 2, "second stream did not connect");

        // The first guest leaves; the lobby stays and the second keeps playing.
        guest.cancel();
        until(&host, Duration::from_secs(10), |event| match event {
            Event::Players(players) if players.len() == 1 && players[0].name == "Второй" => Some(String::new()),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        });
        assert!(!second.finished());

        host.cancel();
        let result = until(&second, Duration::from_secs(10), |event| match event {
            Event::Finished { message: reason, .. } => Some(reason),
            _ => None,
        });
        assert!(result.contains("Хост завершил") || result.contains("закрыт"), "{result}");
        until(&host, Duration::from_secs(10), |event| match event {
            Event::Finished { .. } => Some(String::new()),
            _ => None,
        });
        thread::sleep(Duration::from_millis(500));
        assert!(
            TcpStream::connect((address, MEDIA_PORT)).is_err(),
            "media still listening after the lobby closed"
        );

        let host = Session::host(data.clone());
        let code = until(&host, Duration::from_secs(20), |event| match event {
            Event::Invite(code) => Some(code),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        });
        let guest = Session::guest(data, Invite::decode(&code).unwrap(), prefs, "Тест".into());
        let id = waiting(&host);
        host.decide(id, false);
        let result = until(&guest, Duration::from_secs(8), |event| match event {
            Event::Finished {
                message: reason, ..
            } => Some(reason),
            _ => None,
        });
        assert!(result.contains("Хост отклонил подключение"), "{result}");
        host.cancel();
    }
}
