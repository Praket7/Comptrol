use comptrol::{
    CompiledWorkflow, MAX_PROTOCOL_BYTES, OperationRequest, PROTOCOL_VERSION, Runtime,
    SERVER_VERSION, TraceMode, capabilities, compile_verified_trace, default_state_dir,
    integration, mcp, pairing::PairingStore, privacy_network_endpoints, privacy_status, read_trace,
    validate_compiled_workflow,
};
use comptrol_adapter_sdk::AdapterManifest;
use rusqlite::{Connection, OptionalExtension, params};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls_pemfile::{certs, private_key};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, RawHandle};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
#[cfg(windows)]
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

const DEFAULT_TASK_TTL_MS: u64 = 300_000;
const TASK_POLL_INTERVAL_MS: u64 = 50;
const HTTP_SESSION_TTL_MS: u128 = 86_400_000;
const HTTP_EVENT_CAPACITY: usize = 256;
const HTTP_STREAM_IDLE_MS: u64 = 300_000;
const HTTP_MAX_CONNECTIONS: usize = 32;

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
                let started_at_ms = now_ms();
                let result = {
                    let mut runtime = runtime.lock().expect("runtime lock poisoned");
                    call_tool_with_cancel(&mut runtime, params, Some(Arc::clone(&cancelled)))
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

fn main() {
    let result = match env::args().nth(1).as_deref() {
        None | Some("mcp") => {
            auto_start_chrome_cdp();
            run_stdio()
        }
        Some("doctor") => run_doctor(env::args().skip(2).collect()),
        Some("status") => print_json(run_inspect("status")),
        Some("capabilities") => print_json(json!(capabilities())),
        Some("stop") => change_stop(true),
        Some("resume") => change_stop(false),
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
                "commands are mcp doctor status capabilities stop resume serve-http serve-mtls daemon daemon-health record replay workflow adapter integrate pair privacy version"
            );
            2
        }
    };
    if result != 0 {
        std::process::exit(result);
    }
}

fn auto_start_chrome_cdp() {
    if env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
        || env::var("COMPTROL_AUTO_START_CHROME_CDP").as_deref() == Ok("0")
    {
        return;
    }

    #[cfg(not(windows))]
    return;

    #[cfg(windows)]
    {
        let Some(chrome) = windows_chrome_path() else {
            eprintln!("Comptrol Chrome CDP auto-start skipped because Chrome was not found");
            return;
        };
        let port = match TcpListener::bind(("127.0.0.1", 0))
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
        {
            Ok(port) => port,
            Err(error) => {
                eprintln!(
                    "Comptrol Chrome CDP auto-start skipped because a local port was unavailable: {error}"
                );
                return;
            }
        };
        let profile = env::var_os("COMPTROL_CHROME_PROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_state_dir().join("chrome-cdp-profile"));
        if let Err(error) = std::fs::create_dir_all(&profile) {
            eprintln!(
                "Comptrol Chrome CDP auto-start skipped because the profile could not be created: {error}"
            );
            return;
        }
        let url =
            env::var("COMPTROL_CHROME_START_URL").unwrap_or_else(|_| "about:blank".to_owned());
        let endpoint = format!("http://127.0.0.1:{port}");
        let spawn = Command::new(&chrome)
            .args([
                "--remote-debugging-address=127.0.0.1".to_owned(),
                format!("--remote-debugging-port={port}"),
                format!("--user-data-dir={}", profile.display()),
                "--no-first-run".to_owned(),
                "--no-default-browser-check".to_owned(),
                url,
            ])
            .spawn();
        if let Err(error) = spawn {
            eprintln!(
                "Comptrol Chrome CDP auto-start skipped because Chrome could not launch: {error}"
            );
            return;
        }
        let ready = (0..100).any(|_| {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                let _ = stream.write_all(
                    b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
                );
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
                if response.contains("200 OK") && response.contains("Browser") {
                    return true;
                }
            }
            thread::sleep(Duration::from_millis(50));
            false
        });
        if !ready {
            eprintln!(
                "Comptrol Chrome CDP auto-start skipped because Chrome did not expose /json/version"
            );
            return;
        }
        unsafe {
            env::set_var("COMPTROL_CDP_ENDPOINT", endpoint);
            env::set_var("COMPTROL_ALLOW_BROWSER_CDP", "1");
        }
        eprintln!(
            "Comptrol Chrome CDP auto-started with profile {}",
            profile.display()
        );
    }
}

#[cfg(windows)]
fn windows_chrome_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(program_files) = env::var_os("PROGRAMFILES") {
        candidates.push(PathBuf::from(program_files).join("Google/Chrome/Application/chrome.exe"));
    }
    if let Some(program_files_x86) = env::var_os("PROGRAMFILES(X86)") {
        candidates
            .push(PathBuf::from(program_files_x86).join("Google/Chrome/Application/chrome.exe"));
    }
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local_app_data).join("Google/Chrome/Application/chrome.exe"));
    }
    candidates.into_iter().find(|path| path.is_file())
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
                    _ => {
                        eprintln!("pair show accepts --ttl-ms and repeated --scope");
                        return 2;
                    }
                }
                index += 1;
            }
            if scopes.is_empty() {
                scopes.push("observe".to_owned());
            }
            match store.create(scopes, ttl_ms) {
                Ok((record, code)) => print_json(json!({
                    "pairing_id": record.pairing_id,
                    "code": code,
                    "scopes": record.scopes,
                    "expires_at_ms": record.expires_at_ms,
                    "remote_transport": "disabled_until_mtls"
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
            "remote_transport": "disabled_until_mtls",
            "pairings": store.list().iter().map(public_pairing).collect::<Vec<_>>()
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
            json!({ "protocolVersion": protocol_version, "capabilities": { "tools": { "listChanged": false }, "tasks": task_capabilities, "extensions": extensions }, "serverInfo": { "name": "comptrol", "version": SERVER_VERSION }, "instructions": "Use operate for one bounded intent. Use inspect for current state. Results distinguish delivery, effect, and verification. Unsupported capabilities refuse safely.", "comptrol": { "protocol_mode": if protocol_mode == mcp::ProtocolMode::Current { "stateless" } else { "legacy_compatibility" } } })
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
                    ),
                }
            } else {
                call_tool_with_cancel(
                    &mut runtime.lock().expect("runtime lock poisoned"),
                    params,
                    None,
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
    let Some(task_id) = request
        .get("params")
        .and_then(|params| params.get("taskId"))
        .and_then(Value::as_str)
    else {
        return task_error("tasks/get needs taskId");
    };
    tasks
        .and_then(|manager| manager.get(task_id))
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
        { "name": "operate", "description": "Execute one bounded local intent with policy, idempotency, background posture, and verification state", "inputSchema": { "type": "object", "required": ["intent"], "properties": { "intent": {"type":"string"}, "target": {"type":"object"}, "params": {"type":"object"}, "postcondition": {"type":"object"}, "risk": {"type":"string"}, "idempotency_key": {"type":"string"}, "dry_run": {"type":"boolean"}, "background": {"type":"string", "enum":["strict_background","prefer_background","foreground_allowed","foreground_required"]} } } },
        { "name": "inspect", "description": "Inspect doctor, status, capabilities, deterministic route plans, platform state, events, checkpoints, adapters, or current desktop observation", "inputSchema": { "type": "object", "properties": { "kind": {"type":"string", "enum":["doctor","status","capabilities","routes","platform","desktop","events","checkpoints","adapters"]} } } },
        { "name": "watch", "description": "Return the known state of an operation without repeating its mutation", "inputSchema": { "type": "object", "required":["operation_id"], "properties": { "operation_id": {"type":"string"} } } },
        { "name": "reconcile", "description": "Reconcile a durable unknown operation from observed local state without repeating its mutation", "inputSchema": { "type": "object", "required":["operation_id"], "properties": { "operation_id": {"type":"string"} } } },
        { "name": "restore_checkpoint", "description": "Restore a local sandbox checkpoint under explicit local write policy", "inputSchema": { "type": "object", "required":["checkpoint"], "properties": { "checkpoint": {"type":"string"}, "idempotency_key": {"type":"string"} } } },
        { "name": "capabilities", "description": "Return capabilities that are actually available in this runtime", "inputSchema": { "type": "object" } }
    ])
}

fn call_tool_with_cancel(
    runtime: &mut Runtime,
    params: Value,
    cancellation: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let value = match name {
        "operate" => serde_json::from_value::<OperationRequest>(arguments).map(|request| {
            json!(match cancellation {
                Some(cancellation) => runtime.operate_with_cancel(request, cancellation),
                None => runtime.operate(request),
            })
        }).unwrap_or_else(|error| json!({ "error": { "code": "invalid_input", "message": error.to_string() } })),
        "inspect" => json!(runtime.inspect(arguments.get("kind").and_then(Value::as_str).unwrap_or("status"))),
        "watch" => json!(runtime.watch(arguments.get("operation_id").and_then(Value::as_str).unwrap_or_default())),
        "reconcile" => json!(runtime.reconcile(arguments.get("operation_id").and_then(Value::as_str).unwrap_or_default())),
        "restore_checkpoint" => serde_json::from_value::<OperationRequest>(json!({ "intent": "filesystem.restore_checkpoint", "params": arguments.clone(), "idempotency_key": arguments.get("idempotency_key"), "risk": "R1" })).map(|request| json!(runtime.operate(request))).unwrap_or_else(|error| json!({ "error": { "code": "invalid_input", "message": error.to_string() } })),
        "capabilities" => json!(capabilities()),
        _ => json!({ "error": { "code": "tool_not_found", "message": format!("Unknown tool {name}") } }),
    };
    json!({ "content": [{ "type": "text", "text": serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned()) }], "structuredContent": value })
}

fn run_inspect(kind: &str) -> Value {
    match Runtime::new(default_state_dir()) {
        Ok(mut runtime) => runtime.inspect(kind),
        Err(error) => json!({ "error": error.to_string() }),
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
                let active_connections = Arc::clone(&active_connections);
                thread::spawn(move || {
                    if let Err(error) = handle_http(&mut stream, &runtime, &tasks, &http_state) {
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
                let mut tls = rustls::StreamOwned::new(connection, stream);
                handle_http(&mut tls, &runtime, &tasks, &http_state)
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
    let tasks = TaskManager::new(Arc::clone(&runtime), tasks);
    eprintln!("comptrol daemon listening on {}", path.display());
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_ipc_connection(&mut stream, &runtime, &tasks) {
                    eprintln!("daemon connection failed: {error}");
                }
            }
            Err(error) => eprintln!("daemon accept failed: {error}"),
        }
    }
    0
}

#[cfg(not(unix))]
fn run_daemon() -> i32 {
    #[cfg(windows)]
    {
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
        let tasks = TaskManager::new(Arc::clone(&runtime), tasks);
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
            if connected && let Err(error) = handle_ipc_connection(&mut stream, &runtime, &tasks) {
                eprintln!("daemon connection failed: {error}");
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("daemon named pipe transport is not implemented on this platform");
        2
    }
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
    tasks: &TaskManager,
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
                &json!({ "version": 1, "id": id, "result": { "ready": true, "server": SERVER_VERSION, "protocol": PROTOCOL_VERSION } }),
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

fn handle_http<S: HttpStream>(
    stream: &mut S,
    runtime: &Arc<Mutex<Runtime>>,
    tasks: &Arc<TaskManager>,
    http_state: &Arc<HttpStore>,
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
    let is_initialize = request.get("method").and_then(Value::as_str) == Some("initialize");
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
