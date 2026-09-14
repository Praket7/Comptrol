use comptrol::{
    MAX_PROTOCOL_BYTES, OperationRequest, PROTOCOL_VERSION, Runtime, SERVER_VERSION, TraceMode,
    capabilities, default_state_dir, integration, privacy_network_endpoints, privacy_status,
    read_trace,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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
        Some("record") => run_record(env::args().skip(2).collect()),
        Some("replay") => run_replay(env::args().skip(2).collect()),
        Some("integrate") => run_integrate(env::args().skip(2).collect()),
        Some("privacy") => run_privacy(env::args().skip(2).collect()),
        Some("version") => {
            println!("{SERVER_VERSION}");
            0
        }
        Some(other) => {
            eprintln!("unknown command {other}");
            eprintln!(
                "commands are mcp doctor status capabilities stop resume serve-http record replay integrate privacy version"
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

fn handle_message(runtime: &mut Runtime, line: &str) -> Option<Value> {
    let mut tasks_enabled = false;
    handle_message_with_state(runtime, None, &mut tasks_enabled, line, |_| {})
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
    // ponytail: one blocking loop, add bounded concurrency when multiple clients need simultaneous long operations
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_http(&mut stream, &mut runtime) {
                    eprintln!("http request failed: {error}");
                }
            }
            Err(error) => eprintln!("http accept failed: {error}"),
        }
    }
    0
}

fn handle_http(stream: &mut TcpStream, runtime: &mut Runtime) -> io::Result<()> {
    // ponytail: bounded local parser, replace with a full HTTP implementation before public network exposure
    let mut buffer = vec![0_u8; 2 * 1024 * 1024];
    let size = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..size]);
    let declared_length = request
        .split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        });
    if declared_length.is_some_and(|length| length > MAX_PROTOCOL_BYTES)
        || (size == buffer.len() && !request.contains("\r\n\r\n"))
    {
        let payload = serde_json::to_vec(&json!({
            "error": "message_too_large",
            "limit": MAX_PROTOCOL_BYTES
        }))
        .unwrap_or_default();
        write!(
            stream,
            "HTTP/1.1 413 Payload Too Large\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )?;
        stream.write_all(&payload)?;
        return Ok(());
    }
    let mut sections = request.split("\r\n\r\n");
    let header = sections.next().unwrap_or_default();
    let body = sections.next().unwrap_or_default();
    let origin = header
        .lines()
        .find_map(|line| line.strip_prefix("Origin:").map(str::trim));
    let origin_ok = origin.is_none_or(|value| {
        matches!(
            value,
            "http://localhost" | "http://127.0.0.1" | "http://[::1]"
        )
    });
    let method_ok = header.starts_with("POST /mcp ");
    let (status, content_type, payload) = if !origin_ok {
        (
            403,
            "application/json",
            serde_json::to_vec(&json!({"error":"origin_denied"})).unwrap_or_default(),
        )
    } else if header.starts_with("GET /dashboard ") {
        (
            200,
            "text/html; charset=utf-8",
            dashboard(runtime).into_bytes(),
        )
    } else if !method_ok {
        (
            405,
            "application/json",
            serde_json::to_vec(&json!({"error":"method_not_allowed"})).unwrap_or_default(),
        )
    } else {
        let value = handle_message(runtime, body).unwrap_or_else(|| json!({}));
        (
            200,
            "application/json",
            serde_json::to_vec(&value).unwrap_or_default(),
        )
    };
    write!(
        stream,
        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        payload.len()
    )?;
    stream.write_all(&payload)
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
