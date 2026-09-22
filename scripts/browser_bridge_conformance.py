#!/usr/bin/env python3
"""Static, framing, and loopback-auth conformance for the Browser Bridge."""

import hashlib
import hmac
import http.server
import json
import os
import pathlib
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
BRIDGE = ROOT / "extensions" / "comptrol-browser-bridge"
PROTOCOL = "comptrol.browser.bridge/0.1.0"


def require(path: pathlib.Path) -> None:
    if not path.is_file():
        raise SystemExit(f"missing Browser Bridge asset: {path.relative_to(ROOT)}")


def send_native_message(process: subprocess.Popen, value: dict) -> dict:
    encoded = json.dumps(value, separators=(",", ":")).encode("utf-8")
    assert process.stdin is not None
    assert process.stdout is not None
    process.stdin.write(struct.pack("<I", len(encoded)) + encoded)
    process.stdin.flush()
    raw_length = process.stdout.read(4)
    if len(raw_length) != 4:
        raise RuntimeError("native host did not emit a framed response")
    length = struct.unpack("<I", raw_length)[0]
    return json.loads(process.stdout.read(length).decode("utf-8"))


class ProbeServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, token: str, valid_proof: bool):
        super().__init__(("127.0.0.1", 0), ProbeHandler)
        self.token = token
        self.valid_proof = valid_proof
        self.seen = []


class ProbeHandler(http.server.BaseHTTPRequestHandler):
    server: ProbeServer

    def log_message(self, *_args) -> None:
        pass

    def _reply(self, status: int, value: dict) -> None:
        encoded = json.dumps(value, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw.decode("utf-8") or "{}")
        except ValueError:
            body = {}
        self.server.seen.append(
            {
                "path": self.path,
                "token": self.headers.get("X-Comptrol-Bridge-Token"),
                "body": body,
            }
        )

        if self.path == "/browser-auth/challenge":
            nonce = body.get("nonce", "")
            proof = hmac.new(
                self.server.token.encode("ascii"),
                (PROTOCOL + "\0" + nonce).encode("ascii"),
                hashlib.sha256,
            ).hexdigest()
            if not self.server.valid_proof:
                proof = "00" * 32
            self._reply(
                200,
                {"ok": True, "protocol": PROTOCOL, "proof": proof},
            )
            return

        if self.headers.get("X-Comptrol-Bridge-Token") != self.server.token:
            self._reply(403, {"ok": False, "error": "browser_bridge_auth_required"})
            return

        if self.path == "/browser/extension/heartbeat":
            self._reply(200, {"ok": True})
        elif self.path == "/browser/command/poll":
            self._reply(200, {"ok": True, "commands": [], "count": 0})
        else:
            self._reply(200, {"ok": True})


def exercise_host_against_probe(native_host: pathlib.Path, valid_proof: bool) -> list[dict]:
    token = "ab" * 32
    with tempfile.TemporaryDirectory(prefix="comptrol-bridge-conformance-") as temp:
        state_dir = pathlib.Path(temp)
        (state_dir / "browser-bridge.token").write_text(token + "\n", encoding="utf-8")
        server = ProbeServer(token, valid_proof)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        env = {
            **os.environ,
            "COMPTROL_STATE_DIR": str(state_dir),
            "COMPTROL_DAEMON_URL": f"http://127.0.0.1:{server.server_port}",
        }
        process = subprocess.Popen(
            [sys.executable, str(native_host)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        try:
            response = send_native_message(
                process,
                {"type": "handshake", "protocol": PROTOCOL},
            )
            if response.get("type") != "handshake_ack":
                raise RuntimeError(f"unexpected native host handshake response: {response}")
            time.sleep(0.35)
        finally:
            if process.stdin is not None:
                process.stdin.close()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)
        return list(server.seen)


def main() -> None:
    manifest_path = BRIDGE / "manifest.json"
    require(manifest_path)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("manifest_version") != 3:
        raise SystemExit("Browser Bridge must use Manifest V3")
    permissions = set(manifest.get("permissions", []))
    required_permissions = {
        "debugger",
        "tabs",
        "tabGroups",
        "sessions",
        "storage",
        "alarms",
        "nativeMessaging",
        "downloads",
    }
    missing_permissions = required_permissions - permissions
    if missing_permissions:
        raise SystemExit(f"Browser Bridge missing permissions: {sorted(missing_permissions)}")

    referenced = [
        manifest["background"]["service_worker"],
        manifest["action"]["default_popup"],
        *manifest.get("icons", {}).values(),
    ]
    for relative in referenced:
        require(BRIDGE / relative)

    service_worker = BRIDGE / manifest["background"]["service_worker"]
    node = shutil.which("node")
    if not node:
        raise SystemExit("node is required for Browser Bridge conformance")
    subprocess.run([node, "--check", str(service_worker)], check=True)

    native_host = BRIDGE / "native_host.py"
    installer = BRIDGE / "install.py"
    require(native_host)
    require(installer)
    subprocess.run(
        [sys.executable, "-m", "py_compile", str(native_host), str(installer)],
        check=True,
    )

    # Framing remains valid even when no daemon is available.
    env = {**os.environ, "COMPTROL_DAEMON_URL": "http://127.0.0.1:1"}
    process = subprocess.Popen(
        [sys.executable, str(native_host)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    response = send_native_message(
        process,
        {"type": "handshake", "protocol": PROTOCOL},
    )
    if response.get("type") != "handshake_ack":
        process.kill()
        raise SystemExit(f"unexpected native host handshake response: {response}")
    assert process.stdin is not None
    process.stdin.close()
    process.wait(timeout=5)

    # A listener that cannot prove possession of the install secret must never
    # receive the bearer token.
    bad_seen = exercise_host_against_probe(native_host, valid_proof=False)
    if not any(item["path"] == "/browser-auth/challenge" for item in bad_seen):
        raise SystemExit("native host did not challenge the loopback daemon")
    leaked = [item for item in bad_seen if item["token"]]
    if leaked:
        raise SystemExit(f"native host leaked Browser Bridge token before identity proof: {leaked}")

    # A listener with the correct HMAC proof receives authenticated bridge
    # traffic after proof succeeds.
    good_seen = exercise_host_against_probe(native_host, valid_proof=True)
    authenticated = [
        item
        for item in good_seen
        if item["path"] == "/browser/extension/heartbeat" and item["token"] == "ab" * 32
    ]
    if not authenticated:
        raise SystemExit(f"native host did not authenticate to verified daemon: {good_seen}")

    print(
        json.dumps(
            {
                "manifest": "valid",
                "javascript": "syntax_clean",
                "python": "syntax_clean",
                "native_messaging_handshake": "passed",
                "rogue_daemon_token_leak": "blocked",
                "verified_daemon_authentication": "passed",
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
