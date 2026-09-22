use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::Sha256;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const COMPANION_BRIDGE_ENDPOINT: &str = "comptrol+bridge://local";
pub const BRIDGE_PROTOCOL_VERSION: &str = "comptrol.browser.bridge/0.1.0";
pub const DEFAULT_HEALTH_MAX_AGE: Duration = Duration::from_secs(15);
const BRIDGE_TOKEN_FILE: &str = "browser-bridge.token";
const COMMAND_CAPACITY: i64 = 256;
const COMPLETED_RETENTION_MS: i64 = 10 * 60 * 1000;
const EVENT_RETENTION_MS: i64 = 10 * 60 * 1000;

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeCommand {
    pub request_id: String,
    pub command_type: String,
    pub payload: Value,
    pub created_at: i64,
    pub attempts: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeHealth {
    pub active: bool,
    pub last_heartbeat_ms: Option<i64>,
    pub target_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeEvent {
    pub id: i64,
    pub event_type: String,
    pub payload: Value,
    pub created_at_ms: i64,
}

pub struct BridgeStore {
    connection: Connection,
}

pub fn ensure_auth_token(state_dir: &Path) -> io::Result<String> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(BRIDGE_TOKEN_FILE);
    if let Ok(token) = fs::read_to_string(&path) {
        let token = token.trim().to_owned();
        if token.len() >= 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(token);
        }
    }

    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("generate browser bridge token: {error}")))?;
    let token = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(&path) {
            Ok(mut file) => {
                writeln!(file, "{token}")?;
                file.sync_all()?;
                return Ok(token);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return auth_token(state_dir)?.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "browser bridge token is invalid")
                });
            }
            Err(error) => return Err(error),
        }
    }

    #[cfg(not(unix))]
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&path) {
            Ok(mut file) => {
                writeln!(file, "{token}")?;
                file.sync_all()?;
                return Ok(token);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return auth_token(state_dir)?.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "browser bridge token is invalid")
                });
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn auth_token(state_dir: &Path) -> io::Result<Option<String>> {
    let path = state_dir.join(BRIDGE_TOKEN_FILE);
    match fs::read_to_string(path) {
        Ok(token) => {
            let token = token.trim().to_owned();
            if token.len() >= 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                Ok(Some(token))
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "browser bridge token is invalid",
                ))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn challenge_proof(state_dir: &Path, nonce: &str) -> io::Result<String> {
    if nonce.len() < 32
        || nonce.len() > 256
        || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "browser bridge challenge nonce must be 32-256 hexadecimal characters",
        ));
    }
    let token = ensure_auth_token(state_dir)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes())
        .map_err(|error| io::Error::other(format!("initialize browser bridge HMAC: {error}")))?;
    mac.update(BRIDGE_PROTOCOL_VERSION.as_bytes());
    mac.update(b"\0");
    mac.update(nonce.as_bytes());
    let digest = mac.finalize().into_bytes();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl BridgeStore {
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(state_dir)?;
        let connection = Connection::open(state_dir.join("browser-bridge.sqlite3"))
            .map_err(|error| sqlite_error("open browser bridge database", error))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;
                 CREATE TABLE IF NOT EXISTS bridge_commands (
                   request_id TEXT PRIMARY KEY,
                   command_type TEXT NOT NULL,
                   payload TEXT NOT NULL,
                   state TEXT NOT NULL CHECK(state IN ('pending','inflight','completed')),
                   created_at_ms INTEGER NOT NULL,
                   leased_until_ms INTEGER,
                   attempts INTEGER NOT NULL DEFAULT 0,
                   result TEXT,
                   completed_at_ms INTEGER
                 );
                 CREATE INDEX IF NOT EXISTS bridge_commands_state_created
                   ON bridge_commands(state, created_at_ms);
                 CREATE TABLE IF NOT EXISTS bridge_targets (
                   target_id TEXT PRIMARY KEY,
                   payload TEXT NOT NULL,
                   updated_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS bridge_meta (
                   key TEXT PRIMARY KEY,
                   value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS bridge_events (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   event_type TEXT NOT NULL,
                   payload TEXT NOT NULL,
                   created_at_ms INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS bridge_events_created
                   ON bridge_events(created_at_ms);",
            )
            .map_err(|error| sqlite_error("initialize browser bridge database", error))?;
        Ok(Self { connection })
    }

    pub fn submit(&mut self, command_type: &str, payload: Value) -> io::Result<String> {
        self.cleanup()?;
        let active: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM bridge_commands WHERE state IN ('pending','inflight')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| sqlite_error("count browser bridge commands", error))?;
        if active >= COMMAND_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "browser bridge command queue is full",
            ));
        }
        let request_id = format!(
            "br_{}_{}_{}",
            now_ms(),
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let payload = serde_json::to_string(&payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.connection
            .execute(
                "INSERT INTO bridge_commands
                 (request_id, command_type, payload, state, created_at_ms, attempts)
                 VALUES (?1, ?2, ?3, 'pending', ?4, 0)",
                params![request_id, command_type, payload, now_ms()],
            )
            .map_err(|error| sqlite_error("enqueue browser bridge command", error))?;
        Ok(request_id)
    }

    pub fn lease_pending(
        &mut self,
        limit: usize,
        lease_duration: Duration,
    ) -> io::Result<Vec<BridgeCommand>> {
        self.cleanup()?;
        let now = now_ms();
        let lease_until = now.saturating_add(duration_ms(lease_duration));
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin browser bridge lease", error))?;
        let rows = {
            let mut statement = transaction
                .prepare(
                    "SELECT request_id, command_type, payload, created_at_ms, attempts
                     FROM bridge_commands
                     WHERE state = 'pending'
                     ORDER BY created_at_ms ASC
                     LIMIT ?1",
                )
                .map_err(|error| sqlite_error("prepare browser bridge lease", error))?;
            statement
                .query_map(params![limit as i64], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, u32>(4)?,
                    ))
                })
                .map_err(|error| sqlite_error("query browser bridge lease", error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| sqlite_error("read browser bridge lease", error))?
        };

        for (request_id, _, _, _, _) in &rows {
            transaction
                .execute(
                    "UPDATE bridge_commands
                     SET state = 'inflight', leased_until_ms = ?2, attempts = attempts + 1
                     WHERE request_id = ?1 AND state = 'pending'",
                    params![request_id, lease_until],
                )
                .map_err(|error| sqlite_error("lease browser bridge command", error))?;
        }
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit browser bridge lease", error))?;

        rows.into_iter()
            .map(
                |(request_id, command_type, payload, created_at, attempts)| {
                    let payload = serde_json::from_str(&payload)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    Ok(BridgeCommand {
                        request_id,
                        command_type,
                        payload,
                        created_at,
                        attempts: attempts.saturating_add(1),
                    })
                },
            )
            .collect()
    }

    pub fn store_result(&mut self, request_id: &str, result: Value) -> io::Result<bool> {
        let encoded = serde_json::to_string(&result)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let changed = self
            .connection
            .execute(
                "UPDATE bridge_commands
                 SET state = 'completed', result = ?2, completed_at_ms = ?3, leased_until_ms = NULL
                 WHERE request_id = ?1",
                params![request_id, encoded, now_ms()],
            )
            .map_err(|error| sqlite_error("store browser bridge result", error))?;
        Ok(changed > 0)
    }

    pub fn result(&self, request_id: &str) -> io::Result<Option<Value>> {
        let encoded = self
            .connection
            .query_row(
                "SELECT result FROM bridge_commands
                 WHERE request_id = ?1 AND state = 'completed'",
                params![request_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|error| sqlite_error("read browser bridge result", error))?
            .flatten();
        encoded
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .transpose()
    }

    pub fn wait_result(&self, request_id: &str, timeout: Duration) -> io::Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(result) = self.result(request_id)? {
                return Ok(result);
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("browser bridge command {request_id} timed out"),
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn store_targets(&mut self, targets: &[Value]) -> io::Result<usize> {
        let timestamp = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin browser bridge target update", error))?;
        transaction
            .execute("DELETE FROM bridge_targets", [])
            .map_err(|error| sqlite_error("clear browser bridge targets", error))?;
        let mut stored = 0usize;
        for target in targets {
            let Some(target_id) = target
                .get("id")
                .or_else(|| target.get("targetId"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let payload = serde_json::to_string(target)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            transaction
                .execute(
                    "INSERT INTO bridge_targets(target_id, payload, updated_at_ms)
                     VALUES (?1, ?2, ?3)",
                    params![target_id, payload, timestamp],
                )
                .map_err(|error| sqlite_error("store browser bridge target", error))?;
            stored += 1;
        }
        transaction
            .execute(
                "INSERT INTO bridge_meta(key, value) VALUES ('last_targets_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![timestamp.to_string()],
            )
            .map_err(|error| sqlite_error("record browser bridge target time", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit browser bridge targets", error))?;
        Ok(stored)
    }

    pub fn targets(&self) -> io::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare("SELECT payload FROM bridge_targets ORDER BY target_id")
            .map_err(|error| sqlite_error("prepare browser bridge targets", error))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| sqlite_error("query browser bridge targets", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("read browser bridge targets", error))?;
        rows.into_iter()
            .map(|payload| {
                serde_json::from_str(&payload)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .collect()
    }

    pub fn record_heartbeat(&mut self, protocol: Option<&str>) -> io::Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin browser bridge heartbeat", error))?;
        transaction
            .execute(
                "INSERT INTO bridge_meta(key, value) VALUES ('last_heartbeat_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![now_ms().to_string()],
            )
            .map_err(|error| sqlite_error("record browser bridge heartbeat", error))?;
        if let Some(protocol) = protocol {
            transaction
                .execute(
                    "INSERT INTO bridge_meta(key, value) VALUES ('protocol', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![protocol],
                )
                .map_err(|error| sqlite_error("record browser bridge protocol", error))?;
        }
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit browser bridge heartbeat", error))
    }

    pub fn health(&self, max_age: Duration) -> io::Result<BridgeHealth> {
        let last_heartbeat_ms = self
            .connection
            .query_row(
                "SELECT value FROM bridge_meta WHERE key = 'last_heartbeat_ms'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| sqlite_error("read browser bridge heartbeat", error))?
            .and_then(|value| value.parse::<i64>().ok());
        let target_count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM bridge_targets", [], |row| row.get(0))
            .map_err(|error| sqlite_error("count browser bridge targets", error))?;
        let max_age_ms = duration_ms(max_age);
        let active = last_heartbeat_ms
            .map(|heartbeat| now_ms().saturating_sub(heartbeat) <= max_age_ms)
            .unwrap_or(false);
        Ok(BridgeHealth {
            active,
            last_heartbeat_ms,
            target_count: target_count.max(0) as usize,
        })
    }

    pub fn append_event(&mut self, event_type: &str, payload: Value) -> io::Result<i64> {
        let encoded = serde_json::to_string(&payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.connection
            .execute(
                "INSERT INTO bridge_events(event_type, payload, created_at_ms)
                 VALUES (?1, ?2, ?3)",
                params![event_type, encoded, now_ms()],
            )
            .map_err(|error| sqlite_error("store browser bridge event", error))?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn events_since(&self, id: i64, event_type: Option<&str>) -> io::Result<Vec<BridgeEvent>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, event_type, payload, created_at_ms
                 FROM bridge_events
                 WHERE id > ?1 AND (?2 IS NULL OR event_type = ?2)
                 ORDER BY id ASC LIMIT 256",
            )
            .map_err(|error| sqlite_error("prepare browser bridge events", error))?;
        let rows = statement
            .query_map(params![id, event_type], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|error| sqlite_error("query browser bridge events", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("read browser bridge events", error))?;
        rows.into_iter()
            .map(|(id, event_type, payload, created_at_ms)| {
                let payload = serde_json::from_str(&payload)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                Ok(BridgeEvent {
                    id,
                    event_type,
                    payload,
                    created_at_ms,
                })
            })
            .collect()
    }

    pub fn cleanup(&mut self) -> io::Result<()> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin browser bridge cleanup", error))?;
        transaction
            .execute(
                "UPDATE bridge_commands
                 SET state = 'pending', leased_until_ms = NULL
                 WHERE state = 'inflight' AND leased_until_ms IS NOT NULL AND leased_until_ms <= ?1",
                params![now],
            )
            .map_err(|error| sqlite_error("requeue expired browser bridge leases", error))?;
        transaction
            .execute(
                "DELETE FROM bridge_commands
                 WHERE state = 'completed' AND completed_at_ms IS NOT NULL AND completed_at_ms < ?1",
                params![now.saturating_sub(COMPLETED_RETENTION_MS)],
            )
            .map_err(|error| sqlite_error("prune browser bridge results", error))?;
        transaction
            .execute(
                "DELETE FROM bridge_events WHERE created_at_ms < ?1",
                params![now.saturating_sub(EVENT_RETENTION_MS)],
            )
            .map_err(|error| sqlite_error("prune browser bridge events", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit browser bridge cleanup", error))
    }
}

pub fn bridge_is_active() -> bool {
    BridgeStore::open(&crate::default_state_dir())
        .and_then(|store| store.health(DEFAULT_HEALTH_MAX_AGE))
        .map(|health| health.active)
        .unwrap_or(false)
}

fn sqlite_error(context: &str, error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("{context}: {error}"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn duration_ms(duration: Duration) -> i64 {
    duration.as_millis().min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "comptrol-bridge-{name}-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn lease_expiry_requeues_command() {
        let state = temp_state("lease");
        let mut store = BridgeStore::open(&state).expect("store");
        let request_id = store.submit("test", serde_json::json!({"a": 1})).expect("submit");
        let first = store
            .lease_pending(8, Duration::from_millis(1))
            .expect("lease");
        assert_eq!(first.len(), 1);
        thread::sleep(Duration::from_millis(3));
        let second = store
            .lease_pending(8, Duration::from_secs(1))
            .expect("re-lease");
        assert_eq!(second[0].request_id, request_id);
        assert!(second[0].attempts >= 2);
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn results_are_repeatable_until_cleanup() {
        let state = temp_state("result");
        let mut store = BridgeStore::open(&state).expect("store");
        let request_id = store.submit("test", Value::Null).expect("submit");
        assert!(
            store
                .store_result(&request_id, serde_json::json!({"ok": true}))
                .expect("result")
        );
        assert_eq!(
            store.result(&request_id).expect("read"),
            Some(serde_json::json!({"ok": true}))
        );
        assert_eq!(
            store.result(&request_id).expect("read twice"),
            Some(serde_json::json!({"ok": true}))
        );
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn heartbeat_controls_health_independently_of_targets() {
        let state = temp_state("health");
        let mut store = BridgeStore::open(&state).expect("store");
        assert!(!store.health(Duration::from_secs(1)).expect("health").active);
        store.record_heartbeat(Some("test")).expect("heartbeat");
        let health = store.health(Duration::from_secs(1)).expect("health");
        assert!(health.active);
        assert_eq!(health.target_count, 0);
        let _ = fs::remove_dir_all(state);
    }
}
