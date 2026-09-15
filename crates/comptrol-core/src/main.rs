use comptrol::{
    MAX_PROTOCOL_BYTES, OperationRequest, PROTOCOL_VERSION, Runtime, SERVER_VERSION, TraceMode,
    capabilities, default_state_dir, integration, pairing::PairingStore, privacy_network_endpoints,
    privacy_status, read_trace,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, RawHandle};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
};
#[cfg(windows)]
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_ACCESS_DUPLEX, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

const DEFAULT_TASK_TTL_MS: u64 = 300_000;
const TASK_POLL_INTERVAL_MS: u64 = 50;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredTask {
    task_id: String,
    status: String,
    ttl_ms: u64,
    poll_interval_ms: u64,
    created_at_ms: u128,
    result: Value,
}

struct TaskStore {
    file: File,
    records: HashMap<String, StoredTask>,
    sequence: u64,
}

struct HttpState {
    sessions: HashSet<String>,
}

impl HttpState {
    fn new() -> Self {
        Self {
            sessions: HashSet::new(),
        }
    }

    fn create_session(&mut self) -> io::Result<String> {
        let mut bytes = [0_u8; 24];
        getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
        let session = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        self.sessions.insert(session.clone());
        Ok(session)
    }

    fn contains(&self, session: Option<&str>) -> bool {
        session.is_some_and(|session| self.sessions.contains(session))
    }
}

impl TaskStore {
    fn open(state_dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("tasks.jsonl");
        let mut records = HashMap::new();
        if path.exists() {
            for line in BufReader::new(File::open(&path)?).lines() {
                if let Ok(task) = serde_json::from_str::<StoredTask>(&line?) {
                    records.insert(task.task_id.clone(), task);
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            records,
            sequence: 0,
        })
    }

    fn create(&mut self, result: Value, ttl_ms: u64) -> io::Result<Value> {
        self.sequence = self.sequence.saturating_add(1);
        let task_id = format!("task-{}-{}", now_ms(), self.sequence);
        let task = StoredTask {
            task_id: task_id.clone(),
            status: "completed".to_owned(),
            ttl_ms,
            poll_interval_ms: TASK_POLL_INTERVAL_MS,
            created_at_ms: now_ms(),
            result,
        };
        self.write(task.clone())?;
        Ok(task_view(&task))
    }

    fn get(&self, task_id: &str) -> Option<Value> {
        self.records.get(task_id).map(task_view)
    }

    fn result(&self, task_id: &str) -> Option<Value> {
        self.records.get(task_id).map(|task| task.result.clone())
    }

    fn list(&self) -> Value {
        json!({ "tasks": self.records.values().map(task_view).collect::<Vec<_>>() })
    }

    fn write(&mut self, task: StoredTask) -> io::Result<()> {
        serde_json::to_writer(&mut self.file, &task)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.records.insert(task.task_id.clone(), task);
        Ok(())
    }
}

fn task_view(task: &StoredTask) -> Value {
    json!({
        "taskId": task.task_id,
        "status": task.status,
        "ttlMs": task.ttl_ms,
        "pollIntervalMs": task.poll_interval_ms,
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
        None | Some("mcp") => run_stdio(),
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
        Some("daemon") => run_daemon(),
        Some("daemon-health") => run_daemon_health(),
        Some("record") => run_record(env::args().skip(2).collect()),
        Some("replay") => run_replay(env::args().skip(2).collect()),
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
                "commands are mcp doctor status capabilities stop resume serve-http daemon daemon-health record replay integrate pair privacy version"
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
    let mut runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    let mut tasks = match TaskStore::open(&default_state_dir()) {
        Ok(tasks) => tasks,
        Err(error) => {
            eprintln!("task store startup failed: {error}");
            return 1;
        }
    };
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
                        &mut runtime,
                        Some(&mut tasks),
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
    runtime: &mut Runtime,
    mut tasks: Option<&mut TaskStore>,
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
    let result = match method {
        "initialize" => {
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
            json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": { "tools": { "listChanged": false }, "tasks": task_capabilities, "extensions": extensions }, "serverInfo": { "name": "comptrol", "version": SERVER_VERSION }, "instructions": "Use operate for one bounded intent. Use inspect for current state. Results distinguish delivery, effect, and verification. Unsupported capabilities refuse safely." })
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
                let result = call_tool(runtime, params.clone());
                match tasks.as_deref_mut() {
                    Some(store) => match store.create(result, ttl_ms) {
                        Ok(task) => json!({ "resultType": "task", "task": task }),
                        Err(error) => {
                            json!({ "error": { "code": "task_store_failed", "message": error.to_string() } })
                        }
                    },
                    None => call_tool(
                        runtime,
                        request.get("params").cloned().unwrap_or(Value::Null),
                    ),
                }
            } else {
                call_tool(runtime, params)
            }
        }
        "tasks/get" => task_get(tasks.as_deref(), &request),
        "tasks/result" => task_result(tasks.as_deref(), &request),
        "tasks/list" => tasks
            .as_deref()
            .map(TaskStore::list)
            .unwrap_or_else(|| task_error("Tasks are available only on the stdio transport")),
        "tasks/cancel" => task_error("Completed Comptrol tasks cannot be cancelled"),
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

fn task_get(tasks: Option<&TaskStore>, request: &Value) -> Value {
    let Some(task_id) = request
        .get("params")
        .and_then(|params| params.get("taskId"))
        .and_then(Value::as_str)
    else {
        return task_error("tasks/get needs taskId");
    };
    tasks
        .and_then(|store| store.get(task_id))
        .unwrap_or_else(|| task_error("task not found"))
}

fn task_result(tasks: Option<&TaskStore>, request: &Value) -> Value {
    let Some(task_id) = request
        .get("params")
        .and_then(|params| params.get("taskId"))
        .and_then(Value::as_str)
    else {
        return task_error("tasks/result needs taskId");
    };
    tasks
        .and_then(|store| store.result(task_id))
        .unwrap_or_else(|| task_error("task not found"))
}

fn task_error(message: &str) -> Value {
    json!({ "error": { "code": -32602, "message": message } })
}

fn tools() -> Value {
    json!([
        { "name": "operate", "description": "Execute one bounded local intent with policy, idempotency, background posture, and verification state", "inputSchema": { "type": "object", "required": ["intent"], "properties": { "intent": {"type":"string"}, "target": {"type":"object"}, "params": {"type":"object"}, "postcondition": {"type":"object"}, "risk": {"type":"string"}, "idempotency_key": {"type":"string"}, "dry_run": {"type":"boolean"}, "background": {"type":"string", "enum":["strict_background","prefer_background","foreground_allowed","foreground_required"]} } } },
        { "name": "inspect", "description": "Inspect doctor, status, capabilities, platform state, events, checkpoints, adapters, or current desktop observation", "inputSchema": { "type": "object", "properties": { "kind": {"type":"string", "enum":["doctor","status","capabilities","platform","desktop","events","checkpoints","adapters"]} } } },
        { "name": "watch", "description": "Return the known state of an operation without repeating its mutation", "inputSchema": { "type": "object", "required":["operation_id"], "properties": { "operation_id": {"type":"string"} } } },
        { "name": "reconcile", "description": "Reconcile a durable unknown operation from observed local state without repeating its mutation", "inputSchema": { "type": "object", "required":["operation_id"], "properties": { "operation_id": {"type":"string"} } } },
        { "name": "restore_checkpoint", "description": "Restore a local sandbox checkpoint under explicit local write policy", "inputSchema": { "type": "object", "required":["checkpoint"], "properties": { "checkpoint": {"type":"string"}, "idempotency_key": {"type":"string"} } } },
        { "name": "capabilities", "description": "Return capabilities that are actually available in this runtime", "inputSchema": { "type": "object" } }
    ])
}

fn call_tool(runtime: &mut Runtime, params: Value) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let value = match name {
        "operate" => serde_json::from_value::<OperationRequest>(arguments).map(|request| json!(runtime.operate(request))).unwrap_or_else(|error| json!({ "error": { "code": "invalid_input", "message": error.to_string() } })),
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
    let mut runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    let mut http_state = HttpState::new();
    // ponytail: one blocking loop, add bounded concurrency when multiple clients need simultaneous long operations
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_http(&mut stream, &mut runtime, &mut http_state) {
                    eprintln!("http request failed: {error}");
                }
            }
            Err(error) => eprintln!("http accept failed: {error}"),
        }
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
    let mut runtime = match Runtime::new(default_state_dir()) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("startup failed: {error}");
            return 1;
        }
    };
    let mut tasks = match TaskStore::open(&default_state_dir()) {
        Ok(tasks) => tasks,
        Err(error) => {
            eprintln!("task store startup failed: {error}");
            return 1;
        }
    };
    eprintln!("comptrol daemon listening on {}", path.display());
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_ipc_connection(&mut stream, &mut runtime, &mut tasks) {
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
        let mut runtime = match Runtime::new(default_state_dir()) {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("startup failed: {error}");
                return 1;
            }
        };
        let mut tasks = match TaskStore::open(&default_state_dir()) {
            Ok(tasks) => tasks,
            Err(error) => {
                eprintln!("task store startup failed: {error}");
                return 1;
            }
        };
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
                if let Err(error) = handle_ipc_connection(&mut stream, &mut runtime, &mut tasks) {
                    eprintln!("daemon connection failed: {error}");
                }
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
    runtime: &mut Runtime,
    tasks: &mut TaskStore,
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
                    tasks_enabled,
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

fn handle_http(
    stream: &mut TcpStream,
    runtime: &mut Runtime,
    http_state: &mut HttpState,
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
    let header = String::from_utf8_lossy(&buffer[..header_end]);
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
    let origin = header_value("Origin");
    let session = header_value("MCP-Session-Id");
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
        return write_http_response(
            stream,
            200,
            "OK",
            "text/html; charset=utf-8",
            dashboard(runtime).into_bytes(),
            None,
        );
    }
    if request_line.starts_with("DELETE /mcp ") {
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
        if !http_state.sessions.remove(session) {
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
        if !http_state.contains(session) {
            return write_http_response(
                stream,
                if session.is_some() { 404 } else { 400 },
                if session.is_some() { "Not Found" } else { "Bad Request" },
                "application/json",
                serde_json::to_vec(&json!({"error": if session.is_some() { "session_not_found" } else { "session_required" }})).unwrap_or_default(),
                None,
            );
        }
        return write_http_response(
            stream,
            200,
            "OK",
            "text/event-stream",
            b"event: ready\ndata: {}\n\n".to_vec(),
            None,
        );
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
    } else if !http_state.contains(session) {
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
        None,
        &mut tasks_enabled,
        body.as_ref(),
        |notification| notifications.push(notification),
    )
    .unwrap_or_else(|| json!({}));
    let new_session = if is_initialize {
        Some(http_state.create_session()?)
    } else {
        None
    };
    let (content_type, payload) = if notifications.is_empty() {
        (
            "application/json",
            serde_json::to_vec(&value).unwrap_or_default(),
        )
    } else {
        let mut payload = notifications;
        payload.push(value);
        (
            "text/event-stream",
            payload
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

fn write_http_response(
    stream: &mut TcpStream,
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

fn write_http_error(stream: &mut TcpStream, status: u16, error: &str) -> io::Result<()> {
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
