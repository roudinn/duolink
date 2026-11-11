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
    media::{DEFAULT_FEC, GuestStream, HostStream, MOONLIGHT, PATH_SHARE, Preferences, SUNSHINE, fec_for_loss},
    platform::{
        KeepAwake, ProcessLock, WifiQuiet, background_recording, display_max_refresh, display_mode,
        vigem_present,
    },
    protocol::{
        CONTROL_PORT, Identity, Invite, MEDIA_PORT, Reply, Request, Wire, client_config,
        new_token, valid_pem,
    },
    tunnel::{GUEST_LOOPBACK, GuestTunnel, HostTunnel, PROBE_MBPS, describe_peer, dropped},
};

const HEARTBEAT: Duration = Duration::from_secs(1);
const SILENCE: Duration = Duration::from_secs(6);
const DECISION_TIMEOUT: Duration = Duration::from_secs(90);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CODE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug)]
pub enum Event {
    Status(String),
    Invite(String),
    Approval(String),
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
    decision: Sender<bool>,
    task: Option<thread::JoinHandle<()>>,
}

impl Session {
    pub(crate) fn start(run: impl FnOnce(&Worker, Receiver<bool>) -> Result<String> + Send + 'static) -> Self {
        let (events, receiver) = mpsc::channel();
        let (decision, decisions) = mpsc::channel();
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
            decision,
            task: Some(task),
        }
    }

    pub fn host(data: PathBuf) -> Self {
        Self::start(move |worker, decision| host(worker, decision, data))
    }

    pub fn guest(data: PathBuf, invite: Invite, preferences: Preferences, nickname: String) -> Self {
        Self::start(move |worker, _| guest(worker, data, invite, preferences, nickname))
    }

    pub fn decide(&self, approve: bool) {
        let _ = self.decision.send(approve);
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
    for port in [MEDIA_PORT + 9, MEDIA_PORT + 10, MEDIA_PORT + 11] {
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

fn host(worker: &Worker, decision: Receiver<bool>, data: PathBuf) -> Result<String> {
    let address = Ipv4Addr::LOCALHOST;
    let _lock = ProcessLock::try_acquire(r"Local\DuoLink.Host.v1")
        .context("Не удалось зарезервировать сессию хоста.")?
        .context("На этом ПК уже открыта сессия DuoLink.")?;
    reserve_check(address)?;
    if !vigem_present() {
        worker.fact("Геймпад гостя", "Нет драйвера");
    }
    if !SUNSHINE.path(&data).is_file() {
        worker.status("Скачиваем Sunshine…");
    }
    let engine = SUNSHINE.ensure(&data, || worker.check())?;
    worker.check()?;

    let identity = Identity::generate("DuoLink Host")?;
    let token = new_token();
    worker.status("Открываем доступ…");
    let tunnel = HostTunnel::open(&identity, token)?;
    worker.check()?;
    worker.fact("Доступ", reachability(&tunnel));
    let invite = Invite {
        token,
        fingerprint: identity.fingerprint(),
        candidates: tunnel.candidates.clone(),
    };
    let tls = identity.server_config()?;
    let listener = TcpListener::bind((address, CONTROL_PORT))
        .with_context(|| format!("Не удалось открыть порт {CONTROL_PORT}."))?;
    listener.set_nonblocking(true)?;
    worker.emit(Event::Invite(invite.encode()));

    let expires = Instant::now() + CODE_TIMEOUT;
    let (mut wire, name, peer, certificate, fec) = loop {
        worker.check()?;
        ensure!(
            Instant::now() < expires,
            "Код истёк."
        );
        let (socket, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(70));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        configure_socket(&socket)?;
        let mut wire = Wire::new(StreamOwned::new(
            ServerConnection::new(tls.clone())?,
            socket,
        ));
        let hello = Instant::now() + Duration::from_secs(4);
        let request = loop {
            worker.check()?;
            if Instant::now() >= hello {
                break None;
            }
            match wire.poll::<Request>() {
                Ok(Some(request)) => break Some(request),
                Ok(None) => {}
                Err(_) => break None,
            }
        };
        if let Some(Request::Join {
            token,
            name,
            certificate,
            fec,
        }) = request
            && invite.authorizes(&token)
            && valid_pem(&certificate)
        {
            let name = clean_name(&name);
            wire.send(&Reply::Pending)?;
            let peer = tunnel.peer().map(|(addr, _)| addr.ip().to_string()).unwrap_or_default();
            break (wire, name, peer, certificate, fec.unwrap_or(DEFAULT_FEC));
        }
        let _ = wire.send(&Reply::Closed {
            reason: "Неверный код.".into(),
        });
    };
    // This invitation admits a single request. Once used, the listening socket closes.
    drop(listener);
    worker.emit(Event::Approval(name.clone()));
    worker.fact("Гость", format!("{name} · {peer}"));
    if background_recording() {
        worker.fact("Xbox Game Bar", "Фоновая запись включена");
    }
    let mut next_rtt = Instant::now();
    let mut next_latency = Instant::now();
    let mut drops_shown = dropped();
    let mut report = |worker: &Worker| {
        if Instant::now() >= next_latency
            && let Some((_, rtt)) = tunnel.peer()
        {
            worker.emit(Event::Latency(rtt));
            next_latency = Instant::now() + LATENCY_EVERY;
        }
        if Instant::now() >= next_rtt
            && let Some((addr, rtt)) = tunnel.peer()
        {
            worker.fact("Соединение", connection_fact(describe_peer(addr), rtt));
            report_drops(worker, &mut drops_shown);
            next_rtt = Instant::now() + Duration::from_secs(2);
        }
    };
    report(worker);

    let mut last_ping = Instant::now();
    let by = Instant::now() + DECISION_TIMEOUT;
    loop {
        if worker.check().is_err() {
            let _ = wire.send(&Reply::Closed {
                reason: "Хост закрыл сессию.".into(),
            });
            return Ok("Сессия завершена.".into());
        }
        if Instant::now() >= by {
            wire.send(&Reply::Closed {
                reason: "Хост не ответил.".into(),
            })?;
            return Ok("Запрос истёк.".into());
        }
        match decision.try_recv() {
            Ok(true) => break,
            Ok(false) => {
                wire.send(&Reply::Closed {
                    reason: "Хост отклонил подключение.".into(),
                })?;
                return Ok("Запрос отклонён.".into());
            }
            Err(_) => {}
        }
        if !host_tick(&mut wire, &mut last_ping)? {
            return Ok("Гость отменил подключение.".into());
        }
    }

    worker.status("Запускаем…");
    let _wifi = quiet_wifi(worker);
    wire.send(&Reply::Preparing)?;
    let host_id = Uuid::new_v4();
    let mut mode = display_mode()?;
    let current_hz = mode.fps;
    mode.fps = display_max_refresh().unwrap_or(current_hz);
    worker.fact(
        "Экран",
        if mode.fps > current_hz {
            format!("{}×{} · {current_hz} → {} Гц", mode.width, mode.height, mode.fps)
        } else {
            format!("{}×{} · {} Гц", mode.width, mode.height, mode.fps)
        },
    );
    // Desktop Duplication stops producing frames once the display sleeps.
    let _awake = KeepAwake::hold();
    let mut stream = HostStream::start(
        &engine,
        &data,
        address,
        &identity,
        host_id,
        &name,
        &certificate,
        fec,
    )?;
    let by = Instant::now() + START_TIMEOUT;
    while !stream.ready(address)? {
        worker.check()?;
        ensure!(
            Instant::now() < by,
            "Sunshine не запустился. Подробности в host.log."
        );
        if !host_tick(&mut wire, &mut last_ping)? {
            return Ok("Гость отменил подключение.".into());
        }
    }
    wire.send(&Reply::Ready {
        host: host_id,
        certificate: identity.cert_pem,
        mode,
    })?;

    let by = Instant::now() + START_TIMEOUT;
    let mut streaming = false;
    let reason = loop {
        if worker.check().is_err() {
            break "Хост завершил сессию.";
        }
        if tunnel.peer().is_none() {
            // The tunnel is gone: the guest closed DuoLink or the network dropped.
            break "Гость отключился.";
        }
        report(worker);
        let (connected, disconnected) = stream.events()?;
        if disconnected {
            break "Поток закрыт.";
        }
        if connected && !streaming {
            streaming = true;
            worker.emit(Event::Streaming);
            wire.send(&Reply::Connected)?;
        }
        if !streaming && Instant::now() >= by {
            break "Поток не запустился. Подробности в host.log.";
        }
        if !host_tick(&mut wire, &mut last_ping)? {
            break "Гость отключился.";
        }
    };
    // Closing Sunshine removes the virtual controller before notifying the guest.
    drop(stream);
    let _ = wire.send(&Reply::Closed {
        reason: reason.into(),
    });
    drop(wire);
    drop(tunnel);
    Ok(reason.into())
}

fn host_tick(wire: &mut HostWire, last_ping: &mut Instant) -> Result<bool> {
    if last_ping.elapsed() > SILENCE {
        return Ok(false);
    }
    match wire.poll::<Request>() {
        Ok(Some(Request::Ping)) => {
            *last_ping = Instant::now();
            wire.send(&Reply::Pong)?;
        }
        Ok(None) => {}
        _ => return Ok(false),
    }
    Ok(true)
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
    let identity = Identity::generate("DuoLink Guest")?;

    worker.status("Ищем хост…");
    let tunnel = GuestTunnel::connect(invite.token, invite.fingerprint, &invite.candidates, || {
        worker.check()
    })?;
    worker.fact("Соединение", connection_fact(tunnel.describe(), tunnel.rtt()));
    let _wifi = quiet_wifi(worker);
    worker.status("Меряем канал…");
    let channel = tunnel.measure(PROBE_MBPS, Duration::from_millis(700));
    let path_mbps = channel.filter(|c| c.limited).map(|c| c.mbps);
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
    worker.status("Меряем потери…");
    let working_rate = path_mbps.map_or(150.0, |mbps| mbps * PATH_SHARE * 1.3).clamp(5.0, 150.0);
    let fec = match tunnel.measure(working_rate as u32, Duration::from_millis(1000)) {
        Some(probe) => {
            let fec = fec_for_loss(probe.loss);
            worker.fact("Потери", format!("{:.1} % · коррекция {fec} %", probe.loss * 100.0));
            fec
        }
        None => DEFAULT_FEC,
    };
    let mut next_rtt = Instant::now() + Duration::from_secs(2);
    let mut next_latency = Instant::now();
    let mut drops_shown = dropped();
    worker.status("Ждём хост…");
    let endpoint = SocketAddr::from((GUEST_LOOPBACK, CONTROL_PORT));
    let mut socket = TcpStream::connect_timeout(&endpoint, Duration::from_secs(5))
        .context("Хост не ответил.")?;
    socket.set_read_timeout(Some(Duration::from_secs(4)))?;
    socket.set_write_timeout(Some(Duration::from_secs(4)))?;
    let mut tls = ClientConnection::new(
        client_config(&invite),
        ServerName::from(std::net::IpAddr::from(GUEST_LOOPBACK)),
    )?;
    tls.complete_io(&mut socket)
        .context("Не удалось связаться с хостом.")?;
    configure_socket(&socket)?;
    let mut wire = Wire::new(StreamOwned::new(tls, socket));
    wire.send(&Request::Join {
        token: invite.token,
        name: clean_name(&nickname),
        certificate: identity.cert_pem.clone(),
        fec: Some(fec),
    })
    .context("Не удалось связаться с хостом.")?;

    let mut last_pong = Instant::now();
    let mut next_ping = Instant::now();
    let mut by = Instant::now() + DECISION_TIMEOUT + SILENCE;
    let mut stream = None::<GuestStream>;
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
            }) if stream.is_none() && valid_pem(&certificate) => {
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
                    GUEST_LOOPBACK,
                    tunnel.same_machine,
                    host,
                    &certificate,
                    &identity,
                    mode,
                    preferences,
                )?);
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
                Event::Approval(_) => eprintln!("Запрос гостя получен"),
                _ => eprintln!("{event:?}"),
            }
            if let Some(value) = accept(event) {
                return value;
            }
        }
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
        until(&host, Duration::from_secs(20), |event| match event {
            Event::Approval(_) => Some(String::new()),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        });
        assert!(
            TcpStream::connect((address, MEDIA_PORT)).is_err(),
            "media open before approval"
        );
        host.decide(true);
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
                let host_log = std::fs::read_to_string(data.join("logs/host.log")).unwrap();
                assert!(host_log.contains("CLIENT CONNECTED"));
                break;
            }
            assert!(
                Instant::now() < by,
                "audio/video packets or decoded stream frame absent from guest.log"
            );
            thread::sleep(Duration::from_millis(250));
        }
        assert!(TcpStream::connect((address, MEDIA_PORT)).is_ok());
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
        guest.cancel();
        let result = until(&host, Duration::from_secs(8), |event| match event {
            Event::Finished {
                message: reason, ..
            } => Some(reason),
            _ => None,
        });
        assert!(
            result.contains("отключился") || result.contains("закрыт"),
            "{result}"
        );
        assert!(
            TcpStream::connect((address, MEDIA_PORT)).is_err(),
            "media still listening after disconnect"
        );

        let host = Session::host(data.clone());
        let code = until(&host, Duration::from_secs(20), |event| match event {
            Event::Invite(code) => Some(code),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        });
        let guest = Session::guest(data, Invite::decode(&code).unwrap(), prefs, "Тест".into());
        until(&host, Duration::from_secs(20), |event| match event {
            Event::Approval(_) => Some(String::new()),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        });
        host.decide(false);
        let result = until(&guest, Duration::from_secs(8), |event| match event {
            Event::Finished {
                message: reason, ..
            } => Some(reason),
            _ => None,
        });
        assert!(result.contains("Хост отклонил подключение"), "{result}");
        assert!(TcpStream::connect((address, MEDIA_PORT)).is_err());
    }
}
