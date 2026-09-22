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

import json
import os
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

LOCAL_DAEMON_URL = os.environ.get("COMPTROL_DAEMON_URL", "http://127.0.0.1:7317")
PROTOCOL_VERSION = "comptrol.browser.bridge/0.1.0"
NATIVE_HOST_ID = "comptrol_browser_bridge"
POLL_INTERVAL_MS = 200
POLL_INTERVAL_MAX_MS = 2000


# Shared lock for stdout writes to prevent interleaved messages
stdout_lock = threading.Lock()

def read_message():
    """Read a length-prefixed JSON message from stdin."""
    raw_length = sys.stdin.buffer.read(4)
    if len(raw_length) == 0:
        return None
    length = struct.unpack("<I", raw_length)[0]
    message = sys.stdin.buffer.read(length).decode("utf-8")
    return json.loads(message)


def write_message(message):
    """Write a length-prefixed JSON message to stdout with locking."""
    encoded = json.dumps(message, separators=(",", ":")).encode("utf-8")
    with stdout_lock:
        sys.stdout.buffer.write(struct.pack("<I", len(encoded)))
        sys.stdout.buffer.write(encoded)
        sys.stdout.buffer.flush()


def daemon_post(endpoint, params=None):
    """POST to daemon HTTP endpoint."""
    url = f"{LOCAL_DAEMON_URL}{endpoint}"
    data = json.dumps(params or {}).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/json"},
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


def command_poll_loop(native_port_ref):
    """Poll leased daemon commands and keep the active bridge heartbeat fresh."""
    poll_interval = POLL_INTERVAL_MS / 1000.0
    last_heartbeat = 0.0

    while True:
        time.sleep(poll_interval)
        if not native_port_ref.get("connected", False):
            continue

        now = time.monotonic()
        if now - last_heartbeat >= 2.0:
            heartbeat = daemon_post("/browser/extension/heartbeat", {
                "protocol": PROTOCOL_VERSION,
            })
            if heartbeat.get("ok"):
                last_heartbeat = now

        response = daemon_post("/browser/command/poll")
        if not response.get("ok"):
            poll_interval = min(
                poll_interval * 1.5,
                POLL_INTERVAL_MAX_MS / 1000.0,
            )
            continue

        commands = response.get("commands", [])
        if not commands:
            poll_interval = POLL_INTERVAL_MS / 1000.0
            continue

        poll_interval = POLL_INTERVAL_MS / 1000.0
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
