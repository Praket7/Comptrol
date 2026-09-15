use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Event {
    pub sequence: u64,
    pub kind: String,
    pub payload: Value,
    pub emitted_at_ms: u128,
}

#[derive(Debug)]
struct EventState {
    capacity: usize,
    next_sequence: u64,
    events: VecDeque<Event>,
}

#[derive(Debug)]
pub struct EventBus {
    state: Mutex<EventState>,
    notification: Condvar,
    subscribers: Mutex<Vec<mpsc::Sender<Event>>>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(EventState {
                capacity: capacity.max(1),
                next_sequence: 0,
                events: VecDeque::new(),
            }),
            notification: Condvar::new(),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    pub fn emit(&self, kind: &str, payload: Value) -> Event {
        let mut state = self.state.lock().expect("event bus state lock");
        if let Some(previous) = state
            .events
            .back()
            .filter(|event| event.kind == kind && event.payload == payload)
        {
            return previous.clone();
        }
        state.next_sequence += 1;
        let event = Event {
            sequence: state.next_sequence,
            kind: kind.to_owned(),
            payload,
            emitted_at_ms: now_ms(),
        };
        state.events.push_back(event.clone());
        while state.events.len() > state.capacity {
            state.events.pop_front();
        }
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
        }
        self.notification.notify_all();
        event
    }

    /// Subscribe without coupling producers to a consumer's processing speed.
    /// The channel is unbounded and delivery is best-effort; replay remains
    /// available through `snapshot_since` for reconnecting consumers.
    pub fn subscribe(&self) -> mpsc::Receiver<Event> {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.push(sender);
        }
        receiver
    }

    pub fn snapshot_since(&self, sequence: u64, kind: Option<&str>) -> Vec<Event> {
        self.since(sequence, kind)
    }

    pub fn latest_sequence(&self) -> u64 {
        self.state
            .lock()
            .map(|state| state.next_sequence)
            .unwrap_or_default()
    }

    pub fn since(&self, sequence: u64, kind: Option<&str>) -> Vec<Event> {
        let state = self.state.lock().expect("event bus state lock");
        state
            .events
            .iter()
            .filter(|event| event.sequence > sequence)
            .filter(|event| kind.is_none_or(|expected| expected == event.kind))
            .cloned()
            .collect()
    }

    pub fn wait_for(&self, sequence: u64, kind: Option<&str>, timeout: Duration) -> Option<Event> {
        let mut state = self.state.lock().ok()?;
        if let Some(event) = state
            .events
            .iter()
            .find(|event| {
                event.sequence > sequence && kind.is_none_or(|expected| expected == event.kind)
            })
            .cloned()
        {
            return Some(event);
        }
        if timeout.is_zero() {
            return None;
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(event) = state
                .events
                .iter()
                .find(|event| {
                    event.sequence > sequence && kind.is_none_or(|expected| expected == event.kind)
                })
                .cloned()
            {
                return Some(event);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let (next, result) = self.notification.wait_timeout(state, remaining).ok()?;
            state = next;
            if result.timed_out() {
                return state
                    .events
                    .iter()
                    .find(|event| {
                        event.sequence > sequence
                            && kind.is_none_or(|expected| expected == event.kind)
                    })
                    .cloned();
            }
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

/// SQLite-backed event replay for MCP clients that reconnect after a process
/// restart. Consumer cursors are explicit state, separate from protocol
/// sessions, and can therefore be used by the current stateless transport.
#[derive(Debug)]
pub struct DurableEventHub {
    path: PathBuf,
    capacity: u64,
}

impl DurableEventHub {
    pub fn open(path: impl AsRef<Path>, capacity: u64) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let connection = Connection::open(&path).map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;
                 CREATE TABLE IF NOT EXISTS event_log (
                   sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                   kind TEXT NOT NULL,
                   payload_json TEXT NOT NULL,
                   emitted_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS event_cursors (
                   consumer_id TEXT PRIMARY KEY,
                   sequence INTEGER NOT NULL,
                   updated_at_ms INTEGER NOT NULL
                 );",
            )
            .map_err(|error| error.to_string())?;
        Ok(Self {
            path,
            capacity: capacity.max(1),
        })
    }

    fn connection(&self) -> Result<Connection, String> {
        Connection::open(&self.path).map_err(|error| error.to_string())
    }

    pub fn emit(&self, kind: &str, payload: Value) -> Result<Event, String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let emitted_at_ms = now_ms() as i64;
        let payload_json = serde_json::to_string(&payload).map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO event_log(kind, payload_json, emitted_at_ms) VALUES (?1, ?2, ?3)",
                params![kind, payload_json, emitted_at_ms],
            )
            .map_err(|error| error.to_string())?;
        let sequence = transaction.last_insert_rowid() as u64;
        let cutoff = sequence.saturating_sub(self.capacity) as i64;
        transaction
            .execute(
                "DELETE FROM event_log WHERE sequence <= ?1",
                params![cutoff],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(Event {
            sequence,
            kind: kind.to_owned(),
            payload,
            emitted_at_ms: emitted_at_ms as u128,
        })
    }

    pub fn snapshot_since(&self, sequence: u64, kind: Option<&str>) -> Result<Vec<Event>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT sequence, kind, payload_json, emitted_at_ms
                 FROM event_log WHERE sequence > ?1 AND (?2 IS NULL OR kind = ?2)
                 ORDER BY sequence",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![sequence as i64, kind], |row| {
                let payload_json: String = row.get(2)?;
                Ok(Event {
                    sequence: row.get::<_, i64>(0)? as u64,
                    kind: row.get(1)?,
                    payload: serde_json::from_str(&payload_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            payload_json.len(),
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    emitted_at_ms: row.get::<_, i64>(3)? as u128,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.map(|row| row.map_err(|error| error.to_string()))
            .collect()
    }

    pub fn cursor(&self, consumer_id: &str) -> Result<u64, String> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT sequence FROM event_cursors WHERE consumer_id = ?1",
                params![consumer_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| error.to_string())
            .map(|value| value.unwrap_or_default() as u64)
    }

    pub fn acknowledge(&self, consumer_id: &str, sequence: u64) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO event_cursors(consumer_id, sequence, updated_at_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(consumer_id) DO UPDATE SET
                   sequence = excluded.sequence,
                   updated_at_ms = excluded.updated_at_ms
                 WHERE excluded.sequence >= event_cursors.sequence",
                params![consumer_id, sequence as i64, now_ms() as i64],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn subscription_is_non_blocking_and_replay_is_bounded() {
        let bus = EventBus::new(2);
        let receiver = bus.subscribe();
        bus.emit("one", json!({"n": 1}));
        bus.emit("two", json!({"n": 2}));
        bus.emit("three", json!({"n": 3}));
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_millis(20))
                .unwrap()
                .kind,
            "one"
        );
        assert_eq!(bus.latest_sequence(), 3);
        assert_eq!(bus.snapshot_since(0, None).len(), 2);
        assert_eq!(bus.snapshot_since(1, Some("three")).len(), 1);
    }

    #[test]
    fn durable_event_hub_replays_and_monotonically_acknowledges_cursors() {
        let path = std::env::temp_dir().join(format!(
            "comptrol-events-{}-{}.db",
            std::process::id(),
            now_ms()
        ));
        let hub = DurableEventHub::open(&path, 4).unwrap();
        let first = hub.emit("browser", json!({"sequence": 17})).unwrap();
        hub.emit("task", json!({"status": "running"})).unwrap();
        assert_eq!(hub.cursor("client-a").unwrap(), 0);
        hub.acknowledge("client-a", first.sequence).unwrap();
        hub.acknowledge("client-a", 0).unwrap();
        assert_eq!(hub.cursor("client-a").unwrap(), first.sequence);
        assert_eq!(hub.snapshot_since(first.sequence, None).unwrap().len(), 1);
        drop(hub);
        let reopened = DurableEventHub::open(&path, 4).unwrap();
        assert_eq!(reopened.cursor("client-a").unwrap(), first.sequence);
        std::fs::remove_file(path).unwrap();
    }
}
