use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
    routing::get,
};
use futures_util::StreamExt;
use tokio::sync::{Semaphore, mpsc};

const MAX_DATAGRAM: usize = 1232;
const MAX_GUESTS: usize = 4;
const MAX_RELAY_SESSIONS: usize = 5;
const ROOM_BYTES_PER_SECOND: f64 = 1_000_000.0;
const ROOM_BURST: f64 = 64_000.0;

async fn report_full(socket: &mut WebSocket) {
    send(socket, Message::Binary(b"RelayFull".to_vec().into())).await;
    let _ = send(socket, Message::Close(None)).await;
}
const BYTES_PER_SECOND: f64 = 3_000_000.0;
const BURST: f64 = 160_000.0;
type Key = [u8; 32];
type Queue = mpsc::Sender<Message>;

struct Room {
    host: Queue,
    guests: HashMap<u32, Queue>,
    budget: Budget,
}

struct Budget {
    remaining: f64,
    updated: Instant,
}

impl Budget {
    fn new(burst: f64) -> Self {
        Self {
            remaining: burst,
            updated: Instant::now(),
        }
    }

    fn take(&mut self, bytes: usize, rate: f64, burst: f64) -> bool {
        let now = Instant::now();
        self.remaining =
            (self.remaining + now.duration_since(self.updated).as_secs_f64() * rate).min(burst);
        self.updated = now;
        if self.remaining < bytes as f64 {
            return false;
        }
        self.remaining -= bytes as f64;
        true
    }
}

struct Relay {
    rooms: Mutex<HashMap<Key, Room>>,
    sockets: Semaphore,
    next_guest: AtomicU32,
    budget: Mutex<Budget>,
}

pub fn router() -> Router {
    Router::new()
        .route("/relay", get(upgrade))
        .with_state(Arc::new(Relay {
            rooms: Mutex::new(HashMap::new()),
            sockets: Semaphore::new(48),
            next_guest: AtomicU32::new(1),
            budget: Mutex::new(Budget::new(BURST)),
        }))
}

async fn upgrade(State(relay): State<Arc<Relay>>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_DATAGRAM + 4)
        .max_frame_size(MAX_DATAGRAM + 4)
        .write_buffer_size(0)
        .max_write_buffer_size(128 * 1024)
        .on_upgrade(move |socket| serve(relay, socket))
}

async fn send(socket: &mut WebSocket, msg: Message) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), socket.send(msg)).await,
        Ok(Ok(()))
    )
}

async fn serve(relay: Arc<Relay>, mut socket: WebSocket) {
    let Ok(_permit) = relay.sockets.try_acquire() else {
        report_full(&mut socket).await;
        return;
    };
    let Ok(Some(Ok(Message::Binary(hello)))) =
        tokio::time::timeout(Duration::from_secs(4), socket.next()).await
    else {
        return;
    };
    let Ok(key) = <Key>::try_from(hello.get(1..).unwrap_or_default()) else {
        return;
    };
    let (outgoing, mut receive) = mpsc::channel(32);
    // Zero is the host, nonzero IDs name guests inside this room only.
    let id = {
        let mut rooms = relay.rooms.lock().unwrap();
        match hello[0] {
            0 if !rooms.contains_key(&key) => {
                rooms.insert(
                    key,
                    Room {
                        host: outgoing,
                        guests: HashMap::new(),
                        budget: Budget::new(ROOM_BURST),
                    },
                );
                Some(0)
            }
            1 => {
                let active = rooms
                    .values()
                    .filter(|room| !room.guests.is_empty())
                    .count();
                let Some(room) = rooms.get_mut(&key) else {
                    return;
                };
                if room.guests.len() >= MAX_GUESTS
                    || (room.guests.is_empty() && active >= MAX_RELAY_SESSIONS)
                {
                    None
                } else {
                    let id = relay.next_guest.fetch_add(1, Ordering::Relaxed);
                    if id == 0 || room.guests.contains_key(&id) {
                        return;
                    }
                    room.guests.insert(id, outgoing);
                    Some(id)
                }
            }
            _ => return,
        }
    };

    let Some(id) = id else {
        report_full(&mut socket).await;
        return;
    };

    if send(&mut socket, Message::Binary(b"ready"[..].to_vec().into())).await {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if !send(&mut socket, Message::Ping(Vec::new().into())).await { break; }
                }
                outgoing = receive.recv() => {
                    let Some(outgoing) = outgoing else { break };
                    if !send(&mut socket, outgoing).await { break; }
                }
                incoming = socket.next() => match incoming {
                    Some(Ok(Message::Binary(packet))) => forward(&relay, &key, id, packet),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    _ => break,
                }
            }
        }
    }

    let mut rooms = relay.rooms.lock().unwrap();
    if id == 0 {
        rooms.remove(&key);
    } else if let Some(room) = rooms.get_mut(&key)
        && room.guests.remove(&id).is_some()
    {
        let _ = room
            .host
            .try_send(Message::Binary(id.to_be_bytes().to_vec().into()));
    }
}

fn forward(relay: &Relay, key: &Key, id: u32, packet: axum::body::Bytes) {
    let mut rooms = relay.rooms.lock().unwrap();
    let Some(room) = rooms.get_mut(key) else {
        return;
    };
    let (target, datagram) = if id == 0 {
        if packet.len() <= 4 {
            return;
        }
        let peer = u32::from_be_bytes(packet[..4].try_into().unwrap());
        let Some(target) = room.guests.get(&peer) else {
            return;
        };
        (target, packet.slice(4..))
    } else {
        if packet.is_empty() || packet.len() > MAX_DATAGRAM || !room.guests.contains_key(&id) {
            return;
        }
        let mut datagram = Vec::with_capacity(packet.len() + 4);
        datagram.extend_from_slice(&id.to_be_bytes());
        datagram.extend_from_slice(&packet);
        (&room.host, datagram.into())
    };
    let bytes = datagram.len() + 48;
    if datagram.len() <= MAX_DATAGRAM + 4
        && room.budget.take(bytes, ROOM_BYTES_PER_SECOND, ROOM_BURST)
        && relay
            .budget
            .lock()
            .unwrap()
            .take(bytes, BYTES_PER_SECOND, BURST)
    {
        let _ = target.try_send(Message::Binary(datagram));
    }
}
