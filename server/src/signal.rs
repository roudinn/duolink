use std::{
    collections::HashMap,
    convert::Infallible,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as RoutePath, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, broadcast};

const BODY_LIMIT: usize = 2048;
// Each client also uses a pair of sockets in the VPS's shared nginx.
const SUBSCRIBERS: usize = 128;
const SESSION_TTL: i64 = 40;
const MAIL_TTL: i64 = 12 * 3600;
const DIRECTORY_TTL: i64 = 7 * 24 * 3600;
const HTTP_WORK: usize = 32;

#[derive(Clone, Copy)]
enum Topic {
    Session,
    Network,
    Inbox,
    Directory,
}

impl Topic {
    fn parse(topic: &str) -> Option<Self> {
        let suffix = topic.strip_prefix("duolink-")?;
        let (kind, key) = match suffix.split_once('-') {
            Some(("n", key)) => (Self::Network, key),
            Some(("i", key)) => (Self::Inbox, key),
            Some(("d", key)) => (Self::Directory, key),
            None => (Self::Session, suffix),
            _ => return None,
        };
        (key.len() == 32
            && key
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
        .then_some(kind)
    }

    fn retention(self) -> (i64, i64) {
        match self {
            Self::Session => (SESSION_TTL, 24),
            Self::Network => (MAIL_TTL, 1),
            Self::Inbox => (MAIL_TTL, 128),
            Self::Directory => (DIRECTORY_TTL, 8),
        }
    }
}

#[derive(Debug, Serialize)]
struct Message {
    event: &'static str,
    id: i64,
    topic: String,
    time: i64,
    message: String,
}

impl Message {
    fn line(&self) -> Bytes {
        let mut line = serde_json::to_vec(self).expect("serializing a message");
        line.push(b'\n');
        line.into()
    }
}

struct Inner {
    db: Mutex<Connection>,
    listeners: Mutex<HashMap<String, broadcast::Sender<Arc<Message>>>>,
    permits: Arc<Semaphore>,
    requests: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct Signal(Arc<Inner>);

impl Signal {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA max_page_count = 65536;
             PRAGMA journal_size_limit = 4194304;
             CREATE TABLE IF NOT EXISTS message (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 topic TEXT NOT NULL,
                 time INTEGER NOT NULL,
                 expires INTEGER NOT NULL,
                 body TEXT NOT NULL,
                 UNIQUE(topic, body)
             );
             CREATE INDEX IF NOT EXISTS message_by_topic ON message(topic, id);
             CREATE INDEX IF NOT EXISTS message_expiry ON message(expires);",
        )?;
        Ok(Self(Arc::new(Inner {
            db: Mutex::new(db),
            listeners: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(SUBSCRIBERS)),
            requests: Arc::new(Semaphore::new(HTTP_WORK)),
        })))
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/healthz", get(|| async { "ok\n" }))
            .route("/signal/{topic}", axum::routing::post(publish))
            .route("/signal/{topic}/json", get(read))
            .layer(DefaultBodyLimit::max(BODY_LIMIT))
            .with_state(self)
    }

    fn listen(&self, topic: &str) -> broadcast::Receiver<Arc<Message>> {
        self.0
            .listeners
            .lock()
            .unwrap()
            .entry(topic.to_owned())
            .or_insert_with(|| broadcast::channel(32).0)
            .subscribe()
    }

    async fn history(&self, topic: String, since: i64) -> Result<Vec<Message>> {
        let request = self.0.requests.clone().try_acquire_owned()?;
        let signal = self.clone();
        tokio::task::spawn_blocking(move || {
            let _request = request;
            let db = signal.0.db.lock().unwrap();
            let mut rows = db.prepare_cached(
                "SELECT id, time, body FROM message
                 WHERE topic = ?1 AND time >= ?2 AND expires > ?3 ORDER BY id",
            )?;
            let messages = rows
                .query_map(params![topic, since, crate::unix_time()], |row| {
                    Ok(Message {
                        event: "message",
                        id: row.get(0)?,
                        topic: topic.clone(),
                        time: row.get(1)?,
                        message: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(messages)
        })
        .await?
    }

    async fn store(&self, kind: Topic, topic: String, body: String) -> Result<()> {
        let request = self.0.requests.clone().try_acquire_owned()?;
        let signal = self.clone();
        tokio::task::spawn_blocking(move || {
            let _request = request;
            let (ttl, limit) = kind.retention();
            let mut db = signal.0.db.lock().unwrap();
            let transaction = db.transaction()?;
            let now = crate::unix_time();
            let expires = now + ttl;
            // A retried envelope keeps its original ID and time. Directory and
            // host network records are refreshed because they describe current state.
            let sql = if matches!(kind, Topic::Directory | Topic::Network) {
                "INSERT INTO message(topic, time, expires, body) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(topic, body) DO UPDATE SET time = excluded.time, expires = excluded.expires
                 RETURNING id"
            } else {
                "INSERT INTO message(topic, time, expires, body) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(topic, body) DO NOTHING RETURNING id"
            };
            let mut statement = transaction.prepare_cached(sql)?;
            let id = statement.query(params![topic, now, expires, body])?
                .next()?.map(|row| row.get::<_, i64>(0)).transpose()?;
            drop(statement);
            if id.is_some() {
                transaction.execute(
                    "DELETE FROM message WHERE topic = ?1 AND id IN (
                         SELECT id FROM message WHERE topic = ?1 ORDER BY time DESC, id DESC LIMIT -1 OFFSET ?2
                     )",
                    params![topic, limit],
                )?;
            }
            transaction.commit()?;
            if let Some(id) = id
                && let Some(listener) = signal.0.listeners.lock().unwrap().get(&topic)
            {
                let _ = listener.send(Arc::new(Message { event: "message", id, topic, time: now, message: body }));
            }
            Ok(())
        }).await?
    }

    pub async fn cleanup(&self) -> Result<()> {
        let signal = self.clone();
        tokio::task::spawn_blocking(move || {
            signal.0.db.lock().unwrap().execute(
                "DELETE FROM message WHERE expires <= ?1",
                [crate::unix_time()],
            )?;
            signal
                .0
                .listeners
                .lock()
                .unwrap()
                .retain(|_, listener| listener.receiver_count() != 0);
            Ok(())
        })
        .await?
    }
}

async fn publish(
    State(signal): State<Signal>,
    RoutePath(topic): RoutePath<String>,
    body: Bytes,
) -> StatusCode {
    let Some(kind) = Topic::parse(&topic) else {
        return StatusCode::NOT_FOUND;
    };
    let Ok(body) = std::str::from_utf8(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    if body.is_empty()
        || !body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return StatusCode::BAD_REQUEST;
    }
    match signal.store(kind, topic, body.to_owned()).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(error) => {
            eprintln!("signal store write failed: {error}");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

#[derive(Deserialize)]
struct ReadOptions {
    poll: Option<u8>,
    since: Option<String>,
}

async fn read(
    State(signal): State<Signal>,
    RoutePath(topic): RoutePath<String>,
    Query(options): Query<ReadOptions>,
) -> Result<Response, StatusCode> {
    let kind = Topic::parse(&topic).ok_or(StatusCode::NOT_FOUND)?;
    let poll = options.poll == Some(1);
    let floor = crate::unix_time().saturating_sub(kind.retention().0);
    let since = match options.since.as_deref() {
        None | Some("all") => floor,
        Some(value) => value
            .parse::<i64>()
            .map_err(|_| StatusCode::BAD_REQUEST)?
            .max(floor),
    };
    let headers = [
        (header::CONTENT_TYPE, "application/x-ndjson"),
        (header::CACHE_CONTROL, "no-store"),
    ];
    if poll {
        let history = signal
            .history(topic, since)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let bytes: Vec<u8> = history.iter().flat_map(|item| item.line()).collect();
        return Ok((headers, bytes).into_response());
    }
    let permit = signal
        .0
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mut receiver = signal.listen(&topic);
    let history = signal
        .history(topic, since)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let body = async_stream::stream! {
        let _permit = permit;
        yield Ok::<_, Infallible>(Bytes::from_static(b"\n"));
        let mut last = 0;
        for message in history {
            last = message.id;
            yield Ok(message.line());
        }
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(15), Duration::from_secs(15),
        );
        loop {
            tokio::select! {
                next = receiver.recv() => match next {
                    Ok(message) if message.id > last => {
                        last = message.id;
                        yield Ok(message.line());
                    }
                    Ok(_) => {}
                    // The client reconnects using its last timestamp.
                    Err(_) => break,
                },
                _ = heartbeat.tick() => yield Ok(Bytes::from_static(b"\n")),
            }
        }
    };
    Ok((headers, Body::from_stream(body)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mailbox_replays_after_restart_without_duplicating_retries() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("signal.db");
        let topic = "duolink-i-0123456789abcdef0123456789abcdef".to_string();
        let signal = Signal::open(&db).unwrap();
        let mut listener = signal.listen(&topic);
        let kind = Topic::parse(&topic).unwrap();
        signal
            .store(kind, topic.clone(), "first".into())
            .await
            .unwrap();
        signal
            .store(kind, topic.clone(), "first".into())
            .await
            .unwrap();
        signal
            .store(kind, topic.clone(), "second".into())
            .await
            .unwrap();
        assert_eq!(listener.try_recv().unwrap().message, "first");
        assert_eq!(listener.try_recv().unwrap().message, "second");
        assert!(listener.try_recv().is_err());
        drop(signal);
        let signal = Signal::open(&db).unwrap();
        let messages = signal.history(topic, 0).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].id < messages[1].id);
    }

    #[tokio::test]
    async fn session_records_expire_and_topic_limits_apply() {
        let directory = tempfile::tempdir().unwrap();
        let signal = Signal::open(&directory.path().join("signal.db")).unwrap();
        let topic = "duolink-0123456789abcdef0123456789abcdef".to_string();
        for index in 0..30 {
            signal
                .store(Topic::Session, topic.clone(), format!("offer{index}"))
                .await
                .unwrap();
        }
        let history = signal.history(topic.clone(), 0).await.unwrap();
        assert_eq!(history.len(), 24);
        assert_eq!(history[0].message, "offer6");
        signal
            .0
            .db
            .lock()
            .unwrap()
            .execute("UPDATE message SET expires = 0", [])
            .unwrap();
        signal.cleanup().await.unwrap();
        assert!(signal.history(topic, 0).await.unwrap().is_empty());
        assert!(Topic::parse("unrelated").is_none());
    }
}
