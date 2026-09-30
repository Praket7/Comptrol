use comptrol::{
    CompiledWorkflow, MAX_PROTOCOL_BYTES, OperationRequest, PROTOCOL_VERSION, Runtime,
    SERVER_VERSION, TraceMode,
    browser_bridge::{
        BridgeCommand, BridgeStore, COMPANION_BRIDGE_ENDPOINT, DEFAULT_HEALTH_MAX_AGE,
    },
    capability_catalog, compile_verified_trace, default_state_dir, integration, intent_schema, mcp,
    pairing::PairingStore,
    privacy_network_endpoints, privacy_status, read_trace, validate_compiled_workflow,
};
use comptrol_adapter_sdk::AdapterManifest;
#[allow(unused_imports)]
use getrandom::fill;
use rusqlite::{Connection, OptionalExtension, params};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls_pemfile::{certs, private_key};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, RawHandle};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
#[cfg(windows)]
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::CreateMutexW;

const DEFAULT_TASK_TTL_MS: u64 = 300_000;
const TASK_POLL_INTERVAL_MS: u64 = 50;
const HTTP_SESSION_TTL_MS: u128 = 86_400_000;
const HTTP_EVENT_CAPACITY: usize = 256;
const HTTP_STREAM_IDLE_MS: u64 = 300_000;
const HTTP_MAX_CONNECTIONS: usize = 32;
const BROWSER_BRIDGE_POLL_MAX_WAIT_MS: u64 = 800;
const BROWSER_BRIDGE_POLL_INTERVAL_MS: u64 = 20;
static PROCESS_STARTED: OnceLock<Instant> = OnceLock::new();
static STDIO_READY_MS: AtomicUsize = AtomicUsize::new(0);
static MCP_OPERATION_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredTask {
    task_id: String,
    status: String,
    ttl_ms: u64,
    poll_interval_ms: u64,
    created_at_ms: u128,
    #[serde(default)]
    progress: Value,
    result: Value,
}

struct TaskStore {
    connection: Connection,
    sequence: u64,
}

struct TaskManager {
    store: Arc<Mutex<TaskStore>>,
    runtime: Arc<Mutex<Runtime>>,
    cancellation: Arc<Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HttpEvent {
    id: u64,
    data: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredHttpSession {
    session_id: String,
    created_at_ms: u128,
    next_event_id: u64,
    events: VecDeque<HttpEvent>,
}

#[derive(Deserialize, Serialize)]
struct HttpStateFile {
    sessions: Vec<StoredHttpSession>,
}

struct HttpState {
    path: PathBuf,
    sessions: HashMap<String, StoredHttpSession>,
}

struct HttpStore {
    state: Mutex<HttpState>,
    changed: Condvar,
}

enum HttpWait {
    Missing,
    Timeout,
    Events(Vec<HttpEvent>),
}

impl HttpStore {
    fn open(state_dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("http-sessions.json");
        let mut sessions = HashMap::new();
        if path.exists() {
            let bytes = std::fs::read(&path)?;
            let file = serde_json::from_slice::<HttpStateFile>(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid HTTP state: {error}"),
                )
            })?;
            for mut session in file.sessions {
                if valid_session_id(&session.session_id)
                    && now_ms().saturating_sub(session.created_at_ms) <= HTTP_SESSION_TTL_MS
                {
                    session.events.truncate(HTTP_EVENT_CAPACITY);
                    sessions.insert(session.session_id.clone(), session);
                }
            }
        }
        Ok(Self {
            state: Mutex::new(HttpState { path, sessions }),
            changed: Condvar::new(),
        })
    }

    fn persist(state: &HttpState) -> io::Result<()> {
        let file = HttpStateFile {
            sessions: state.sessions.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&file).map_err(io::Error::other)?;
        let temporary = state
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let mut output = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        output.write_all(&bytes)?;
        output.sync_all()?;
        std::fs::rename(temporary, &state.path)
    }

    fn create_session(&self) -> io::Result<String> {
        let mut bytes = [0_u8; 24];
        getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
        let session: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let mut state = self.state.lock().expect("HTTP state lock poisoned");
        state.sessions.insert(
            session.clone(),
            StoredHttpSession {
                session_id: session.clone(),
                created_at_ms: now_ms(),
                next_event_id: 1,
                events: VecDeque::new(),
            },
        );
        if let Err(error) = Self::persist(&state) {
            state.sessions.remove(&session);
            return Err(error);
        }
        Ok(session)
    }

    fn contains(&self, session: Option<&str>) -> bool {
        let state = self.state.lock().expect("HTTP state lock poisoned");
        session.is_some_and(|session| state.sessions.contains_key(session))
    }

    fn latest(&self, session: &str) -> Option<u64> {
        let state = self.state.lock().expect("HTTP state lock poisoned");
        state
            .sessions
            .get(session)
            .and_then(|session| session.events.back().map(|event| event.id))
    }

    fn delete(&self, session: &str) -> io::Result<bool> {
        let mut state = self.state.lock().expect("HTTP state lock poisoned");
        let Some(removed) = state.sessions.remove(session) else {
            return Ok(false);
        };
        if let Err(error) = Self::persist(&state) {
            state.sessions.insert(session.to_owned(), removed);
            return Err(error);
        }
        self.changed.notify_all();
        Ok(true)
    }

    fn append(&self, session: &str, data: Value) -> io::Result<Option<u64>> {
        let mut state = self.state.lock().expect("HTTP state lock poisoned");
        let Some(record) = state.sessions.get_mut(session) else {
            return Ok(None);
        };
        let previous = record.clone();
        let id = record.next_event_id;
        record.next_event_id = record.next_event_id.saturating_add(1);
        record.events.push_back(HttpEvent { id, data });
        while record.events.len() > HTTP_EVENT_CAPACITY {
            record.events.pop_front();
        }
        if let Err(error) = Self::persist(&state) {
            state.sessions.insert(session.to_owned(), previous);
            return Err(error);
        }
        self.changed.notify_all();
        Ok(Some(id))
    }

    fn wait_for_events(&self, session: &str, after: u64, timeout: Duration) -> HttpWait {
        let mut state = self.state.lock().expect("HTTP state lock poisoned");
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let Some(record) = state.sessions.get(session) else {
                return HttpWait::Missing;
            };
            let events = record
                .events
                .iter()
                .filter(|event| event.id > after)
                .cloned()
                .collect::<Vec<_>>();
            if !events.is_empty() {
                return HttpWait::Events(events);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return HttpWait::Timeout;
            }
            let (next_state, result) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("HTTP state lock poisoned");
            state = next_state;
            if result.timed_out() {
                return HttpWait::Timeout;
            }
        }
    }
}

fn valid_session_id(session: &str) -> bool {
    session.len() == 48 && session.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl TaskStore {
    fn open(state_dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("comptrol.db");
        let mut connection = Connection::open(path).map_err(sqlite_io_error)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")
            .map_err(sqlite_io_error)?;
        let foreign_keys: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .map_err(sqlite_io_error)?;
        if foreign_keys != 1 {
            return Err(io::Error::other(
                "SQLite refused to enable foreign key enforcement",
            ));
        }
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS schema_migrations (
                   version INTEGER PRIMARY KEY,
                   applied_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS tasks (
                   task_id TEXT PRIMARY KEY,
                   status TEXT NOT NULL,
                   ttl_ms INTEGER NOT NULL,
                   poll_interval_ms INTEGER NOT NULL,
                   created_at_ms INTEGER NOT NULL,
                   progress_json TEXT NOT NULL DEFAULT 'null',
                   result_json TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS task_events (
                   seq INTEGER PRIMARY KEY AUTOINCREMENT,
                   task_id TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   payload_json TEXT NOT NULL,
                   at_ms INTEGER NOT NULL,
                   FOREIGN KEY(task_id) REFERENCES tasks(task_id)
                 );
                 CREATE TABLE IF NOT EXISTS route_stats (
                   route_key TEXT PRIMARY KEY,
                   attempts INTEGER NOT NULL DEFAULT 0,
                   verified_successes INTEGER NOT NULL DEFAULT 0,
                   verification_failures INTEGER NOT NULL DEFAULT 0,
                   dispatch_failures INTEGER NOT NULL DEFAULT 0,
                   disturbance_events INTEGER NOT NULL DEFAULT 0,
                   ewma_latency_ms REAL,
                   p95_latency_ms REAL,
                   last_success_at_ms INTEGER,
                   app_version TEXT,
                   adapter_version TEXT
                 );
                 INSERT OR IGNORE INTO schema_migrations(version, applied_at_ms)
                   VALUES (1, strftime('%s','now') * 1000);",
            )
            .map_err(sqlite_io_error)?;
        if let Err(error) = connection.execute(
            "ALTER TABLE tasks ADD COLUMN progress_json TEXT NOT NULL DEFAULT 'null'",
            [],
        ) && !error.to_string().contains("duplicate column name")
        {
            return Err(sqlite_io_error(error));
        }
        migrate_legacy_tasks(&mut connection, &state_dir.join("tasks.jsonl"))?;
        let mut store = Self {
            connection,
            sequence: 0,
        };
        let interrupted = store.task_ids_with_status().map_err(sqlite_io_error)?;
        for task_id in interrupted {
            if let Some(previous) = store.get_record(&task_id).map_err(sqlite_io_error)? {
                store.write(StoredTask {
                    status: "unknown".to_owned(),
                    result: json!({
                        "code": "operation_unknown",
                        "message": "Task was interrupted by daemon restart; reconcile before retrying"
                    }),
                    ..previous
                })?;
            }
        }
        Ok(store)
    }

    fn create_pending(&mut self, ttl_ms: u64) -> io::Result<StoredTask> {
        self.sequence = self.sequence.saturating_add(1);
        let task_id = format!("task-{}-{}", now_ms(), self.sequence);
        let task = StoredTask {
            task_id: task_id.clone(),
            status: "queued".to_owned(),
            ttl_ms,
            poll_interval_ms: TASK_POLL_INTERVAL_MS,
            created_at_ms: now_ms(),
            progress: Value::Null,
            result: Value::Null,
        };
        self.write(task.clone())?;
        Ok(task)
    }

    fn get(&self, task_id: &str) -> Option<Value> {
        self.get_record(task_id)
            .ok()
            .flatten()
            .map(|task| task_view(&task))
    }

    fn events_since(
        &self,
        task_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> rusqlite::Result<Vec<Value>> {
        let mut statement = self.connection.prepare(
            "SELECT seq, kind, payload_json, at_ms
             FROM task_events
             WHERE task_id = ?1 AND seq > ?2
             ORDER BY seq ASC LIMIT ?3",
        )?;
        let rows =
            statement.query_map(params![task_id, after_seq as i64, limit as i64], |row| {
                let payload: String = row.get(2)?;
                Ok(json!({
                    "eventId": row.get::<_, i64>(0)? as u64,
                    "taskId": task_id,
                    "kind": row.get::<_, String>(1)?,
                    "payload": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
                    "at": row.get::<_, i64>(3)? as u128
                }))
            })?;
        rows.collect()
    }

    fn result(&self, task_id: &str) -> Option<Value> {
        self.get_record(task_id).ok().flatten().map(|task| {
            if task.status == "completed" {
                task.result.clone()
            } else {
                json!({
                    "taskId": task.task_id,
                    "status": task.status,
                    "result": task.result
                })
            }
        })
    }

    fn request_cancel(&mut self, task_id: &str) -> io::Result<bool> {
        let transaction = self.connection.transaction().map_err(sqlite_io_error)?;
        let status = transaction
            .query_row(
                "SELECT status FROM tasks WHERE task_id = ?1",
                params![task_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sqlite_io_error)?;
        if !matches!(status.as_deref(), Some("queued" | "running")) {
            return Ok(false);
        }
        transaction
            .execute(
                "INSERT INTO task_events(task_id, kind, payload_json, at_ms)
                 VALUES (?1, 'cancel_requested', ?2, ?3)",
                params![
                    task_id,
                    serde_json::to_string(&json!({
                        "code": "operation_cancelled",
                        "message": "Cancellation was requested"
                    }))
                    .map_err(io::Error::other)?,
                    now_ms() as i64,
                ],
            )
            .map_err(sqlite_io_error)?;
        transaction.commit().map_err(sqlite_io_error)?;
        Ok(true)
    }

    fn cancel_requested(&self, task_id: &str) -> bool {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM task_events WHERE task_id = ?1 AND kind = 'cancel_requested')",
                params![task_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .unwrap_or(false)
    }

    fn list(&self) -> Value {
        let mut statement = match self.connection.prepare(
            "SELECT task_id, status, ttl_ms, poll_interval_ms, created_at_ms, progress_json, result_json
             FROM tasks ORDER BY created_at_ms, task_id",
        ) {
            Ok(statement) => statement,
            Err(error) => return task_error(&format!("task store query failed: {error}")),
        };
        let rows = match statement.query_map([], task_from_row) {
            Ok(rows) => rows,
            Err(error) => return task_error(&format!("task store query failed: {error}")),
        };
        let tasks = rows
            .filter_map(Result::ok)
            .map(|task| task_view(&task))
            .collect::<Vec<_>>();
        json!({ "tasks": tasks })
    }

    fn write(&mut self, task: StoredTask) -> io::Result<()> {
        let result_json = serde_json::to_string(&task.result).map_err(io::Error::other)?;
        let transaction = self.connection.transaction().map_err(sqlite_io_error)?;
        let previous_status = transaction
            .query_row(
                "SELECT status FROM tasks WHERE task_id = ?1",
                params![&task.task_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sqlite_io_error)?;
        transaction
            .execute(
                "INSERT INTO tasks(task_id, status, ttl_ms, poll_interval_ms, created_at_ms, progress_json, result_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(task_id) DO UPDATE SET
                   status = excluded.status,
                   ttl_ms = excluded.ttl_ms,
                   poll_interval_ms = excluded.poll_interval_ms,
                   created_at_ms = excluded.created_at_ms,
                   progress_json = excluded.progress_json,
                   result_json = excluded.result_json",
                params![
                    &task.task_id,
                    &task.status,
                    task.ttl_ms as i64,
                    task.poll_interval_ms as i64,
                    task.created_at_ms as i64,
                    serde_json::to_string(&task.progress).map_err(io::Error::other)?,
                    result_json,
                ],
            )
            .map_err(sqlite_io_error)?;
        if previous_status.as_deref() != Some(task.status.as_str()) {
            transaction
                .execute(
                    "INSERT INTO task_events(task_id, kind, payload_json, at_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        &task.task_id,
                        &task.status,
                        serde_json::to_string(&task.result).map_err(io::Error::other)?,
                        now_ms() as i64,
                    ],
                )
                .map_err(sqlite_io_error)?;
        }
        transaction.commit().map_err(sqlite_io_error)
    }

    fn get_record(&self, task_id: &str) -> rusqlite::Result<Option<StoredTask>> {
        self.connection
            .query_row(
                "SELECT task_id, status, ttl_ms, poll_interval_ms, created_at_ms, progress_json, result_json
                 FROM tasks WHERE task_id = ?1",
                params![task_id],
                task_from_row,
            )
            .optional()
    }

    fn task_ids_with_status(&self) -> rusqlite::Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT task_id FROM tasks WHERE status IN ('queued', 'running') ORDER BY task_id",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect()
    }

    fn record_route_outcome(
        &mut self,
        route: &str,
        verified: bool,
        dispatch_failed: bool,
        disturbance: bool,
        latency_ms: u128,
    ) -> rusqlite::Result<()> {
        let latency = latency_ms as f64;
        self.connection.execute(
            "INSERT INTO route_stats(route_key, attempts, verified_successes, verification_failures, dispatch_failures, disturbance_events, ewma_latency_ms, p95_latency_ms, last_success_at_ms)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?6, CASE WHEN ?2 = 1 THEN ?7 ELSE NULL END)
             ON CONFLICT(route_key) DO UPDATE SET
               attempts = attempts + 1,
               verified_successes = verified_successes + excluded.verified_successes,
               verification_failures = verification_failures + excluded.verification_failures,
               dispatch_failures = dispatch_failures + excluded.dispatch_failures,
               disturbance_events = disturbance_events + excluded.disturbance_events,
               ewma_latency_ms = (COALESCE(route_stats.ewma_latency_ms, excluded.ewma_latency_ms) * 0.8) + (excluded.ewma_latency_ms * 0.2),
               p95_latency_ms = MAX(COALESCE(route_stats.p95_latency_ms, 0), excluded.p95_latency_ms),
               last_success_at_ms = CASE WHEN excluded.last_success_at_ms IS NOT NULL THEN excluded.last_success_at_ms ELSE route_stats.last_success_at_ms END",
            params![
                route,
                i64::from(verified),
                i64::from(!verified && !dispatch_failed),
                i64::from(dispatch_failed),
                i64::from(disturbance),
                latency,
                now_ms() as i64,
            ],
        )?;
        Ok(())
    }
}

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTask> {
    let progress_json: String = row.get(5)?;
    let result_json: String = row.get(6)?;
    Ok(StoredTask {
        task_id: row.get(0)?,
        status: row.get(1)?,
        ttl_ms: row.get::<_, i64>(2)?.max(0) as u64,
        poll_interval_ms: row.get::<_, i64>(3)?.max(0) as u64,
        created_at_ms: row.get::<_, i64>(4)?.max(0) as u128,
        progress: serde_json::from_str(&progress_json).unwrap_or(Value::Null),
        result: serde_json::from_str(&result_json).unwrap_or(Value::Null),
    })
}

fn migrate_legacy_tasks(connection: &mut Connection, path: &Path) -> io::Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let mut legacy = Vec::new();
    for line in BufReader::new(File::open(path)?).lines() {
        if let Ok(task) = serde_json::from_str::<StoredTask>(&line?) {
            legacy.push(task);
        }
    }
    let transaction = connection.transaction().map_err(sqlite_io_error)?;
    for task in legacy {
        transaction
            .execute(
                "INSERT OR IGNORE INTO tasks(task_id, status, ttl_ms, poll_interval_ms, created_at_ms, progress_json, result_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    task.task_id,
                    task.status,
                    task.ttl_ms as i64,
                    task.poll_interval_ms as i64,
                    task.created_at_ms as i64,
                    serde_json::to_string(&task.progress).map_err(io::Error::other)?,
                    serde_json::to_string(&task.result).map_err(io::Error::other)?,
                ],
            )
            .map_err(sqlite_io_error)?;
    }
    transaction.commit().map_err(sqlite_io_error)
}

fn sqlite_io_error(error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("SQLite task store error: {error}"))
}

impl TaskManager {
    fn new(runtime: Arc<Mutex<Runtime>>, store: TaskStore) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            runtime,
            cancellation: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn spawn(&self, params: Value, ttl_ms: u64) -> io::Result<Value> {
        let queued_at = Instant::now();
        let task = self
            .store
            .lock()
            .expect("task store lock poisoned")
            .create_pending(ttl_ms)?;
        let task_id = task.task_id.clone();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.cancellation
            .lock()
            .expect("task cancellation lock poisoned")
            .insert(task_id.clone(), Arc::clone(&cancelled));
        let store = Arc::clone(&self.store);
        let runtime = Arc::clone(&self.runtime);
        let cancellation = Arc::clone(&self.cancellation);
        thread::Builder::new()
            .name(format!("comptrol-task-{task_id}"))
            .spawn(move || {
                if cancelled.load(Ordering::Acquire) {
                    let _ = update_task(&store, &task_id, "cancelled", json!({
                        "code": "operation_cancelled",
                        "message": "Task was cancelled before execution started"
                    }));
                    cancellation
                        .lock()
                        .expect("task cancellation lock poisoned")
                        .remove(&task_id);
                    return;
                }
                let _ = update_task(&store, &task_id, "running", Value::Null);
                let queue_wait_ms = queued_at.elapsed().as_secs_f64() * 1000.0;
                let started_at_ms = now_ms();
                let result = {
                    let mut runtime = runtime.lock().expect("runtime lock poisoned");
                    call_tool_with_cancel(&mut runtime, params, Some(Arc::clone(&cancelled)), queue_wait_ms)
                };
                let requested = cancelled.load(Ordering::Acquire)
                    || store
                        .lock()
                        .map(|store| store.cancel_requested(&task_id))
                        .unwrap_or(true);
                let (status, stored_result) = if requested {
                    (
                        "unknown",
                        json!({
                            "code": "operation_unknown",
                            "message": "Cancellation arrived after execution began; reconcile the operation before retrying",
                            "result": result
                        }),
                    )
                } else {
                    ("completed", result)
                };
                if status == "completed" {
                    let route = stored_result
                        .get("route")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let verified = stored_result
                        .get("verification")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value == "verified");
                    let dispatch_failed = stored_result
                        .get("delivery")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value == "failed" || value == "refused");
                    let disturbance = stored_result
                        .get("disturbance")
                        .and_then(|value| value.get("foreground_changed"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if let Ok(mut store) = store.lock() {
                        let _ = store.record_route_outcome(
                            route,
                            verified,
                            dispatch_failed,
                            disturbance,
                            now_ms().saturating_sub(started_at_ms),
                        );
                    }
                }
                let _ = update_task(&store, &task_id, status, stored_result);
                cancellation
                    .lock()
                    .expect("task cancellation lock poisoned")
                    .remove(&task_id);
            })
            .map_err(io::Error::other)?;
        Ok(task_view(&task))
    }

    fn get(&self, task_id: &str) -> Option<Value> {
        self.store
            .lock()
            .expect("task store lock poisoned")
            .get(task_id)
    }

    fn get_with_events(&self, task_id: &str, after_seq: u64) -> Option<Value> {
        let store = self.store.lock().ok()?;
        let mut task = store.get(task_id)?;
        let events = store.events_since(task_id, after_seq, 256).ok()?;
        let cursor = events
            .last()
            .and_then(|event| event.get("eventId"))
            .and_then(Value::as_u64)
            .unwrap_or(after_seq);
        task["events"] = Value::Array(events);
        task["eventCursor"] = json!(cursor);
        Some(task)
    }

    fn result(&self, task_id: &str) -> Option<Value> {
        self.store
            .lock()
            .expect("task store lock poisoned")
            .result(task_id)
    }

    fn list(&self) -> Value {
        self.store.lock().expect("task store lock poisoned").list()
    }

    fn cancel(&self, task_id: &str) -> Value {
        let Some(flag) = self
            .cancellation
            .lock()
            .expect("task cancellation lock poisoned")
            .get(task_id)
            .cloned()
        else {
            let requested = self
                .store
                .lock()
                .ok()
                .and_then(|mut store| store.request_cancel(task_id).ok())
                .unwrap_or(false);
            return self
                .get(task_id)
                .map(|task| json!({ "task": task, "cancelRequested": requested }))
                .unwrap_or_else(|| task_error("task not found"));
        };
        flag.store(true, Ordering::Release);
        let persisted = self
            .store
            .lock()
            .ok()
            .and_then(|mut store| store.request_cancel(task_id).ok())
            .unwrap_or(false);
        json!({ "taskId": task_id, "cancelRequested": persisted })
    }
}

fn update_task(
    store: &Arc<Mutex<TaskStore>>,
    task_id: &str,
    status: &str,
    result: Value,
) -> io::Result<()> {
    let mut store = store.lock().expect("task store lock poisoned");
    let Some(previous) = store.get_record(task_id).map_err(sqlite_io_error)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "task record disappeared",
        ));
    };
    let task = StoredTask {
        status: status.to_owned(),
        progress: match status {
            "running" => json!({ "value": 0.0, "message": "Task execution started" }),
            "completed" => json!({ "value": 1.0, "message": "Task completed" }),
            "cancelled" => json!({ "value": 1.0, "message": "Task cancelled before execution" }),
            "unknown" => json!({ "message": "Task requires reconciliation" }),
            _ => Value::Null,
        },
        result,
        ..previous
    };
    store.write(task)
}

fn task_view(task: &StoredTask) -> Value {
    json!({
        "taskId": task.task_id,
        "status": task.status,
        "ttlMs": task.ttl_ms,
        "pollIntervalMs": task.poll_interval_ms,
        "progress": task.progress,
        "result": task.result
    })
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn run_open(args: Vec<String>) -> i32 {
    let Some(target) = args.first() else {
        eprintln!("open needs an app name or URL");
        return 2;
    };
    if args.len() != 1 {
        eprintln!("open accepts one exact app name or URL");
        return 2;
    }
    let mut runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    let is_url = target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("about:");
    if !is_url {
        let result = runtime.operate(OperationRequest {
            intent: "app.launch".to_owned(),
            target: None,
            params: json!({"app": target}),
            postcondition: None,
            risk: None,
            idempotency_key: None,
            dry_run: false,
            background: Some("foreground_allowed".to_owned()),
        });
        return print_open_result(result);
    }

    // `comptrol open <url>` is an explicit request for a browser, so this is the
    // one place that may start the local Chrome before deciding how to reach it.
    comptrol::chrome_autostart::ensure();
    let can_verify = std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1")
        && (std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
            || comptrol::browser_bridge::bridge_is_active());
    let result = runtime.operate(OperationRequest {
        intent: if can_verify {
            "browser.cdp.open_tab"
        } else {
            "browser.chrome.open_tab"
        }
        .to_owned(),
        target: None,
        params: json!({"url": target, "background": false}),
        postcondition: None,
        risk: None,
        idempotency_key: None,
        dry_run: false,
        background: Some("foreground_allowed".to_owned()),
    });
    if result.error.is_some() || !can_verify {
        return print_open_result(result);
    }
    let opened = result.data.clone();
    let target_info = &opened["target"];
    let (Some(target_id), Some(context_id), Some(revision)) = (
        target_info["id"].as_str(),
        target_info["browser_context_id"].as_str(),
        target_info["revision"].as_str(),
    ) else {
        return print_open_result(result);
    };
    let wait = runtime.operate(OperationRequest {
        intent: "browser.cdp.wait_for".to_owned(),
        target: None,
        params: json!({
            "target_id": target_id,
            "browser_context_id": context_id,
            "revision": revision,
            "selector": "document",
            "property": "readyState",
            "equals": "complete",
            "timeout_ms": 30_000
        }),
        postcondition: None,
        risk: None,
        idempotency_key: None,
        dry_run: false,
        background: Some("foreground_allowed".to_owned()),
    });
    print_json(json!({
        "open": result,
        "page_load": wait,
        "verified": wait.error.is_none() && wait.verification == comptrol::VerificationState::Verified
    }))
}

fn print_open_result(result: comptrol::ActionResult) -> i32 {
    let success = result.error.is_none();
    let value = serde_json::to_value(result).unwrap_or(Value::Null);
    print_json(value);
    if success { 0 } else { 1 }
}

fn main() {
    let _ = PROCESS_STARTED.set(Instant::now());
    let result = match env::args().nth(1).as_deref() {
        #[cfg(windows)]
        Some("__windows-uia-worker") => comptrol_platform_windows::run_worker_stdio(),
        None | Some("mcp") => {
            // Chrome is not started here. A browser operation starts it on
            // first use, windowless, so a desktop-only session never shows a
            // browser window at all. See `comptrol::chrome_autostart`.
            let result = run_stdio();
            comptrol::chrome_autostart::shutdown();
            result
        }
        Some("setup") => comptrol::setup::run_setup(&env::args().skip(2).collect::<Vec<String>>()),
        Some("doctor") => run_doctor(env::args().skip(2).collect()),
        Some("open") => run_open(env::args().skip(2).collect()),
        Some("status") => print_json(run_inspect("status")),
        Some("capabilities") => print_json(capability_catalog()),
        Some("stop") => change_stop(true),
        Some("resume") => change_stop(false),
        Some("resolve-action") => resolve_human_action_cli(env::args().skip(2).collect()),
        Some("serve-http") => run_http(
            env::args()
                .nth(2)
                .and_then(|port| port.parse().ok())
                .unwrap_or(7317),
        ),
        Some("serve-mtls") => run_mtls(
            env::args()
                .nth(2)
                .and_then(|port| port.parse().ok())
                .unwrap_or(7443),
        ),
        Some("daemon") => run_daemon(),
        Some("daemon-health") => run_daemon_health(),
        Some("record") => run_record(env::args().skip(2).collect()),
        Some("replay") => run_replay(env::args().skip(2).collect()),
        Some("workflow") => run_workflow(env::args().skip(2).collect()),
        Some("adapter") => run_adapter(env::args().skip(2).collect()),
        Some("integrate") => run_integrate(env::args().skip(2).collect()),
        Some("pair") => run_pair(env::args().skip(2).collect()),
        Some("privacy") => run_privacy(env::args().skip(2).collect()),
        Some("version") => {
            println!("{SERVER_VERSION}");
            0
        }
        Some(other) => {
            eprintln!("unknown command {other}");
            eprintln!(
                "commands are mcp doctor setup open status capabilities stop resume serve-http serve-mtls daemon daemon-health record replay workflow adapter integrate pair privacy version"
            );
            2
        }
    };
    if result != 0 {
        std::process::exit(result);
    }
}

fn run_privacy(args: Vec<String>) -> i32 {
    match args.first().map(String::as_str) {
        Some("status") => print_json(privacy_status()),
        Some("network-endpoints") => print_json(privacy_network_endpoints()),
        _ => {
            eprintln!("privacy accepts status or network-endpoints");
            2
        }
    }
}

fn run_pair(args: Vec<String>) -> i32 {
    let Some(command) = args.first().map(String::as_str) else {
        eprintln!("pair accepts show, accept, revoke, or list");
        return 2;
    };
    if let Err(error) = Runtime::new(default_state_dir()) {
        eprintln!("startup failed: {error}");
        return 1;
    }
    let mut store = match PairingStore::open(&default_state_dir()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("pairing store failed: {error}");
            return 1;
        }
    };
    match command {
        "show" | "create" => {
            let mut ttl_ms = None;
            let mut scopes = Vec::new();
            let mut index = 1;
            let mut fingerprint = None;
            while index < args.len() {
                match args[index].as_str() {
                    "--ttl-ms" => {
                        index += 1;
                        ttl_ms = args.get(index).and_then(|value| value.parse().ok());
                    }
                    "--scope" => {
                        index += 1;
                        if let Some(scope) = args.get(index) {
                            scopes.push(scope.clone());
                        }
                    }
                    "--fingerprint" => {
                        index += 1;
                        fingerprint = args.get(index).cloned();
                    }
                    _ => {
                        eprintln!(
                            "pair show accepts --ttl-ms, --scope, and repeated --fingerprint"
                        );
                        return 2;
                    }
                }
                index += 1;
            }
            if scopes.is_empty() {
                scopes.push("observe".to_owned());
            }
            match store.create(scopes, ttl_ms, fingerprint) {
                Ok((record, code)) => print_json(json!({
                    "pairing_id": record.pairing_id,
                    "code": code,
                    "scopes": record.scopes,
                    "expires_at_ms": record.expires_at_ms,
                    "identity_fingerprint": record.identity_fingerprint,
                    "remote_transport": if record.identity_fingerprint.is_some() { "mtls_bound" } else { "disabled_until_mtls" }
                })),
                Err(error) => {
                    eprintln!("pairing creation refused: {error}");
                    1
                }
            }
        }
        "accept" => {
            let Some(code) = args.get(1) else {
                eprintln!("pair accept needs a short lived code");
                return 2;
            };
            match store.accept(code) {
                Ok(record) => print_json(public_pairing(&record)),
                Err(error) => {
                    eprintln!("pairing acceptance refused: {error}");
                    1
                }
            }
        }
        "revoke" => {
            let Some(pairing_id) = args.get(1) else {
                eprintln!("pair revoke needs a pairing id");
                return 2;
            };
            match store.revoke(pairing_id) {
                Ok(record) => print_json(public_pairing(&record)),
                Err(error) => {
                    eprintln!("pairing revocation failed: {error}");
                    1
                }
            }
        }
        "list" => print_json(json!({
            "pairings": store.list().iter().map(|r| {
                let mut public = public_pairing(r);
                public["identity_fingerprint"] = match &r.identity_fingerprint {
                    Some(fp) => json!(fp),
                    None => json!(null),
                };
                public["remote_transport"] = if r.identity_fingerprint.is_some() { json!("mtls_bound") } else { json!("disabled_until_mtls") };
                public
            }).collect::<Vec<_>>()
        })),
        _ => {
            eprintln!("pair accepts show, accept, revoke, or list");
            2
        }
    }
}

fn public_pairing(record: &comptrol::pairing::PairingRecord) -> Value {
    json!({
        "pairing_id": record.pairing_id,
        "scopes": record.scopes,
        "created_at_ms": record.created_at_ms,
        "expires_at_ms": record.expires_at_ms,
        "accepted": record.accepted,
        "revoked": record.revoked
    })
}

fn run_doctor(args: Vec<String>) -> i32 {
    let human = match args.as_slice() {
        [] => false,
        [flag] if flag == "--human" => true,
        _ => {
            eprintln!("doctor accepts --human");
            return 2;
        }
    };
    let value = run_inspect("doctor");
    if !human {
        return print_json(value);
    }
    println!("Comptrol doctor");
    println!(
        "Platform: {} {}",
        value["platform"]["os"].as_str().unwrap_or("unknown"),
        value["architecture"].as_str().unwrap_or("unknown")
    );
    println!(
        "Daemon: {}",
        value["daemon"]["state"].as_str().unwrap_or("unknown")
    );
    println!(
        "Policy: maximum risk {}",
        value["policy"]["max_risk"].as_str().unwrap_or("unknown")
    );
    println!(
        "Accessibility: {}",
        value["platform"]["brokers"]["macos_ax"]["status"]
            .as_str()
            .unwrap_or("unknown")
    );
    println!(
        "Browser: {}",
        value["browser"]["status"].as_str().unwrap_or("unknown")
    );
    println!(
        "Remote: {}",
        value["remote"]["binding"].as_str().unwrap_or("unknown")
    );
    0
}

fn run_integrate(args: Vec<String>) -> i32 {
    if args.is_empty() || args.iter().any(|arg| arg == "--list") {
        return print_json(integration::list());
    }
    let mut client = None;
    let mut config = None;
    let mut apply = false;
    let mut undo = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--client" => {
                index += 1;
                client = args.get(index).cloned();
            }
            "--config" => {
                index += 1;
                config = args.get(index).map(std::path::PathBuf::from);
            }
            "--apply" => apply = true,
            "--undo" => undo = true,
            _ => {
                eprintln!("integrate accepts --list, --client, --config, --apply, and --undo");
                return 2;
            }
        }
        index += 1;
    }
    let Some(config) = config else {
        eprintln!("integrate requires an explicit --config path for proposal or apply");
        return 2;
    };
    if undo {
        return match integration::undo(&config) {
            Ok(value) => print_json(value),
            Err(error) => {
                eprintln!("integration undo refused: {error}");
                1
            }
        };
    }
    let Some(client) = client else {
        eprintln!("integrate requires --client");
        return 2;
    };
    let result = if apply {
        integration::apply(&client, &config)
    } else {
        integration::proposal(&client, &config)
    };
    match result {
        Ok(value) => print_json(value),
        Err(error) => {
            eprintln!("integration refused: {error}");
            1
        }
    }
}

fn run_stdio() -> i32 {
    let startup_started = Instant::now();
    let runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    let tasks = match TaskStore::open(&default_state_dir()) {
        Ok(tasks) => tasks,
        Err(error) => {
            eprintln!("task store startup failed: {error}");
            return 1;
        }
    };
    let runtime = Arc::new(Mutex::new(runtime));
    let tasks = TaskManager::new(Arc::clone(&runtime), tasks);
    STDIO_READY_MS.store(
        startup_started
            .elapsed()
            .as_millis()
            .min(usize::MAX as u128) as usize,
        Ordering::Release,
    );
    let mut tasks_enabled = false;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        let mut line = Vec::new();
        match input.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) if line.len() > MAX_PROTOCOL_BYTES => {
                println!(
                    "{}",
                    json!({ "jsonrpc": "2.0", "id": Value::Null, "error": { "code": "message_too_large", "message": format!("MCP messages are limited to {MAX_PROTOCOL_BYTES} bytes") } })
                );
                let _ = io::stdout().flush();
            }
            Ok(_) => {
                let line = match std::str::from_utf8(&line) {
                    Ok(line) => line,
                    Err(error) => {
                        eprintln!("stdin is not UTF-8: {error}");
                        return 1;
                    }
                };
                if !line.trim().is_empty() {
                    let response = handle_message_with_state(
                        &runtime,
                        Some(&tasks),
                        &mut tasks_enabled,
                        line,
                        |notification| {
                            println!("{}", notification);
                            let _ = io::stdout().flush();
                        },
                    );
                    if let Some(response) = response {
                        println!("{}", response);
                        let _ = io::stdout().flush();
                    }
                }
            }
            Err(error) => {
                eprintln!("stdin failed: {error}");
                return 1;
            }
        }
    }
    0
}

fn handle_message_with_state<F>(
    runtime: &Arc<Mutex<Runtime>>,
    tasks: Option<&TaskManager>,
    tasks_enabled: &mut bool,
    line: &str,
    mut emit: F,
) -> Option<Value>
where
    F: FnMut(Value),
{
    if line.len() > MAX_PROTOCOL_BYTES {
        return Some(
            json!({ "jsonrpc": "2.0", "id": Value::Null, "error": { "code": "message_too_large", "message": format!("MCP messages are limited to {MAX_PROTOCOL_BYTES} bytes") } }),
        );
    }
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Some(
                json!({ "jsonrpc": "2.0", "id": Value::Null, "error": { "code": -32700, "message": error.to_string() } }),
            );
        }
    };
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    request.get("id")?;
    let task_transport = tasks.is_some();
    let progress_token = request
        .get("params")
        .filter(|_| method == "tools/call")
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get("progressToken"))
        .cloned();
    if let Some(token) = progress_token.as_ref() {
        emit(json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": token,
                "progress": 0,
                "total": 1,
                "message": "operation_started"
            }
        }));
    }
    if method == "initialize" {
        let requested = request
            .get("params")
            .and_then(|params| params.get("protocolVersion"))
            .and_then(Value::as_str);
        if let Err(error) = mcp::negotiate(requested) {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": "protocol_version_unsupported", "message": error }
            }));
        }
    }
    let result = match method {
        "initialize" => {
            let (protocol_version, protocol_mode) = mcp::negotiate(
                request
                    .get("params")
                    .and_then(|params| params.get("protocolVersion"))
                    .and_then(Value::as_str),
            )
            .expect("initialize version was validated");
            *tasks_enabled = task_transport
                && request
                    .get("params")
                    .and_then(|params| params.get("capabilities"))
                    .and_then(|capabilities| {
                        capabilities
                            .get("tasks")
                            .and_then(|tasks| tasks.get("requests"))
                            .and_then(|requests| requests.get("tools"))
                            .and_then(|tools| tools.get("call"))
                            .or_else(|| {
                                capabilities.get("extensions").and_then(|extensions| {
                                    extensions.get("io.modelcontextprotocol/tasks")
                                })
                            })
                    })
                    .is_some();
            let task_capabilities = if task_transport {
                json!({
                    "list": {},
                    "cancel": {},
                    "requests": { "tools": { "call": {} } }
                })
            } else {
                json!({})
            };
            let extensions = if task_transport {
                json!({ "io.modelcontextprotocol/tasks": {} })
            } else {
                json!({})
            };
            json!({ "protocolVersion": protocol_version, "capabilities": { "tools": { "listChanged": false }, "tasks": task_capabilities, "extensions": extensions }, "serverInfo": { "name": "comptrol", "version": SERVER_VERSION }, "instructions": "Use capabilities.intents for callable intent names; platform_observations are not callable. intent_schema returns the enforced parameter schema and example for supported intents. Use operate for one bounded intent and inspect for current state. Results distinguish delivery, effect, and verification.", "comptrol": { "protocol_mode": if protocol_mode == mcp::ProtocolMode::Current { "stateless" } else { "legacy_compatibility" } } })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            if *tasks_enabled && params.get("task").is_some() {
                let ttl_ms = params
                    .get("task")
                    .and_then(|task| task.get("ttl"))
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_TASK_TTL_MS)
                    .clamp(1_000, 86_400_000);
                match tasks {
                    Some(manager) => match manager.spawn(params.clone(), ttl_ms) {
                        Ok(task) => json!({ "resultType": "task", "task": task }),
                        Err(error) => {
                            json!({ "error": { "code": "task_store_failed", "message": error.to_string() } })
                        }
                    },
                    None => call_tool_with_cancel(
                        &mut runtime.lock().expect("runtime lock poisoned"),
                        request.get("params").cloned().unwrap_or(Value::Null),
                        None,
                        0.0,
                    ),
                }
            } else {
                call_tool_with_cancel(
                    &mut runtime.lock().expect("runtime lock poisoned"),
                    params,
                    None,
                    0.0,
                )
            }
        }
        "tasks/get" => task_get(tasks, &request),
        "tasks/result" => task_result(tasks, &request),
        "tasks/list" => tasks.map(TaskManager::list).unwrap_or_else(|| {
            task_error("Tasks are unavailable because the task store is not initialized")
        }),
        "tasks/cancel" => task_cancel(tasks, &request),
        "tasks/update" => task_error("Comptrol tasks do not accept input updates"),
        _ => {
            json!({ "error": { "code": "method_not_found", "message": format!("Unknown MCP method {method}") } })
        }
    };
    if let Some(token) = progress_token {
        emit(json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": token,
                "progress": 1,
                "total": 1,
                "message": "operation_completed"
            }
        }));
    }
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn task_get(tasks: Option<&TaskManager>, request: &Value) -> Value {
    let Some(params) = request.get("params") else {
        return task_error("tasks/get needs taskId");
    };
    let Some(task_id) = params.get("taskId").and_then(Value::as_str) else {
        return task_error("tasks/get needs taskId");
    };
    let after_seq = params
        .get("afterEventId")
        .or_else(|| params.get("lastEventId"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    tasks
        .and_then(|manager| manager.get_with_events(task_id, after_seq))
        .unwrap_or_else(|| task_error("task not found"))
}

fn task_result(tasks: Option<&TaskManager>, request: &Value) -> Value {
    let Some(task_id) = request
        .get("params")
        .and_then(|params| params.get("taskId"))
        .and_then(Value::as_str)
    else {
        return task_error("tasks/result needs taskId");
    };
    tasks
        .and_then(|manager| manager.result(task_id))
        .unwrap_or_else(|| task_error("task not found"))
}

fn task_cancel(tasks: Option<&TaskManager>, request: &Value) -> Value {
    let Some(task_id) = request
        .get("params")
        .and_then(|params| params.get("taskId"))
        .and_then(Value::as_str)
    else {
        return task_error("tasks/cancel needs taskId");
    };
    tasks
        .map(|manager| manager.cancel(task_id))
        .unwrap_or_else(|| {
            task_error("Tasks are unavailable because the task store is not initialized")
        })
}

fn task_error(message: &str) -> Value {
    json!({ "error": { "code": -32602, "message": message } })
}

fn tools() -> Value {
    json!([
        { "name": "operate", "description": "Execute one bounded local intent with policy, idempotency, background posture, and verification state", "annotations": {"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true,"idempotentHint":false}, "inputSchema": { "type": "object", "required": ["intent"], "properties": { "intent": {"type":"string"}, "target": {"type":"object"}, "params": {"type":"object"}, "postcondition": {"type":"object"}, "risk": {"type":"string"}, "idempotency_key": {"type":"string"}, "dry_run": {"type":"boolean"}, "background": {"type":"string", "enum":["strict_background","prefer_background","foreground_allowed","foreground_required"]} } } },
        { "name": "intent_schema", "description": "Return the exact parameter schema, constraints, and example for a supported intent when discovery is needed; operate validates against the same schema before dispatch", "annotations": {"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false,"idempotentHint":true}, "inputSchema": {"type":"object","required":["intent"],"properties":{"intent":{"type":"string"}}} },
        { "name": "inspect", "description": "Inspect doctor, status, capabilities, deterministic route plans, platform state, events, checkpoints, adapters, or current desktop observation", "annotations": {"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false,"idempotentHint":true}, "inputSchema": { "type": "object", "properties": { "kind": {"type":"string", "enum":["doctor","status","capabilities","routes","platform","desktop","events","checkpoints","adapters"]} } } },
        { "name": "watch", "description": "Return the known state of an operation without repeating its mutation", "annotations": {"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false,"idempotentHint":true}, "inputSchema": { "type": "object", "required":["operation_id"], "properties": { "operation_id": {"type":"string"} } } },
        { "name": "reconcile", "description": "Reconcile a durable unknown operation from observed local state without repeating its mutation", "annotations": {"readOnlyHint":false,"destructiveHint":false,"openWorldHint":true,"idempotentHint":true}, "inputSchema": { "type": "object", "required":["operation_id"], "properties": { "operation_id": {"type":"string"} } } },
        { "name": "restore_checkpoint", "description": "Restore a local sandbox checkpoint under explicit local write policy", "annotations": {"readOnlyHint":false,"destructiveHint":true,"openWorldHint":false,"idempotentHint":false}, "inputSchema": { "type": "object", "required":["checkpoint"], "properties": { "checkpoint": {"type":"string"}, "idempotency_key": {"type":"string"} } } },
        { "name": "capabilities", "description": "Return capabilities that are actually available in this runtime", "annotations": {"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false,"idempotentHint":true}, "inputSchema": { "type": "object" } },
        { "name": "human_action.resolve", "description": "Resolve a pending human action (approve or decline) after the user has acted in the native prompt. Use this after awaiting_human_action to record the user's decision so the operation can be retried.", "annotations": {"readOnlyHint":false,"destructiveHint":false,"openWorldHint":false,"idempotentHint":false}, "inputSchema": { "type": "object", "required": ["action_id", "resolution"], "properties": { "action_id": {"type":"string", "description":"The human_action_id from the awaiting_human_action response"}, "resolution": {"type":"string", "enum":["approved","declined"], "description":"The user's decision"} } } }
    ])
}

fn call_tool_with_cancel(
    runtime: &mut Runtime,
    params: Value,
    cancellation: Option<Arc<std::sync::atomic::AtomicBool>>,
    queue_wait_ms: f64,
) -> Value {
    let call_started = Instant::now();
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let call_number = if name == "operate" {
        MCP_OPERATION_COUNT.fetch_add(1, Ordering::AcqRel)
    } else {
        MCP_OPERATION_COUNT.load(Ordering::Acquire)
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let client_timing = params
        .get("_meta")
        .and_then(|meta| meta.get("comptrolTiming"));
    let workflow_id = client_timing
        .and_then(|value| value.get("workflow_id"))
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        })
        .map(str::to_owned);
    let client_model_ms =
        external_duration_ms(client_timing.and_then(|value| value.get("client_model_ms")));
    let human_prompt_ms =
        external_duration_ms(client_timing.and_then(|value| value.get("human_prompt_ms")));
    let perceived_elapsed_ms =
        external_duration_ms(client_timing.and_then(|value| value.get("perceived_elapsed_ms")));
    let mut value = match name {
        "operate" => {
            let started = Instant::now();
            let mut runtime_operate_ms = None;
            let mut value = serde_json::from_value::<OperationRequest>(arguments)
                .map(|request| {
                    let operation_started = Instant::now();
                    let result = match cancellation {
                        Some(cancellation) => runtime.operate_with_cancel(request, cancellation),
                        None => runtime.operate(request),
                    };
                    runtime_operate_ms = Some(operation_started.elapsed().as_secs_f64() * 1000.0);
                    json!(result)
                })
                .unwrap_or_else(|error| json!({ "error": { "code": "invalid_input", "message": error.to_string() } }));
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "timings".to_owned(),
                    json!({
                        "runtime_total_ms": started.elapsed().as_secs_f64() * 1000.0,
                        "runtime_operate_ms": runtime_operate_ms
                    }),
                );
            }
            value
        }
        "intent_schema" => match arguments.get("intent").and_then(Value::as_str) {
            Some(intent) => intent_schema::schema_for(intent).unwrap_or_else(|| {
                json!({
                    "error": {"code":"schema_unavailable","message":format!("No intent-specific schema is published for {intent}")},
                    "available_schemas": intent_schema::available_schemas()
                })
            }),
            None => json!({"error":{"code":"invalid_input","message":"intent_schema requires a string intent"}}),
        },
        "inspect" => json!(runtime.inspect(arguments.get("kind").and_then(Value::as_str).unwrap_or("status"))),
        "watch" => json!(runtime.watch(arguments.get("operation_id").and_then(Value::as_str).unwrap_or_default())),
        "reconcile" => json!(runtime.reconcile(arguments.get("operation_id").and_then(Value::as_str).unwrap_or_default())),
        "restore_checkpoint" => serde_json::from_value::<OperationRequest>(json!({ "intent": "filesystem.restore_checkpoint", "params": arguments.clone(), "idempotency_key": arguments.get("idempotency_key"), "risk": "R1" })).map(|request| json!(runtime.operate(request))).unwrap_or_else(|error| json!({ "error": { "code": "invalid_input", "message": error.to_string() } })),
        "capabilities" => capability_catalog(),
        "human_action.resolve" => {
            let action_id = arguments.get("action_id").and_then(Value::as_str).unwrap_or_default();
            let resolution_str = arguments.get("resolution").and_then(Value::as_str).unwrap_or_default();
            let resolution = match resolution_str {
                "approved" => comptrol_consent::human_action::HumanActionResolution::Approved,
                "declined" => comptrol_consent::human_action::HumanActionResolution::Declined,
                _ => comptrol_consent::human_action::HumanActionResolution::TimedOut,
            };
            if action_id.is_empty() {
                json!({ "error": { "code": "invalid_input", "message": "human_action.resolve needs action_id" } })
            } else if runtime.human_actions.resolve(action_id, resolution.clone()) {
                json!({ "content": [{ "type": "text", "text": serde_json::to_string(&json!({
                    "resolved": true,
                    "action_id": action_id,
                    "resolution": resolution_str,
                    "note": "Human action recorded. Retry the original operation to continue."
                })).unwrap_or_default() }], "structuredContent": json!({
                    "resolved": true,
                    "action_id": action_id,
                    "resolution": resolution_str,
                    "note": "Human action recorded. Retry the original operation to continue."
                }) })
            } else {
                json!({ "error": { "code": "action_not_found", "message": format!("No pending human action with id {action_id}") } })
            }
        }
        _ => json!({ "error": { "code": "tool_not_found", "message": format!("Unknown tool {name}") } }),
    };
    if name == "operate" {
        let server_call_ms = call_started.elapsed().as_secs_f64() * 1000.0;
        let process_uptime_ms = PROCESS_STARTED
            .get()
            .map(|start| start.elapsed().as_millis());
        let action_and_verification_ms = value
            .pointer("/data/_comptrol_timing/action_and_verification_ms")
            .cloned();
        let intent = value["intent"].as_str().unwrap_or_default().to_owned();
        let app_launch_timings = value.pointer("/data/timings_ms").cloned();
        let focus_timings = value.pointer("/data/window/timings_ms").cloned();
        let retry_count = value
            .pointer("/data/retry_count")
            .cloned()
            .unwrap_or(json!(0));
        let user_intervention = value
            .pointer("/data/awaiting_human_action")
            .cloned()
            .unwrap_or(Value::Null);
        let queue_wait = value
            .pointer("/timings/queue_wait_ms")
            .cloned()
            .unwrap_or(json!(0));
        if let Some(data) = value.get_mut("data").and_then(Value::as_object_mut) {
            data.remove("_comptrol_timing");
        }
        let action_ms = action_and_verification_ms.as_ref().and_then(Value::as_f64);
        if let Some(timings) = value.get_mut("timings").and_then(Value::as_object_mut) {
            let engine_ms = timings
                .get("runtime_operate_ms")
                .cloned()
                .unwrap_or(Value::Null);
            let engine_total = engine_ms.as_f64();
            timings.insert("server_call_ms".to_owned(), json!(server_call_ms));
            timings.insert("engine_total_ms".to_owned(), engine_ms.clone());
            timings.insert(
                "action_and_verification_ms".to_owned(),
                action_and_verification_ms.clone().unwrap_or(Value::Null),
            );
            timings.insert(
                "engine_overhead_ms".to_owned(),
                json!(
                    engine_total
                        .zip(action_ms)
                        .map(|(total, action)| (total - action).max(0.0))
                ),
            );
            timings.insert("queue_wait_ms".to_owned(), json!(queue_wait_ms));
            timings.insert(
                "daemon_startup_ms".to_owned(),
                json!(STDIO_READY_MS.load(Ordering::Acquire)),
            );
            timings.insert("process_uptime_ms".to_owned(), json!(process_uptime_ms));
            timings.insert(
                "process_state".to_owned(),
                json!(if call_number == 0 {
                    "first_operation_after_start"
                } else {
                    "warm"
                }),
            );
            timings.insert("retry_count".to_owned(), retry_count);
            timings.insert("user_intervention".to_owned(), user_intervention);
            timings.insert("app_network_ms".to_owned(), Value::Null);
            timings.insert("client_model_ms".to_owned(), client_model_ms);
            timings.insert("human_prompt_ms".to_owned(), human_prompt_ms);
            timings.insert("perceived_elapsed_ms".to_owned(), perceived_elapsed_ms);
            let route_ms = action_and_verification_ms.clone().unwrap_or(Value::Null);
            let browser_route = intent.starts_with("browser.");
            let application_route = intent.starts_with("app.") || intent.starts_with("blender.");
            timings.insert("phase_spans_ms".to_owned(), json!({
                "queue": queue_wait,
                "startup": STDIO_READY_MS.load(Ordering::Acquire),
                "app_resolution": app_launch_timings.as_ref().and_then(|v| v.get("app_resolution_ms")).cloned(),
                "launch_and_settle": app_launch_timings.as_ref().and_then(|v| v.pointer("/launch/launch_and_settle_ms")).cloned(),
                "window_resolution": focus_timings.as_ref().and_then(|v| v.get("window_resolution_ms")).cloned(),
                "foreground_activation_and_verification": focus_timings.as_ref().and_then(|v| v.get("foreground_activation_and_verification_ms")).cloned(),
                "adapter_host_startup": app_launch_timings.as_ref().and_then(|v| v.get("adapter_host_startup_ms")).cloned(),
                "adapter_execution_and_verification": app_launch_timings.as_ref().and_then(|v| v.get("adapter_execution_and_verification_ms")).cloned(),
                "content_readiness": null,
                "observation": if matches!(intent.as_str(), "desktop.observe" | "browser.session.list") { action_and_verification_ms.clone().unwrap_or(Value::Null) } else { Value::Null },
                "network_route_inclusive": if browser_route { route_ms.clone() } else { Value::Null },
                "application_route_inclusive": if application_route { route_ms.clone() } else { Value::Null },
                "action_and_verification": action_and_verification_ms.clone().unwrap_or(Value::Null),
                "process_identity_verification": app_launch_timings.as_ref().and_then(|v| v.pointer("/launch/process_identity_verification_ms")).cloned(),
                "recovery": if matches!(intent.as_str(), "reconcile" | "restore_checkpoint" | "browser.chrome.restore_recent") { route_ms.clone() } else { Value::Null }
            }));
        }
        let mut detailed_timings = value.get("timings").cloned().unwrap_or_else(|| json!({}));
        let mut compact_timings = json!({
            "runtime_total_ms": detailed_timings["runtime_total_ms"],
            "runtime_operate_ms": detailed_timings["runtime_operate_ms"],
            "server_call_ms": detailed_timings["server_call_ms"],
            "action_and_verification_ms": detailed_timings["action_and_verification_ms"],
            "queue_wait_ms": detailed_timings["queue_wait_ms"],
            "process_state": detailed_timings["process_state"]
        });
        for field in ["client_model_ms", "human_prompt_ms", "perceived_elapsed_ms"] {
            if !detailed_timings[field].is_null() {
                compact_timings[field] = detailed_timings[field].clone();
            }
        }
        let trace_id = value.get("operation_id").cloned();
        if let Some(object) = value.as_object_mut() {
            object.insert("timings".to_owned(), compact_timings);
            // P4.6: trace_id for MCP trace spans (inspect kind:events with
            // kind "trace.span" returns the per-step spans for this trace).
            if let Some(trace_id) = trace_id {
                object.insert("trace_id".to_owned(), trace_id);
            }
        }
        let measured_bytes = serde_json::to_vec(&value).map_or(0, |bytes| bytes.len());
        if let Some(timings) = value.get_mut("timings").and_then(Value::as_object_mut) {
            timings.insert("output_bytes".to_owned(), json!(measured_bytes));
            timings.insert(
                "output_tokens_estimate".to_owned(),
                json!(measured_bytes.div_ceil(4)),
            );
        }
        let final_bytes = serde_json::to_vec(&value).map_or(measured_bytes, |bytes| bytes.len());
        if let Some(timings) = value.get_mut("timings").and_then(Value::as_object_mut) {
            timings.insert("output_bytes".to_owned(), json!(final_bytes));
            timings.insert(
                "output_tokens_estimate".to_owned(),
                json!(final_bytes.div_ceil(4)),
            );
        }
        if let Some(full) = detailed_timings.as_object_mut() {
            full.insert("output_bytes".to_owned(), json!(final_bytes));
            full.insert(
                "output_tokens_estimate".to_owned(),
                json!(final_bytes.div_ceil(4)),
            );
        }
        append_timing_trace(&value, &detailed_timings, workflow_id.as_deref());
    }
    // MCP structuredContent must be a JSON object. Capabilities and route
    // inspection are naturally lists, so preserve their shape under a named
    // field while keeping the text content backward compatible.
    let structured_content = structured_content_value(&value);
    json!({ "content": [{ "type": "text", "text": serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned()) }], "structuredContent": structured_content })
}

fn external_duration_ms(value: Option<&Value>) -> Value {
    value
        .and_then(Value::as_f64)
        .filter(|milliseconds| {
            milliseconds.is_finite() && (0.0..=86_400_000.0).contains(milliseconds)
        })
        .map_or(Value::Null, |milliseconds| json!(milliseconds))
}

fn append_timing_trace(value: &Value, timings: &Value, workflow_id: Option<&str>) {
    let Some(path) = env::var_os("COMPTROL_TIMING_TRACE_PATH") else {
        return;
    };
    append_timing_trace_to(Path::new(&path), value, timings, workflow_id);
}

fn append_timing_trace_to(path: &Path, value: &Value, timings: &Value, workflow_id: Option<&str>) {
    let record = json!({
        "kind":"comptrol_timing",
        "monotonic_offset_ms": PROCESS_STARTED.get().map(|start| start.elapsed().as_secs_f64() * 1000.0),
        "intent":value["intent"],
        "route":value["route"],
        "verification":value["verification"],
        "workflow_id":workflow_id,
        "timings":timings
    });
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{record}"));
    if let Err(error) = result {
        eprintln!("comptrol timing trace append failed: {error}");
    }
}

fn structured_content_value(value: &Value) -> Value {
    if value.is_object() {
        value.clone()
    } else {
        json!({ "result": value })
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod mcp_result_tests {
    use super::{
        append_timing_trace_to, browser_bridge_poll_wait_ms, call_tool_with_cancel,
        structured_content_value,
    };
    use comptrol::{Runtime, capability_catalog};
    use serde_json::json;

    #[test]
    fn structured_content_is_always_an_object_without_changing_list_shape() {
        let list = json!([{"name":"one"},{"name":"two"}]);
        assert_eq!(structured_content_value(&list), json!({"result":list}));

        let object = json!({"status":"ready"});
        assert_eq!(structured_content_value(&object), object);
    }

    #[test]
    fn browser_bridge_poll_wait_is_bounded() {
        assert_eq!(browser_bridge_poll_wait_ms(&json!({"wait_ms": 500})), 500);
        assert_eq!(
            browser_bridge_poll_wait_ms(&json!({"wait_ms": 50_000})),
            800
        );
        assert_eq!(browser_bridge_poll_wait_ms(&json!({"wait_ms": -1})), 0);
        assert_eq!(browser_bridge_poll_wait_ms(&json!({})), 0);
    }

    #[test]
    fn operate_response_has_compact_measured_timing_and_no_private_fields() {
        let state = std::env::temp_dir().join(format!(
            "comptrol-stage1-timing-{}-{}",
            std::process::id(),
            super::now_ms()
        ));
        let mut runtime = Runtime::new(state.clone()).expect("runtime");
        let result = call_tool_with_cancel(
            &mut runtime,
            json!({"name":"operate","arguments":{"intent":"system.ping","params":{}},"_meta":{"comptrolTiming":{"client_model_ms":12.5,"human_prompt_ms":3.0,"perceived_elapsed_ms":40.0}}}),
            None,
            12.5,
        );
        let structured = &result["structuredContent"];
        for field in [
            "runtime_total_ms",
            "runtime_operate_ms",
            "server_call_ms",
            "action_and_verification_ms",
            "queue_wait_ms",
            "process_state",
            "output_bytes",
            "output_tokens_estimate",
        ] {
            assert!(
                !structured["timings"][field].is_null(),
                "missing timing {field}"
            );
        }
        assert!(structured["data"].get("_comptrol_timing").is_none());
        assert!(structured["timings"].get("phase_spans_ms").is_none());
        assert!(structured["timings"].get("engine_overhead_ms").is_none());
        assert_eq!(structured["timings"]["client_model_ms"], json!(12.5));
        assert_eq!(structured["timings"]["human_prompt_ms"], json!(3.0));
        assert_eq!(structured["timings"]["perceived_elapsed_ms"], json!(40.0));
        assert_eq!(structured["timings"]["queue_wait_ms"], json!(12.5));
        drop(runtime);
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn capability_catalog_separates_callable_intents_from_platform_observations() {
        let catalog = capability_catalog();
        assert!(catalog["intents"].as_array().is_some());
        assert!(catalog["platform_observations"].as_array().is_some());
        // `platform.broker.observe` is a callable intent that happens to
        // share the prefix, so classification uses the explicit observation
        // list rather than a name prefix.
        assert!(
            catalog["intents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| { item["name"].as_str() == Some("platform.broker.observe") })
        );
        assert!(catalog["intents"].as_array().unwrap().iter().all(|item| {
            !comptrol::PLATFORM_OBSERVATIONS.contains(&item["name"].as_str().unwrap_or_default())
        }));
        assert!(catalog["capability_families"].as_array().is_some());
        assert!(
            catalog["capability_families"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| {
                    comptrol::CAPABILITY_FAMILIES
                        .contains(&item["name"].as_str().unwrap_or_default())
                })
        );
        assert!(catalog["intents"].as_array().unwrap().iter().all(|item| {
            !comptrol::CAPABILITY_FAMILIES.contains(&item["name"].as_str().unwrap_or_default())
        }));
        assert!(
            catalog["platform_observations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| {
                    item["name"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("platform.")
                })
        );
    }

    #[test]
    fn opt_in_timing_trace_contains_no_request_or_result_content() {
        let path = std::env::temp_dir().join(format!(
            "comptrol-stage1-trace-{}-{}.jsonl",
            std::process::id(),
            super::now_ms()
        ));
        append_timing_trace_to(
            &path,
            &json!({
                "intent":"system.ping","route":"native","verification":"verified",
                "data":{"secret":"must not be recorded"},"timings":{"server_call_ms":1.5}
            }),
            &json!({"server_call_ms":1.5,"phase_spans_ms":{"queue":0,"recovery":null}}),
            Some("wf-123"),
        );
        let contents = std::fs::read_to_string(&path).expect("trace was written");
        let record: serde_json::Value = serde_json::from_str(contents.trim()).expect("valid JSONL");
        assert_eq!(record["kind"], "comptrol_timing");
        assert_eq!(record["timings"]["server_call_ms"], 1.5);
        assert!(record["timings"]["phase_spans_ms"].is_object());
        assert_eq!(record["workflow_id"], "wf-123");
        assert!(!contents.contains("must not be recorded"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mcp_schema_tool_and_pre_dispatch_error_share_the_same_definition() {
        // desktop.notify is deliberately used here: it is a non-default
        // intent with NO browser-autostart linkage. A browser intent (the
        // historical choice) made this test depend on ambient Chrome state:
        // `operate` auto-starts Chrome for browser intents and then refreshes
        // the policy from the environment, which re-granted the intent behind
        // the test's back and could even spawn a real browser window.
        let intent = "desktop.notify";
        let state = std::env::temp_dir().join(format!(
            "comptrol-stage1-schema-{}-{}",
            std::process::id(),
            super::now_ms()
        ));
        let mut runtime = Runtime::new(state.clone()).expect("runtime");
        let schema = call_tool_with_cancel(
            &mut runtime,
            json!({"name":"intent_schema","arguments":{"intent":intent}}),
            None,
            0.0,
        );
        assert_eq!(
            schema["structuredContent"]["params"]["required"][0],
            "title"
        );
        // The policy gate runs before schema validation, so a forbidden
        // intent is refused as policy_denied without describing the
        // parameters it would have accepted. Grant the intent first to
        // reach the shared schema definition.
        let denied = call_tool_with_cancel(
            &mut runtime,
            json!({"name":"operate","arguments":{"intent":intent,"params":{}}}),
            None,
            0.0,
        );
        assert_eq!(
            denied["structuredContent"]["error"]["code"],
            "policy_denied"
        );
        // Grant only the intent: desktop.notify is R1, within the default
        // risk ceiling, and needs no consent or extra gates.
        runtime.policy.allowed_intents.insert(intent.to_owned());
        let invalid = call_tool_with_cancel(
            &mut runtime,
            json!({"name":"operate","arguments":{"intent":intent,"params":{}}}),
            None,
            0.0,
        );
        assert_eq!(
            invalid["structuredContent"]["error"]["code"],
            "invalid_input"
        );
        assert!(
            invalid["structuredContent"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("minimal example")
        );
        drop(runtime);
        let _ = std::fs::remove_dir_all(state);
    }
}

fn run_inspect(kind: &str) -> Value {
    match Runtime::new(default_state_dir()) {
        Ok(mut runtime) => runtime.inspect(kind),
        Err(error) => json!({ "error": error.to_string() }),
    }
}

fn resolve_human_action_cli(args: Vec<String>) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: comptrol resolve-action <action_id> <approved|declined>");
        return 1;
    }
    let action_id = &args[0];
    let resolution = match args[1].as_str() {
        "approved" => comptrol_consent::human_action::HumanActionResolution::Approved,
        "declined" => comptrol_consent::human_action::HumanActionResolution::Declined,
        _ => {
            eprintln!("resolution must be 'approved' or 'declined'");
            return 1;
        }
    };
    let mut runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    if runtime.human_actions.resolve(action_id, resolution) {
        println!("resolved");
        0
    } else {
        eprintln!("no pending human action with id {action_id}");
        1
    }
}

fn change_stop(stop: bool) -> i32 {
    let runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    let result = if stop {
        runtime.stop.engage()
    } else {
        runtime.stop.resume()
    };
    match result {
        Ok(()) => {
            println!("{}", if stop { "stopped" } else { "resumed" });
            0
        }
        Err(error) => {
            eprintln!("stop state failed: {error}");
            1
        }
    }
}

fn run_http(port: u16) -> i32 {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("http bind failed: {error}");
            return 1;
        }
    };
    eprintln!("comptrol Streamable HTTP preview listening on 127.0.0.1:{port}/mcp");
    let runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    let http_state = match HttpStore::open(&default_state_dir()) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("HTTP state startup failed: {error}");
            return 1;
        }
    };
    let runtime = Arc::new(Mutex::new(runtime));
    let tasks = match TaskStore::open(&default_state_dir()) {
        Ok(store) => Arc::new(TaskManager::new(Arc::clone(&runtime), store)),
        Err(error) => {
            eprintln!("task store startup failed: {error}");
            return 1;
        }
    };
    let http_state = Arc::new(http_state);
    if let Err(error) = comptrol::browser_bridge::ensure_auth_token(&default_state_dir()) {
        eprintln!("browser bridge auth startup failed: {error}");
        return 1;
    }
    let command_queue = match BridgeStore::open(&default_state_dir()) {
        Ok(store) => Arc::new(Mutex::new(store)),
        Err(error) => {
            eprintln!("browser bridge state startup failed: {error}");
            return 1;
        }
    };
    let active_connections = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let active = active_connections.fetch_add(1, Ordering::AcqRel) + 1;
                if active > HTTP_MAX_CONNECTIONS {
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    let _ = write_http_response(
                        &mut stream,
                        503,
                        "Service Unavailable",
                        "application/json",
                        serde_json::to_vec(&json!({"error":"too_many_connections"}))
                            .unwrap_or_default(),
                        None,
                    );
                    continue;
                }
                let runtime = Arc::clone(&runtime);
                let tasks = Arc::clone(&tasks);
                let http_state = Arc::clone(&http_state);
                let command_queue = Arc::clone(&command_queue);
                let active_connections = Arc::clone(&active_connections);
                thread::spawn(move || {
                    let pairing_store = Arc::new(Mutex::new(
                        PairingStore::open(&default_state_dir()).unwrap(),
                    ));
                    if let Err(error) = handle_http(
                        &mut stream,
                        &runtime,
                        &tasks,
                        &http_state,
                        pairing_store,
                        None,
                        &command_queue,
                    ) {
                        eprintln!("http request failed: {error}");
                    }
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                });
            }
            Err(error) => eprintln!("http accept failed: {error}"),
        }
    }
    0
}

fn load_certificates(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path)?;
    certs(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

fn load_private_key(path: &Path) -> io::Result<PrivateKeyDer<'static>> {
    let file = File::open(path)?;
    private_key(&mut BufReader::new(file))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no private key found"))
}

fn is_mutation_method(method: &str) -> bool {
    matches!(
        method,
        "tools/call"
            | "workflow.execute"
            | "trace.replay"
            | "browser.session.ensure_state"
            | "browser.cdp.navigate"
            | "browser.cdp.dialog"
            | "browser.upload.stage"
            | "browser.download.verify"
            | "desktop.settings.write"
            | "file.write"
            | "terminal.execute"
    )
}

fn scope_for_method(method: &str, params: Option<&Value>) -> &'static str {
    // For tools/call, extract the actual tool name and derive scope from the operation
    if method == "tools/call"
        && let Some(params) = params
        && let Some(name) = params.get("name").and_then(Value::as_str)
    {
        return scope_for_tool(name, params.get("arguments"));
    }
    if method == "tools/call" {
        return "semantic_input"; // fallback
    }
    match method {
        "workflow.execute" | "trace.replay" => "semantic_input",
        "browser.session.ensure_state"
        | "browser.cdp.navigate"
        | "browser.cdp.dialog"
        | "browser.upload.stage"
        | "browser.download.verify" => "accessibility_read",
        "desktop.settings.write" | "file.write" => "file_write",
        "terminal.execute" => "terminal",
        _ => "observe",
    }
}

fn scope_for_tool(tool_name: &str, arguments: Option<&Value>) -> &'static str {
    match tool_name {
        "operate" => {
            // Extract intent from OperationRequest arguments
            if let Some(args) = arguments
                && let Some(intent) = args.get("intent").and_then(Value::as_str)
            {
                return scope_for_intent(intent);
            }
            "semantic_input"
        }
        "inspect" => "observe",
        "watch" | "reconcile" => "observe",
        "restore_checkpoint" => "file_read",
        "capabilities" => "observe",
        _ => "semantic_input",
    }
}

fn scope_for_intent(intent: &str) -> &'static str {
    // Map intents to their required scopes
    match intent {
        // Browser intents
        i if i.starts_with("browser.") => "accessibility_read",
        // Desktop intents
        i if i.starts_with("desktop.") && (i.contains("write") || i.contains("settings")) => {
            "file_write"
        }
        i if i.starts_with("desktop.") => "accessibility_read",
        // File intents
        i if i.starts_with("file.write") => "file_write",
        i if i.starts_with("file.read") => "file_read",
        // Terminal intents
        i if i.starts_with("terminal.") => "terminal",
        // Workflow intents
        i if i.starts_with("workflow.") => "semantic_input",
        // Trace intents
        i if i.starts_with("trace.") => "semantic_input",
        // Other intents default to semantic_input
        _ => "semantic_input",
    }
}

fn mtls_config() -> io::Result<Arc<ServerConfig>> {
    let cert_path = env::var_os("COMPTROL_MTLS_CERT")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "COMPTROL_MTLS_CERT is required",
            )
        })?;
    let key_path = env::var_os("COMPTROL_MTLS_KEY")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "COMPTROL_MTLS_KEY is required")
        })?;
    let client_ca_path = env::var_os("COMPTROL_MTLS_CLIENT_CA")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "COMPTROL_MTLS_CLIENT_CA is required",
            )
        })?;
    let mut roots = rustls::RootCertStore::empty();
    for certificate in load_certificates(&client_ca_path)? {
        roots
            .add(certificate)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(load_certificates(&cert_path)?, load_private_key(&key_path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    Ok(Arc::new(config))
}

fn run_mtls(port: u16) -> i32 {
    let bind = env::var("COMPTROL_MTLS_BIND").unwrap_or_else(|_| "0.0.0.0".to_owned());
    let listener = match TcpListener::bind((bind.as_str(), port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("mTLS bind failed: {error}");
            return 1;
        }
    };
    let config = match mtls_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("mTLS configuration failed: {error}");
            return 1;
        }
    };
    let runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => Arc::new(Mutex::new(runtime)),
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    let tasks = match TaskStore::open(&default_state_dir()) {
        Ok(store) => Arc::new(TaskManager::new(Arc::clone(&runtime), store)),
        Err(error) => {
            eprintln!("task store startup failed: {error}");
            return 1;
        }
    };
    let http_state = match HttpStore::open(&default_state_dir()) {
        Ok(state) => Arc::new(state),
        Err(error) => {
            eprintln!("HTTP state startup failed: {error}");
            return 1;
        }
    };
    let pairing_store = match PairingStore::open(&default_state_dir()) {
        Ok(store) => Arc::new(Mutex::new(store)),
        Err(error) => {
            eprintln!("pairing store startup failed: {error}");
            return 1;
        }
    };
    if let Err(error) = comptrol::browser_bridge::ensure_auth_token(&default_state_dir()) {
        eprintln!("browser bridge auth startup failed: {error}");
        return 1;
    }
    let command_queue = match BridgeStore::open(&default_state_dir()) {
        Ok(store) => Arc::new(Mutex::new(store)),
        Err(error) => {
            eprintln!("browser bridge state startup failed: {error}");
            return 1;
        }
    };
    eprintln!("comptrol mutual-TLS HTTP listening on {bind}:{port}/mcp");
    let active_connections = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        if active_connections.fetch_add(1, Ordering::AcqRel) + 1 > HTTP_MAX_CONNECTIONS {
            active_connections.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let config = Arc::clone(&config);
        let runtime = Arc::clone(&runtime);
        let tasks = Arc::clone(&tasks);
        let http_state = Arc::clone(&http_state);
        let pairing_store = Arc::clone(&pairing_store);
        let command_queue = Arc::clone(&command_queue);
        let active_connections = Arc::clone(&active_connections);
        thread::spawn(move || {
            let result = (|| -> io::Result<()> {
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(10)))?;
                let mut connection = rustls::ServerConnection::new(config).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?;
                while connection.is_handshaking() {
                    connection.complete_io(&mut stream)?;
                }
                let peer_cert_fingerprint = connection
                    .peer_certificates()
                    .map(|certs| {
                        certs
                            .first()
                            .map(|der| {
                                let mut hasher = Sha256::new();
                                hasher.update(der.as_ref());
                                let digest = hasher.finalize();
                                digest
                                    .iter()
                                    .map(|byte| format!("{byte:02x}").to_string())
                                    .collect::<String>()
                            })
                            .unwrap_or_default()
                    })
                    .filter(|fp| !fp.is_empty());
                let mut tls = rustls::StreamOwned::new(connection, stream);
                handle_http(
                    &mut tls,
                    &runtime,
                    &tasks,
                    &http_state,
                    Arc::clone(&pairing_store),
                    peer_cert_fingerprint,
                    &command_queue,
                )
            })();
            if let Err(error) = result {
                eprintln!("mTLS request failed: {error}");
            }
            active_connections.fetch_sub(1, Ordering::AcqRel);
        });
    }
    0
}

#[cfg(windows)]
fn daemon_pipe_name() -> String {
    env::var("COMPTROL_PIPE_NAME").unwrap_or_else(|_| r"\\.\pipe\comptrol".to_owned())
}

#[cfg(windows)]
fn wide_pipe_name(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(unix)]
fn daemon_socket_path() -> PathBuf {
    env::var_os("COMPTROL_SOCKET_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_state_dir().join("comptrol.sock"))
}

#[cfg(unix)]
fn run_daemon() -> i32 {
    use std::os::unix::fs::FileTypeExt;

    let path = daemon_socket_path();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("daemon state directory failed: {error}");
        return 1;
    }
    if path.exists() {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => {}
            Ok(_) => {
                eprintln!("daemon socket path is not a socket");
                return 1;
            }
            Err(error) => {
                eprintln!("daemon socket path cannot be inspected: {error}");
                return 1;
            }
        }
        if UnixStream::connect(&path).is_ok() {
            eprintln!("daemon is already running");
            return 1;
        }
        if let Err(error) = std::fs::remove_file(&path) {
            eprintln!("stale daemon socket cannot be removed: {error}");
            return 1;
        }
    }
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("daemon socket bind failed: {error}");
            return 1;
        }
    };
    if let Err(error) = restrict_socket(&path) {
        eprintln!("daemon socket permissions failed: {error}");
        return 1;
    }
    let runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    let tasks = match TaskStore::open(&default_state_dir()) {
        Ok(tasks) => tasks,
        Err(error) => {
            eprintln!("task store startup failed: {error}");
            return 1;
        }
    };
    let runtime = Arc::new(Mutex::new(runtime));
    let tasks = Arc::new(TaskManager::new(Arc::clone(&runtime), tasks));
    // P3.3: doctor sees this daemon as the resident runtime owner.
    comptrol::DAEMON_RESIDENT.store(true, Ordering::Relaxed);
    eprintln!("comptrol daemon listening on {}", path.display());
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                // P3.2: one thread per MCP client; the Runtime/TaskStore stay
                // single-owner behind their mutexes. A slow or idle client
                // must never block the others.
                let runtime = Arc::clone(&runtime);
                let tasks = Arc::clone(&tasks);
                thread::spawn(move || {
                    let _clients = DaemonClientGuard::new();
                    if let Err(error) = handle_ipc_connection(&mut stream, &runtime, &tasks) {
                        eprintln!("daemon connection failed: {error}");
                    }
                });
            }
            Err(error) => eprintln!("daemon accept failed: {error}"),
        }
    }
    0
}

/// P3.3: tracks live MCP client connections for doctor's `clients` count.
struct DaemonClientGuard;

impl DaemonClientGuard {
    fn new() -> Self {
        comptrol::DAEMON_CLIENTS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for DaemonClientGuard {
    fn drop(&mut self) {
        comptrol::DAEMON_CLIENTS.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(not(unix))]
fn run_daemon() -> i32 {
    #[cfg(windows)]
    {
        // A per-pipe mutex ensures racing MCP launchers converge on one broker
        // and one Runtime/SQLite owner. Keep its handle alive until shutdown.
        let identity = Sha256::digest(daemon_pipe_name().as_bytes());
        let mutex_name = format!("Local\\ComptrolDaemon-{}", hex_digest(&identity));
        let mutex_name_wide: Vec<u16> = mutex_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name_wide.as_ptr()) };
        if mutex.is_null() {
            eprintln!(
                "daemon singleton mutex creation failed: {}",
                io::Error::last_os_error()
            );
            return 1;
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            eprintln!("daemon is already running");
            unsafe { CloseHandle(mutex) };
            return 1;
        }
        let pipe_name = wide_pipe_name(&daemon_pipe_name());
        let runtime = match Runtime::new(default_state_dir()) {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("startup failed: {error}");
                return 1;
            }
        };
        let tasks = match TaskStore::open(&default_state_dir()) {
            Ok(tasks) => tasks,
            Err(error) => {
                eprintln!("task store startup failed: {error}");
                return 1;
            }
        };
        let runtime = Arc::new(Mutex::new(runtime));
        let tasks = Arc::new(TaskManager::new(Arc::clone(&runtime), tasks));
        // P3.3: doctor sees this daemon as the resident runtime owner.
        comptrol::DAEMON_RESIDENT.store(true, Ordering::Relaxed);
        eprintln!("comptrol daemon listening on {}", daemon_pipe_name());
        loop {
            let handle = unsafe {
                CreateNamedPipeW(
                    pipe_name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    (MAX_PROTOCOL_BYTES + 4) as u32,
                    (MAX_PROTOCOL_BYTES + 4) as u32,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                eprintln!(
                    "daemon named pipe creation failed: {}",
                    io::Error::last_os_error()
                );
                return 1;
            }
            let connected = unsafe {
                ConnectNamedPipe(handle, std::ptr::null_mut()) != 0
                    || GetLastError() == ERROR_PIPE_CONNECTED
            };
            let mut stream = unsafe { File::from_raw_handle(handle as RawHandle) };
            if connected {
                // P3.2: serve each pipe client on its own thread so multiple
                // MCP clients share the daemon concurrently. The next pipe
                // instance is created immediately in the accept loop.
                let runtime = Arc::clone(&runtime);
                let tasks = Arc::clone(&tasks);
                thread::spawn(move || {
                    let _clients = DaemonClientGuard::new();
                    if let Err(error) = handle_ipc_connection(&mut stream, &runtime, &tasks) {
                        eprintln!("daemon connection failed: {error}");
                    }
                });
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("daemon named pipe transport is not implemented on this platform");
        2
    }
}

#[cfg(windows)]
fn hex_digest(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(unix)]
fn run_daemon_health() -> i32 {
    let path = daemon_socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("daemon unavailable: {error}");
            return 1;
        }
    };
    if let Err(error) = write_ipc_frame(
        &mut stream,
        &json!({ "version": 1, "id": "health", "method": "health" }),
    ) {
        eprintln!("daemon health request failed: {error}");
        return 1;
    }
    match read_ipc_frame(&mut stream).and_then(|frame| {
        let frame = frame.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "missing daemon health response",
            )
        })?;
        serde_json::from_slice::<Value>(&frame)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }) {
        Ok(value) => print_json(value),
        Err(error) => {
            eprintln!("daemon health response failed: {error}");
            1
        }
    }
}

#[cfg(not(unix))]
fn run_daemon_health() -> i32 {
    #[cfg(windows)]
    {
        let name = wide_pipe_name(&daemon_pipe_name());
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            eprintln!("daemon unavailable: {}", io::Error::last_os_error());
            return 1;
        }
        let mut stream = unsafe { File::from_raw_handle(handle as RawHandle) };
        if let Err(error) = write_ipc_frame(
            &mut stream,
            &json!({ "version": 1, "id": "health", "method": "health" }),
        ) {
            eprintln!("daemon health request failed: {error}");
            return 1;
        }
        match read_ipc_frame(&mut stream).and_then(|frame| {
            let frame = frame.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "missing daemon health response",
                )
            })?;
            serde_json::from_slice::<Value>(&frame)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }) {
            Ok(value) => print_json(value),
            Err(error) => {
                eprintln!("daemon health response failed: {error}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("daemon named pipe transport is not implemented on this platform");
        2
    }
}

fn handle_ipc_connection<S: Read + Write>(
    stream: &mut S,
    runtime: &Arc<Mutex<Runtime>>,
    tasks: &Arc<TaskManager>,
) -> io::Result<()> {
    let mut tasks_enabled = false;
    while let Some(frame) = read_ipc_frame(stream)? {
        let request: Value = match serde_json::from_slice(&frame) {
            Ok(value) => value,
            Err(error) => {
                write_ipc_frame(
                    stream,
                    &json!({ "version": 1, "id": Value::Null, "error": { "code": "invalid_json", "message": error.to_string() } }),
                )?;
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        if request.get("version").and_then(Value::as_u64) != Some(1) {
            write_ipc_frame(
                stream,
                &json!({ "version": 1, "id": id, "error": { "code": "protocol_version_unsupported", "message": "IPC version 1 is required" } }),
            )?;
            continue;
        }
        match request.get("method").and_then(Value::as_str) {
            Some("health") => write_ipc_frame(
                stream,
                &json!({
                    "version": 1,
                    "id": id,
                    "result": {
                        "ready": true,
                        "server": SERVER_VERSION,
                        "protocol": PROTOCOL_VERSION,
                        // P3.3: daemon ownership + live client count.
                        "resident": comptrol::DAEMON_RESIDENT.load(Ordering::Relaxed),
                        "clients": comptrol::DAEMON_CLIENTS.load(Ordering::Relaxed),
                    }
                }),
            )?,
            Some("mcp") => {
                let raw_message = request.get("raw_message").and_then(Value::as_str);
                let message = request.get("message");
                if raw_message.is_none() && message.is_none() {
                    write_ipc_frame(
                        stream,
                        &json!({ "version": 1, "id": id, "error": { "code": "message_required", "message": "IPC mcp requests need a message" } }),
                    )?;
                    continue;
                }
                let line = match raw_message {
                    Some(raw_message) => raw_message.to_owned(),
                    None => serde_json::to_string(message.unwrap()).map_err(io::Error::other)?,
                };
                let mut notifications = Vec::new();
                let response = handle_message_with_state(
                    runtime,
                    Some(tasks),
                    &mut tasks_enabled,
                    &line,
                    |notification| notifications.push(notification),
                )
                .unwrap_or_else(|| json!({}));
                for notification in notifications {
                    write_ipc_frame(
                        stream,
                        &json!({ "version": 1, "id": id, "event": notification }),
                    )?;
                }
                write_ipc_frame(
                    stream,
                    &json!({ "version": 1, "id": id, "result": response }),
                )?;
            }
            Some(method) => write_ipc_frame(
                stream,
                &json!({ "version": 1, "id": id, "error": { "code": "method_not_found", "message": format!("Unknown IPC method {method}") } }),
            )?,
            None => write_ipc_frame(
                stream,
                &json!({ "version": 1, "id": id, "error": { "code": "method_required", "message": "IPC requests need a method" } }),
            )?,
        }
    }
    Ok(())
}

fn read_ipc_frame<S: Read>(stream: &mut S) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0_u8; 4];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_PROTOCOL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame exceeds the protocol limit",
        ));
    }
    let mut frame = vec![0_u8; length];
    stream.read_exact(&mut frame)?;
    Ok(Some(frame))
}

fn write_ipc_frame<S: Write>(stream: &mut S, value: &Value) -> io::Result<()> {
    let frame = serde_json::to_vec(value).map_err(io::Error::other)?;
    if frame.len() > MAX_PROTOCOL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC response exceeds the protocol limit",
        ));
    }
    stream.write_all(&(frame.len() as u32).to_be_bytes())?;
    stream.write_all(&frame)
}

#[cfg(unix)]
fn restrict_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

trait HttpStream: Read + Write {
    fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }

    fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

impl HttpStream for TcpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }
}

impl HttpStream for rustls::StreamOwned<rustls::ServerConnection, TcpStream> {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.get_ref().set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.get_ref().set_write_timeout(timeout)
    }
}

fn drain_wake_notice_locked(
    command_queue: &Arc<Mutex<BridgeStore>>,
) -> Option<comptrol::browser_bridge::WakeNotice> {
    let mut queue = command_queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    queue.drain_wake_notice().unwrap_or(None)
}

/// P2.1: how long a push stream stays open before the client reconnects.
const BROWSER_BRIDGE_STREAM_MAX_S: u64 = 300;

fn browser_bridge_poll_wait_ms(request_body: &Value) -> u64 {
    request_body
        .get("wait_ms")
        .and_then(Value::as_u64)
        .unwrap_or_default()
        .min(BROWSER_BRIDGE_POLL_MAX_WAIT_MS)
}

fn lease_browser_bridge_commands(
    command_queue: &Arc<Mutex<BridgeStore>>,
    wait: Duration,
) -> io::Result<Vec<BridgeCommand>> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        // NOTE: deliberately no heartbeat recording here. A lease that happens
        // to be polled is not proof the extension service worker is alive;
        // recording one made health read green while the command channel was
        // dead. Host liveness comes from native-host heartbeat posts, and
        // channel truth comes from completed bridge_ping round trips.
        let commands = {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            queue.lease_pending(64, Duration::from_secs(60))?
        };
        if !commands.is_empty() {
            return Ok(commands);
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(commands);
        }
        thread::sleep(Duration::from_millis(BROWSER_BRIDGE_POLL_INTERVAL_MS).min(deadline - now));
    }
}

fn handle_http<S: HttpStream>(
    stream: &mut S,
    runtime: &Arc<Mutex<Runtime>>,
    tasks: &Arc<TaskManager>,
    http_state: &Arc<HttpStore>,
    pairing_store: Arc<Mutex<PairingStore>>,
    peer_fingerprint: Option<String>,
    command_queue: &Arc<Mutex<BridgeStore>>,
) -> io::Result<()> {
    // ponytail: bounded local parser, replace with a full HTTP implementation before public network exposure
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut buffer = Vec::with_capacity(8192);
    let header_end = loop {
        let mut chunk = [0_u8; 8192];
        let size = stream.read(&mut chunk)?;
        if size == 0 {
            break None;
        }
        buffer.extend_from_slice(&chunk[..size]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break Some(position);
        }
        if buffer.len() > MAX_PROTOCOL_BYTES {
            break None;
        }
    };
    let Some(header_end) = header_end else {
        return write_http_error(stream, 413, "message_too_large");
    };
    let header = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let declared_length = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    if declared_length.is_some_and(|length| length > MAX_PROTOCOL_BYTES) {
        return write_http_error(stream, 413, "message_too_large");
    }
    let body_start = header_end + 4;
    let body_length = declared_length.unwrap_or(0);
    while buffer.len() < body_start + body_length {
        let mut chunk = [0_u8; 8192];
        let size = stream.read(&mut chunk)?;
        if size == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..size]);
    }
    if buffer.len() < body_start + body_length {
        return write_http_response(
            stream,
            400,
            "Bad Request",
            "application/json",
            serde_json::to_vec(&json!({"error":"incomplete_body"})).unwrap_or_default(),
            None,
        );
    }
    let body_end = body_start + body_length;
    let body = String::from_utf8_lossy(&buffer[body_start..body_end]);
    let header_value = |name: &str| {
        header.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    };
    let protocol_mode = match mcp::from_header(header_value("MCP-Protocol-Version")) {
        Ok(mode) => mode,
        Err(error) => {
            return write_http_response(
                stream,
                400,
                "Bad Request",
                "application/json",
                serde_json::to_vec(
                    &json!({"error":"protocol_version_unsupported","message":error}),
                )
                .unwrap_or_default(),
                None,
            );
        }
    };
    let current_protocol = protocol_mode == mcp::ProtocolMode::Current;
    let origin = header_value("Origin");
    let session = header_value("MCP-Session-Id");
    let last_event_id = match header_value("Last-Event-ID") {
        Some(value) => match value.parse::<u64>() {
            Ok(id) => Some(id),
            Err(_) => {
                return write_http_response(
                    stream,
                    400,
                    "Bad Request",
                    "application/json",
                    serde_json::to_vec(&json!({"error":"invalid_last_event_id"}))
                        .unwrap_or_default(),
                    None,
                );
            }
        },
        None => None,
    };
    let origin_ok = origin.is_none_or(|value| {
        matches!(
            value,
            "http://localhost" | "http://127.0.0.1" | "http://[::1]"
        )
    });
    let request_line = header.lines().next().unwrap_or_default();
    if !origin_ok {
        return write_http_response(
            stream,
            403,
            "Forbidden",
            "application/json",
            serde_json::to_vec(&json!({"error":"origin_denied"})).unwrap_or_default(),
            None,
        );
    }
    if request_line.starts_with("GET /dashboard ") {
        let mut runtime = runtime.lock().expect("runtime lock poisoned");
        return write_http_response(
            stream,
            200,
            "OK",
            "text/html; charset=utf-8",
            dashboard(&mut runtime).into_bytes(),
            None,
        );
    }
    if request_line.starts_with("DELETE /mcp ") {
        if current_protocol {
            return write_http_response(
                stream,
                405,
                "Method Not Allowed",
                "application/json",
                serde_json::to_vec(&json!({"error":"stateless_protocol_has_no_session"}))
                    .unwrap_or_default(),
                None,
            );
        }
        let Some(session) = session else {
            return write_http_response(
                stream,
                400,
                "Bad Request",
                "application/json",
                serde_json::to_vec(&json!({"error":"session_required"})).unwrap_or_default(),
                None,
            );
        };
        if !http_state.delete(session)? {
            return write_http_response(
                stream,
                404,
                "Not Found",
                "application/json",
                serde_json::to_vec(&json!({"error":"session_not_found"})).unwrap_or_default(),
                None,
            );
        }
        return write_http_response(
            stream,
            204,
            "No Content",
            "application/json",
            Vec::new(),
            None,
        );
    }
    if request_line.starts_with("GET /mcp ") {
        if current_protocol {
            return write_http_response(
                stream,
                405,
                "Method Not Allowed",
                "application/json",
                serde_json::to_vec(&json!({"error":"stateless_protocol_has_no_get_stream"}))
                    .unwrap_or_default(),
                None,
            );
        }
        let Some(session) = session else {
            return write_http_response(
                stream,
                400,
                "Bad Request",
                "application/json",
                serde_json::to_vec(&json!({"error":"session_required"})).unwrap_or_default(),
                None,
            );
        };
        if !http_state.contains(Some(session)) {
            return write_http_response(
                stream,
                404,
                "Not Found",
                "application/json",
                serde_json::to_vec(&json!({"error":"session_not_found"})).unwrap_or_default(),
                None,
            );
        }
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        write_sse_headers(stream)?;
        let mut cursor = last_event_id.unwrap_or_else(|| http_state.latest(session).unwrap_or(0));
        write_sse_event(stream, None, "ready", &json!({}))?;
        loop {
            match http_state.wait_for_events(
                session,
                cursor,
                Duration::from_millis(
                    env::var("COMPTROL_HTTP_STREAM_IDLE_MS")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(HTTP_STREAM_IDLE_MS),
                ),
            ) {
                HttpWait::Missing | HttpWait::Timeout => {
                    let _ = write_sse_end(stream);
                    return Ok(());
                }
                HttpWait::Events(events) => {
                    for event in events {
                        cursor = event.id;
                        write_sse_event(stream, Some(event.id), "message", &event.data)?;
                    }
                }
            }
        }
    }
    // ── Browser Bridge HTTP API (native messaging host → daemon) ──────────
    // Prove daemon identity before a native host sends the bearer token. The
    // challenge endpoint deliberately sits outside /browser/* authentication.
    if request_line.starts_with("POST /browser-auth/challenge ") {
        let request_body: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
        let nonce = request_body
            .get("nonce")
            .and_then(Value::as_str)
            .unwrap_or("");
        return match comptrol::browser_bridge::challenge_proof(&default_state_dir(), nonce) {
            Ok(proof) => write_http_response(
                stream,
                200,
                "OK",
                "application/json",
                serde_json::to_vec(&json!({
                    "ok": true,
                    "protocol": comptrol::browser_bridge::BRIDGE_PROTOCOL_VERSION,
                    "proof": proof
                }))
                .unwrap_or_default(),
                None,
            ),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => write_http_response(
                stream,
                400,
                "Bad Request",
                "application/json",
                serde_json::to_vec(&json!({
                    "ok": false,
                    "error": "invalid_browser_bridge_challenge",
                    "message": error.to_string()
                }))
                .unwrap_or_default(),
                None,
            ),
            Err(error) => write_http_response(
                stream,
                500,
                "Internal Server Error",
                "application/json",
                serde_json::to_vec(&json!({
                    "ok": false,
                    "error": "browser_bridge_auth_unavailable",
                    "message": error.to_string()
                }))
                .unwrap_or_default(),
                None,
            ),
        };
    }

    // Unauthenticated channel-truth summary. Intentionally before the signed
    // /browser/* section: doctor and tooling need liveness without holding the
    // bridge token, and the payload contains no user data (counts and states
    // only). Everything that submits work still requires a valid signature.
    if request_line.starts_with("GET /browser/healthz ") {
        let state_dir = default_state_dir();
        let report = BridgeStore::open(&state_dir).map(|store| {
            let host_health = store
                .health(comptrol::browser_bridge::DEFAULT_HEALTH_MAX_AGE)
                .ok();
            let channel = store
                .health_round_trip(comptrol::browser_bridge::DEFAULT_HEALTH_MAX_AGE)
                .ok();
            json!({
                "ok": channel.as_ref().map(|health| health.active).unwrap_or(false),
                "channel": channel,
                "host_heartbeat": host_health,
                // K5: page-op tier — a channel can answer pings while every
                // in-page command hangs; health reports both tiers.
                "page_ops": store.page_ops_health().ok(),
                "state": match (&host_health, &channel) {
                    (_, Some(health)) if health.active => "alive",
                    (Some(host), _) if host.active => "degraded_extension_unreachable",
                    _ => "down",
                }
            })
        });
        return write_http_response(
            stream,
            200,
            "OK",
            "application/json",
            serde_json::to_vec(
                &report.unwrap_or_else(|error| json!({ "ok": false, "error": error.to_string() })),
            )
            .unwrap_or_default(),
            None,
        );
    }

    let mut request_parts = request_line.split_whitespace();
    let request_method = request_parts.next().unwrap_or("");
    let request_path = request_parts.next().unwrap_or("");
    let bridge_request = request_path.starts_with("/browser/");
    if bridge_request {
        let expected_token = match comptrol::browser_bridge::ensure_auth_token(&default_state_dir())
        {
            Ok(token) => token,
            Err(error) => {
                return write_http_response(
                    stream,
                    500,
                    "Internal Server Error",
                    "application/json",
                    serde_json::to_vec(&json!({
                        "ok": false,
                        "error": "browser_bridge_auth_unavailable",
                        "message": error.to_string()
                    }))
                    .unwrap_or_default(),
                    None,
                );
            }
        };
        let nonce = header_value("X-Comptrol-Bridge-Nonce").unwrap_or("");
        let signature = header_value("X-Comptrol-Bridge-Signature").unwrap_or("");
        let signature_valid = comptrol::browser_bridge::verify_request_signature(
            &expected_token,
            request_method,
            request_path,
            nonce,
            body.as_bytes(),
            signature,
        )
        .unwrap_or(false);
        if !signature_valid {
            return write_http_response(
                stream,
                403,
                "Forbidden",
                "application/json",
                serde_json::to_vec(&json!({
                    "ok": false,
                    "error": "browser_bridge_auth_required"
                }))
                .unwrap_or_default(),
                None,
            );
        }
        let nonce_claimed = {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            queue.claim_auth_nonce(nonce)
        };
        match nonce_claimed {
            Ok(true) => {}
            Ok(false) => {
                return write_http_response(
                    stream,
                    409,
                    "Conflict",
                    "application/json",
                    serde_json::to_vec(&json!({
                        "ok": false,
                        "error": "browser_bridge_replay"
                    }))
                    .unwrap_or_default(),
                    None,
                );
            }
            Err(error) => {
                return write_http_response(
                    stream,
                    500,
                    "Internal Server Error",
                    "application/json",
                    serde_json::to_vec(&json!({
                        "ok": false,
                        "error": "browser_bridge_auth_unavailable",
                        "message": error.to_string()
                    }))
                    .unwrap_or_default(),
                    None,
                );
            }
        }
    }
    if request_line.starts_with("POST /browser/targets ") {
        let cdp_endpoint = std::env::var("COMPTROL_CDP_ENDPOINT").unwrap_or_default();
        if cdp_endpoint.is_empty() {
            return write_http_response(
                stream,
                503,
                "Service Unavailable",
                "application/json",
                serde_json::to_vec(&json!({"ok":false,"error":"browser_unavailable"}))
                    .unwrap_or_default(),
                None,
            );
        }
        return match comptrol::browser::discover_cached_targets(&cdp_endpoint) {
            Ok(targets) => {
                let list: Vec<Value> = targets
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "type": t.target_type,
                            "title": t.title,
                            "url": t.url,
                            "web_socket_url": t.web_socket_url,
                        })
                    })
                    .collect();
                write_http_response(
                    stream,
                    200,
                    "OK",
                    "application/json",
                    serde_json::to_vec(&json!({"ok":true,"targets":list})).unwrap_or_default(),
                    None,
                )
            }
            Err(error) => write_http_response(
                stream,
                502,
                "Bad Gateway",
                "application/json",
                serde_json::to_vec(&json!({"ok":false,"error":error.code,"message":error.message}))
                    .unwrap_or_default(),
                None,
            ),
        };
    }
    if request_line.starts_with("POST /browser/cdp/command ") {
        let cdp_endpoint = std::env::var("COMPTROL_CDP_ENDPOINT").unwrap_or_else(|_| {
            let active = command_queue
                .lock()
                .expect("command queue lock poisoned")
                .health(DEFAULT_HEALTH_MAX_AGE)
                .map(|health| health.active)
                .unwrap_or(false);
            if active {
                COMPANION_BRIDGE_ENDPOINT.to_owned()
            } else {
                String::new()
            }
        });
        if cdp_endpoint.is_empty() {
            return write_http_response(
                stream,
                503,
                "Service Unavailable",
                "application/json",
                serde_json::to_vec(&json!({"ok":false,"error":"browser_unavailable"}))
                    .unwrap_or_default(),
                None,
            );
        }
        let request_body: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                return write_http_response(
                    stream,
                    400,
                    "Bad Request",
                    "application/json",
                    serde_json::to_vec(&json!({"ok":false,"error":"invalid_json"}))
                        .unwrap_or_default(),
                    None,
                );
            }
        };
        let target_id = request_body
            .get("target_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let method = request_body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("");
        // CDP method allowlist: only safe read-only methods are permitted
        // through the unauthenticated HTTP bridge.
        // All mutation/navigate/evaluate/input methods require the normal
        // MCP intent/policy/consent/verification pipeline.
        const CDP_ALLOWLIST: &[&str] = &[
            // Page inspection (read-only)
            "Page.getFrameTree",
            "Page.captureScreenshot",
            // DOM inspection (read-only)
            "DOM.getDocument",
            "DOM.querySelector",
            "DOM.querySelectorAll",
            "DOM.getOuterHTML",
            "DOM.getBoxModel",
            // Runtime inspection (read-only, no evaluate)
            "Runtime.getProperties",
            "Runtime.consoleAPICalled",
            // Target listing (read-only)
            "Target.getTargets",
            // Browser info (read-only)
            "Browser.getVersion",
            // Console (read-only)
            "Console.enable",
            // Network inspection (read-only)
            "Network.enable",
            "Network.getResponseBody",
            "Network.getRequestPostData",
            // Accessibility (read-only)
            "Accessibility.getFullAXTree",
            // Overlay (visual feedback only, no DOM mutation)
            "Overlay.highlightNode",
            "Overlay.hideHighlight",
        ];
        if !CDP_ALLOWLIST.contains(&method) {
            return write_http_response(
                stream,
                403,
                "Forbidden",
                "application/json",
                serde_json::to_vec(&json!({
                    "ok":false,
                    "error":"method_not_allowed",
                    "message":format!("CDP method '{method}' is not allowed through the unauthenticated bridge"),
                    "recovery":"Use the MCP tools/call interface for methods not in the bridge allowlist"
                }))
                    .unwrap_or_default(),
                None,
            );
        }
        let params = request_body
            .get("params")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        return match comptrol::browser::cdp_call(
            &cdp_endpoint,
            target_id,
            None,
            None,
            method,
            params,
        ) {
            Ok(result) => write_http_response(
                stream,
                200,
                "OK",
                "application/json",
                serde_json::to_vec(&json!({"ok":true,"result":result})).unwrap_or_default(),
                None,
            ),
            Err(error) => write_http_response(
                stream,
                502,
                "Bad Gateway",
                "application/json",
                serde_json::to_vec(&json!({"ok":false,"error":error.code,"message":error.message}))
                    .unwrap_or_default(),
                None,
            ),
        };
    }
    if request_line.starts_with("POST /browser/debugger/attach ")
        || request_line.starts_with("POST /browser/debugger/detach ")
    {
        let request_body: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                return write_http_response(
                    stream,
                    400,
                    "Bad Request",
                    "application/json",
                    serde_json::to_vec(&json!({"ok":false,"error":"invalid_json"}))
                        .unwrap_or_default(),
                    None,
                );
            }
        };
        let target_id = request_body
            .get("target_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let command_type = if request_line.contains("/attach ") {
            "attach_debugger"
        } else {
            "detach_debugger"
        };
        let request_id = {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            match queue.submit(command_type, json!({"targetId": target_id})) {
                Ok(request_id) => request_id,
                Err(error) => {
                    return write_http_response(
                        stream,
                        503,
                        "Service Unavailable",
                        "application/json",
                        serde_json::to_vec(&json!({
                            "ok": false,
                            "error": "browser_bridge_queue_unavailable",
                            "message": error.to_string(),
                        }))
                        .unwrap_or_default(),
                        None,
                    );
                }
            }
        };
        return write_http_response(
            stream,
            202,
            "Accepted",
            "application/json",
            serde_json::to_vec(&json!({
                "ok": true,
                "request_id": request_id,
                "state": "pending",
                "message": format!("{} command queued for browser extension", command_type),
            }))
            .unwrap_or_default(),
            None,
        );
    }
    if request_line.starts_with("POST /browser/groups/restore ") {
        let request_body: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                return write_http_response(
                    stream,
                    400,
                    "Bad Request",
                    "application/json",
                    serde_json::to_vec(&json!({"ok":false,"error":"invalid_json"}))
                        .unwrap_or_default(),
                    None,
                );
            }
        };
        let group_id = request_body
            .get("group_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let request_id = {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            match queue.submit("restore_group", json!({"groupId": group_id})) {
                Ok(request_id) => request_id,
                Err(error) => {
                    return write_http_response(
                        stream,
                        503,
                        "Service Unavailable",
                        "application/json",
                        serde_json::to_vec(&json!({
                            "ok": false,
                            "error": "browser_bridge_queue_unavailable",
                            "message": error.to_string(),
                        }))
                        .unwrap_or_default(),
                        None,
                    );
                }
            }
        };
        return write_http_response(
            stream,
            202,
            "Accepted",
            "application/json",
            serde_json::to_vec(&json!({
                "ok": true,
                "request_id": request_id,
                "state": "pending",
                "message": "restore_group command queued for browser extension",
            }))
            .unwrap_or_default(),
            None,
        );
    }
    if request_line.starts_with("POST /browser/status ") {
        let cdp_endpoint = std::env::var("COMPTROL_CDP_ENDPOINT").unwrap_or_default();
        let cdp_connected = !cdp_endpoint.is_empty()
            && comptrol::browser::discover_cached_targets(&cdp_endpoint).is_ok();
        let bridge_health = {
            let queue = command_queue.lock().expect("command queue lock poisoned");
            queue.health(DEFAULT_HEALTH_MAX_AGE)
        };
        let (extension_connected, last_heartbeat_ms, target_count) = match bridge_health {
            Ok(health) => (health.active, health.last_heartbeat_ms, health.target_count),
            Err(_) => (false, None, 0),
        };
        let connected = cdp_connected || extension_connected;
        return write_http_response(
            stream,
            200,
            "OK",
            "application/json",
            serde_json::to_vec(&json!({
                "ok": true,
                "connected": connected,
                "cdp_connected": cdp_connected,
                "extension_connected": extension_connected,
                "last_heartbeat_ms": last_heartbeat_ms,
                "target_count": target_count
            }))
            .unwrap_or_default(),
            None,
        );
    }

    // Signed discovery over the extension bridge: returns the service
    // worker's pushed target list (real tabs in the user's real profile). If
    // the push is stale, submits a bounded get_targets request through the
    // command queue and waits for the extension's next targets_list push.
    // The wait releases the queue mutex so the host keeps polling; a timeout
    // returns degraded with whatever the last push held.
    if request_line.starts_with("POST /browser/discovery ") {
        const DISCOVERY_FRESH_MS: i64 = 15_000;
        const DISCOVERY_WAIT_MS: u64 = 5_000;
        const DISCOVERY_POLL_MS: u64 = 250;
        let submitted_at_ms = now_ms();
        let submit = {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            queue.submit("get_targets", json!({ "discovery": true }))
        };
        if let Err(error) = submit {
            return write_http_response(
                stream,
                503,
                "Service Unavailable",
                "application/json",
                serde_json::to_vec(&json!({"ok":false,"error":error.to_string()}))
                    .unwrap_or_default(),
                None,
            );
        }
        // Wait (without holding the queue mutex) for a targets push newer than
        // the submit; the extension answers get_targets by pushing its list.
        let deadline = Instant::now() + Duration::from_millis(DISCOVERY_WAIT_MS);
        loop {
            let push_ms = BridgeStore::open(&default_state_dir())
                .ok()
                .and_then(|store| store.last_targets_ms().ok().flatten());
            if push_ms
                .map(|ms| ms as u128 > submitted_at_ms)
                .unwrap_or(false)
            {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(DISCOVERY_POLL_MS));
        }
        let (targets, push_age_ms) = match BridgeStore::open(&default_state_dir()) {
            Ok(store) => {
                let push_ms = store.last_targets_ms().ok().flatten();
                let age = push_ms.map(|ms| now_ms().saturating_sub(ms as u128));
                (store.targets().unwrap_or_default(), age)
            }
            Err(error) => {
                return write_http_response(
                    stream,
                    502,
                    "Bad Gateway",
                    "application/json",
                    serde_json::to_vec(&json!({"ok":false,"error":error.to_string()}))
                        .unwrap_or_default(),
                    None,
                );
            }
        };
        let fresh = matches!(push_age_ms, Some(age) if age <= DISCOVERY_FRESH_MS as u128);
        return write_http_response(
            stream,
            200,
            "OK",
            "application/json",
            serde_json::to_vec(&json!({
                "ok": fresh,
                "fresh": fresh,
                "push_age_ms": push_age_ms,
                "count": targets.len(),
                "targets": targets
            }))
            .unwrap_or_default(),
            None,
        );
    }

    // Native-host liveness probe: the host submits a bridge_ping through the
    // real command channel; the extension answers over its native port and the
    // result flows back through /browser/command/result, which records the
    // measured round trip as channel truth. A completed probe therefore
    // proves the whole host -> Chrome -> service worker -> host path.
    if request_line.starts_with("POST /browser/probe ") {
        let request_id = {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            queue.submit("bridge_ping", json!({ "probe": true }))
        };
        return match request_id {
            Ok(request_id) => write_http_response(
                stream,
                200,
                "OK",
                "application/json",
                serde_json::to_vec(&json!({ "ok": true, "request_id": request_id }))
                    .unwrap_or_default(),
                None,
            ),
            Err(error) => write_http_response(
                stream,
                503,
                "Service Unavailable",
                "application/json",
                serde_json::to_vec(&json!({ "ok": false, "error": error.to_string() }))
                    .unwrap_or_default(),
                None,
            ),
        };
    }

    if request_line.starts_with("POST /browser/extension/heartbeat ") {
        let request_body: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
        let protocol = request_body.get("protocol").and_then(Value::as_str);
        let result = {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            queue.record_heartbeat(protocol)
        };
        return match result {
            Ok(()) => write_http_response(
                stream,
                200,
                "OK",
                "application/json",
                serde_json::to_vec(&json!({"ok": true})).unwrap_or_default(),
                None,
            ),
            Err(error) => write_http_response(
                stream,
                500,
                "Internal Server Error",
                "application/json",
                serde_json::to_vec(&json!({"ok": false, "error": error.to_string()}))
                    .unwrap_or_default(),
                None,
            ),
        };
    }

    // ── Browser Bridge extension event endpoints ──────────────────────────
    if request_line.starts_with("POST /browser/extension/targets ") {
        let request_body: Value = serde_json::from_str(&body).unwrap_or(json!({}));
        let targets = request_body
            .get("targets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let result = {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            let stored = queue.store_targets(&targets);
            let heartbeat = queue.record_heartbeat(Some("comptrol.browser.bridge/0.1.0"));
            stored.and_then(|count| heartbeat.map(|_| count))
        };
        return match result {
            Ok(count) => write_http_response(
                stream,
                200,
                "OK",
                "application/json",
                serde_json::to_vec(&json!({"ok": true, "targets_stored": true, "count": count}))
                    .unwrap_or_default(),
                None,
            ),
            Err(error) => write_http_response(
                stream,
                500,
                "Internal Server Error",
                "application/json",
                serde_json::to_vec(&json!({"ok": false, "error": error.to_string()}))
                    .unwrap_or_default(),
                None,
            ),
        };
    }
    if request_line.starts_with("POST /browser/extension/cdp_result ") {
        let request_body: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => json!({}),
        };
        let request_id = request_body
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !request_id.is_empty() {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            let result = if let Some(error) = request_body.get("error").filter(|v| !v.is_null()) {
                json!({"ok": false, "error": error})
            } else {
                json!({"ok": true, "result": request_body.get("result").cloned().unwrap_or(Value::Null)})
            };
            let _ = queue.store_result(request_id, result);
        }
        return write_http_response(
            stream,
            200,
            "OK",
            "application/json",
            serde_json::to_vec(&json!({"ok":true})).unwrap_or_default(),
            None,
        );
    }
    if request_line.starts_with("POST /browser/extension/event ") {
        let request_body: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => json!({}),
        };
        let request_id = request_body
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let event = request_body
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            let _ = queue.append_event(event, request_body.clone());
            let _ = queue.record_heartbeat(Some("comptrol.browser.bridge/0.1.0"));
            if !request_id.is_empty() {
                let result = if let Some(error) = request_body.get("error").filter(|v| !v.is_null())
                {
                    json!({"ok": false, "error": error, "event": event})
                } else {
                    json!({"ok": true, "event": event, "data": request_body.get("data").cloned().unwrap_or(Value::Null)})
                };
                let _ = queue.store_result(request_id, result);
            }
        }
        return write_http_response(
            stream,
            200,
            "OK",
            "application/json",
            serde_json::to_vec(&json!({"ok":true})).unwrap_or_default(),
            None,
        );
    }

    // ── Browser Bridge command queue endpoints ──────────────────────────────
    if request_line.starts_with("POST /browser/command/poll ") {
        let request_body: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
        let wait = Duration::from_millis(browser_bridge_poll_wait_ms(&request_body));
        let commands = lease_browser_bridge_commands(command_queue, wait);
        return match commands {
            Ok(commands) => write_http_response(
                stream,
                200,
                "OK",
                "application/json",
                serde_json::to_vec(&json!({
                    "ok": true,
                    "commands": commands,
                    "count": commands.len(),
                }))
                .unwrap_or_default(),
                None,
            ),
            Err(error) => write_http_response(
                stream,
                500,
                "Internal Server Error",
                "application/json",
                serde_json::to_vec(&json!({"ok": false, "error": error.to_string()}))
                    .unwrap_or_default(),
                None,
            ),
        };
    }
    // P2.1 push transport: one persistent chunked connection over which the
    // sidecar pushes commands the moment they are submitted (no poll gap),
    // with wake notices multiplexed on the same stream. The stream self-caps
    // at BROWSER_BRIDGE_STREAM_MAX_S and the client reconnects immediately;
    // POST /browser/command/poll remains the fallback for older hosts.
    if request_line.starts_with("POST /browser/command/stream ") {
        write_sse_headers(stream)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(BROWSER_BRIDGE_STREAM_MAX_S);
        let mut last_beat = std::time::Instant::now();
        loop {
            let commands =
                lease_browser_bridge_commands(command_queue, Duration::from_millis(250))?;
            for command in commands {
                write_sse_event(
                    stream,
                    None,
                    "command",
                    &json!({
                        "request_id": command.request_id,
                        "command_type": command.command_type,
                        "payload": command.payload,
                        "attempts": command.attempts,
                    }),
                )?;
            }
            if let Some(notice) = drain_wake_notice_locked(command_queue) {
                write_sse_event(stream, None, "wake", &json!({ "reason": notice.reason }))?;
            }
            if last_beat.elapsed() >= Duration::from_secs(15) {
                write_sse_event(stream, None, "heartbeat", &json!({ "ok": true }))?;
                last_beat = std::time::Instant::now();
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        write_sse_end(stream)?;
        return Ok(());
    }
    // Wake-notice drain: native_host.py polls this when its Chrome pipe looks
    // stale (or proactively); if the daemon has requested a wake, the host
    // forwards it to Chrome over the native messaging pipe and the service
    // worker's onMessage listener resuscitates the suspended worker.
    if request_line.starts_with("POST /browser/wake ") {
        let notice = drain_wake_notice_locked(command_queue);
        return write_http_response(
            stream,
            200,
            "OK",
            "application/json",
            serde_json::to_vec(&json!({
                "ok": true,
                "wake": notice,
            }))
            .unwrap_or_default(),
            None,
        );
    }
    if request_line.starts_with("POST /browser/command/result ") {
        // native_host.py posts command results back here
        let request_body: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                return write_http_response(
                    stream,
                    400,
                    "Bad Request",
                    "application/json",
                    serde_json::to_vec(&json!({"ok":false,"error":"invalid_json"}))
                        .unwrap_or_default(),
                    None,
                );
            }
        };
        let request_id = request_body
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        if request_id.is_empty() {
            return write_http_response(
                stream,
                400,
                "Bad Request",
                "application/json",
                serde_json::to_vec(&json!({"ok":false,"error":"missing_request_id"}))
                    .unwrap_or_default(),
                None,
            );
        }
        let result = if let Some(error) = request_body.get("error").filter(|value| !value.is_null())
        {
            json!({"ok": false, "error": error})
        } else if request_body.get("ok").and_then(Value::as_bool) == Some(false) {
            json!({"ok": false, "error": "extension_command_failed"})
        } else {
            json!({"ok": true, "result": request_body.get("result").cloned().unwrap_or(Value::Null)})
        };
        let stored = {
            let mut queue = command_queue.lock().expect("command queue lock poisoned");
            // Command results arrive over the live native messaging pipe, so
            // they double as host liveness; but only a bridge_ping result
            // proves the extension round trip (recorded inside store_result).
            let _ = queue.record_heartbeat(Some("comptrol.browser.bridge/0.1.0"));
            queue.store_result(request_id, result)
        };
        return match stored {
            Ok(true) => write_http_response(
                stream,
                200,
                "OK",
                "application/json",
                serde_json::to_vec(&json!({"ok":true})).unwrap_or_default(),
                None,
            ),
            Ok(false) => write_http_response(
                stream,
                404,
                "Not Found",
                "application/json",
                serde_json::to_vec(&json!({"ok":false,"error":"unknown_request_id"}))
                    .unwrap_or_default(),
                None,
            ),
            Err(error) => write_http_response(
                stream,
                500,
                "Internal Server Error",
                "application/json",
                serde_json::to_vec(&json!({"ok":false,"error":error.to_string()}))
                    .unwrap_or_default(),
                None,
            ),
        };
    }
    if request_line.starts_with("GET /browser/command/result/") {
        // Agent polls for a specific command result by request_id.
        // Request line format: "GET /browser/command/result/br_123 HTTP/1.1"
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        let path = parts.get(1).copied().unwrap_or("");
        let request_id = path
            .strip_prefix("/browser/command/result/")
            .unwrap_or(path)
            .to_owned();
        let result = {
            let queue = command_queue.lock().expect("command queue lock poisoned");
            queue.result(&request_id)
        };
        return match result {
            Ok(Some(result)) => write_http_response(
                stream,
                200,
                "OK",
                "application/json",
                serde_json::to_vec(&result).unwrap_or_default(),
                None,
            ),
            Ok(None) => write_http_response(
                stream,
                404,
                "Not Found",
                "application/json",
                serde_json::to_vec(&json!({
                    "ok": false,
                    "error": "result_not_ready",
                    "request_id": request_id,
                }))
                .unwrap_or_default(),
                None,
            ),
            Err(error) => write_http_response(
                stream,
                500,
                "Internal Server Error",
                "application/json",
                serde_json::to_vec(&json!({"ok":false,"error":error.to_string()}))
                    .unwrap_or_default(),
                None,
            ),
        };
    }

    if !request_line.starts_with("POST /mcp ") {
        return write_http_response(
            stream,
            405,
            "Method Not Allowed",
            "application/json",
            serde_json::to_vec(&json!({"error":"method_not_allowed"})).unwrap_or_default(),
            None,
        );
    }
    let request: Value = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(error) => {
            return write_http_response(
                stream,
                400,
                "Bad Request",
                "application/json",
                serde_json::to_vec(&json!({"error":"invalid_json","message":error.to_string()}))
                    .unwrap_or_default(),
                None,
            );
        }
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let is_initialize = method == "initialize";
    let is_mutation = is_mutation_method(method);
    if let Some(peer_fp) = peer_fingerprint.as_deref() {
        let mut store = pairing_store.lock().expect("pairing store lock poisoned");
        let pairing = match store.find_by_fingerprint(peer_fp).cloned() {
            Some(p) => p,
            None => {
                if std::env::var("COMPTROL_MTLS_AUTO_PAIR").as_deref() == Ok("1") {
                    match store.auto_pair(peer_fp) {
                        Ok(p) => p,
                        Err(_) => {
                            return write_http_response(
                                stream,
                                403,
                                "Forbidden",
                                "application/json",
                                serde_json::to_vec(&json!({"error":"mtls_identity_not_paired"}))
                                    .unwrap_or_default(),
                                None,
                            );
                        }
                    }
                } else {
                    return write_http_response(
                        stream,
                        403,
                        "Forbidden",
                        "application/json",
                        serde_json::to_vec(&json!({"error":"mtls_identity_not_paired"}))
                            .unwrap_or_default(),
                        None,
                    );
                }
            }
        };
        let pairing_id = pairing.pairing_id.clone();
        let required_scope = scope_for_method(method, request.get("params"));
        if !pairing.scopes.iter().any(|s| s == required_scope) {
            return write_http_response(
                stream,
                403,
                "Forbidden",
                "application/json",
                serde_json::to_vec(
                    &json!({"error":"mtls_scope_insufficient","required":required_scope}),
                )
                .unwrap_or_default(),
                None,
            );
        }
        if is_mutation {
            let nonce = match request
                .get("params")
                .and_then(|p| p.get("nonce"))
                .and_then(Value::as_str)
            {
                Some(n) => n,
                None => {
                    return write_http_response(
                        stream,
                        403,
                        "Forbidden",
                        "application/json",
                        serde_json::to_vec(&json!({"error":"mtls_nonce_required"}))
                            .unwrap_or_default(),
                        None,
                    );
                }
            };
            drop(store);
            let mut store = pairing_store.lock().expect("pairing store lock poisoned");
            if !store.record_nonce(nonce, &pairing_id).unwrap_or(false) {
                return write_http_response(
                    stream,
                    403,
                    "Forbidden",
                    "application/json",
                    serde_json::to_vec(&json!({"error":"nonce_replay"})).unwrap_or_default(),
                    None,
                );
            }
        }
    }
    if is_initialize {
        if session.is_some() {
            return write_http_response(
                stream,
                400,
                "Bad Request",
                "application/json",
                serde_json::to_vec(&json!({"error":"initialize_cannot_use_session"}))
                    .unwrap_or_default(),
                None,
            );
        }
    } else if !current_protocol && !http_state.contains(session) {
        return write_http_response(
            stream,
            if session.is_some() { 404 } else { 400 },
            if session.is_some() { "Not Found" } else { "Bad Request" },
            "application/json",
            serde_json::to_vec(&json!({"error": if session.is_some() { "session_not_found" } else { "session_required" }})).unwrap_or_default(),
            None,
        );
    }
    let mut notifications = Vec::new();
    let mut tasks_enabled = false;
    let value = handle_message_with_state(
        runtime,
        Some(tasks),
        &mut tasks_enabled,
        body.as_ref(),
        |notification| notifications.push(notification),
    )
    .unwrap_or_else(|| json!({}));
    let new_session = if is_initialize && !current_protocol {
        Some(http_state.create_session()?)
    } else {
        None
    };
    let event_session = if current_protocol {
        None
    } else {
        new_session.as_deref().or(session)
    };
    let mut messages = notifications;
    messages.push(value);
    if let Some(event_session) = event_session {
        for message in &messages {
            http_state.append(event_session, message.clone())?;
        }
    }
    let (content_type, payload) = if current_protocol {
        (
            "application/json",
            serde_json::to_vec(messages.last().unwrap_or(&json!({}))).unwrap_or_default(),
        )
    } else if messages.len() == 1 {
        (
            "application/json",
            serde_json::to_vec(&messages[0]).unwrap_or_default(),
        )
    } else {
        (
            "text/event-stream",
            messages
                .into_iter()
                .map(|message| {
                    format!(
                        "event: message\ndata: {}\n\n",
                        serde_json::to_string(&message).unwrap_or_else(|_| "{}".to_owned())
                    )
                })
                .collect::<String>()
                .into_bytes(),
        )
    };
    write_http_response(
        stream,
        200,
        "OK",
        content_type,
        payload,
        new_session.as_deref(),
    )
}

fn write_sse_headers<S: Write>(stream: &mut S) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nTransfer-Encoding: chunked\r\n\r\n"
    )?;
    stream.flush()
}

fn write_sse_event<S: Write>(
    stream: &mut S,
    id: Option<u64>,
    event: &str,
    data: &Value,
) -> io::Result<()> {
    let id_line = id.map(|id| format!("id: {id}\n")).unwrap_or_default();
    let payload = format!(
        "{id_line}event: {event}\ndata: {}\n\n",
        serde_json::to_string(data).unwrap_or_else(|_| "{}".to_owned())
    );
    write!(stream, "{:X}\r\n{}\r\n", payload.len(), payload)?;
    stream.flush()
}

fn write_sse_end<S: Write>(stream: &mut S) -> io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

fn write_http_response<S: Write>(
    stream: &mut S,
    status: u16,
    reason: &str,
    content_type: &str,
    payload: Vec<u8>,
    session: Option<&str>,
) -> io::Result<()> {
    let session_header = session
        .map(|session| format!("MCP-Session-Id: {session}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{session_header}Connection: close\r\n\r\n",
        payload.len()
    )?;
    stream.write_all(&payload)
}

fn write_http_error<S: Write>(stream: &mut S, status: u16, error: &str) -> io::Result<()> {
    let payload = serde_json::to_vec(&json!({
        "error": error,
        "limit": MAX_PROTOCOL_BYTES
    }))
    .unwrap_or_default();
    write_http_response(
        stream,
        status,
        "Payload Too Large",
        "application/json",
        payload,
        None,
    )
}

fn dashboard(runtime: &mut Runtime) -> String {
    let doctor = serde_json::to_string_pretty(&runtime.inspect("doctor"))
        .unwrap_or_else(|_| "{}".to_owned());
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Comptrol</title><style>body{{font:15px system-ui;margin:40px;max-width:900px}}pre{{background:#f4f4f4;padding:16px;overflow:auto}}</style></head><body><h1>Comptrol</h1><p>Local status and capability diagnostics</p><pre>{}</pre></body></html>",
        html_escape(&doctor)
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn print_json(value: Value) -> i32 {
    println!(
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
    );
    0
}

fn run_record(args: Vec<String>) -> i32 {
    let Some(input_path) = args.first() else {
        eprintln!("record needs an input JSONL path and optional trace path and mode");
        return 2;
    };
    let trace_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "comptrol.trace.jsonl".to_owned());
    let mode = args.get(2).map(String::as_str).unwrap_or("privacy_minimal");
    let mode = match mode {
        "privacy_minimal" => TraceMode::PrivacyMinimal,
        "developer" => TraceMode::Developer,
        "fixture_full" => TraceMode::FixtureFull,
        _ => {
            eprintln!("unknown trace mode");
            return 2;
        }
    };
    let contents = match if input_path == "-" {
        let mut stdin = io::stdin();
        let mut contents = String::new();
        stdin.read_to_string(&mut contents).map(|_| contents)
    } else {
        std::fs::read_to_string(input_path)
    } {
        Ok(contents) => contents,
        Err(error) => {
            eprintln!("record input failed: {error}");
            return 1;
        }
    };
    let mut runtime = match Runtime::with_trace(
        default_state_dir(),
        std::path::PathBuf::from(trace_path),
        mode,
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<OperationRequest>(line) {
            Ok(request) => println!(
                "{}",
                serde_json::to_string(&runtime.operate(request))
                    .unwrap_or_else(|_| "{}".to_owned())
            ),
            Err(error) => {
                eprintln!("record input is invalid: {error}");
                return 2;
            }
        }
    }
    0
}

fn run_replay(args: Vec<String>) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("replay needs a trace JSONL path");
        return 2;
    };
    if env::var("COMPTROL_REPLAY_FIXTURE").as_deref() != Ok("1") {
        eprintln!("replay requires COMPTROL_REPLAY_FIXTURE=1");
        return 2;
    }
    let entries = match read_trace(std::path::Path::new(path)) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("trace read failed: {error}");
            return 1;
        }
    };
    let mut runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    for entry in entries {
        if !matches!(
            entry.request.intent.as_str(),
            "system.ping" | "desktop.observe" | "filesystem.write"
        ) {
            eprintln!("replay refuses non fixture intent {}", entry.request.intent);
            return 2;
        }
        let actual = runtime.operate(entry.request);
        let matched = actual.intent == entry.result.intent
            && actual.route == entry.result.route
            && actual.delivery == entry.result.delivery
            && actual.effect == entry.result.effect
            && actual.verification == entry.result.verification
            && actual.error.as_ref().map(|error| &error.code)
                == entry.result.error.as_ref().map(|error| &error.code);
        println!("{}", serde_json::to_string(&json!({"matched": matched, "expected": entry.result, "actual": actual, "divergence": if matched { Value::Null } else { json!("result_fields_differ") }})).unwrap_or_else(|_| "{}".to_owned()));
        if !matched {
            return 1;
        }
    }
    0
}

fn run_workflow(args: Vec<String>) -> i32 {
    match args.first().map(String::as_str) {
        Some("compile") => {
            let Some(trace_path) = args.get(1) else {
                eprintln!("workflow compile needs a trace JSONL path and workflow id");
                return 2;
            };
            let Some(workflow_id) = args.get(2) else {
                eprintln!("workflow compile needs a workflow id");
                return 2;
            };
            let entries = match read_trace(Path::new(trace_path)) {
                Ok(entries) => entries,
                Err(error) => {
                    eprintln!("trace read failed: {error}");
                    return 1;
                }
            };
            match compile_verified_trace(&entries, workflow_id) {
                Ok(workflow) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&workflow).unwrap_or_else(|_| "{}".to_owned())
                    );
                    0
                }
                Err(error) => {
                    eprintln!("workflow compilation failed: {}", error.message);
                    1
                }
            }
        }
        Some("validate") => {
            let Some(workflow_path) = args.get(1) else {
                eprintln!("workflow validate needs a compiled workflow JSON path");
                return 2;
            };
            let workflow: CompiledWorkflow = match std::fs::read_to_string(workflow_path)
                .ok()
                .and_then(|contents| serde_json::from_str(&contents).ok())
            {
                Some(workflow) => workflow,
                None => {
                    eprintln!("compiled workflow JSON is invalid or unreadable");
                    return 2;
                }
            };
            let observed = args
                .get(2)
                .and_then(|value| serde_json::from_str(value).ok())
                .unwrap_or(Value::Null);
            match validate_compiled_workflow(&workflow, &observed) {
                Ok(()) => {
                    println!(
                        "{}",
                        json!({"valid": true, "workflow_id": workflow.workflow_id, "workflow_version": workflow.workflow_version})
                    );
                    0
                }
                Err(error) => {
                    println!("{}", json!({"valid": false, "error": error}));
                    1
                }
            }
        }
        _ => {
            eprintln!(
                "workflow commands: compile <trace.jsonl> <workflow-id> | validate <workflow.json> [observed-json]"
            );
            2
        }
    }
}

fn run_adapter(args: Vec<String>) -> i32 {
    match args.first().map(String::as_str) {
        Some("validate") => {
            let Some(path) = args.get(1) else {
                eprintln!("adapter validate needs an adapter.toml path");
                return 2;
            };
            match std::fs::read_to_string(path)
                .map_err(|error| error.to_string())
                .and_then(|text| {
                    AdapterManifest::from_toml(&text).map_err(|error| error.to_string())
                }) {
                Ok(manifest) => {
                    println!(
                        "{}",
                        json!({"valid": true, "id": manifest.id, "version": manifest.version, "capabilities": manifest.capabilities.len()})
                    );
                    0
                }
                Err(error) => {
                    println!("{}", json!({"valid": false, "error": error}));
                    1
                }
            }
        }
        Some("scaffold") => {
            let Some(name) = args.get(1) else {
                eprintln!("adapter scaffold needs a lowercase adapter name");
                return 2;
            };
            if name.is_empty()
                || name.chars().any(|character| {
                    !(character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || character == '-')
                })
            {
                eprintln!(
                    "adapter name must contain only lowercase ASCII letters, digits, and hyphens"
                );
                return 2;
            }
            let root = env::var_os("COMPTROL_ADAPTER_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("adapters"));
            let directory = root.join(name);
            if directory.exists() {
                eprintln!("adapter directory already exists: {}", directory.display());
                return 1;
            }
            if let Err(error) = std::fs::create_dir_all(directory.join("src")) {
                eprintln!("adapter scaffold failed: {error}");
                return 1;
            }
            let manifest = format!(
                "manifest_version = 1\nid = \"comptrol.{name}\"\nname = \"{name}\"\nversion = \"0.1.0\"\nplatforms = [\"windows\", \"macos\", \"linux\"]\napplications = [\"{name}\"]\n\n[isolation]\nmode = \"out_of_process\"\nnetwork = \"loopback_only\"\nfilesystem = \"declared_scopes\"\n\n[[capabilities]]\nintent = \"{name}.observe\"\nrisk = \"R0\"\nbackground = \"supported\"\nverification = \"application_state\"\n"
            );
            let files = [
                ("adapter.toml", manifest),
                ("README.md", format!("# {name} adapter\n\nDescribe the real application backend and its support boundary here.\n")),
                ("VERIFY.md", "# Verification contract\n\nDocument an independent postcondition for every capability.\n".to_owned()),
                ("SUPPORT.md", "# Support boundary\n\nDocument supported versions, platforms, and explicit refusals.\n".to_owned()),
                ("THREAT_MODEL.md", "# Threat model\n\nDocument isolation, capabilities, resource scopes, and failure behavior.\n".to_owned()),
                ("src/adapter.py", "#!/usr/bin/env python3\n# Implement the bounded adapter RPC protocol before advertising capabilities.\n".to_owned()),
            ];
            for (relative, content) in files {
                if let Err(error) = std::fs::write(directory.join(relative), content) {
                    eprintln!("adapter scaffold failed while writing {relative}: {error}");
                    return 1;
                }
            }
            println!("{}", json!({"scaffolded": true, "path": directory}));
            0
        }
        _ => {
            eprintln!("adapter commands: validate <adapter.toml> | scaffold <name>");
            2
        }
    }
}
