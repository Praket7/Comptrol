#!/usr/bin/env python3
"""
Comptrol Browser Bridge - Native Messaging Host

This script acts as the native messaging host for the Comptrol Browser Bridge extension.
It communicates with the extension via stdin/stdout using length-prefixed JSON messages,
and forwards events to the local Comptrol daemon via HTTP.

Message format (native messaging):
- 4-byte little-endian length prefix
- UTF-8 JSON message body

Protocol (all fields are camelCase to match Chrome extension conventions):
- Extension -> Host: {"type": "...", ...}
- Host -> Extension: {"type": "...", ...}

Command channel:
- Host polls daemon for pending commands via POST /browser/command/poll
- Host forwards commands to extension via native messaging
- Extension sends results back through native messaging
- Host posts results to daemon via POST /browser/command/result
"""

import hashlib
import hmac
import json
import os
import secrets
import struct
import sys
import threading
import time
import urllib.error
import urllib.request

if os.name == "nt":
    import msvcrt
    msvcrt.setmode(sys.stdin.fileno(), os.O_BINARY)
    msvcrt.setmode(sys.stdout.fileno(), os.O_BINARY)

HOST_DIR = os.path.dirname(os.path.abspath(__file__))
CONFIG_PATH = os.path.join(HOST_DIR, "native_host_config.json")


def load_host_config():
    try:
        with open(CONFIG_PATH, "r", encoding="utf-8") as config_file:
            value = json.load(config_file)
            return value if isinstance(value, dict) else {}
    except (OSError, ValueError):
        return {}


HOST_CONFIG = load_host_config()
STATE_DIR = os.environ.get(
    "COMPTROL_STATE_DIR",
    HOST_CONFIG.get("state_dir")
    or os.path.join(os.path.expanduser("~"), ".comptrol"),
)
LOCAL_DAEMON_URL = os.environ.get(
    "COMPTROL_DAEMON_URL",
    HOST_CONFIG.get("daemon_url") or "http://127.0.0.1:7317",
)
BRIDGE_TOKEN_PATH = os.path.join(STATE_DIR, "browser-bridge.token")
PROTOCOL_VERSION = "comptrol.browser.bridge/0.1.0"
NATIVE_HOST_ID = "comptrol_browser_bridge"
MAX_NATIVE_MESSAGE_BYTES = 1024 * 1024
LONG_POLL_MS = 1000
ERROR_BACKOFF_MS = 50
DAEMON_IDENTITY_CACHE_SECONDS = 5.0


# Shared lock for stdout writes to prevent interleaved messages
stdout_lock = threading.Lock()

def read_message():
    """Read one bounded length-prefixed JSON message from stdin."""
    raw_length = sys.stdin.buffer.read(4)
    if len(raw_length) == 0:
        return None
    if len(raw_length) != 4:
        raise ValueError("truncated native messaging length prefix")
    length = struct.unpack("<I", raw_length)[0]
    if length > MAX_NATIVE_MESSAGE_BYTES:
        raise ValueError(
            f"native messaging frame exceeds {MAX_NATIVE_MESSAGE_BYTES} bytes"
        )
    payload = sys.stdin.buffer.read(length)
    if len(payload) != length:
        raise ValueError("truncated native messaging payload")
    return json.loads(payload.decode("utf-8"))


def write_message(message):
    """Write one bounded length-prefixed JSON message to stdout with locking."""
    encoded = json.dumps(message, separators=(",", ":")).encode("utf-8")
    if len(encoded) > MAX_NATIVE_MESSAGE_BYTES:
        raise ValueError(
            f"native messaging response exceeds {MAX_NATIVE_MESSAGE_BYTES} bytes"
        )
    with stdout_lock:
        sys.stdout.buffer.write(struct.pack("<I", len(encoded)))
        sys.stdout.buffer.write(encoded)
        sys.stdout.buffer.flush()


def load_bridge_token():
    try:
        with open(BRIDGE_TOKEN_PATH, "r", encoding="utf-8") as token_file:
            token = token_file.read().strip()
        if len(token) >= 64 and all(ch in "0123456789abcdefABCDEF" for ch in token):
            return token
    except OSError:
        pass
    return None


_daemon_identity_lock = threading.Lock()
_daemon_identity_verified_until = 0.0
_daemon_identity_token_digest = None


def _daemon_post_raw(endpoint, params=None, token=None):
    url = f"{LOCAL_DAEMON_URL}{endpoint}"
    data = json.dumps(params or {}, separators=(",", ":")).encode("utf-8")
    headers = {"Content-Type": "application/json"}
    if token:
        nonce = secrets.token_hex(32)
        body_hash = hashlib.sha256(data).hexdigest()
        signing_input = (
            PROTOCOL_VERSION
            + "\0POST\0"
            + endpoint
            + "\0"
            + nonce
            + "\0"
            + body_hash
            + "\0"
        ).encode("ascii")
        signature = hmac.new(
            token.encode("ascii"),
            signing_input,
            hashlib.sha256,
        ).hexdigest()
        headers["X-Comptrol-Bridge-Nonce"] = nonce
        headers["X-Comptrol-Bridge-Signature"] = signature
    req = urllib.request.Request(
        url,
        data=data,
        headers=headers,
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        return {"ok": False, "error": f"daemon_http_{e.code}"}
    except urllib.error.URLError:
        return {"ok": False, "error": "daemon_unreachable"}
    except Exception as e:
        return {"ok": False, "error": "daemon_error", "details": str(e)}


def verify_daemon_identity():
    """Verify the loopback daemon, reusing a short-lived successful proof."""
    global _daemon_identity_verified_until, _daemon_identity_token_digest
    with _daemon_identity_lock:
        token = load_bridge_token()
        if not token:
            return False
        token_digest = hashlib.sha256(token.encode("ascii")).digest()
        now = time.monotonic()
        if (
            _daemon_identity_token_digest == token_digest
            and now < _daemon_identity_verified_until
        ):
            return True
        nonce = secrets.token_hex(32)
        response = _daemon_post_raw("/browser-auth/challenge", {"nonce": nonce})
        proof = response.get("proof") if isinstance(response, dict) else None
        expected = hmac.new(
            token.encode("ascii"),
            (PROTOCOL_VERSION + "\0" + nonce).encode("ascii"),
            hashlib.sha256,
        ).hexdigest()
        verified = (
            response.get("ok") is True
            and response.get("protocol") == PROTOCOL_VERSION
            and isinstance(proof, str)
            and hmac.compare_digest(proof.lower(), expected.lower())
        )
        if verified:
            _daemon_identity_token_digest = token_digest
            _daemon_identity_verified_until = now + DAEMON_IDENTITY_CACHE_SECONDS
        else:
            _daemon_identity_token_digest = None
            _daemon_identity_verified_until = 0.0
        return verified


def daemon_post(endpoint, params=None):
    """POST only after the current local daemon proves knowledge of the secret."""
    token = load_bridge_token()
    if not token:
        return {"ok": False, "error": "bridge_token_missing"}
    if not verify_daemon_identity():
        return {"ok": False, "error": "daemon_identity_unverified"}
    return _daemon_post_raw(endpoint, params, token=token)


def command_poll_loop(native_port_ref):
    """Long-poll commands and keep the active bridge heartbeat fresh."""
    last_heartbeat = 0.0

    while True:
        if not native_port_ref.get("connected", False):
            time.sleep(0.05)
            continue

        now = time.monotonic()
        if now - last_heartbeat >= 2.0:
            heartbeat = daemon_post("/browser/extension/heartbeat", {
                "protocol": PROTOCOL_VERSION,
            })
            if heartbeat.get("ok"):
                last_heartbeat = now

        # The daemon blocks this request on an in-process queue signal. The
        # request returns immediately when a command is submitted and after a
        # bounded timeout when idle, eliminating the old 200 ms pickup tax.
        response = daemon_post(
            "/browser/command/poll",
            {"wait_ms": LONG_POLL_MS},
        )
        if not response.get("ok"):
            time.sleep(ERROR_BACKOFF_MS / 1000.0)
            continue

        commands = response.get("commands", [])
        if not commands:
            # Old daemons/probe servers can return immediately instead of long
            # polling. Avoid a busy loop while retaining a fast compatibility
            # path.
            time.sleep(0.01)
            continue

        for cmd in commands:
            command_type = cmd.get("command_type", "")
            request_id = cmd.get("request_id", "")
            payload = cmd.get("payload", {})
            extension_msg = {
                "type": command_type,
                "requestId": request_id,
                **payload,
            }
            try:
                write_message(extension_msg)
            except Exception as e:
                daemon_post("/browser/command/result", {
                    "request_id": request_id,
                    "ok": False,
                    "error": {
                        "type": "send_failed",
                        "details": str(e),
                    },
                })


def main():
    """Main loop: read from extension, forward to daemon, write response."""
    handshake_complete = False
    native_port_ref = {"port": None, "connected": False}

    # Start the command poll thread
    poll_thread = threading.Thread(
        target=command_poll_loop,
        args=(native_port_ref,),
        daemon=True,
    )
    poll_thread.start()

    while True:
        message = read_message()
        if message is None:
            break

        if not isinstance(message, dict):
            write_message({"type": "error", "error": "invalid_message"})
            continue

        msg_type = message.get("type", "")
        request_id = message.get("requestId") or message.get("request_id")

        # Validate protocol only on handshake
        if msg_type == "handshake":
            if message.get("protocol") != PROTOCOL_VERSION:
                write_message({"type": "error", "error": "invalid_protocol"})
                continue
            handshake_complete = True
            native_port_ref["connected"] = True
            daemon_post("/browser/extension/heartbeat", {
                "protocol": PROTOCOL_VERSION,
            })
            write_message({
                "type": "handshake_ack",
                "protocol": PROTOCOL_VERSION,
                "timestamp": time.time(),
            })
            continue

        # All non-handshake messages require completed handshake
        if not handshake_complete:
            write_message({"type": "error", "error": "handshake_required"})
            continue

        # Handle extension -> daemon message types (all camelCase fields)
        if msg_type == "targets_list":
            result = daemon_post("/browser/extension/targets", {
                "targets": message.get("targets", []),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "cdp_command_result":
            result = daemon_post("/browser/extension/cdp_result", {
                "requestId": request_id,
                "result": message.get("result"),
                "error": message.get("error"),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "debugger_event":
            result = daemon_post("/browser/extension/event", {
                "event": message.get("method") or message.get("event"),
                "targetId": message.get("targetId"),
                "generation": message.get("generation"),
                "data": message.get("params") or message.get("data"),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "group_snapshots":
            result = daemon_post("/browser/extension/event", {
                "event": "group_snapshots",
                "data": message.get("snapshots"),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "debugger_attached":
            result = daemon_post("/browser/extension/event", {
                "event": "debugger_attached",
                "targetId": message.get("targetId"),
                "requestId": request_id,
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "debugger_detached":
            result = daemon_post("/browser/extension/event", {
                "event": "debugger_detached",
                "targetId": message.get("targetId"),
                "reason": message.get("reason"),
                "requestId": request_id,
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type.endswith("_result") and request_id:
            result_value = message.get("result")
            if result_value is None:
                # Preserve useful top-level fields for legacy command handlers.
                result_value = {
                    key: value
                    for key, value in message.items()
                    if key not in {"type", "requestId", "request_id", "error", "ok"}
                }
            daemon_post("/browser/command/result", {
                "request_id": request_id,
                "ok": message.get("ok", message.get("error") is None),
                "result": result_value,
                "error": message.get("error"),
            })
            write_message({"ok": True, "requestId": request_id})

        else:
            write_message({
                "type": "error",
                "error": f"unknown_message_type: {msg_type}",
                "requestId": request_id,
            })


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        try:
            write_message({"type": "error", "error": f"host_crash: {str(e)}"})
        except Exception:
            pass
        sys.exit(1)
