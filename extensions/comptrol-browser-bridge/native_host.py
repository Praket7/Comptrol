#!/usr/bin/env python3
"""
Comptrol Browser Bridge - Native Messaging Host (v2 channel-truth)

This script acts as the native messaging host for the Comptrol Browser Bridge
extension. It communicates with the extension via stdin/stdout using
length-prefixed JSON messages, and forwards events to the local Comptrol
sidecar (comptrol serve-http) via HTTP.

v2 changes (from live failure analysis):

1. ROUND-TRIP TRUTH: every few polls the host submits a `bridge_ping` probe
   through the real command channel. The extension answers over its native
   port; the sidecar records the measured round trip. Health becomes
   "round-trip verified" instead of "we recently POSTed a heartbeat", which
   is what allowed a dead service worker to read as healthy.

2. WAKE POLLING: the host drains POST /browser/wake and forwards a `wake`
   message to the extension, whose onMessage listener resuscitates a
   suspended MV3 service worker.

3. IDENTITY CACHE: the daemon HMAC identity challenge is verified once per
   token (mtime) instead of on every POST, removing per-command overhead.

4. PIPE LIVENESS: the host tracks the last time the extension actually
   answered anything. If the pipe looks stale, the host exits so Chrome
   relaunches a fresh host on the SW's next connectNative - host restarts
   become cheap recovery instead of zombie long-polls.
"""

import hashlib
import hmac
import http.client
import json
import os
import secrets
import struct
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

if os.name == "nt":
    import msvcrt
    msvcrt.setmode(sys.stdin.fileno(), os.O_BINARY)
    msvcrt.setmode(sys.stdout.fileno(), os.O_BINARY)

HOST_DIR = os.path.dirname(os.path.abspath(__file__))
DEFAULT_STATE_DIR = os.environ.get(
    "COMPTROL_STATE_DIR",
    os.path.join(os.path.expanduser("~"), ".comptrol"),
)
CONFIG_PATH = os.environ.get(
    "COMPTROL_BRIDGE_CONFIG",
    os.path.join(DEFAULT_STATE_DIR, "browser-bridge-config.json"),
)


def load_host_config():
    try:
        with open(CONFIG_PATH, "r", encoding="utf-8") as config_file:
            value = json.load(config_file)
            return value if isinstance(value, dict) else {}
    except (OSError, ValueError):
        return {}


HOST_CONFIG = load_host_config()
STATE_DIR = HOST_CONFIG.get("state_dir") or os.environ.get("COMPTROL_STATE_DIR", DEFAULT_STATE_DIR)
LOCAL_DAEMON_URL = HOST_CONFIG.get("daemon_url") or os.environ.get("COMPTROL_DAEMON_URL", "http://127.0.0.1:7317")
BRIDGE_TOKEN_PATH = os.path.join(STATE_DIR, "browser-bridge.token")
PROTOCOL_VERSION = "comptrol.browser.bridge/0.1.0"
NATIVE_HOST_ID = "comptrol_browser_bridge"
MAX_NATIVE_MESSAGE_BYTES = 1024 * 1024
POLL_INTERVAL_MS = 200
POLL_INTERVAL_MAX_MS = 2000
PROBE_INTERVAL_SECONDS = 6.0
WAKE_POLL_INTERVAL_SECONDS = 4.0
PIPE_STALE_SECONDS = 90.0
# P2.1: the push stream sends a heartbeat at least this often (server uses15 s);
# a silent stream longer than this is dead and gets reconnected.
STREAM_IDLE_TIMEOUT_SECONDS = 45.0
STREAM_STALE_SECONDS = 5.0
# Identity challenges are latency-critical (they sit in front of every daemon
# POST and inside the extension handshake budget), so they fail fast and are
# retried instead of blocking the pipe for the full daemon timeout.
IDENTITY_TIMEOUT_SECONDS = 2.0


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
# Cache: (token_mtime, ok) - the challenge is re-verified only when the token
# file changes, not on every daemon_post() call.
_daemon_identity_cache = {"key": None, "ok": False, "at": 0.0}


def _daemon_post_raw(endpoint, params=None, token=None, timeout=10):
    parsed_endpoint = urllib.parse.urlsplit(LOCAL_DAEMON_URL)
    if (
        parsed_endpoint.scheme != "http"
        or parsed_endpoint.hostname not in {"127.0.0.1", "::1"}
        or parsed_endpoint.username is not None
        or parsed_endpoint.password is not None
        or parsed_endpoint.path not in {"", "/"}
        or parsed_endpoint.query
        or parsed_endpoint.fragment
    ):
        return {"ok": False, "error": "daemon_endpoint_must_be_loopback_http"}
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
        with urllib.request.urlopen(req, timeout=timeout) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        return {"ok": False, "error": f"daemon_http_{e.code}"}
    except urllib.error.URLError:
        return {"ok": False, "error": "daemon_unreachable"}
    except Exception as e:
        return {"ok": False, "error": "daemon_error", "details": str(e)}


def verify_daemon_identity():
    """Verify the process bound to the daemon port knows the secret.

    Cached per token-file mtime: the challenge round trips cost several HTTP
    exchanges, and identity cannot change without the token changing. Only a
    successful verification is cached for the token's lifetime; a failure is
    retried after a short pause so a daemon that starts (or restarts) later is
    re-probed promptly instead of staying "unverified" until the token file
    happens to change.
    """
    token = load_bridge_token()
    if not token:
        return False
    try:
        cache_key = os.stat(BRIDGE_TOKEN_PATH).st_mtime_ns
    except OSError:
        cache_key = None
    with _daemon_identity_lock:
        now = time.monotonic()
        if cache_key is not None and _daemon_identity_cache["key"] == cache_key:
            cached_ok = _daemon_identity_cache["ok"]
            if cached_ok or now - _daemon_identity_cache["at"] < 1.0:
                return cached_ok
        nonce = secrets.token_hex(32)
        response = _daemon_post_raw(
            "/browser-auth/challenge",
            {"nonce": nonce},
            timeout=IDENTITY_TIMEOUT_SECONDS,
        )
        proof = response.get("proof") if isinstance(response, dict) else None
        expected = hmac.new(
            token.encode("ascii"),
            (PROTOCOL_VERSION + "\0" + nonce).encode("ascii"),
            hashlib.sha256,
        ).hexdigest()
        ok = (
            response.get("ok") is True
            and response.get("protocol") == PROTOCOL_VERSION
            and isinstance(proof, str)
            and hmac.compare_digest(proof.lower(), expected.lower())
        )
        _daemon_identity_cache["key"] = cache_key
        _daemon_identity_cache["ok"] = ok
        _daemon_identity_cache["at"] = now
        return ok


def daemon_post(endpoint, params=None, timeout=10):
    """POST only after the current local daemon proves knowledge of the secret."""
    token = load_bridge_token()
    if not token:
        return {"ok": False, "error": "bridge_token_missing"}
    if not verify_daemon_identity():
        return {"ok": False, "error": "daemon_identity_unverified"}
    return _daemon_post_raw(endpoint, params, token=token, timeout=timeout)


def signed_headers(endpoint, body, token):
    """HMAC request signature, shared by POSTs and the push stream."""
    nonce = secrets.token_hex(32)
    body_hash = hashlib.sha256(body).hexdigest()
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
    return {
        "Content-Type": "application/json",
        "X-Comptrol-Bridge-Nonce": nonce,
        "X-Comptrol-Bridge-Signature": signature,
    }


def stream_events(endpoint, params, stream_state):
    """P2.1 push transport: one persistent signed connection to the sidecar.

    Yields (event_name, payload_dict) frames pushed by the sidecar: commands
    arrive the instant they are submitted (no poll gap) and wake notices are
    multiplexed on the same connection. Raises on any transport error so the
    caller can fall back to the poll loop and reconnect.
    """
    token = load_bridge_token()
    if not token:
        raise RuntimeError("bridge_token_missing")
    if not verify_daemon_identity():
        raise RuntimeError("daemon_identity_unverified")
    parsed = urllib.parse.urlsplit(LOCAL_DAEMON_URL)
    connection = http.client.HTTPConnection(
        parsed.hostname,
        parsed.port,
        timeout=STREAM_IDLE_TIMEOUT_SECONDS,
    )
    body = json.dumps(params or {}, separators=(",", ":")).encode("utf-8")
    try:
        connection.request(
            "POST",
            endpoint,
            body=body,
            headers=signed_headers(endpoint, body, token),
        )
        response = connection.getresponse()
        if response.status != 200:
            raise RuntimeError(f"stream_http_{response.status}")
        buffer = b""
        while True:
            # read(n) waits for n bytes on an open-ended HTTP response. SSE
            # commands are small and the server deliberately keeps the stream
            # open, so that call silently held leased commands until timeout.
            chunk = response.read1(65536)
            if not chunk:
                return
            buffer += chunk
            while b"\n\n" in buffer:
                raw, buffer = buffer.split(b"\n\n", 1)
                event_name = "message"
                data = None
                for line in raw.split(b"\n"):
                    if line.startswith(b"event: "):
                        event_name = line[7:].decode("utf-8", "replace")
                    elif line.startswith(b"data: "):
                        try:
                            data = json.loads(line[6:].decode("utf-8"))
                        except ValueError:
                            data = None
                if data is not None:
                    stream_state["last_frame"] = time.monotonic()
                    yield event_name, data
    finally:
        try:
            connection.close()
        except Exception:
            pass


def forward_command(command, liveness):
    """Deliver one leased bridge command to the extension over the pipe."""
    extension_msg = {
        "type": command.get("command_type", ""),
        "requestId": command.get("request_id", ""),
        **(command.get("payload") or {}),
    }
    try:
        write_message(extension_msg)
    except Exception as e:
        daemon_post("/browser/command/result", {
            "request_id": command.get("request_id", ""),
            "ok": False,
            "error": {
                "type": "send_failed",
                "details": str(e),
            },
        })


def command_stream_loop(native_port_ref, liveness, stream_state):
    """P2.1: consume the sidecar push stream; poll is the fallback only."""
    error_backoff = POLL_INTERVAL_MS / 1000.0
    while True:
        if not native_port_ref.get("connected", False):
            time.sleep(POLL_INTERVAL_MS / 1000.0)
            continue
        try:
            for event_name, data in stream_events(
                "/browser/command/stream",
                {"profile_id": native_port_ref.get("profile_id")},
                stream_state,
            ):
                if not native_port_ref.get("connected", False):
                    break
                if event_name == "command":
                    liveness.touch()
                    forward_command(data, liveness)
                elif event_name == "wake":
                    liveness.touch()
                    try:
                        write_message({
                            "type": "wake",
                            "reason": data.get("reason", "daemon_request"),
                        })
                    except Exception:
                        pass
                # heartbeat frames only refresh stream_state.
            # Clean server-side stream expiry: reconnect immediately.
            error_backoff = POLL_INTERVAL_MS / 1000.0
        except Exception:
            stream_state["last_frame"] = 0.0
            time.sleep(error_backoff)
            error_backoff = min(
                error_backoff * 1.5,
                POLL_INTERVAL_MAX_MS / 1000.0,
            )


def stream_healthy(stream_state):
    return (
        stream_state.get("last_frame", 0.0) > 0.0
        and time.monotonic() - stream_state["last_frame"] < STREAM_STALE_SECONDS
    )


class PipeLiveness:
    """Tracks whether the extension actually answered anything recently."""

    def __init__(self):
        self.lock = threading.Lock()
        self.last_extension_activity = time.monotonic()

    def touch(self):
        with self.lock:
            self.last_extension_activity = time.monotonic()

    def stale_seconds(self):
        with self.lock:
            return time.monotonic() - self.last_extension_activity


def command_poll_loop(native_port_ref, liveness, stream_state):
    """Long-poll leased commands; keep host heartbeat and channel truth fresh.

    Two distinct signals, never conflated:
      - HOST HEARTBEAT: this process is alive and its Chrome pipe exists.
      - CHANNEL ROUND TRIP: a bridge_ping probe answered by the actual
        service worker. Only the probe proves the extension channel works.
    """
    error_backoff = POLL_INTERVAL_MS / 1000.0
    last_heartbeat = 0.0
    last_probe = 0.0
    last_wake_poll = 0.0

    while True:
        if not native_port_ref.get("connected", False):
            time.sleep(POLL_INTERVAL_MS / 1000.0)
            continue

        # Recoverable-death path: if the extension has not answered anything
        # for a long time, exit. Chrome relaunches the host when the service
        # worker next calls connectNative, and the fresh host re-handshakes.
        # This is what turns SW suspension from permanent zombie into a
        # sub-second recovery.
        if liveness.stale_seconds() > PIPE_STALE_SECONDS:
            print("Comptrol native host: Chrome pipe stale; exiting for Chrome relaunch", file=sys.stderr)
            os._exit(0)

        now = time.monotonic()

        # Host liveness heartbeat (daemon-side only, explicitly NOT channel
        # truth - the sidecar no longer records these as extension proof).
        if now - last_heartbeat >= 2.0:
            heartbeat = daemon_post("/browser/extension/heartbeat", {
                "protocol": PROTOCOL_VERSION,
                "profile_id": native_port_ref.get("profile_id"),
                "extension_id": HOST_CONFIG.get("extension_id", ""),
            }, timeout=3)
            if heartbeat.get("ok"):
                last_heartbeat = now

        # While the P2.1 push stream is healthy it carries commands AND wake
        # notices instantly; polling is the fallback for stream outages. The
        # channel-truth probe and heartbeat above keep running either way —
        # only their delivery path changes (the stream carries the ping).
        stream_ok = stream_healthy(stream_state)

        if not stream_ok:
            # Wake polling: drain daemon-recorded wake requests and forward them;
            # the SW's onMessage handler wakes a suspended worker.
            if now - last_wake_poll >= WAKE_POLL_INTERVAL_SECONDS:
                last_wake_poll = now
                wake = daemon_post("/browser/wake", {}, timeout=3)
                if wake.get("ok") and wake.get("wake"):
                    try:
                        write_message({"type": "wake", "reason": wake["wake"].get("reason", "daemon_request")})
                    except Exception:
                        pass

        # Channel truth probe: submit a ping through the real command queue;
        # the sidecar records the measured round trip when the result lands.
        if now - last_probe >= PROBE_INTERVAL_SECONDS:
            last_probe = now
            daemon_post("/browser/probe", {}, timeout=3)

        if stream_ok:
            time.sleep(POLL_INTERVAL_MS / 1000.0)
            continue

        response = daemon_post(
            "/browser/command/poll",
            {"wait_ms": 800, "profile_id": native_port_ref.get("profile_id")},
        )
        if not response.get("ok"):
            time.sleep(error_backoff)
            error_backoff = min(
                error_backoff * 1.5,
                POLL_INTERVAL_MAX_MS / 1000.0,
            )
            continue

        commands = response.get("commands", [])
        error_backoff = POLL_INTERVAL_MS / 1000.0
        for cmd in commands:
            liveness.touch()
            forward_command(cmd, liveness)


def main():
    """Main loop: read from extension, forward to daemon, write response."""
    handshake_complete = False
    native_port_ref = {"port": None, "connected": False}
    liveness = PipeLiveness()

    # Chrome supplies the caller's exact extension origin as argv[1]. Its
    # native-messaging permission check is primary; this identity check makes
    # accidental registrations or wrapper mixups fail closed as well.
    caller_origin = sys.argv[1] if len(sys.argv) > 1 else None
    expected_origin = f"chrome-extension://{HOST_CONFIG.get('extension_id', '')}/"
    if caller_origin is not None and caller_origin != expected_origin:
        print("Native messaging caller does not match the registered Browser Bridge extension", file=sys.stderr)
        return

    # P2.1: the push stream delivers commands instantly; the poll loop is
    # kept as automatic fallback while the stream is down.
    stream_state = {"last_frame": 0.0}
    stream_thread = threading.Thread(
        target=command_stream_loop,
        args=(native_port_ref, liveness, stream_state),
        daemon=True,
    )
    stream_thread.start()
    poll_thread = threading.Thread(
        target=command_poll_loop,
        args=(native_port_ref, liveness, stream_state),
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

        # Any message from the extension proves the pipe is alive.
        liveness.touch()

        msg_type = message.get("type", "")
        request_id = message.get("requestId") or message.get("request_id")

        # Validate protocol only on handshake
        if msg_type == "handshake":
            if message.get("protocol") != PROTOCOL_VERSION:
                write_message({"type": "error", "error": "invalid_protocol"})
                continue
            daemon_identity_ok = verify_daemon_identity()
            heartbeat = daemon_post("/browser/extension/heartbeat", {
                "protocol": PROTOCOL_VERSION,
                "profile_id": message.get("profileId"),
                "extension_id": HOST_CONFIG.get("extension_id", ""),
            }, timeout=2)
            native_port_ref["profile_id"] = message.get("profileId")
            handshake_complete = daemon_identity_ok and bool(heartbeat.get("ok"))
            native_port_ref["connected"] = handshake_complete
            write_message({
                "type": "handshake_ack",
                "protocol": PROTOCOL_VERSION,
                "daemon_authorized": daemon_identity_ok,
                "daemon_state": "ready" if heartbeat.get("ok") else heartbeat.get("error", "daemon_unavailable"),
                "extension_id": HOST_CONFIG.get("extension_id"),
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
                "profile_id": message.get("profileId"),
                "extension_id": HOST_CONFIG.get("extension_id", ""),
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
