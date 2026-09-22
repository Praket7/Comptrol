use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static AUTH_NONCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Daemon {
    child: Child,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn unique_state_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "comptrol-bridge-e2e-{}-{nanos}",
        std::process::id()
    ))
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local address").port()
}

fn start_daemon(state_dir: &PathBuf) -> Daemon {
    let port = free_port();
    let binary = env!("CARGO_BIN_EXE_comptrol");
    let mut child = Command::new(binary)
        .args(["serve-http", &port.to_string()])
        .env("COMPTROL_STATE_DIR", state_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn Comptrol HTTP daemon");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok()
            && state_dir.join("browser-bridge.token").is_file()
        {
            return Daemon { child, port };
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("Comptrol HTTP daemon did not become ready");
}

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    body: Value,
    bridge_token: Option<&str>,
) -> (u16, Value) {
    let encoded = if body.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(&body).expect("serialize HTTP request")
    };
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect daemon");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let bridge_header = bridge_token
        .map(|token| {
            let sequence = AUTH_NONCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let nonce = format!("{:032x}{:032x}", std::process::id(), sequence);
            let body_hash = Sha256::digest(&encoded)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("hmac key");
            for part in [
                "comptrol.browser.bridge/0.1.0",
                method,
                path,
                nonce.as_str(),
                body_hash.as_str(),
            ] {
                mac.update(part.as_bytes());
                mac.update(b"\0");
            }
            let signature = mac
                .finalize()
                .into_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            format!(
                "X-Comptrol-Bridge-Nonce: {nonce}\r\nX-Comptrol-Bridge-Signature: {signature}\r\n"
            )
        })
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nMCP-Protocol-Version: 2026-07-28\r\nContent-Type: application/json\r\n{bridge_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
        encoded.len()
    );
    stream.write_all(request.as_bytes()).expect("write headers");
    if !encoded.is_empty() {
        stream.write_all(&encoded).expect("write body");
    }
    stream.flush().expect("flush request");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let response = String::from_utf8(response).expect("UTF-8 response");
    let (headers, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("HTTP status");
    let body = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body).expect("JSON response")
    };
    (status, body)
}

#[test]
fn daemon_bridge_queue_poll_result_and_persistence_round_trip() {
    let state_dir = unique_state_dir();
    fs::create_dir_all(&state_dir).expect("create state directory");

    {
        let daemon = start_daemon(&state_dir);
        let token = fs::read_to_string(state_dir.join("browser-bridge.token"))
            .expect("read browser bridge auth token");
        let token = token.trim();

        let nonce = "0123456789abcdef0123456789abcdef";
        let (status, challenge) = http_request(
            daemon.port,
            "POST",
            "/browser-auth/challenge",
            json!({"nonce": nonce}),
            None,
        );
        assert_eq!(status, 200, "{challenge}");
        assert_eq!(challenge["ok"], true);
        assert_eq!(challenge["protocol"], "comptrol.browser.bridge/0.1.0");
        let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("hmac key");
        mac.update(b"comptrol.browser.bridge/0.1.0");
        mac.update(b"\0");
        mac.update(nonce.as_bytes());
        let expected = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(challenge["proof"], expected);

        let (status, unauthorized) =
            http_request(daemon.port, "POST", "/browser/status", json!({}), None);
        assert_eq!(status, 403, "{unauthorized}");
        assert_eq!(unauthorized["error"], "browser_bridge_auth_required");

        let (status, heartbeat) = http_request(
            daemon.port,
            "POST",
            "/browser/extension/heartbeat",
            json!({"protocol": "comptrol.browser.bridge/0.1.0"}),
            Some(token),
        );
        assert_eq!(status, 200, "{heartbeat}");
        assert_eq!(heartbeat["ok"], true);

        let (status, accepted) = http_request(
            daemon.port,
            "POST",
            "/browser/debugger/attach",
            json!({"target_id": "123"}),
            Some(token),
        );
        assert_eq!(status, 202, "{accepted}");
        let request_id = accepted["request_id"]
            .as_str()
            .expect("queued request id")
            .to_owned();

        let (status, polled) = http_request(
            daemon.port,
            "POST",
            "/browser/command/poll",
            json!({}),
            Some(token),
        );
        assert_eq!(status, 200, "{polled}");
        let command = polled["commands"]
            .as_array()
            .and_then(|commands| {
                commands
                    .iter()
                    .find(|command| command["request_id"] == request_id)
            })
            .expect("leased bridge command");
        assert_eq!(command["command_type"], "attach_debugger");
        assert_eq!(command["payload"]["targetId"], "123");
        assert_eq!(command["attempts"], 1);

        let (status, stored) = http_request(
            daemon.port,
            "POST",
            "/browser/command/result",
            json!({
                "request_id": request_id,
                "ok": true,
                "result": {"attached": true, "targetId": "123"}
            }),
            Some(token),
        );
        assert_eq!(status, 200, "{stored}");

        for _ in 0..2 {
            let (status, result) = http_request(
                daemon.port,
                "GET",
                &format!("/browser/command/result/{request_id}"),
                Value::Null,
                Some(token),
            );
            assert_eq!(status, 200, "{result}");
            assert_eq!(result["ok"], true);
            assert_eq!(result["result"]["attached"], true);
            assert_eq!(result["result"]["targetId"], "123");
        }

        let (status, targets) = http_request(
            daemon.port,
            "POST",
            "/browser/extension/targets",
            json!({
                "targets": [{
                    "id": "123",
                    "type": "page",
                    "browserContextId": "default",
                    "url": "https://example.test/",
                    "title": "Example",
                    "revision": "bridge:123:0:https://example.test/"
                }]
            }),
            Some(token),
        );
        assert_eq!(status, 200, "{targets}");
        assert_eq!(targets["count"], 1);
    }

    {
        let daemon = start_daemon(&state_dir);
        let token = fs::read_to_string(state_dir.join("browser-bridge.token"))
            .expect("read browser bridge auth token after restart");
        let token = token.trim();
        let (status, heartbeat) = http_request(
            daemon.port,
            "POST",
            "/browser/extension/heartbeat",
            json!({"protocol": "comptrol.browser.bridge/0.1.0"}),
            Some(token),
        );
        assert_eq!(status, 200, "{heartbeat}");

        let (status, health) = http_request(
            daemon.port,
            "POST",
            "/browser/status",
            json!({}),
            Some(token),
        );
        assert_eq!(status, 200, "{health}");
        assert_eq!(health["extension_connected"], true);
        assert_eq!(health["target_count"], 1);
    }

    let _ = fs::remove_dir_all(state_dir);
}
