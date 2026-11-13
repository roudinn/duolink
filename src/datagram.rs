use std::{collections::VecDeque, sync::{Arc, Mutex}};

use bytes::{Bytes, BytesMut};
use quinn::{Connection, SendDatagramError};
use tokio::sync::Notify;

// The local socket buffer also has room for oversized packets so Windows can
// report them without ending the forwarding task.
pub(crate) const LOCAL_PACKET: usize = 2048;
const VIDEO_QUEUE: usize = 512;
const URGENT_QUEUE: usize = 128;
const URGENT_RESERVE: usize = 8 * 1024;

/// Each producer owns its slabs. Quinn gets reference-counted slices, and a
/// slab can be reused once all its slices have left the send queue.
pub(crate) struct PacketPool {
    slabs: Vec<BytesMut>,
    current: usize,
}

impl PacketPool {
    pub fn new(video: bool) -> Self {
        let (count, size) = if video { (8, 128 << 10) } else { (3, 32 << 10) };
        let slabs = (0..count).map(|_| {
            let mut slab = BytesMut::with_capacity(size);
            // Promote the slab to shared storage once, before receiving UDP.
            drop(slab.split_to(0));
            slab
        }).collect();
        Self { slabs, current: 0 }
    }

    pub fn packet(&mut self, port: u16, payload: &[u8]) -> Option<Bytes> {
        let len = 2 + payload.len();
        for _ in 0..self.slabs.len() {
            let slab = &mut self.slabs[self.current];
            if slab.try_reclaim(len) {
                slab.extend_from_slice(&port.to_be_bytes());
                slab.extend_from_slice(payload);
                return Some(slab.split_to(len).freeze());
            }
            self.current = (self.current + 1) % self.slabs.len();
        }
        None
    }
}

/// Audio, ENet and pings can enter Quinn even while a video send waits
/// for space. Video has its own bounded queue and never blocks a UDP reader.
pub(crate) struct DatagramSender {
    conn: Connection,
    queues: Mutex<Queues>,
    ready: Notify,
    urgent: Notify,
}

struct Queues {
    urgent: VecDeque<Bytes>,
    video: VecDeque<Bytes>,
    video_room: usize,
}

impl DatagramSender {
    pub fn new(conn: Connection) -> Arc<Self> {
        Arc::new(Self {
            conn,
            queues: Mutex::new(Queues {
                urgent: VecDeque::with_capacity(URGENT_QUEUE),
                video: VecDeque::with_capacity(VIDEO_QUEUE),
                video_room: 0,
            }),
            ready: Notify::new(),
            urgent: Notify::new(),
        })
    }

    pub fn enqueue(&self, pool: &mut PacketPool, port: u16, payload: &[u8], urgent: bool) {
        let Some(packet) = pool.packet(port, payload) else {
            super::tunnel::record_drop();
            return;
        };
        let mut queues = self.queues.lock().unwrap();
        if !urgent && queues.urgent.is_empty() && queues.video.is_empty() {
            // Quinn charges for the payload and its Bytes handle. Leave room
            // for urgent sends and refresh the available space once per batch.
            let cost = packet.len() + std::mem::size_of::<Bytes>();
            if queues.video_room < cost {
                queues.video_room = self.conn.datagram_send_buffer_space().saturating_sub(URGENT_RESERVE);
            }
            if queues.video_room >= cost {
                queues.video_room -= cost;
                let _ = sent_ok(self.conn.send_datagram(packet));
                return;
            }
        }
        queues.video_room = 0;
        let (queue, limit) = if urgent { (&mut queues.urgent, URGENT_QUEUE) } else { (&mut queues.video, VIDEO_QUEUE) };
        if queue.len() == limit {
            queue.pop_front();
            super::tunnel::record_drop();
        }
        queue.push_back(packet);
        drop(queues);
        if urgent {
            self.urgent.notify_one();
        }
        self.ready.notify_one();
    }

    pub async fn run(&self) {
        let conn = &self.conn;
        loop {
            let ready = self.ready.notified();
            let next = {
                let mut queues = self.queues.lock().unwrap();
                queues.urgent.pop_front().map(|packet| (packet, true))
                    .or_else(|| queues.video.pop_front().map(|packet| (packet, false)))
            };
            let Some((packet, urgent)) = next else {
                tokio::select! {
                    _ = ready => continue,
                    _ = conn.closed() => return,
                }
            };
            if urgent || conn.datagram_send_buffer_space() >= packet.len() + URGENT_RESERVE {
                if !sent_ok(conn.send_datagram(packet)) { return }
            } else if !self.send_video(conn, packet).await {
                return;
            }
            tokio::task::consume_budget().await;
        }
    }

    async fn send_video(&self, conn: &Connection, packet: Bytes) -> bool {
        let send = conn.send_datagram_wait(packet);
        tokio::pin!(send);
        loop {
            let urgent = self.urgent.notified();
            let packet = self.queues.lock().unwrap().urgent.pop_front();
            if let Some(packet) = packet {
                if !sent_ok(conn.send_datagram(packet)) { return false }
                tokio::task::consume_budget().await;
                continue;
            }
            tokio::select! {
                biased;
                _ = urgent => continue,
                result = &mut send => return sent_ok(result),
            }
        }
    }
}

fn sent_ok(result: Result<(), SendDatagramError>) -> bool {
    match result {
        Ok(()) => true,
        Err(SendDatagramError::ConnectionLost(_)) => false,
        Err(_) => {
            super::tunnel::record_drop();
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_pool_preserves_live_slices_and_reuses_slabs() {
        let mut pool = PacketPool::new(false);
        let bytes = [17; LOCAL_PACKET - 2];
        let first = pool.packet(1234, &bytes).unwrap();
        let pointer = first.as_ptr();
        assert!(pool.packet(1234, &bytes).is_some());
        for _ in 0..1024 {
            assert!(pool.packet(4321, &[99; LOCAL_PACKET - 2]).is_some());
        }
        assert_eq!(&first[..2], &1234u16.to_be_bytes());
        assert_eq!(&first[2..], &bytes);
        drop(first);
        pool.current = 0;
        while pool.slabs[0].capacity() >= LOCAL_PACKET {
            drop(pool.packet(1234, &bytes));
        }
        assert_eq!(pool.packet(1234, &bytes).unwrap().as_ptr(), pointer);
    }
}
