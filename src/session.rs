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
    media::{GuestStream, HostStream, MOONLIGHT, Preferences, SUNSHINE},
    platform::{HostLock, KeepAwake, display_mode, tailscale_route, vigem_present},
    protocol::{
        CONTROL_PORT, Identity, Invite, MEDIA_PORT, Profile, Reply, Request, Wire, client_config,
        tailscale_address, valid_pem,
    },
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

    /// Measures the Tailscale path in the background; the result arrives as a fact.
    fn probe_route(&self, peer: Ipv4Addr) {
        if !tailscale_address(peer) {
            return;
        }
        let events = self.events.clone();
        thread::spawn(move || {
            let value = match tailscale_route(peer) {
                Some(route) if route.direct => route.describe(),
                Some(route) => format!("{} — NAT не пробит, задержка выше", route.describe()),
                None => "Не удалось измерить".into(),
            };
            let _ = events.send(Event::Fact("Маршрут", value));
        });
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

    pub fn host(data: PathBuf, address: Ipv4Addr) -> Self {
        Self::start(move |worker, decision| host(worker, decision, data, address))
    }

    pub fn guest(data: PathBuf, invite: Invite, preferences: Preferences) -> Self {
        Self::start(move |worker, _| guest(worker, data, invite, preferences))
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

pub fn profile_label(profile: Profile) -> &'static str {
    match profile {
        Profile::Latency => "Минимальная задержка",
        Profile::Balanced => "Баланс",
        Profile::Quality => "Качество",
    }
}

type HostWire = Wire<StreamOwned<ServerConnection, TcpStream>>;

fn configure_socket(socket: &TcpStream) -> io::Result<()> {
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(180)))?;
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

fn host(
    worker: &Worker,
    decision: Receiver<bool>,
    data: PathBuf,
    address: Ipv4Addr,
) -> Result<String> {
    let _lock = HostLock::acquire()?;
    ensure!(
        address.is_loopback() || vigem_present(),
        "На хосте нужен драйвер ViGEmBus. Откройте «Настройка»."
    );
    reserve_check(address)?;
    worker.status("Подготовка Sunshine. При первом запуске загружается около 38 МБ.");
    let engine = SUNSHINE.ensure(&data, || worker.check())?;
    worker.check()?;

    let identity = Identity::generate("DuoLink Host")?;
    let invite = Invite::new(address, &identity);
    let tls = identity.server_config()?;
    let listener = TcpListener::bind((address, CONTROL_PORT))
        .with_context(|| format!("Не удалось открыть {address}:{CONTROL_PORT}."))?;
    listener.set_nonblocking(true)?;
    worker.emit(Event::Invite(invite.encode()));

    let expires = Instant::now() + CODE_TIMEOUT;
    let (mut wire, name, peer, certificate, profile) = loop {
        worker.check()?;
        ensure!(
            Instant::now() < expires,
            "Срок приглашения истёк. Создайте новую сессию."
        );
        let (socket, peer) = match listener.accept() {
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
            profile,
        }) = request
            && invite.authorizes(&token)
            && valid_pem(&certificate)
        {
            let name = clean_name(&name);
            wire.send(&Reply::Pending)?;
            break (wire, name, peer, certificate, profile);
        }
        let _ = wire.send(&Reply::Closed {
            reason: "Приглашение отклонено.".into(),
        });
    };
    // This invitation admits a single request. Once used, the listening socket closes.
    drop(listener);
    worker.emit(Event::Approval(format!("{name} · {}", peer.ip())));
    worker.fact("Гость", format!("{name} · {}", peer.ip()));
    worker.fact("Профиль", profile_label(profile));
    if let std::net::IpAddr::V4(ip) = peer.ip() {
        worker.probe_route(ip);
    }

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
                reason: "Хост не успел подтвердить запрос.".into(),
            })?;
            return Ok("Время подтверждения истекло.".into());
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

    worker.status("Гость допущен. Запускаем захват экрана и звука…");
    wire.send(&Reply::Preparing)?;
    let host_id = Uuid::new_v4();
    let mode = display_mode()?;
    worker.fact("Экран", format!("{}×{} · {} Гц", mode.width, mode.height, mode.fps));
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
        profile,
    )?;
    let by = Instant::now() + START_TIMEOUT;
    while !stream.ready(address)? {
        worker.check()?;
        ensure!(
            Instant::now() < by,
            "Sunshine не открыл порт. Проверьте host.log."
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
            break "Поток не запустился. Проверьте журналы и порты Sunshine.";
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

fn clean_name(name: &str) -> String {
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
    preferences: Preferences,
) -> Result<String> {
    worker.status("Подготовка Moonlight. При первом запуске загружается около 37 МБ.");
    let engine = MOONLIGHT.ensure(&data, || worker.check())?;
    worker.check()?;
    let identity = Identity::generate("DuoLink Guest")?;

    worker.status(format!("Связываемся с {}…", invite.address));
    worker.probe_route(invite.address);
    let endpoint = SocketAddr::from((invite.address, CONTROL_PORT));
    let mut socket = TcpStream::connect_timeout(&endpoint, Duration::from_secs(5))
        .context("Хост недоступен. Проверьте код, сеть и доступ к TCP 48171.")?;
    socket.set_read_timeout(Some(Duration::from_secs(4)))?;
    socket.set_write_timeout(Some(Duration::from_secs(4)))?;
    let mut tls = ClientConnection::new(
        client_config(&invite),
        ServerName::from(std::net::IpAddr::from(invite.address)),
    )?;
    tls.complete_io(&mut socket)
        .context("Не удалось подтвердить подлинность приглашения или связаться с хостом.")?;
    configure_socket(&socket)?;
    let mut wire = Wire::new(StreamOwned::new(tls, socket));
    wire.send(&Request::Join {
        token: invite.token,
        name: clean_name(&std::env::var("COMPUTERNAME").unwrap_or_default()),
        certificate: identity.cert_pem.clone(),
        profile: preferences.profile,
    })
    .context("Не удалось подтвердить подлинность приглашения или связаться с хостом.")?;

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
            bail!("Moonlight завершился ({status}). Откройте guest.log.");
        }
        ensure!(last_pong.elapsed() <= SILENCE, "Связь с хостом потеряна.");
        ensure!(
            streaming || Instant::now() < by,
            "Хост или поток не ответил вовремя."
        );
        if Instant::now() >= next_ping {
            wire.send(&Request::Ping)?;
            next_ping = Instant::now() + HEARTBEAT;
        }
        match wire.poll::<Reply>().context("Связь с хостом закрыта.")? {
            Some(Reply::Pending) => worker.status("Запрос отправлен. Ждём подтверждения хоста…"),
            Some(Reply::Preparing) => {
                by = Instant::now() + START_TIMEOUT + SILENCE;
                worker.status("Хост подтвердил запрос. Готовим поток…");
            }
            Some(Reply::Ready {
                host,
                certificate,
                mode,
            }) if stream.is_none() && valid_pem(&certificate) => {
                let mode = preferences.mode(mode, display_mode().ok());
                ensure!(
                    mode.width > 0 && mode.height > 0 && mode.fps > 0,
                    "Получен неверный режим видео."
                );
                let bitrate = preferences.bitrate_for(mode);
                worker.fact("Режим", format!("{}×{} · {} FPS", mode.width, mode.height, mode.fps));
                worker.fact("Битрейт", format!("{} Мбит/с", bitrate.div_ceil(1000)));
                worker.fact("Кодек", preferences.codec.label());
                worker.fact("Профиль", profile_label(preferences.profile));
                worker.status(format!(
                    "Открываем поток {}×{} · {} FPS…",
                    mode.width, mode.height, mode.fps
                ));
                stream = Some(GuestStream::start(
                    &engine,
                    &data,
                    invite.address,
                    host,
                    &certificate,
                    &identity,
                    mode,
                    preferences,
                )?);
                by = Instant::now() + START_TIMEOUT + SILENCE;
            }
            Some(Reply::Ready { .. }) => bail!("Хост прислал неверные параметры потока."),
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

    #[test]
    #[ignore = "needs interactive Windows desktop, display/audio and engine downloads"]
    fn live_loopback() {
        assert!(
            std::env::var_os("DUOLINK_DATA_DIR").is_some(),
            "set DUOLINK_DATA_DIR to an isolated test directory"
        );
        let data = data_dir().unwrap();
        let address = Ipv4Addr::LOCALHOST;
        let host = Session::host(data.clone(), address);
        let code = until(&host, Duration::from_secs(200), |event| match event {
            Event::Invite(code) => Some(code),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        });
        let mut bad_invite = Invite::decode(&code).unwrap();
        bad_invite.token[0] ^= 1;
        let unauthorized = Session::guest(data.clone(), bad_invite, Preferences::default());
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
        assert!(
            result.contains("Приглашение отклонено") || result.contains("закрыта"),
            "{result}"
        );
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
        let guest = Session::guest(data.clone(), invite, prefs);
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
                assert!(host_log.contains("Video encryption enabled"));
                break;
            }
            assert!(
                Instant::now() < by,
                "audio/video packets or decoded stream frame absent from guest.log"
            );
            thread::sleep(Duration::from_millis(250));
        }
        assert!(TcpStream::connect((address, MEDIA_PORT)).is_ok());
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

        let host = Session::host(data.clone(), address);
        let code = until(&host, Duration::from_secs(20), |event| match event {
            Event::Invite(code) => Some(code),
            Event::Finished { message: error, .. } => panic!("{error}"),
            _ => None,
        });
        let guest = Session::guest(data, Invite::decode(&code).unwrap(), prefs);
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
