//! Bridges the existing encrypted QUIC link through a bounded TLS relay.
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

#[derive(Debug)]
pub(crate) struct RelayFull;

impl std::fmt::Display for RelayFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ретранслятор занят. Для прямого подключения попробуйте сделать хостом игрока с публичным IPv4 или включённым UPnP.")
    }
}

impl std::error::Error for RelayFull {}

pub const HOST_BRIDGE: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 254);
const MAX_DATAGRAM: usize = 1232;
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect(url: &str, token: [u8; 16], guest: bool) -> Result<Socket> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_DATAGRAM + 4))
        .max_frame_size(Some(MAX_DATAGRAM + 4))
        .write_buffer_size(0)
        .max_write_buffer_size(128 * 1024);
    tokio::time::timeout(Duration::from_secs(5), async {
        let (mut socket, _) = connect_async_with_config(url, Some(config), true).await?;
        let mut hash = Sha256::new();
        hash.update(b"duolink/relay/v1");
        hash.update(token);
        let mut hello = vec![u8::from(guest)];
        hello.extend_from_slice(&hash.finalize());
        socket.send(Message::Binary(hello.into())).await?;
        loop {
            match socket.next().await {
                Some(Ok(Message::Binary(data))) if data.as_ref() == b"ready" => return Ok(socket),
                Some(Ok(Message::Binary(data))) if data.as_ref() == b"RelayFull" => {
                    return Err(RelayFull.into());
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => anyhow::bail!("Сервер ретрансляции недоступен."),
            }
        }
    })
    .await
    .context("Сервер ретрансляции не ответил.")?
}

async fn send(socket: &mut Socket, packet: Vec<u8>) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(2),
        socket.send(Message::Binary(packet.into())),
    )
    .await??;
    Ok(())
}

pub async fn host(url: String, token: [u8; 16], port: u16) {
    loop {
        if let Ok(socket) = connect(&url, token, false).await {
            let _ = serve_host(socket, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

struct Peer {
    incoming: mpsc::Sender<bytes::Bytes>,
    task: JoinHandle<()>,
    last_seen: Instant,
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_host(mut socket: Socket, target: SocketAddrV4) -> Result<()> {
    let mut peers = HashMap::<u32, Peer>::new();
    let (tx, mut output) = mpsc::channel(32);
    loop {
        tokio::select! {
            Some(packet) = output.recv() => send(&mut socket, packet).await?,
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Binary(packet))) if packet.len() >= 4 => {
                    let id = u32::from_be_bytes(packet[..4].try_into().unwrap());
                    if packet.len() == 4 {
                        peers.remove(&id);
                        continue;
                    }
                    let now = Instant::now();
                    if !peers.contains_key(&id) {
                        peers.retain(|_, peer| now.duration_since(peer.last_seen) < Duration::from_secs(10));
                    }
                    if !peers.contains_key(&id) && peers.len() < 4 {
                        let (incoming, input) = mpsc::channel(32);
                        let task = tokio::spawn(host_peer(target, id, input, tx.clone()));
                        peers.insert(id, Peer { incoming, task, last_seen: now });
                    }
                    if let Some(peer) = peers.get_mut(&id) {
                        peer.last_seen = now;
                        let _ = peer.incoming.try_send(packet.slice(4..));
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => { tokio::time::timeout(Duration::from_secs(2), socket.flush()).await??; }
                _ => return Ok(()),
            }
        }
    }
}

async fn host_peer(
    target: SocketAddrV4,
    id: u32,
    mut input: mpsc::Receiver<bytes::Bytes>,
    output: mpsc::Sender<Vec<u8>>,
) {
    let Ok(socket) = UdpSocket::bind((HOST_BRIDGE, 0)).await else {
        return;
    };
    if socket.connect(target).await.is_err() {
        return;
    }
    let mut buffer = [0u8; MAX_DATAGRAM + 1];
    loop {
        tokio::select! {
            next = input.recv() => {
                let Some(next) = next else { break };
                let _ = socket.send(&next).await;
            }
            received = socket.recv(&mut buffer) => {
                let Ok(n) = received else { break };
                if n <= MAX_DATAGRAM {
                    let mut packet = Vec::with_capacity(n + 4);
                    packet.extend_from_slice(&id.to_be_bytes());
                    packet.extend_from_slice(&buffer[..n]);
                    let _ = output.try_send(packet);
                }
            }
        }
    }
}

pub async fn guest(
    url: &str,
    token: [u8; 16],
    port: u16,
) -> Result<(SocketAddrV4, JoinHandle<()>)> {
    let socket = connect(url, token, true).await?;
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    udp.connect((Ipv4Addr::LOCALHOST, port)).await?;
    let std::net::SocketAddr::V4(address) = udp.local_addr()? else {
        unreachable!()
    };
    Ok((
        address,
        tokio::spawn(async move {
            let _ = serve_guest(socket, udp).await;
        }),
    ))
}

async fn serve_guest(mut socket: Socket, udp: UdpSocket) -> Result<()> {
    let mut buffer = [0u8; MAX_DATAGRAM + 1];
    loop {
        tokio::select! {
            received = udp.recv(&mut buffer) => {
                let n = received?;
                if n <= MAX_DATAGRAM { send(&mut socket, buffer[..n].to_vec()).await?; }
            }
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Binary(payload))) => {
                    ensure!(payload.len() <= MAX_DATAGRAM, "Invalid relay datagram");
                    let _ = udp.send(&payload).await;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => { tokio::time::timeout(Duration::from_secs(2), socket.flush()).await??; }
                _ => return Ok(()),
            }
        }
    }
}
