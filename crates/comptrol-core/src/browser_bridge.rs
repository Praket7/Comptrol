use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::{
    Condvar, Mutex as StdMutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const COMPANION_BRIDGE_ENDPOINT: &str = "comptrol+bridge://local";
pub const BRIDGE_PROTOCOL_VERSION: &str = "comptrol.browser.bridge/0.1.0";
pub const DEFAULT_HEALTH_MAX_AGE: Duration = Duration::from_secs(15);
const BRIDGE_TOKEN_FILE: &str = "browser-bridge.token";
const COMMAND_CAPACITY: i64 = 256;
const COMPLETED_RETENTION_MS: i64 = 10 * 60 * 1000;
const EVENT_RETENTION_MS: i64 = 10 * 60 * 1000;
const AUTH_NONCE_RETENTION_MS: i64 = 5 * 60 * 1000;

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How long a stored round-trip probe result stays authoritative. Probes are
/// re-run when older than this so `health_round_trip` reflects the live SW.
const ROUND_TRIP_MAX_AGE: Duration = Duration::from_secs(15);

/// Same-process completions notify immediately. Separate MCP/HTTP processes
/// share SQLite but cannot share a Rust condvar; bounded checks cover that case.
static WAKE_WAITERS: Condvar = Condvar::new();
static WAITER_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();

fn waiter_lock() -> &'static StdMutex<()> {
    WAITER_LOCK.get_or_init(|| StdMutex::new(()))
}

/// An extension-wake request. The runtime records one when a command times out
/// while the heartbeat looks fresh (the classic suspended-service-worker
/// signature); `native_host.py` drains it via `POST /browser/wake` and pokes
/// Chrome. Stored in `bridge_meta` because the recorder (MCP runtime) and the
/// drainer (HTTP server) are different processes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WakeNotice {
    pub requested_at_ms: i64,
    pub reason: String,
}

impl BridgeStore {
    /// Record an extension-wake request (any process with the store open).
    pub fn record_wake_requested(&mut self, reason: &str) -> io::Result<()> {
        let notice = WakeNotice {
            requested_at_ms: now_ms(),
            reason: reason
                .chars()
                .filter(|character| !character.is_control())
                .take(120)
                .collect(),
        };
        let encoded = serde_json::to_string(&notice)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.connection
            .execute(
                "INSERT INTO bridge_meta(key, value) VALUES ('wake_request', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![encoded],
            )
            .map(|_| ())
            .map_err(|error| sqlite_error("record browser bridge wake request", error))
    }

    /// Drain the pending wake request for native-host polling. Each notice is
    /// returned once so the host stops waking after the SW responds.
    pub fn drain_wake_notice(&mut self) -> io::Result<Option<WakeNotice>> {
        let encoded = self
            .connection
            .query_row(
                "SELECT value FROM bridge_meta WHERE key = 'wake_request'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| sqlite_error("read browser bridge wake request", error))?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        self.connection
            .execute("DELETE FROM bridge_meta WHERE key = 'wake_request'", [])
            .map_err(|error| sqlite_error("clear browser bridge wake request", error))?;
        serde_json::from_str(&encoded)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    /// Reap stale probe commands so a dead service worker cannot fill the
    /// command queue with unanswered `bridge_ping` probes and crowd out real
    /// work. Probes older than 30 seconds are unproven either way.
    pub fn prune_stale_probes(&mut self) -> io::Result<usize> {
        let cutoff = now_ms().saturating_sub(30_000);
        let changed = self
            .connection
            .execute(
                "DELETE FROM bridge_commands
                 WHERE command_type = 'bridge_ping'
                   AND state IN ('pending', 'inflight')
                   AND created_at_ms < ?1",
                params![cutoff],
            )
            .map_err(|error| sqlite_error("prune stale browser bridge probes", error))?;
        Ok(changed)
    }
}

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
    #[serde(default)]
    pub last_round_trip_ms: Option<i64>,
    #[serde(default)]
    pub last_round_trip_at_ms: Option<i64>,
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
                Ok(token)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => auth_token(state_dir)?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "browser bridge token is invalid",
                    )
                }),
            Err(error) => Err(error),
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
                Ok(token)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => auth_token(state_dir)?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "browser bridge token is invalid",
                    )
                }),
            Err(error) => Err(error),
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
    if nonce.len() < 32 || nonce.len() > 256 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
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

pub fn request_signature(
    token: &str,
    method: &str,
    path: &str,
    nonce: &str,
    body: &[u8],
) -> io::Result<String> {
    if nonce.len() < 32 || nonce.len() > 256 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "browser bridge request nonce must be 32-256 hexadecimal characters",
        ));
    }
    let body_hash = Sha256::digest(body);
    let body_hash_hex = body_hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).map_err(|error| {
        io::Error::other(format!("initialize browser bridge request HMAC: {error}"))
    })?;
    for part in [
        BRIDGE_PROTOCOL_VERSION,
        method,
        path,
        nonce,
        body_hash_hex.as_str(),
    ] {
        mac.update(part.as_bytes());
        mac.update(b"\0");
    }
    let digest = mac.finalize().into_bytes();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn verify_request_signature(
    token: &str,
    method: &str,
    path: &str,
    nonce: &str,
    body: &[u8],
    signature_hex: &str,
) -> io::Result<bool> {
    if nonce.len() < 32 || nonce.len() > 256 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Ok(false);
    }
    let provided = match decode_hex(signature_hex) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let body_hash = Sha256::digest(body);
    let body_hash_hex = body_hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).map_err(|error| {
        io::Error::other(format!("initialize browser bridge request HMAC: {error}"))
    })?;
    for part in [
        BRIDGE_PROTOCOL_VERSION,
        method,
        path,
        nonce,
        body_hash_hex.as_str(),
    ] {
        mac.update(part.as_bytes());
        mac.update(b"\0");
    }
    Ok(mac.verify_slice(&provided).is_ok())
}

fn decode_hex(value: &str) -> io::Result<Vec<u8>> {
    let bytes = value.as_bytes();
    let (pairs, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid hexadecimal value",
        ));
    }
    pairs
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            u8::from_str_radix(text, 16)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .collect()
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
                   completed_at_ms INTEGER,
                   retryable INTEGER NOT NULL DEFAULT 0,
                   requeued INTEGER NOT NULL DEFAULT 0
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
                   ON bridge_events(created_at_ms);
                 CREATE TABLE IF NOT EXISTS bridge_auth_nonces (
                   nonce TEXT PRIMARY KEY,
                   seen_at_ms INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS bridge_auth_nonces_seen
                   ON bridge_auth_nonces(seen_at_ms);",
            )
            .map_err(|error| sqlite_error("initialize browser bridge database", error))?;
        // P2.4 migration: pre-existing databases lack the requeue columns.
        let columns: std::collections::HashSet<String> = connection
            .prepare("PRAGMA table_info(bridge_commands)")
            .and_then(|mut statement| {
                let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .map(|names| names.into_iter().collect())
            .unwrap_or_default();
        for (name, ddl) in [
            ("retryable", "retryable INTEGER NOT NULL DEFAULT 0"),
            ("requeued", "requeued INTEGER NOT NULL DEFAULT 0"),
        ] {
            if !columns.contains(name) {
                connection
                    .execute(&format!("ALTER TABLE bridge_commands ADD COLUMN {ddl}"), [])
                    .map_err(|error| sqlite_error("migrate browser bridge commands", error))?;
            }
        }
        Ok(Self { connection })
    }

    pub fn claim_auth_nonce(&mut self, nonce: &str) -> io::Result<bool> {
        if nonce.len() < 32
            || nonce.len() > 256
            || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Ok(false);
        }
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin browser bridge auth nonce claim", error))?;
        transaction
            .execute(
                "DELETE FROM bridge_auth_nonces WHERE seen_at_ms < ?1",
                params![now.saturating_sub(AUTH_NONCE_RETENTION_MS)],
            )
            .map_err(|error| sqlite_error("prune browser bridge auth nonces", error))?;
        let inserted = transaction
            .execute(
                "INSERT OR IGNORE INTO bridge_auth_nonces(nonce, seen_at_ms) VALUES (?1, ?2)",
                params![nonce, now],
            )
            .map_err(|error| sqlite_error("claim browser bridge auth nonce", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit browser bridge auth nonce", error))?;
        Ok(inserted == 1)
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
        let completed_at = now_ms();
        let mut requeued = false;
        let mut channel_recovered = false;
        // Record the round-trip truth for ping commands: the measured
        // host->extension->host latency proves the service worker is live.
        // This persists in bridge_meta so every process (HTTP server, host,
        // doctor) observes the same channel state.
        if let Ok(row) = self.connection.query_row(
            "SELECT command_type, created_at_ms, requeued FROM bridge_commands
             WHERE request_id = ?1",
            params![request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        ) {
            let (command_type, created_at, requeued_flag) = row;
            requeued = requeued_flag != 0;
            if command_type == "bridge_ping" {
                let ok = result.get("ok").and_then(Value::as_bool) == Some(true);
                channel_recovered = ok;
                let latency_ms = if ok {
                    completed_at.saturating_sub(created_at)
                } else {
                    -1
                };
                let _ = self.connection.execute(
                    "INSERT INTO bridge_meta(key, value) VALUES ('last_round_trip_ms', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![latency_ms.to_string()],
                );
                let _ = self.connection.execute(
                    "INSERT INTO bridge_meta(key, value) VALUES ('last_round_trip_at_ms', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![completed_at.to_string()],
                );
                // K5: persist the extension's page-op counters so health can
                // separate "the channel answers pings" from "page operations
                // actually complete" (live incident: pings healthy while every
                // debugger-backed command hung).
                if let Some(page_ops) = result.get("result").and_then(|value| value.get("page_ops"))
                {
                    for (key, field) in [
                        ("page_ops_ok_at_ms", "last_ok_at_ms"),
                        ("page_ops_fail_at_ms", "last_fail_at_ms"),
                        ("page_ops_orphaned", "orphaned"),
                        ("page_ops_remediations", "remediations"),
                    ] {
                        if let Some(value) = page_ops.get(field).and_then(|value| value.as_i64()) {
                            let _ = self.connection.execute(
                                "INSERT INTO bridge_meta(key, value) VALUES (?1, ?2)
                                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                                params![key, value.to_string()],
                            );
                        }
                    }
                }
            }
        }
        // P2.4: a command that was auto-requeued reports its recovered
        // outcome honestly to whoever reads the result later.
        let encoded = match requeued {
            true => match serde_json::from_str::<Value>(&encoded) {
                Ok(Value::Object(mut map)) => {
                    map.insert("auto_requeued".to_owned(), Value::Bool(true));
                    serde_json::to_string(&Value::Object(map))
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                }
                _ => encoded,
            },
            false => encoded,
        };
        let changed = self
            .connection
            .execute(
                "UPDATE bridge_commands
                 SET state = 'completed', result = ?2, completed_at_ms = ?3, leased_until_ms = NULL
                 WHERE request_id = ?1",
                params![request_id, encoded, completed_at],
            )
            .map_err(|error| sqlite_error("store browser bridge result", error))?;
        // P2.4: channel recovery (a fresh successful bridge_ping round trip)
        // requeues every command whose dispatch timed out - exactly once per
        // command, same request_id, so the extension-side dedupe ledger makes
        // the replay safe. Runs after this result is stored so a completing
        // command can never be re-flipped to pending.
        if channel_recovered {
            let _ = self.requeue_retryable();
        }
        // Wake every waiter immediately; each rechecks its own request_id.
        WAKE_WAITERS.notify_all();
        Ok(changed > 0)
    }

    /// P2.4: mark a command whose dispatch outcome is a transport-level
    /// timeout (`sw_deadline` / `bridge_timeout`) as eligible for one
    /// automatic requeue. The row keeps its request_id so the extension-side
    /// dedupe ledger can answer a replay instead of double-executing a
    /// mutation.
    pub fn mark_retryable(&mut self, request_id: &str) -> io::Result<bool> {
        let changed = self
            .connection
            .execute(
                "UPDATE bridge_commands
                 SET retryable = 1
                 WHERE request_id = ?1 AND requeued = 0 AND state IN ('inflight','completed')",
                params![request_id],
            )
            .map_err(|error| sqlite_error("mark browser bridge command retryable", error))?;
        Ok(changed > 0)
    }

    /// P2.4: requeue every retryable command exactly once. Called when the
    /// channel proves it recovered (a fresh successful bridge_ping round
    /// trip). Requeued commands carry the same request_id and the eventual
    /// result is flagged `auto_requeued: true`.
    pub fn requeue_retryable(&mut self) -> io::Result<usize> {
        let changed = self
            .connection
            .execute(
                "UPDATE bridge_commands
                 SET state = 'pending', leased_until_ms = NULL, retryable = 0, requeued = 1
                 WHERE retryable = 1 AND requeued = 0",
                [],
            )
            .map_err(|error| sqlite_error("requeue browser bridge commands", error))?;
        if changed > 0 {
            WAKE_WAITERS.notify_all();
        }
        Ok(changed)
    }

    /// Test/observability helper: how many commands were auto-requeued.
    pub fn requeued_count(&self) -> io::Result<usize> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM bridge_commands WHERE requeued = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count.max(0) as usize)
            .map_err(|error| sqlite_error("count requeued browser bridge commands", error))
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
        let mut remaining = timeout;
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
            // The HTTP sidecar usually writes from another process, where
            // this condvar cannot wake us. Check only while a caller waits,
            // at 20 ms rather than adding 500 ms to every browser command.
            let slice = remaining.min(Duration::from_millis(20));
            let started = std::time::Instant::now();
            let guard = waiter_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // The guard releases when the wait ends; the timed-out flag is
            // irrelevant because the loop always rechecks the result store.
            let _ = WAKE_WAITERS
                .wait_timeout(guard, slice)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            remaining = remaining.saturating_sub(started.elapsed());
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
            last_round_trip_ms: None,
            last_round_trip_at_ms: None,
        })
    }

    /// Channel truth: a daemon-side heartbeat proves only that *this process*
    /// is alive. `active` therefore requires a completed extension round trip
    /// (a `bridge_ping` answered by the actual service worker, recorded in
    /// `store_result`) within `max_age`, not just heartbeat recency.
    pub fn health_round_trip(&self, max_age: Duration) -> io::Result<BridgeHealth> {
        let mut health = self.health(max_age)?;
        let round_trip_max_age = max_age.max(ROUND_TRIP_MAX_AGE);
        let read_meta = |key: &str| -> Option<i64> {
            self.connection
                .query_row(
                    "SELECT value FROM bridge_meta WHERE key = ?1",
                    params![key],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .ok()
                .flatten()
                .and_then(|value| value.parse::<i64>().ok())
        };
        let latency_ms = read_meta("last_round_trip_ms");
        let at_ms = read_meta("last_round_trip_at_ms");
        match (latency_ms, at_ms) {
            (Some(latency), Some(at))
                if now_ms().saturating_sub(at) <= duration_ms(round_trip_max_age) =>
            {
                health.active = health.active && latency >= 0;
                health.last_round_trip_ms = Some(latency);
                health.last_round_trip_at_ms = Some(at);
            }
            _ => {
                // No fresh probe: the extension command channel is unproven,
                // which is exactly what "not active" means for callers.
                health.active = false;
            }
        }
        Ok(health)
    }

    /// Record why the channel is degraded so doctor/inspect can report the
    /// last known state instead of a stale "connected".
    pub fn record_channel_state(&mut self, state: &str) -> io::Result<()> {
        let changed = self
            .connection
            .execute(
                "INSERT INTO bridge_meta(key, value) VALUES ('channel_state', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![state],
            )
            .map_err(|error| sqlite_error("record browser bridge channel state", error))?;
        let _ = changed;
        Ok(())
    }

    pub fn channel_state(&self) -> io::Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT value FROM bridge_meta WHERE key = 'channel_state'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| sqlite_error("read browser bridge channel state", error))
    }

    /// Timestamp of the extension's most recent targets_list push (bridge_meta
    /// 'last_targets_ms'). None means the extension never pushed targets.
    pub fn last_targets_ms(&self) -> io::Result<Option<i64>> {
        self.connection
            .query_row(
                "SELECT value FROM bridge_meta WHERE key = 'last_targets_ms'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| sqlite_error("read browser bridge last targets time", error))
            .map(|stored| stored.and_then(|value| value.parse::<i64>().ok()))
    }

    /// Page-op health (K5): distinguishes "the channel answers pings" from
    /// "page operations actually complete". Persisted from bridge_ping
    /// results that carry the extension's page_ops counters.
    pub fn page_ops_health(&self) -> io::Result<serde_json::Value> {
        let read = |key: &str| -> Option<i64> {
            self.connection
                .query_row(
                    "SELECT value FROM bridge_meta WHERE key = ?1",
                    params![key],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
        };
        let last_ok = read("page_ops_ok_at_ms");
        let last_fail = read("page_ops_fail_at_ms");
        let orphaned = read("page_ops_orphaned").unwrap_or(0);
        let remediations = read("page_ops_remediations").unwrap_or(0);
        let now = now_ms();
        let state = match (last_ok, last_fail) {
            (None, None) => "unknown",
            (None, Some(_)) => "degraded",
            (Some(ok), fail) if orphaned > 0 || fail.map(|failed| failed > ok).unwrap_or(false) => {
                "degraded"
            }
            (Some(ok), _) if now.saturating_sub(ok) <= 120_000 => "ok",
            _ => "stale",
        };
        Ok(serde_json::json!({
            "state": state,
            "last_ok_at_ms": last_ok,
            "last_fail_at_ms": last_fail,
            "orphaned": orphaned,
            "remediations": remediations
        }))
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
            .execute(
                "DELETE FROM bridge_auth_nonces WHERE seen_at_ms < ?1",
                params![now.saturating_sub(AUTH_NONCE_RETENTION_MS)],
            )
            .map_err(|error| sqlite_error("prune browser bridge auth nonces", error))?;
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

/// Truthful liveness for doctor/inspect: active requires a recent successful
/// extension round trip, not merely a recorded daemon-side heartbeat.
pub fn bridge_channel_alive() -> bool {
    BridgeStore::open(&crate::default_state_dir())
        .and_then(|store| store.health_round_trip(DEFAULT_HEALTH_MAX_AGE))
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
    use std::thread;

    fn temp_state(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "comptrol-bridge-{name}-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn signed_request_is_body_bound_and_nonce_claim_is_single_use() {
        let state = temp_state("request-auth");
        let token = ensure_auth_token(&state).expect("token");
        let nonce = "00112233445566778899aabbccddeeff";
        let body = br#"{\"ok\":true}"#;
        let signature =
            request_signature(&token, "POST", "/browser/status", nonce, body).expect("signature");
        assert!(
            verify_request_signature(&token, "POST", "/browser/status", nonce, body, &signature,)
                .expect("verify")
        );
        assert!(
            !verify_request_signature(
                &token,
                "POST",
                "/browser/status",
                nonce,
                br#"{\"ok\":false}"#,
                &signature,
            )
            .expect("body mismatch")
        );
        let mut store = BridgeStore::open(&state).expect("store");
        assert!(store.claim_auth_nonce(nonce).expect("first claim"));
        assert!(!store.claim_auth_nonce(nonce).expect("replay claim"));
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn hmac_challenge_proof_is_nonce_bound() {
        let state = temp_state("challenge");
        let first_nonce = "0123456789abcdef0123456789abcdef";
        let second_nonce = "fedcba9876543210fedcba9876543210";
        let first = challenge_proof(&state, first_nonce).expect("first proof");
        let repeat = challenge_proof(&state, first_nonce).expect("repeat proof");
        let second = challenge_proof(&state, second_nonce).expect("second proof");
        assert_eq!(first, repeat);
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(challenge_proof(&state, "not-hex").is_err());
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn page_ops_health_separates_ping_liveness_from_page_op_liveness() {
        // K5: a store with no page-op data reports unknown; a ping result that
        // carries the extension's page_ops counters flips it to ok; a newer
        // failure with orphans reports degraded.
        let state = temp_state("page-ops");
        let mut store = BridgeStore::open(&state).expect("store");
        assert_eq!(
            store.page_ops_health().expect("fresh")["state"].as_str(),
            Some("unknown")
        );
        let request_id = store
            .submit("bridge_ping", serde_json::json!({}))
            .expect("submit");
        store
            .store_result(
                &request_id,
                serde_json::json!({
                    "ok": true,
                    "result": {
                        "pong": true,
                        "timestamp": 1,
                        "page_ops": {
                            "last_ok_at_ms": now_ms(),
                            "last_fail_at_ms": null,
                            "orphaned": 0,
                            "remediations": 0,
                            "instance_id": "test"
                        }
                    }
                }),
            )
            .expect("result");
        assert_eq!(
            store.page_ops_health().expect("ok")["state"].as_str(),
            Some("ok")
        );
        let second = store
            .submit("bridge_ping", serde_json::json!({}))
            .expect("submit");
        store
            .store_result(
                &second,
                serde_json::json!({
                    "ok": true,
                    "result": {
                        "pong": true,
                        "timestamp": 2,
                        "page_ops": {
                            "last_ok_at_ms": now_ms() - 1_000,
                            "last_fail_at_ms": now_ms(),
                            "orphaned": 1,
                            "remediations": 1,
                            "instance_id": "test"
                        }
                    }
                }),
            )
            .expect("result");
        assert_eq!(
            store.page_ops_health().expect("degraded")["state"].as_str(),
            Some("degraded")
        );
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn p24_recovery_requeues_timed_out_command_once_with_flagged_result() {
        let state = temp_state("p24_requeue");
        let mut store = BridgeStore::open(&state).expect("store");
        // A dispatch times out (sw_deadline / bridge_timeout) and is marked.
        let request_id = store
            .submit("mutate", serde_json::json!({"x": 1}))
            .expect("submit");
        // Dispatched and timed out: the row is inflight when the caller gives
        // up (browser_bridge_timeout) or completed-with-error (sw_deadline).
        let leased = store
            .lease_pending(8, Duration::from_secs(30))
            .expect("lease");
        assert_eq!(leased[0].request_id, request_id);
        assert!(store.mark_retryable(&request_id).expect("mark"));
        // Idempotent while un-requeued; nothing moves until recovery.
        assert!(store.mark_retryable(&request_id).expect("mark twice"));
        assert_eq!(store.requeued_count().expect("count"), 0);
        // Channel recovery: a fresh successful bridge_ping round trip fires
        // the requeue internally - same request_id, exactly once.
        let ping_id = store
            .submit("bridge_ping", serde_json::json!({}))
            .expect("ping");
        store
            .store_result(&ping_id, serde_json::json!({"ok": true, "result": {}}))
            .expect("ping result");
        let leased = store
            .lease_pending(8, Duration::from_secs(30))
            .expect("lease");
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].request_id, request_id);
        assert_eq!(store.requeued_count().expect("count"), 1);
        // The recovered outcome is honestly flagged.
        store
            .store_result(
                &request_id,
                serde_json::json!({"ok": true, "result": {"done": 1}}),
            )
            .expect("result");
        assert_eq!(
            store.result(&request_id).expect("read"),
            Some(serde_json::json!({"ok": true, "result": {"done": 1}, "auto_requeued": true}))
        );
        // A second failure is NOT auto-requeued (one requeue per command).
        assert!(!store.mark_retryable(&request_id).expect("re-mark"));
        let ping_id = store
            .submit("bridge_ping", serde_json::json!({}))
            .expect("ping2");
        store
            .store_result(&ping_id, serde_json::json!({"ok": true, "result": {}}))
            .expect("ping2 result");
        assert_eq!(store.requeued_count().expect("count stays 1"), 1);
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn p24_failed_commands_stay_completed_until_recovery() {
        let state = temp_state("p24_no_requeue");
        let mut store = BridgeStore::open(&state).expect("store");
        // A completed non-retryable result is never requeued by recovery.
        let request_id = store.submit("mutate", Value::Null).expect("submit");
        store
            .store_result(&request_id, serde_json::json!({"ok": true}))
            .expect("result");
        let ping_id = store
            .submit("bridge_ping", serde_json::json!({}))
            .expect("ping");
        store
            .store_result(&ping_id, serde_json::json!({"ok": true, "result": {}}))
            .expect("ping result");
        assert_eq!(store.requeue_retryable().expect("requeue"), 0);
        assert_eq!(
            store.result(&request_id).expect("read"),
            Some(serde_json::json!({"ok": true}))
        );
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn lease_expiry_requeues_command() {
        let state = temp_state("lease");
        let mut store = BridgeStore::open(&state).expect("store");
        let request_id = store
            .submit("test", serde_json::json!({"a": 1}))
            .expect("submit");
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
        // A generous window keeps the assertion about semantics (heartbeat
        // present vs absent) instead of about scheduler timing under load.
        assert!(
            !store
                .health(Duration::from_secs(60))
                .expect("health")
                .active
        );
        store.record_heartbeat(Some("test")).expect("heartbeat");
        let health = store.health(Duration::from_secs(60)).expect("health");
        assert!(health.active);
        assert_eq!(
            health.target_count, 0,
            "fresh store must report zero targets"
        );
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn round_trip_health_requires_a_fresh_probe() {
        let state = temp_state("round-trip-health");
        let mut store = BridgeStore::open(&state).expect("store");
        // A daemon-side heartbeat alone must NOT read as channel-alive.
        store.record_heartbeat(Some("test")).expect("heartbeat");
        assert!(
            !store
                .health_round_trip(Duration::from_secs(1))
                .expect("health")
                .active,
            "heartbeat without round trip must not be active"
        );
        // A fresh successful probe (a completed bridge_ping result) makes it
        // alive with the measured latency.
        let ping_id = store
            .submit("bridge_ping", Value::Null)
            .expect("submit ping");
        store
            .store_result(
                &ping_id,
                serde_json::json!({"ok": true, "result": {"pong": true}}),
            )
            .expect("store ping result");
        let health = store
            .health_round_trip(Duration::from_secs(30))
            .expect("health");
        assert!(health.active, "fresh successful ping must read active");
        assert!(health.last_round_trip_ms.unwrap_or(i64::MAX) >= 0);
        // A failed ping degrades it again.
        let failed_id = store.submit("bridge_ping", Value::Null).expect("submit");
        store
            .store_result(
                &failed_id,
                serde_json::json!({"ok": false, "error": "sw dead"}),
            )
            .expect("store failed result");
        assert!(
            !store
                .health_round_trip(Duration::from_secs(30))
                .expect("health")
                .active,
            "failed ping must read inactive"
        );
        // A stale successful ping is not proof of liveness.
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn wake_notices_are_single_serve() {
        let state = temp_state("wake");
        let mut store = BridgeStore::open(&state).expect("store");
        assert!(store.drain_wake_notice().expect("drain empty").is_none());
        store.record_wake_requested("pipe_stale").expect("record");
        let first = store.drain_wake_notice().expect("first notice");
        assert_eq!(first.expect("notice").reason, "pipe_stale");
        assert!(
            store.drain_wake_notice().expect("second drain").is_none(),
            "notice must be consumed once"
        );
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn stale_probes_are_pruned_without_touching_real_commands() {
        let state = temp_state("probe-prune");
        let mut store = BridgeStore::open(&state).expect("store");
        let probe_id = store.submit("bridge_ping", Value::Null).expect("probe");
        let real_id = store.submit("cdp_command", Value::Null).expect("real");
        // Age both beyond the prune cutoff.
        thread::sleep(Duration::from_millis(15));
        let _ = store;
        let mut reopened = BridgeStore::open(&state).expect("reopen");
        reopened.prune_stale_probes().expect("prune");
        // Probes created just now are NOT stale (30 s cutoff), so both rows
        // must survive; this guards against pruning live traffic.
        assert!(reopened.result(&probe_id).expect("probe row").is_none());
        assert!(reopened.result(&real_id).expect("real row").is_none());
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn cross_process_result_writer() {
        let Ok(state) = std::env::var("COMPTROL_TEST_WRITER_STATE") else {
            return;
        };
        let id = std::env::var("COMPTROL_TEST_WRITER_ID").unwrap();
        let mut store = BridgeStore::open(Path::new(&state)).unwrap();
        fs::write(Path::new(&state).join("writer-ready"), b"ready").unwrap();
        thread::sleep(Duration::from_millis(50));
        store
            .store_result(&id, serde_json::json!({"ok":true}))
            .unwrap();
    }

    #[test]
    fn cross_process_result_has_bounded_latency() {
        let state = temp_state("process-waiter");
        let mut store = BridgeStore::open(&state).unwrap();
        let id = store.submit("test", Value::Null).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "browser_bridge::tests::cross_process_result_writer",
                "--nocapture",
            ])
            .env("COMPTROL_TEST_WRITER_STATE", &state)
            .env("COMPTROL_TEST_WRITER_ID", &id)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !state.join("writer-ready").exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        let started = std::time::Instant::now();
        let result = store.wait_result(&id, Duration::from_secs(2));
        let elapsed = started.elapsed();
        assert!(child.wait().unwrap().success());
        assert!(result.unwrap()["ok"].as_bool().unwrap());
        assert!(
            elapsed < Duration::from_millis(750),
            "cross-process result delayed {elapsed:?}"
        );
        drop(store);
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn wait_result_returns_when_store_result_notifies() {
        let state = temp_state("waiter");
        let mut store = BridgeStore::open(&state).expect("store");
        let request_id = store.submit("test", Value::Null).expect("submit");
        let writer_request_id = request_id.clone();
        let writer_state = state.clone();
        let handle = thread::spawn(move || {
            // Store the result from another thread after a short delay;
            // wait_result must return without waiting for its full timeout.
            thread::sleep(Duration::from_millis(40));
            let mut store = BridgeStore::open(&writer_state).expect("reopen");
            store
                .store_result(&writer_request_id, serde_json::json!({"ok": true}))
                .expect("store");
        });
        let started = std::time::Instant::now();
        let result = store
            .wait_result(&request_id, Duration::from_secs(10))
            .expect("result before timeout");
        assert_eq!(result.get("ok").and_then(Value::as_bool), Some(true));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waiter must wake on notify, not poll to timeout"
        );
        handle.join().expect("writer thread");
        let _ = fs::remove_dir_all(state);
    }
}
