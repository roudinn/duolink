//! RFC 8489 IPv4 Binding responses. No relay, forwarding or alternate source.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use tokio::net::UdpSocket;

const COOKIE: [u8; 4] = 0x2112_A442_u32.to_be_bytes();
const MAX_REPLIES_PER_SECOND: u32 = 600;

pub async fn bind(address: &str) -> Result<UdpSocket> {
    let address: SocketAddrV4 = address.parse()?;
    Ok(UdpSocket::bind(address).await?)
}

fn binding_reply(request: &[u8], peer: SocketAddrV4) -> Option<[u8; 32]> {
    let header = request.get(..20)?;
    let length = u16::from_be_bytes([header[2], header[3]]) as usize;
    if header[..2] != [0, 1]
        || header[4..8] != COOKIE
        || length != request.len() - 20
        || !length.is_multiple_of(4)
    {
        return None;
    }
    let mut reply = [0; 32];
    reply[..4].copy_from_slice(&[1, 1, 0, 12]);
    reply[4..20].copy_from_slice(&header[4..20]);
    reply[20..26].copy_from_slice(&[0, 0x20, 0, 8, 0, 1]);
    reply[26..28].copy_from_slice(&(peer.port() ^ 0x2112).to_be_bytes());
    for (encoded, (ip, cookie)) in reply[28..]
        .iter_mut()
        .zip(peer.ip().octets().into_iter().zip(COOKIE))
    {
        *encoded = ip ^ cookie;
    }
    Some(reply)
}

pub async fn serve(socket: UdpSocket) -> Result<()> {
    let mut buffer = [0; 1500];
    let mut window = Instant::now();
    let mut replies = 0;
    loop {
        let (len, SocketAddr::V4(peer)) = socket.recv_from(&mut buffer).await? else {
            continue;
        };
        if window.elapsed() >= Duration::from_secs(1) {
            window = Instant::now();
            replies = 0;
        }
        if replies >= MAX_REPLIES_PER_SECOND || *peer.ip() == Ipv4Addr::UNSPECIFIED {
            continue;
        }
        if let Some(reply) = binding_reply(&buffer[..len], peer) {
            replies += 1;
            if let Ok(sent) = socket.send_to(&reply, peer).await {
                ensure!(sent == reply.len(), "partial STUN datagram");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_matches_transaction_and_mapped_address() {
        let request = [
            0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        ];
        let peer = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 5), 54321);
        let response = binding_reply(&request, peer).unwrap();
        assert_eq!(&response[..4], &[1, 1, 0, 12]);
        assert_eq!(&response[4..20], &request[4..20]);
        assert_eq!(
            &response[20..],
            &[0, 32, 0, 8, 0, 1, 0xf5, 0x23, 0xea, 0x12, 0xd5, 0x47]
        );
        assert!(binding_reply(&request[..19], peer).is_none());
        assert!(binding_reply(&response, peer).is_none());
    }
}
