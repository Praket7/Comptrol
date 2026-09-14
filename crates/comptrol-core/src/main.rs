use comptrol::{
    OperationRequest, PROTOCOL_VERSION, Runtime, SERVER_VERSION, capabilities, default_state_dir,
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
        Some("version") => {
            println!("{SERVER_VERSION}");
            0
        }
        Some(other) => {
            eprintln!("unknown command {other}");
            eprintln!("commands are mcp doctor status capabilities stop resume serve-http version");
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
        { "name": "inspect", "description": "Inspect doctor, status, capabilities, or current desktop observation", "inputSchema": { "type": "object", "properties": { "kind": {"type":"string", "enum":["doctor","status","capabilities","desktop"]} } } },
        { "name": "watch", "description": "Return the known state of an operation without repeating its mutation", "inputSchema": { "type": "object", "required":["operation_id"], "properties": { "operation_id": {"type":"string"} } } },
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
    let response = if !origin_ok {
        (403, json!({"error":"origin_denied"}))
    } else if !method_ok {
        (405, json!({"error":"method_not_allowed"}))
    } else {
        match handle_message(runtime, body) {
            Some(value) => (200, value),
            None => (202, json!({})),
        }
    };
    let payload = serde_json::to_vec(&response.1).unwrap_or_else(|_| b"{}".to_vec());
    write!(
        stream,
        "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.0,
        payload.len()
    )?;
    stream.write_all(&payload)
}

fn print_json(value: Value) -> i32 {
    println!(
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
    );
    0
}
