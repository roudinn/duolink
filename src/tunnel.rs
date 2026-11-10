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
        atomic::{AtomicBool, Ordering},
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
    platform::system_tool,
    protocol::{CONTROL_PORT, Candidate, Identity, MEDIA_PORT, PathKind, Pin},
};

pub const TUNNEL_PORT: u16 = 48170;
/// Where the guest mirrors the host's ports, so a host on the same PC (127.0.0.1) never collides.
pub const GUEST_LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
const HOST_LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;
pub const TCP_PORTS: [u16; 4] = [CONTROL_PORT, MEDIA_PORT - 5, MEDIA_PORT, MEDIA_PORT + 21];
pub const UDP_PORTS: [u16; 3] = [MEDIA_PORT + 9, MEDIA_PORT + 10, MEDIA_PORT + 11];
const ALPN: &[u8] = b"duolink/2";
const STUN_SERVERS: [&str; 3] = [
    "stun.l.google.com:19302",
    "stun.cloudflare.com:3478",
    "stun1.l.google.com:19302",
];
const SIGNAL_SERVER: &str = "https://ntfy.sh";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

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

fn curl() -> Result<Command> {
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
        .max_idle_timeout(Some(Duration::from_secs(6).try_into().unwrap()))
        .keep_alive_interval(Some(Duration::from_millis(500)))
        .initial_mtu(1280)
        .datagram_receive_buffer_size(Some(16 << 20))
        .datagram_send_buffer_size(16 << 20)
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

fn runtime() -> Result<Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
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
async fn serve_host(conn: Connection) {
    let mut sockets = HashMap::new();
    for port in UDP_PORTS {
        let Ok(socket) = UdpSocket::bind((HOST_LOOPBACK, 0)).await else { return };
        if socket.connect((HOST_LOOPBACK, port)).await.is_err() {
            return;
        }
        let socket = Arc::new(socket);
        sockets.insert(port, socket.clone());
        let conn = conn.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match socket.recv(&mut buffer).await {
                    Ok(count) => {
                        let _ = conn.send_datagram(tag(port, &buffer[..count]));
                    }
                    Err(error) if reset(&error) => {}
                    Err(_) => break,
                }
            }
        });
    }
    let datagrams = {
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(datagram) = conn.read_datagram().await {
                if datagram.len() < 2 {
                    continue;
                }
                let port = u16::from_be_bytes([datagram[0], datagram[1]]);
                if let Some(socket) = sockets.get(&port) {
                    let _ = socket.send(&datagram[2..]).await;
                }
            }
        })
    };
    while let Ok((send, mut recv)) = conn.accept_bi().await {
        tokio::spawn(async move {
            let mut header = [0u8; 2];
            if recv.read_exact(&mut header).await.is_err() {
                return;
            }
            let port = u16::from_be_bytes(header);
            if !TCP_PORTS.contains(&port) {
                return;
            }
            if let Ok(tcp) = TcpStream::connect((HOST_LOOPBACK, port)).await {
                pipe(tcp, send, recv).await;
            }
        });
    }
    datagrams.abort();
}

/// Guest side: the same ports mirrored on 127.0.0.2.
async fn serve_guest(conn: Connection, listeners: Vec<(u16, TcpListener)>, sockets: Vec<(u16, UdpSocket)>) {
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
                        let _ = conn.send_datagram(tag(port, &buffer[..count]));
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
        (PathKind::Forwarded, _) => "Интернет · порт открыт",
        (PathKind::Public, _) => "Интернет · пробитый NAT",
    }
}

/* ------------------------------------------------------------------ host */

pub struct HostTunnel {
    runtime: Option<Runtime>,
    endpoint: Endpoint,
    pub candidates: Vec<Candidate>,
    pub reflexive: Reflexive,
    pub forwarded: bool,
    peer: Arc<Mutex<Option<Connection>>>,
    alive: Arc<AtomicBool>,
    _mapping: Option<PortMapping>,
    _signal: Option<Subscription>,
}

impl HostTunnel {
    pub fn open(identity: &Identity, token: [u8; 16]) -> Result<Self> {
        let socket = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, TUNNEL_PORT))
            .or_else(|_| StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))
            .context("Не удалось открыть сетевой порт DuoLink.")?;
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
        for ip in local_addresses().into_iter().take(8) {
            candidates.push(Candidate { kind: PathKind::Local, addr: SocketAddrV4::new(ip, port) });
        }
        candidates.push(Candidate {
            kind: PathKind::Local,
            addr: SocketAddrV4::new(HOST_LOOPBACK, port),
        });

        let punch_socket = socket.try_clone()?;
        socket.set_read_timeout(None)?;
        socket.set_nonblocking(true)?;
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

        let peer = Arc::new(Mutex::new(None::<Connection>));
        runtime.spawn(accept(endpoint.clone(), token, peer.clone()));

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
            peer,
            alive,
            _mapping: mapping.map(|(_, mapping)| mapping),
            _signal: signal,
        })
    }

    /// The authenticated guest, while its connection is up.
    pub fn peer(&self) -> Option<(SocketAddr, Duration)> {
        let peer = self.peer.lock().unwrap();
        let conn = peer.as_ref()?;
        conn.close_reason().is_none().then(|| (conn.remote_address(), conn.rtt()))
    }
}

impl Drop for HostTunnel {
    fn drop(&mut self) {
        // Let the last control message (e.g. «rejected») reach the guest first.
        if self.peer().is_some() {
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

async fn accept(endpoint: Endpoint, token: [u8; 16], peer: Arc<Mutex<Option<Connection>>>) {
    while let Some(incoming) = endpoint.accept().await {
        if peer.lock().unwrap().is_some() {
            incoming.refuse();
            continue;
        }
        let peer = peer.clone();
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
            {
                let mut slot = peer.lock().unwrap();
                if slot.is_some() {
                    conn.close(VarInt::from_u32(4), b"busy");
                    return;
                }
                *slot = Some(conn.clone());
            }
            let _ = send.write_all(&[1]).await;
            let _ = send.finish();
            serve_host(conn).await;
        });
    }
}

/* ----------------------------------------------------------------- guest */

pub struct GuestTunnel {
    runtime: Option<Runtime>,
    endpoint: Endpoint,
    conn: Connection,
    pub kind: PathKind,
    pub same_machine: bool,
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
        mine.extend(local_addresses().into_iter().take(8).map(|ip| SocketAddrV4::new(ip, port)));
        publish(token, mine);
        punch(
            &socket,
            candidates.iter().map(|c| c.addr).collect(),
            CONNECT_TIMEOUT,
        );

        socket.set_read_timeout(None)?;
        socket.set_nonblocking(true)?;
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
        let (kind, conn) = loop {
            check()?;
            if Instant::now() >= deadline {
                bail!(failure_hint(&reflexive));
            }
            let next = runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await
            });
            match next {
                Ok(Some((kind, conn))) => match runtime.block_on(authenticate(&conn, token)) {
                    Ok(()) => break (kind, conn),
                    Err(error) => {
                        conn.close(VarInt::from_u32(0), b"");
                        if error.to_string().contains("denied") {
                            bail!("Хост отклонил код: он устарел или уже использован.");
                        }
                    }
                },
                Ok(None) => bail!(failure_hint(&reflexive)),
                Err(_) => {}
            }
        };
        // Let the losing attempts go.
        runtime.spawn(async move { while receiver.recv().await.is_some() {} });

        let (listeners, sockets) = runtime.block_on(async {
            let mut listeners = Vec::new();
            for port in TCP_PORTS {
                let listener = TcpListener::bind((GUEST_LOOPBACK, port))
                    .await
                    .with_context(|| format!("Порт {port} на этом ПК занят. Закройте другой стриминг."))?;
                listeners.push((port, listener));
            }
            let mut sockets = Vec::new();
            for port in UDP_PORTS {
                let socket = UdpSocket::bind((GUEST_LOOPBACK, port))
                    .await
                    .with_context(|| format!("Порт {port} на этом ПК занят. Закройте другой стриминг."))?;
                sockets.push((port, socket));
            }
            anyhow::Ok((listeners, sockets))
        })?;
        runtime.spawn(serve_guest(conn.clone(), listeners, sockets));

        let same_machine = is_self(conn.remote_address().ip());
        Ok(Self {
            runtime: Some(runtime),
            endpoint,
            conn,
            kind,
            same_machine,
        })
    }

    pub fn alive(&self) -> bool {
        self.conn.close_reason().is_none()
    }

    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
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
        "Не удалось соединиться: у вашего провайдера строгий NAT. Попробуйте, чтобы сессию создал друг, или включите UPnP на роутере хоста.".into()
    } else {
        "Хост не отвечает. Проверьте, что сессия на хосте ещё открыта и код скопирован полностью. Если не поможет — включите UPnP на роутере хоста.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn signal_round_trips_only_with_the_right_token() {
        let addrs = vec![SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 51000)];
        let sealed = seal(&[1; 16], &addrs);
        assert_eq!(unseal(&[1; 16], &sealed), Some(addrs));
        assert_eq!(unseal(&[2; 16], &sealed), None);
        assert_ne!(topic(&[1; 16]), topic(&[2; 16]));
    }
}
