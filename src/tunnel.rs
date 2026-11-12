//! Peer-to-peer transport that works without accounts, VPNs or servers of our own.
//!
//! The host's Sunshine listens on 127.0.0.1 only. One QUIC connection between
//! the two DuoLink processes carries everything: Sunshine's TCP ports as QUIC
//! streams and its UDP media ports as unreliable QUIC datagrams. On the guest,
//! Moonlight talks to 127.0.0.2, where DuoLink mirrors the same ports.
//!
//! The host is found by racing every candidate in the invite: its LAN
//! addresses, a UPnP-forwarded port and its STUN-reflexive public address.
//! For the last one both NATs must be opened at once, so the guest publishes
//! its own candidates (encrypted with the invite token) through ntfy.sh and
//! both sides fire punch packets at each other while QUIC retries its handshake.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket as StdUdpSocket},
    os::windows::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use quinn::{
    ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime,
    TransportConfig, VarInt,
    congestion::{Controller, ControllerFactory},
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rand::{RngCore, rngs::OsRng};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream, UdpSocket},
    runtime::Runtime,
};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::{
    platform::{prioritize_thread, system_tool},
    protocol::{CONTROL_PORT, Candidate, Identity, MEDIA_PORT, PathKind, Pin},
};

pub const TUNNEL_PORT: u16 = 48170;
/// Where the guest mirrors the host's ports, so a host on the same PC (127.0.0.1) never collides.
/// A second guest on the same PC (tests) takes the next address.
pub const GUEST_LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
const HOST_LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;
pub const TCP_PORTS: [u16; 4] = [CONTROL_PORT, MEDIA_PORT - 5, MEDIA_PORT, MEDIA_PORT + 21];
pub const UDP_PORTS: [u16; 4] = [MEDIA_PORT + 9, MEDIA_PORT + 10, MEDIA_PORT + 11, crate::audio::AUDIO_PORT];
const ALPN: &[u8] = b"duolink/2";
const STUN_SERVERS: [&str; 3] = [
    "stun.l.google.com:19302",
    "stun.cloudflare.com:3478",
    "stun1.l.google.com:19302",
];
pub(crate) const SIGNAL_SERVER: &str = "https://ntfy.sh";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Datagram "port" reserved for the bandwidth probe; Sunshine never uses 0.
const PROBE_PORT: u16 = 0;
/// The capacity probe sends at this rate; reaching most of it means the
/// path is not the bottleneck.
pub const PROBE_MBPS: u32 = 300;

/// Datagrams the tunnel could not send (too large or no connection).
static DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

fn forward(conn: &Connection, port: u16, payload: &[u8]) {
    if conn.send_datagram(tag(port, payload)).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/* ------------------------------------------------------------------ STUN */

const STUN_MAGIC: u32 = 0x2112_A442;

fn stun_request(txid: &[u8; 12]) -> [u8; 20] {
    let mut request = [0u8; 20];
    request[..2].copy_from_slice(&0x0001u16.to_be_bytes());
    request[4..8].copy_from_slice(&STUN_MAGIC.to_be_bytes());
    request[8..].copy_from_slice(txid);
    request
}

fn parse_stun(message: &[u8], txid: &[u8; 12]) -> Option<SocketAddrV4> {
    if message.len() < 20 || message[..2] != [0x01, 0x01] || &message[8..20] != txid {
        return None;
    }
    let mut at = 20;
    let mut mapped = None;
    while at + 4 <= message.len() {
        let kind = u16::from_be_bytes([message[at], message[at + 1]]);
        let len = u16::from_be_bytes([message[at + 2], message[at + 3]]) as usize;
        let value = message.get(at + 4..at + 4 + len)?;
        if len >= 8 && value[1] == 0x01 {
            let port = u16::from_be_bytes([value[2], value[3]]);
            let ip = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
            match kind {
                0x0020 => {
                    return Some(SocketAddrV4::new(
                        Ipv4Addr::from(ip ^ STUN_MAGIC),
                        port ^ (STUN_MAGIC >> 16) as u16,
                    ));
                }
                0x0001 => mapped = Some(SocketAddrV4::new(Ipv4Addr::from(ip), port)),
                _ => {}
            }
        }
        at += 4 + len.div_ceil(4) * 4;
    }
    mapped
}

fn stun_servers() -> Vec<SocketAddr> {
    STUN_SERVERS
        .iter()
        .filter_map(|host| host.to_socket_addrs().ok()?.find(SocketAddr::is_ipv4))
        .collect()
}

fn stun_query(socket: &StdUdpSocket, server: SocketAddr) -> Option<SocketAddrV4> {
    let mut txid = [0u8; 12];
    OsRng.fill_bytes(&mut txid);
    let mut buffer = [0u8; 512];
    for _ in 0..2 {
        socket.send_to(&stun_request(&txid), server).ok()?;
        let deadline = Instant::now() + Duration::from_millis(700);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            socket.set_read_timeout(Some(left.max(Duration::from_millis(1)))).ok()?;
            match socket.recv_from(&mut buffer) {
                Ok((count, from)) if from == server => {
                    if let Some(addr) = parse_stun(&buffer[..count], &txid) {
                        return Some(addr);
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                Err(_) => break,
            }
        }
    }
    None
}

/// The socket's public address, and whether the NAT maps it differently per
/// destination (symmetric NAT, where hole punching cannot work).
#[derive(Clone, Copy, Debug, Default)]
pub struct Reflexive {
    pub addr: Option<SocketAddrV4>,
    pub symmetric: bool,
}

fn discover(socket: &StdUdpSocket) -> Reflexive {
    let answers: Vec<SocketAddrV4> = stun_servers()
        .into_iter()
        .filter_map(|server| stun_query(socket, server))
        .take(2)
        .collect();
    Reflexive {
        addr: answers.first().copied(),
        symmetric: answers.windows(2).any(|pair| pair[0] != pair[1]),
    }
}

/* ------------------------------------------------------------------ UPnP */

pub struct PortMapping {
    gateway: igd_next::Gateway,
    port: u16,
}

impl Drop for PortMapping {
    fn drop(&mut self) {
        let _ = self
            .gateway
            .remove_port(igd_next::PortMappingProtocol::UDP, self.port);
    }
}

fn shared_address(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

/// Asks the router to forward `port`. Returns the public endpoint, unless the
/// router itself sits behind another NAT (then forwarding would not help).
fn forward_port(port: u16) -> Option<(SocketAddrV4, PortMapping)> {
    let gateway = igd_next::search_gateway(igd_next::SearchOptions {
        timeout: Some(Duration::from_millis(2500)),
        single_search_timeout: Some(Duration::from_millis(2500)),
        ..Default::default()
    })
    .ok()?;
    let IpAddr::V4(external) = gateway.get_external_ip().ok()? else {
        return None;
    };
    if external.is_private() || shared_address(external) || external.is_unspecified() {
        return None;
    }
    // The LAN address that routes to the gateway.
    let probe = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect(gateway.addr).ok()?;
    let IpAddr::V4(local) = probe.local_addr().ok()?.ip() else {
        return None;
    };
    let target = SocketAddr::from((local, port));
    let protocol = igd_next::PortMappingProtocol::UDP;
    let lease = 6 * 3600;
    let external_port = match gateway.add_port(protocol, port, target, lease, "DuoLink") {
        Ok(()) => port,
        Err(_) => gateway.add_any_port(protocol, target, lease, "DuoLink").ok()?,
    };
    Some((
        SocketAddrV4::new(external, external_port),
        PortMapping {
            gateway,
            port: external_port,
        },
    ))
}

/* ------------------------------------------------------------ signalling */

fn derive(token: &[u8; 16], label: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(label);
    hash.update(token);
    hash.finalize().into()
}

fn topic(token: &[u8; 16]) -> String {
    let digest = derive(token, b"duolink/topic");
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("duolink-{hex}")
}

fn cipher(token: &[u8; 16]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &derive(token, b"duolink/key")).unwrap())
}

fn seal(token: &[u8; 16], addrs: &[SocketAddrV4]) -> String {
    let mut plain: Vec<u8> = addrs
        .iter()
        .flat_map(|addr| {
            let mut bytes = addr.ip().octets().to_vec();
            bytes.extend_from_slice(&addr.port().to_be_bytes());
            bytes
        })
        .collect();
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    cipher(token)
        .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut plain)
        .unwrap();
    let mut message = nonce.to_vec();
    message.extend_from_slice(&plain);
    URL_SAFE_NO_PAD.encode(message)
}

fn unseal(token: &[u8; 16], text: &str) -> Option<Vec<SocketAddrV4>> {
    let mut bytes = URL_SAFE_NO_PAD.decode(text.trim()).ok()?;
    if bytes.len() < 12 + 16 {
        return None;
    }
    let nonce = Nonce::try_assume_unique_for_key(&bytes[..12]).ok()?;
    let plain = cipher(token)
        .open_in_place(nonce, Aad::empty(), &mut bytes[12..])
        .ok()?;
    if plain.len() % 6 != 0 || plain.len() > 6 * 16 {
        return None;
    }
    Some(
        plain
            .chunks_exact(6)
            .map(|c| SocketAddrV4::new(Ipv4Addr::new(c[0], c[1], c[2], c[3]), u16::from_be_bytes([c[4], c[5]])))
            .collect(),
    )
}

pub(crate) fn curl() -> Result<Command> {
    let mut command = Command::new(system_tool("curl.exe")?);
    command
        .args(["-q", "--silent", "--proto", "=https", "--connect-timeout", "8"])
        .creation_flags(CREATE_NO_WINDOW)
        .stderr(Stdio::null());
    Ok(command)
}

/// Sends the guest's addresses to the host. Fire and forget.
fn publish(token: [u8; 16], addrs: Vec<SocketAddrV4>) {
    thread::spawn(move || {
        let Ok(mut command) = curl() else { return };
        let Ok(mut child) = command
            .args(["--max-time", "10", "--data-binary", "@-"])
            .arg(format!("{SIGNAL_SERVER}/{}", topic(&token)))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
        else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(seal(&token, &addrs).as_bytes());
        }
        let _ = child.wait();
    });
}

/// Host side: listens for guests' addresses for as long as it lives.
struct Subscription(Child);

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn subscribe(token: [u8; 16], on_guest: impl Fn(Vec<SocketAddrV4>) + Send + 'static) -> Option<Subscription> {
    let mut child = curl()
        .ok()?
        .args(["--no-buffer", "--max-time", "1200"])
        .arg(format!("{SIGNAL_SERVER}/{}/json", topic(&token)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if event["event"] == "message"
                && let Some(addrs) = event["message"].as_str().and_then(|m| unseal(&token, m))
            {
                on_guest(addrs);
            }
        }
    });
    Some(Subscription(child))
}

/// Sends throwaway datagrams so our NAT opens a mapping towards each target.
/// QUIC ignores them on arrival.
fn punch(socket: &StdUdpSocket, targets: Vec<SocketAddrV4>, for_how_long: Duration) {
    let Ok(socket) = socket.try_clone() else { return };
    thread::spawn(move || {
        let until = Instant::now() + for_how_long;
        while Instant::now() < until {
            for target in &targets {
                let _ = socket.send_to(&[0], target);
            }
            thread::sleep(Duration::from_millis(80));
        }
    });
}

/* ------------------------------------------------------------------ QUIC */

/// Video is paced by the encoder's bitrate, not by loss: a congestion window
/// would only queue frames and add latency. FEC in Sunshine covers loss.
#[derive(Clone)]
struct Unthrottled;

const WINDOW: u64 = 256 << 20;

impl Controller for Unthrottled {
    fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: u64) {}
    fn on_mtu_update(&mut self, _: u16) {}
    fn window(&self) -> u64 {
        WINDOW
    }
    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }
    fn initial_window(&self) -> u64 {
        WINDOW
    }
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

impl ControllerFactory for Unthrottled {
    fn build(self: Arc<Self>, _: Instant, _: u16) -> Box<dyn Controller> {
        Box::new(Unthrottled)
    }
}

fn transport() -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    config
        // Rides out a short Wi-Fi dropout; a clean disconnect is still instant.
        .max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()))
        .keep_alive_interval(Some(Duration::from_millis(500)))
        .initial_mtu(1280)
        .datagram_receive_buffer_size(Some(16 << 20))
        // Under overload, drop stale video rather than queue it: a late frame is worse than none.
        .datagram_send_buffer_size(2 << 20)
        .max_concurrent_bidi_streams(VarInt::from_u32(64))
        .congestion_controller_factory(Arc::new(Unthrottled));
    Arc::new(config)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn server_config(identity: &Identity) -> Result<ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert_der.clone()], identity.key_der().into())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(transport());
    Ok(config)
}

fn client_config(fingerprint: [u8; 32]) -> Result<ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pin(fingerprint)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(transport());
    Ok(config)
}

/// A video frame at high bitrate arrives as a burst far larger than Windows'
/// default 64 KB socket buffer; without room for it the burst is dropped.
const RECEIVE_BUFFER: usize = 8 << 20;
/// Sending stays shallow: several frames of room, never seconds of backlog.
const SEND_BUFFER: usize = 1 << 20;

fn enlarge(socket: socket2::SockRef<'_>) {
    let _ = socket.set_recv_buffer_size(RECEIVE_BUFFER);
    let _ = socket.set_send_buffer_size(SEND_BUFFER);
}

#[derive(Default)]
struct ProbeGate {
    streaming: AtomicBool,
    running: tokio::sync::Mutex<()>,
}

/// Host side of the bandwidth probe. A zero count means it was skipped.
async fn probe_send(conn: Connection, mut reply: quinn::SendStream, mut recv: quinn::RecvStream, probes: &ProbeGate) {
    let mut request = [0u8; 8];
    if recv.read_exact(&mut request).await.is_err() {
        return;
    }
    let mbps = u32::from_be_bytes(request[..4].try_into().unwrap()).clamp(1, PROBE_MBPS);
    let time = Duration::from_millis(u32::from_be_bytes(request[4..].try_into().unwrap()).clamp(100, 1500) as u64);
    let size = conn.max_datagram_size().unwrap_or(1100).clamp(64, 1100);
    let payload = vec![0u8; size - 2];
    let rate = mbps as f64 * 1e6 / 8.0;
    let start = Instant::now();
    let mut sent: u64 = 0;
    if !probes.streaming.load(Ordering::Acquire)
        && let Ok(_guard) = probes.running.try_lock()
    {
        while !probes.streaming.load(Ordering::Acquire) && start.elapsed() < time {
            let due = (start.elapsed().as_secs_f64() * rate) as u64;
            while sent < due && !probes.streaming.load(Ordering::Acquire) {
                if conn.send_datagram(tag(PROBE_PORT, &payload)).is_err() {
                    break;
                }
                sent += size as u64;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if probes.streaming.load(Ordering::Acquire) {
            sent = 0;
        }
    }
    let _ = reply.write_all(&sent.to_be_bytes()).await;
    let _ = reply.finish();
}

/// Datagram ports 1 and 2 carry an echo over the media path itself, so the
/// latency shown is what a video packet experiences: no ACK delays, no timers.
const PING_PORT: u16 = 1;
const PONG_PORT: u16 = 2;

pub struct Pinger {
    origin: Instant,
    rtt_us: AtomicU64,
}

impl Pinger {
    fn new() -> Arc<Self> {
        Arc::new(Self { origin: Instant::now(), rtt_us: AtomicU64::new(0) })
    }

    fn stamp(&self) -> [u8; 8] {
        (self.origin.elapsed().as_micros() as u64).to_be_bytes()
    }

    fn record(&self, payload: &[u8]) {
        let Ok(sent) = <[u8; 8]>::try_from(payload) else { return };
        let now = self.origin.elapsed().as_micros() as u64;
        let sample = now.saturating_sub(u64::from_be_bytes(sent)).max(1);
        let previous = self.rtt_us.load(Ordering::Relaxed);
        let smoothed = if previous == 0 { sample } else { (previous * 3 + sample) / 4 };
        self.rtt_us.store(smoothed, Ordering::Relaxed);
    }

    pub fn rtt(&self) -> Option<Duration> {
        match self.rtt_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(Duration::from_micros(us)),
        }
    }
}

/// Answers and times pings; true when the datagram was one.
fn ping_datagram(conn: &Connection, pinger: &Pinger, port: u16, payload: &[u8]) -> bool {
    match port {
        PING_PORT => {
            forward(conn, PONG_PORT, payload);
            true
        }
        PONG_PORT => {
            pinger.record(payload);
            true
        }
        _ => false,
    }
}

async fn ping_loop(conn: Connection, pinger: Arc<Pinger>) {
    while conn.close_reason().is_none() {
        forward(&conn, PING_PORT, &pinger.stamp());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Default)]
struct ProbeStats {
    bytes: u64,
    first: Option<Instant>,
    last: Option<Instant>,
}

/// Measured host→guest throughput.
#[derive(Clone, Copy, Debug)]
pub struct Bandwidth {
    pub mbps: f64,
    /// The path, not the probe, set the rate.
    pub limited: bool,
    /// Share of the offered bytes that never arrived.
    pub loss: f64,
}

fn runtime() -> Result<Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .on_thread_start(prioritize_thread)
        .thread_name("duolink-net")
        .enable_all()
        .build()?)
}

/// Joins a QUIC stream pair and a TCP socket until either side closes.
async fn pipe(tcp: TcpStream, mut send: quinn::SendStream, mut recv: quinn::RecvStream) {
    let _ = tcp.set_nodelay(true);
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let upstream = async {
        let _ = tokio::io::copy(&mut tcp_read, &mut send).await;
        let _ = send.finish();
    };
    let downstream = async {
        let _ = tokio::io::copy(&mut recv, &mut tcp_write).await;
        let _ = tcp_write.shutdown().await;
    };
    tokio::join!(upstream, downstream);
}

fn tag(port: u16, payload: &[u8]) -> Bytes {
    let mut datagram = Vec::with_capacity(payload.len() + 2);
    datagram.extend_from_slice(&port.to_be_bytes());
    datagram.extend_from_slice(payload);
    Bytes::from(datagram)
}

fn reset(error: &std::io::Error) -> bool {
    // Windows reports a previous ICMP "port unreachable" on the next recv.
    error.kind() == std::io::ErrorKind::ConnectionReset
}

/// Host side: Sunshine's ports reached through 127.0.0.1.
async fn serve_host(conn: Connection, pinger: Arc<Pinger>, ports: Arc<Mutex<LinkPorts>>, probes: Arc<ProbeGate>) {
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(ping_loop(conn.clone(), pinger.clone()));
    let mut sockets = HashMap::new();
    for port in UDP_PORTS {
        let Ok(socket) = UdpSocket::bind((HOST_LOOPBACK, 0)).await else { return };
        enlarge(socket2::SockRef::from(&socket));
        if socket.connect((HOST_LOOPBACK, port)).await.is_err() {
            return;
        }
        if port == crate::audio::AUDIO_PORT {
            ports.lock().unwrap().audio = socket.local_addr().ok();
        }
        let socket = Arc::new(socket);
        sockets.insert(port, socket.clone());
        let conn = conn.clone();
        tasks.spawn(async move {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match socket.recv(&mut buffer).await {
                    Ok(count) => forward(&conn, port, &buffer[..count]),
                    Err(error) if reset(&error) => {}
                    Err(_) => break,
                }
            }
        });
    }
    {
        let conn = conn.clone();
        tasks.spawn(async move {
            while let Ok(datagram) = conn.read_datagram().await {
                if datagram.len() < 2 {
                    continue;
                }
                let port = u16::from_be_bytes([datagram[0], datagram[1]]);
                if ping_datagram(&conn, &pinger, port, &datagram[2..]) {
                    continue;
                }
                // The guest's audio hello registers its local playback socket.
                // The host selects recipients using the approved control sockets.
                if port == crate::audio::AUDIO_PORT {
                    continue;
                }
                if let Some(socket) = sockets.get(&port) {
                    let _ = socket.send(&datagram[2..]).await;
                }
            }
        });
    }
    while let Ok((send, mut recv)) = conn.accept_bi().await {
        let conn = conn.clone();
        let ports = ports.clone();
        let probes = probes.clone();
        tokio::spawn(async move {
            let mut header = [0u8; 2];
            if recv.read_exact(&mut header).await.is_err() {
                return;
            }
            let port = u16::from_be_bytes(header);
            if port == PROBE_PORT {
                probe_send(conn, send, recv, &probes).await;
                return;
            }
            if !TCP_PORTS.contains(&port) {
                return;
            }
            if let Ok(tcp) = TcpStream::connect((HOST_LOOPBACK, port)).await {
                // The session sees this socket's port and so knows which guest it is.
                let control = if port == CONTROL_PORT {
                    tcp.local_addr().ok().map(|local| local.port())
                } else {
                    None
                };
                if let Some(control) = control {
                    ports.lock().unwrap().control.push(control);
                }
                tokio::select! {
                    _ = pipe(tcp, send, recv) => {}
                    _ = conn.closed() => {}
                }
                if let Some(control) = control {
                    ports.lock().unwrap().control.retain(|&port| port != control);
                }
            }
        });
    }
}

/// Guest side: the same ports mirrored on 127.0.0.2.
async fn serve_guest(
    conn: Connection,
    listeners: Vec<(u16, TcpListener)>,
    sockets: Vec<(u16, UdpSocket)>,
    probe: Arc<Mutex<ProbeStats>>,
    pinger: Arc<Pinger>,
) {
    tokio::spawn(ping_loop(conn.clone(), pinger.clone()));
    for (port, listener) in listeners {
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let conn = conn.clone();
                tokio::spawn(async move {
                    let Ok((mut send, recv)) = conn.open_bi().await else { return };
                    if send.write_all(&port.to_be_bytes()).await.is_ok() {
                        pipe(tcp, send, recv).await;
                    }
                });
            }
        });
    }
    let mut routes = HashMap::new();
    for (port, socket) in sockets {
        let socket = Arc::new(socket);
        let client = Arc::new(Mutex::new(None::<SocketAddr>));
        routes.insert(port, (socket.clone(), client.clone()));
        let conn = conn.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((count, from)) => {
                        *client.lock().unwrap() = Some(from);
                        forward(&conn, port, &buffer[..count]);
                    }
                    Err(error) if reset(&error) => {}
                    Err(_) => break,
                }
            }
        });
    }
    while let Ok(datagram) = conn.read_datagram().await {
        if datagram.len() < 2 {
            continue;
        }
        let port = u16::from_be_bytes([datagram[0], datagram[1]]);
        if ping_datagram(&conn, &pinger, port, &datagram[2..]) {
            continue;
        }
        if port == PROBE_PORT {
            let now = Instant::now();
            let mut stats = probe.lock().unwrap();
            stats.bytes += datagram.len() as u64;
            stats.first.get_or_insert(now);
            stats.last = Some(now);
            continue;
        }
        if let Some((socket, client)) = routes.get(&port) {
            let target = *client.lock().unwrap();
            if let Some(target) = target {
                let _ = socket.send_to(&datagram[2..], target).await;
            }
        }
    }
}

/* ------------------------------------------------------------- addresses */

fn local_addresses() -> Vec<Ipv4Addr> {
    let mut list: Vec<Ipv4Addr> = local_ip_address::list_afinet_netifas()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, ip)| match ip {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() => Some(ip),
            _ => None,
        })
        .collect();
    // Private LANs first: they are the most likely shortcut.
    list.sort_by_key(|ip| !ip.is_private());
    list.dedup();
    list
}

/// Adapter names that belong to VPNs, proxies, tunnels and virtual machines.
/// Traffic sent to their addresses detours through user-space software (or
/// never leaves the PC), so they must never be offered as a path to a friend.
fn virtual_adapter(name: &str) -> bool {
    const MARKERS: [&str; 24] = [
        "virtual", "vpn", "tap-", "tap ", "wintun", "wireguard", "hyper-v", "vethernet", "vmware",
        "virtualbox", "tunnel", "xray", "v2ray", "sing-box", "singbox", "clash", "hiddify", "amnezia",
        "outline", "zerotier", "radmin", "hamachi", "tailscale", "npcap",
    ];
    let name = name.to_lowercase();
    MARKERS.iter().any(|marker| name.contains(marker))
        || name
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| word.strip_prefix("tun").is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit())))
}

fn wide_string(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    let mut length = 0;
    // SAFETY: Windows hands out NUL-terminated adapter strings.
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, length))
    }
}

/// IPv4 addresses of real, connected network cards (Ethernet or Wi-Fi with a
/// default gateway). These are the only local paths worth offering.
fn lan_addresses() -> Vec<Ipv4Addr> {
    use windows_sys::Win32::{
        Foundation::ERROR_BUFFER_OVERFLOW,
        NetworkManagement::{
            IpHelper::{
                GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
                GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
            },
            Ndis::IfOperStatusUp,
        },
        Networking::WinSock::{AF_INET, SOCKADDR_IN},
    };
    const ETHERNET: u32 = 6;
    const WIFI: u32 = 71;
    let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 32 * 1024;
    let mut buffer: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buffer.resize(size as usize / 8 + 1, 0);
        let status = unsafe {
            GetAdaptersAddresses(AF_INET as u32, flags, std::ptr::null(), buffer.as_mut_ptr().cast(), &mut size)
        };
        match status {
            0 => break,
            ERROR_BUFFER_OVERFLOW => continue,
            _ => return Vec::new(),
        }
    }
    let mut found = Vec::new();
    let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    // SAFETY: the buffer holds the linked list GetAdaptersAddresses wrote.
    unsafe {
        while !adapter.is_null() {
            let info = &*adapter;
            let names = format!("{} {}", wide_string(info.FriendlyName), wide_string(info.Description));
            if info.OperStatus == IfOperStatusUp
                && (info.IfType == ETHERNET || info.IfType == WIFI)
                && !info.FirstGatewayAddress.is_null()
                && !virtual_adapter(&names)
            {
                let mut unicast = info.FirstUnicastAddress;
                while !unicast.is_null() {
                    let socket = (*unicast).Address.lpSockaddr;
                    if !socket.is_null() && (*socket).sa_family == AF_INET {
                        let v4 = &*socket.cast::<SOCKADDR_IN>();
                        let ip = Ipv4Addr::from(u32::from_be(v4.sin_addr.S_un.S_addr));
                        if !ip.is_link_local() && !ip.is_loopback() && !found.contains(&ip) {
                            found.push(ip);
                        }
                    }
                    unicast = (*unicast).Next;
                }
            }
            adapter = info.Next;
        }
    }
    found
}

/// The remote end is this very computer (loopback or one of our own addresses).
fn is_self(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || local_addresses().contains(&ip),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

/// How the host sees its guest: by where the guest's packets come from.
pub fn describe_peer(addr: SocketAddr) -> &'static str {
    match addr.ip() {
        ip if is_self(ip) => "Этот ПК",
        IpAddr::V4(ip) if ip.is_private() || shared_address(ip) => "Локальная сеть",
        _ => "Интернет",
    }
}

/// Human-readable description of the path a connection took.
pub fn describe(kind: PathKind, addr: SocketAddr) -> &'static str {
    match (kind, addr.ip()) {
        (_, ip) if is_self(ip) => "Этот ПК",
        (PathKind::Local, IpAddr::V4(ip)) if ip.is_private() => "Локальная сеть",
        (PathKind::Local, _) => "Прямое",
        (PathKind::Forwarded, _) => "Интернет · UPnP",
        (PathKind::Public, _) => "Интернет",
    }
}

/* ------------------------------------------------------------------ host */

pub struct HostTunnel {
    runtime: Option<Runtime>,
    endpoint: Endpoint,
    pub candidates: Vec<Candidate>,
    pub reflexive: Reflexive,
    pub forwarded: bool,
    links: Arc<Mutex<Vec<Link>>>,
    probes: Arc<ProbeGate>,
    alive: Arc<AtomicBool>,
    _mapping: Option<PortMapping>,
    _signal: Option<Subscription>,
}

impl HostTunnel {
    pub fn open(identity: &Identity, token: [u8; 16]) -> Result<Self> {
        let socket = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, TUNNEL_PORT))
            .or_else(|_| StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))
            .context("Сетевой порт занят.")?;
        let port = socket.local_addr()?.port();

        let mapping = thread::spawn(move || forward_port(port));
        let reflexive = discover(&socket);
        let mapping = mapping.join().ok().flatten();

        let mut candidates = Vec::new();
        if let Some((addr, _)) = &mapping {
            candidates.push(Candidate { kind: PathKind::Forwarded, addr: *addr });
        }
        if let Some(addr) = reflexive.addr
            && !candidates.iter().any(|c| c.addr == addr)
        {
            candidates.push(Candidate { kind: PathKind::Public, addr });
        }
        for ip in lan_addresses().into_iter().take(8) {
            candidates.push(Candidate { kind: PathKind::Local, addr: SocketAddrV4::new(ip, port) });
        }
        candidates.push(Candidate {
            kind: PathKind::Local,
            addr: SocketAddrV4::new(HOST_LOOPBACK, port),
        });

        let punch_socket = socket.try_clone()?;
        socket.set_read_timeout(None)?;
        socket.set_nonblocking(true)?;
        enlarge(socket2::SockRef::from(&socket));
        let runtime = runtime()?;
        let endpoint = {
            let _guard = runtime.enter();
            Endpoint::new(
                EndpointConfig::default(),
                Some(server_config(identity)?),
                socket,
                Arc::new(TokioRuntime),
            )?
        };

        let links = Arc::new(Mutex::new(Vec::<Link>::new()));
        let probes = Arc::new(ProbeGate::default());
        runtime.spawn(accept(endpoint.clone(), token, links.clone(), probes.clone()));

        let alive = Arc::new(AtomicBool::new(true));
        // Keep our NAT mapping (and so the reflexive candidate) open while we wait.
        if reflexive.addr.is_some() {
            let socket = punch_socket.try_clone()?;
            let alive = alive.clone();
            thread::spawn(move || {
                let servers = stun_servers();
                while alive.load(Ordering::Relaxed) {
                    for _ in 0..150 {
                        if !alive.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                    if let Some(server) = servers.first() {
                        let _ = socket.send_to(&stun_request(&[7; 12]), server);
                    }
                }
            });
        }
        let signal = subscribe(token, move |guest| punch(&punch_socket, guest, Duration::from_secs(15)));

        Ok(Self {
            runtime: Some(runtime),
            endpoint,
            candidates,
            reflexive,
            forwarded: mapping.is_some(),
            links,
            probes,
            alive,
            _mapping: mapping.map(|(_, mapping)| mapping),
            _signal: signal,
        })
    }

    /// Keep test traffic off the uplink once the lobby starts streaming.
    pub fn stop_probes(&self) {
        self.probes.streaming.store(true, Ordering::Release);
    }

    /// The guest whose control connection reached the session from `control_port`,
    /// while its tunnel is up.
    pub fn link(&self, control_port: u16) -> Option<GuestLink> {
        let links = self.links.lock().unwrap();
        links.iter().filter(|link| link.conn.close_reason().is_none()).find_map(|link| {
            let ports = link.ports.lock().unwrap();
            ports.control.contains(&control_port).then(|| GuestLink {
                address: link.conn.remote_address(),
                rtt: link.pinger.rtt().unwrap_or_else(|| link.conn.rtt()),
                audio: ports.audio,
            })
        })
    }

    fn anyone(&self) -> bool {
        self.links.lock().unwrap().iter().any(|l| l.conn.close_reason().is_none())
    }
}

#[derive(Clone, Copy)]
pub struct GuestLink {
    pub address: SocketAddr,
    pub rtt: Duration,
    pub audio: Option<SocketAddr>,
}

#[derive(Default)]
struct LinkPorts {
    control: Vec<u16>,
    audio: Option<SocketAddr>,
}

struct Link {
    conn: Connection,
    pinger: Arc<Pinger>,
    ports: Arc<Mutex<LinkPorts>>,
}

impl Drop for HostTunnel {
    fn drop(&mut self) {
        // Let the last control message (e.g. «rejected») reach the guest first.
        if self.anyone() {
            thread::sleep(Duration::from_millis(300));
        }
        let _guard = self.runtime.as_ref().map(Runtime::enter);
        self.alive.store(false, Ordering::Relaxed);
        self.endpoint.close(VarInt::from_u32(0), b"closed");
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_millis(300));
        }
    }
}

async fn accept(endpoint: Endpoint, token: [u8; 16], links: Arc<Mutex<Vec<Link>>>, probes: Arc<ProbeGate>) {
    while let Some(incoming) = endpoint.accept().await {
        let links = links.clone();
        let probes = probes.clone();
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            let handshake = tokio::time::timeout(Duration::from_secs(5), async {
                let (send, mut recv) = conn.accept_bi().await.ok()?;
                let mut presented = [0u8; 16];
                recv.read_exact(&mut presented).await.ok()?;
                Some((send, presented))
            })
            .await;
            let Ok(Some((mut send, presented))) = handshake else {
                conn.close(VarInt::from_u32(2), b"timeout");
                return;
            };
            if !bool::from(presented.ct_eq(&token)) {
                conn.close(VarInt::from_u32(3), b"denied");
                return;
            }
            let pinger = Pinger::new();
            let ports = Arc::new(Mutex::new(LinkPorts::default()));
            let id = conn.stable_id();
            links.lock().unwrap().push(Link { conn: conn.clone(), pinger: pinger.clone(), ports: ports.clone() });
            let _ = send.write_all(&[1]).await;
            let _ = send.finish();
            serve_host(conn, pinger, ports, probes).await;
            links.lock().unwrap().retain(|link| link.conn.stable_id() != id);
        });
    }
}

/* ----------------------------------------------------------------- guest */

pub struct GuestTunnel {
    runtime: Option<Runtime>,
    endpoint: Endpoint,
    conn: Connection,
    probe: Arc<Mutex<ProbeStats>>,
    pinger: Arc<Pinger>,
    pub kind: PathKind,
    pub same_machine: bool,
    /// Where this guest mirrors the host's ports.
    pub loopback: Ipv4Addr,
}

impl GuestTunnel {
    pub fn connect(
        token: [u8; 16],
        fingerprint: [u8; 32],
        candidates: &[Candidate],
        check: impl Fn() -> Result<()>,
    ) -> Result<Self> {
        ensure!(!candidates.is_empty(), "В коде приглашения нет адресов хоста.");
        let socket = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let port = socket.local_addr()?.port();
        let reflexive = discover(&socket);
        let mut mine: Vec<SocketAddrV4> = reflexive.addr.into_iter().collect();
        mine.extend(lan_addresses().into_iter().take(8).map(|ip| SocketAddrV4::new(ip, port)));
        publish(token, mine);
        punch(
            &socket,
            candidates.iter().map(|c| c.addr).collect(),
            CONNECT_TIMEOUT,
        );

        socket.set_read_timeout(None)?;
        socket.set_nonblocking(true)?;
        enlarge(socket2::SockRef::from(&socket));
        let runtime = runtime()?;
        let endpoint = {
            let _guard = runtime.enter();
            Endpoint::new(EndpointConfig::default(), None, socket, Arc::new(TokioRuntime))?
        };
        let config = client_config(fingerprint)?;

        // Race every candidate; the first authenticated connection wins.
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let guard = runtime.enter();
        for candidate in candidates {
            let Ok(connecting) = endpoint.connect_with(config.clone(), SocketAddr::V4(candidate.addr), "duolink") else {
                continue;
            };
            let sender = sender.clone();
            let kind = candidate.kind;
            runtime.spawn(async move {
                if let Ok(Ok(conn)) = tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
                    let _ = sender.send((kind, conn));
                }
            });
        }
        drop(sender);
        drop(guard);

        let deadline = Instant::now() + CONNECT_TIMEOUT + Duration::from_secs(1);
        // A refusal from one address (say, our own DuoLink on 127.0.0.1) must
        // not end the race while other addresses may still answer.
        let mut refused = false;
        let give_up = |refused: bool| -> anyhow::Error {
            if refused {
                anyhow::anyhow!("Хост отклонил код: он устарел или уже использован.")
            } else {
                anyhow::anyhow!(failure_hint(&reflexive))
            }
        };
        let (kind, conn) = loop {
            check()?;
            if Instant::now() >= deadline {
                return Err(give_up(refused));
            }
            let next = runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await
            });
            match next {
                Ok(Some((kind, conn))) => match runtime.block_on(authenticate(&conn, token)) {
                    Ok(()) => break (kind, conn),
                    Err(error) => {
                        conn.close(VarInt::from_u32(0), b"");
                        refused |= error.to_string().contains("denied");
                    }
                },
                Ok(None) => return Err(give_up(refused)),
                Err(_) => {}
            }
        };
        // Let the losing attempts go.
        runtime.spawn(async move { while receiver.recv().await.is_some() {} });

        let bind = |loopback: Ipv4Addr| runtime.block_on(async move {
            let mut listeners = Vec::new();
            for port in TCP_PORTS {
                let listener = TcpListener::bind((loopback, port))
                    .await
                    .with_context(|| format!("Порт {port} занят другой программой."))?;
                listeners.push((port, listener));
            }
            let mut sockets = Vec::new();
            for port in UDP_PORTS {
                let socket = UdpSocket::bind((loopback, port))
                    .await
                    .with_context(|| format!("Порт {port} занят другой программой."))?;
                enlarge(socket2::SockRef::from(&socket));
                sockets.push((port, socket));
            }
            anyhow::Ok((listeners, sockets))
        });
        let [a, b, c, d] = GUEST_LOOPBACK.octets();
        let mut bound = bind(GUEST_LOOPBACK);
        let mut loopback = GUEST_LOOPBACK;
        for next in 1..8u8 {
            if bound.is_ok() {
                break;
            }
            loopback = Ipv4Addr::new(a, b, c, d + next);
            bound = bind(loopback);
        }
        let (listeners, sockets) = bound?;
        let probe = Arc::new(Mutex::new(ProbeStats::default()));
        let pinger = Pinger::new();
        runtime.spawn(serve_guest(conn.clone(), listeners, sockets, probe.clone(), pinger.clone()));

        let same_machine = is_self(conn.remote_address().ip());
        Ok(Self {
            runtime: Some(runtime),
            endpoint,
            conn,
            probe,
            pinger,
            kind,
            same_machine,
            loopback,
        })
    }

    /// Asks the host for a short burst and measures what actually arrives.
    pub fn measure(&self, mbps: u32, time: Duration) -> Option<Bandwidth> {
        let runtime = self.runtime.as_ref()?;
        *self.probe.lock().unwrap() = ProbeStats::default();
        let conn = self.conn.clone();
        let mut request = PROBE_PORT.to_be_bytes().to_vec();
        request.extend_from_slice(&mbps.to_be_bytes());
        request.extend_from_slice(&(time.as_millis() as u32).to_be_bytes());
        let offered = runtime.block_on(async move {
            tokio::time::timeout(time + Duration::from_secs(3), async {
                let (mut send, mut recv) = conn.open_bi().await.ok()?;
                send.write_all(&request).await.ok()?;
                let mut total = [0u8; 8];
                recv.read_exact(&mut total).await.ok()?;
                Some(u64::from_be_bytes(total))
            })
            .await
            .ok()
            .flatten()
        })?;
        if offered == 0 {
            return None;
        }
        // Stragglers still in flight.
        thread::sleep(Duration::from_millis(150));
        let stats = self.probe.lock().unwrap();
        let span = stats.last?.duration_since(stats.first?);
        if span < Duration::from_millis(100) {
            return None;
        }
        let measured = stats.bytes as f64 * 8.0 / span.as_secs_f64() / 1e6;
        Some(Bandwidth {
            mbps: measured,
            limited: measured < mbps as f64 * 0.85,
            loss: (1.0 - stats.bytes as f64 / offered as f64).clamp(0.0, 1.0),
        })
    }

    pub fn alive(&self) -> bool {
        self.conn.close_reason().is_none()
    }

    pub fn rtt(&self) -> Duration {
        self.pinger.rtt().unwrap_or_else(|| self.conn.rtt())
    }

    pub fn describe(&self) -> &'static str {
        describe(self.kind, self.conn.remote_address())
    }
}

impl Drop for GuestTunnel {
    fn drop(&mut self) {
        let _guard = self.runtime.as_ref().map(Runtime::enter);
        self.conn.close(VarInt::from_u32(0), b"bye");
        self.endpoint.close(VarInt::from_u32(0), b"bye");
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_millis(300));
        }
    }
}

async fn authenticate(conn: &Connection, token: [u8; 16]) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&token).await?;
        let mut ack = [0u8; 1];
        match recv.read_exact(&mut ack).await {
            Ok(()) if ack[0] == 1 => Ok(()),
            _ => match conn.close_reason() {
                Some(quinn::ConnectionError::ApplicationClosed(close)) if close.error_code == VarInt::from_u32(3) => {
                    bail!("denied")
                }
                _ => bail!("no ack"),
            },
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timeout"))?
}

fn failure_hint(reflexive: &Reflexive) -> String {
    if reflexive.symmetric {
        "У вас строгий NAT. Пусть сессию создаст друг.".into()
    } else {
        "Хост не отвечает. Проверьте, что сессия ещё открыта.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "binds the host control and audio ports on loopback"]
    fn seven_guests_keep_independent_tunnels() {
        use crate::{audio::AUDIO_PORT, protocol::new_token};
        use tokio::io::AsyncReadExt;

        let identity = Identity::generate("DuoLink Test").unwrap();
        let token = new_token();
        runtime().unwrap().block_on(async {
            let localhost = SocketAddr::from((HOST_LOOPBACK, 0));
            let control = TcpListener::bind((HOST_LOOPBACK, CONTROL_PORT)).await.unwrap();
            let audio = UdpSocket::bind((HOST_LOOPBACK, AUDIO_PORT)).await.unwrap();
            let endpoint = Endpoint::server(server_config(&identity).unwrap(), localhost).unwrap();
            let address = endpoint.local_addr().unwrap();
            let client = Endpoint::client(localhost).unwrap();
            let config = client_config(identity.fingerprint()).unwrap();
            let links = Arc::new(Mutex::new(Vec::new()));
            let probes = Arc::new(ProbeGate::default());
            tokio::spawn(accept(endpoint.clone(), token, links.clone(), probes.clone()));
            let host = HostTunnel {
                runtime: None,
                endpoint,
                candidates: Vec::new(),
                reflexive: Reflexive::default(),
                forwarded: false,
                links,
                probes,
                alive: Arc::new(AtomicBool::new(true)),
                _mapping: None,
                _signal: None,
            };
            let join = async || {
                let guest = client.connect_with(config.clone(), address, "duolink").unwrap().await.unwrap();
                authenticate(&guest, token).await.unwrap();
                let (mut send, recv) = guest.open_bi().await.unwrap();
                send.write_all(&CONTROL_PORT.to_be_bytes()).await.unwrap();
                send.write_all(b"hi").await.unwrap();
                let (mut tcp, from) = control.accept().await.unwrap();
                let mut marker = [0; 2];
                tcp.read_exact(&mut marker).await.unwrap();
                assert_eq!(&marker, b"hi");
                (guest, send, recv, tcp, from.port())
            };
            let receive_audio = async |guest: &Connection| {
                loop {
                    let datagram = guest.read_datagram().await.unwrap();
                    if datagram.starts_with(&AUDIO_PORT.to_be_bytes()) {
                        return datagram.slice(2..);
                    }
                }
            };
            let probe = async |guest: &Connection, time_ms: u32| {
                let (mut send, recv) = guest.open_bi().await.unwrap();
                let mut request = PROBE_PORT.to_be_bytes().to_vec();
                request.extend_from_slice(&20u32.to_be_bytes());
                request.extend_from_slice(&time_ms.to_be_bytes());
                send.write_all(&request).await.unwrap();
                send.finish().unwrap();
                recv
            };
            let offered = async |mut recv: quinn::RecvStream| {
                let mut total = [0; 8];
                recv.read_exact(&mut total).await.unwrap();
                u64::from_be_bytes(total)
            };

            tokio::time::timeout(Duration::from_secs(5), async {
                let (first, _first_send, _first_recv, mut first_tcp, first_port) = join().await;
                let (second, _second_send, _second_recv, _second_tcp, second_port) = join().await;
                let mut others = Vec::new();
                for _ in 0..5 {
                    others.push(join().await);
                }
                let first_route = host.link(first_port).unwrap().audio.unwrap();
                let second_route = host.link(second_port).unwrap().audio.unwrap();
                assert_ne!(first_route, second_route);

                audio.send_to(b"first", first_route).await.unwrap();
                audio.send_to(b"second", second_route).await.unwrap();
                assert_eq!(&receive_audio(&first).await[..], b"first");
                assert_eq!(&receive_audio(&second).await[..], b"second");

                assert!(offered(probe(&others[0].0, 120).await).await > 0);
                let ongoing = probe(&first, 1500).await;
                loop {
                    let datagram = first.read_datagram().await.unwrap();
                    if datagram.starts_with(&PROBE_PORT.to_be_bytes()) {
                        break;
                    }
                }
                assert_eq!(offered(probe(&second, 700).await).await, 0);
                host.stop_probes();
                assert_eq!(offered(ongoing).await, 0);
                assert_eq!(offered(probe(&second, 700).await).await, 0);

                let mut round_trips = Vec::new();
                for index in 0..200u64 {
                    let started = Instant::now();
                    second.send_datagram(tag(PING_PORT, &index.to_be_bytes())).unwrap();
                    loop {
                        let reply = second.read_datagram().await.unwrap();
                        if reply == tag(PONG_PORT, &index.to_be_bytes()) {
                            break;
                        }
                    }
                    round_trips.push(started.elapsed());
                }
                let (median, p99, _) = percentiles(round_trips);
                eprintln!("local QUIC RTT with seven guests: median {median:.0} µs, p99 {p99:.0} µs");

                first.close(VarInt::from_u32(0), b"bye");
                assert_eq!(first_tcp.read(&mut [0; 1]).await.unwrap(), 0);
                assert!(host.link(first_port).is_none());
                assert_eq!(host.link(second_port).unwrap().audio, Some(second_route));
                audio.send_to(b"still here", second_route).await.unwrap();
                assert_eq!(&receive_audio(&second).await[..], b"still here");
                second.close(VarInt::from_u32(0), b"bye");
            }).await.expect("loopback audio routing timed out");
        });
    }

    #[test]
    fn stun_xor_mapped_address_parses() {
        let txid = [9u8; 12];
        let mut message = vec![0x01, 0x01, 0x00, 0x0c];
        message.extend_from_slice(&STUN_MAGIC.to_be_bytes());
        message.extend_from_slice(&txid);
        message.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        message.extend_from_slice(&(41641u16 ^ 0x2112).to_be_bytes());
        message.extend_from_slice(&(u32::from(Ipv4Addr::new(203, 0, 113, 7)) ^ STUN_MAGIC).to_be_bytes());
        assert_eq!(
            parse_stun(&message, &txid),
            Some(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 41641))
        );
        assert_eq!(parse_stun(&message, &[0; 12]), None);
    }

    fn percentiles(mut samples: Vec<Duration>) -> (f64, f64, f64) {
        samples.sort();
        let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize].as_secs_f64() * 1e6;
        (at(0.5), at(0.99), at(1.0))
    }

    fn round_trips(socket: &StdUdpSocket, target: SocketAddr, count: usize) -> Vec<Duration> {
        let payload = vec![7u8; 1100];
        let mut reply = [0u8; 2048];
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            let start = Instant::now();
            socket.send_to(&payload, target).unwrap();
            let (got, _) = socket.recv_from(&mut reply).unwrap();
            assert_eq!(got, payload.len());
            samples.push(start.elapsed());
        }
        samples
    }

    /// Measures what the tunnel adds to a packet's round trip on one PC:
    /// loopback → DuoLink host → QUIC → DuoLink guest → loopback and back.
    #[test]
    #[ignore = "binds stream ports and contacts STUN/UPnP"]
    fn tunnel_round_trip_overhead() {
        let echo = StdUdpSocket::bind((HOST_LOOPBACK, UDP_PORTS[2])).unwrap();
        thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            while let Ok((count, from)) = echo.recv_from(&mut buffer) {
                let _ = echo.send_to(&buffer[..count], from);
            }
        });
        let identity = Identity::generate("bench").unwrap();
        let token = [5u8; 16];
        let host = HostTunnel::open(&identity, token).unwrap();
        let loopback: Vec<Candidate> = host
            .candidates
            .iter()
            .filter(|c| c.addr.ip().is_loopback())
            .copied()
            .collect();
        let guest = GuestTunnel::connect(token, identity.fingerprint(), &loopback, || Ok(())).unwrap();

        let client = StdUdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let direct = SocketAddr::from((HOST_LOOPBACK, UDP_PORTS[2]));
        let tunneled = SocketAddr::from((GUEST_LOOPBACK, UDP_PORTS[2]));
        round_trips(&client, tunneled, 200); // warm up
        round_trips(&client, direct, 200);
        let (d50, d99, dmax) = percentiles(round_trips(&client, direct, 5000));
        let (t50, t99, tmax) = percentiles(round_trips(&client, tunneled, 5000));
        eprintln!("direct   RTT µs: p50 {d50:7.1}  p99 {d99:7.1}  max {dmax:8.1}");
        eprintln!("tunneled RTT µs: p50 {t50:7.1}  p99 {t99:7.1}  max {tmax:8.1}");
        eprintln!("one-way overhead ≈ {:.0} µs (p50)", (t50 - d50) / 2.0);
        drop(guest);
        drop(host);
    }

    /// Pushes a sustained stream through the tunnel and counts what survives.
    #[test]
    #[ignore = "binds stream ports and contacts STUN/UPnP"]
    fn tunnel_throughput() {
        let sink = StdUdpSocket::bind((HOST_LOOPBACK, UDP_PORTS[0])).unwrap();
        socket2::SockRef::from(&sink).set_recv_buffer_size(RECEIVE_BUFFER).unwrap();
        let received = Arc::new(AtomicU64::new(0));
        {
            let received = received.clone();
            thread::spawn(move || {
                let mut buffer = [0u8; 2048];
                while let Ok(count) = sink.recv(&mut buffer) {
                    received.fetch_add(count as u64, Ordering::Relaxed);
                }
            });
        }
        let identity = Identity::generate("bench").unwrap();
        let token = [6u8; 16];
        let host = HostTunnel::open(&identity, token).unwrap();
        let loopback: Vec<Candidate> = host.candidates.iter().filter(|c| c.addr.ip().is_loopback()).copied().collect();
        let guest = GuestTunnel::connect(token, identity.fingerprint(), &loopback, || Ok(())).unwrap();

        let client = StdUdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let target = SocketAddr::from((GUEST_LOOPBACK, UDP_PORTS[0]));
        let payload = vec![1u8; 1100];
        for mbps in [100.0, 300.0, 500.0] {
            received.store(0, Ordering::Relaxed);
            let rate = mbps * 1e6 / 8.0;
            let start = Instant::now();
            let mut sent = 0u64;
            while start.elapsed() < Duration::from_secs(1) {
                let due = (start.elapsed().as_secs_f64() * rate) as u64;
                while sent < due {
                    client.send_to(&payload, target).unwrap();
                    sent += payload.len() as u64;
                }
                std::hint::spin_loop();
            }
            thread::sleep(Duration::from_millis(300));
            let got = received.load(Ordering::Relaxed);
            eprintln!(
                "{mbps:>4.0} Мбит/с: доставлено {:.2} % ({} из {} пакетов)",
                got as f64 * 100.0 / sent as f64,
                got / 1100,
                sent / 1100
            );
        }
        drop(guest);
        drop(host);
    }

    #[test]
    fn vpn_and_virtual_adapters_are_not_paths() {
        for name in [
            "happ-xray", "Tailscale Tailscale Tunnel", "vEthernet (WSL) Hyper-V Virtual Ethernet Adapter",
            "Ethernet 3 TAP-Windows Adapter V9", "wg0 WireGuard Tunnel", "AmneziaVPN", "sing-tun Wintun",
            "Radmin VPN", "tun0",
        ] {
            assert!(virtual_adapter(name), "{name}");
        }
        for name in [
            "Ethernet Realtek PCIe 2.5GbE Family Controller",
            "Беспроводная сеть Intel(R) Wi-Fi 6E AX211 160MHz",
            "Ethernet Intel(R) Ethernet Controller I225-V",
        ] {
            assert!(!virtual_adapter(name), "{name}");
        }
        let lan = lan_addresses();
        eprintln!("LAN paths on this PC: {lan:?}");
        assert!(lan.iter().all(|ip| !ip.is_loopback()));
    }

    #[test]
    fn signal_round_trips_only_with_the_right_token() {
        let addrs = vec![SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 51000)];
        let sealed = seal(&[1; 16], &addrs);
        assert_eq!(unseal(&[1; 16], &sealed), Some(addrs));
        assert_eq!(unseal(&[2; 16], &sealed), None);
        assert_ne!(topic(&[1; 16]), topic(&[2; 16]));
    }
}
