use comptrol::{
    OperationRequest, PROTOCOL_VERSION, Runtime, SERVER_VERSION, TraceMode, capabilities,
    default_state_dir, read_trace,
};
use serde_json::{Value, json};
use std::env;
use std::io::{self, BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};

fn main() {
    let result = match env::args().nth(1).as_deref() {
        None | Some("mcp") => run_stdio(),
        Some("doctor") => print_json(run_inspect("doctor")),
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
        Some("version") => {
            println!("{SERVER_VERSION}");
            0
        }
        Some(other) => {
            eprintln!("unknown command {other}");
            eprintln!(
                "commands are mcp doctor status capabilities stop resume serve-http record replay version"
            );
            2
        }
    };
    if result != 0 {
        std::process::exit(result);
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
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        match line {
            Ok(line) if !line.trim().is_empty() => {
                if let Some(response) = handle_message(&mut runtime, &line) {
                    println!("{}", response);
                    let _ = io::stdout().flush();
                }
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("stdin failed: {error}");
                return 1;
            }
        }
    }
    0
}

fn handle_message(runtime: &mut Runtime, line: &str) -> Option<Value> {
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
    let result = match method {
        "initialize" => {
            json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": { "tools": { "listChanged": false } }, "serverInfo": { "name": "comptrol", "version": SERVER_VERSION }, "instructions": "Use operate for one bounded intent. Use inspect for current state. Results distinguish delivery, effect, and verification. Unsupported capabilities refuse safely." })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => call_tool(
            runtime,
            request.get("params").cloned().unwrap_or(Value::Null),
        ),
        _ => {
            json!({ "error": { "code": "method_not_found", "message": format!("Unknown MCP method {method}") } })
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn tools() -> Value {
    json!([
        { "name": "operate", "description": "Execute one bounded local intent with policy, idempotency, and verification state", "inputSchema": { "type": "object", "required": ["intent"], "properties": { "intent": {"type":"string"}, "target": {"type":"object"}, "params": {"type":"object"}, "postcondition": {"type":"object"}, "risk": {"type":"string"}, "idempotency_key": {"type":"string"}, "dry_run": {"type":"boolean"} } } },
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
