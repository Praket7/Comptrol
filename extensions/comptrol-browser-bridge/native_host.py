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
    """
    Background thread that polls the daemon for pending commands
    and forwards them to the extension via native messaging.
    """
    poll_interval = POLL_INTERVAL_MS / 1000.0

    while True:
        # Wait a bit before polling
        time.sleep(poll_interval)

        connected = native_port_ref.get("connected", False)
        if not connected:
            continue

        # Poll daemon for pending commands
        response = daemon_post("/browser/command/poll")
        if not response.get("ok"):
            # Daemon unreachable or error; back off
            poll_interval = min(poll_interval * 1.5, POLL_INTERVAL_MAX_MS / 1000.0)
            continue

        commands = response.get("commands", [])
        if not commands:
            # No commands; use minimum interval
            poll_interval = POLL_INTERVAL_MS / 1000.0
            continue

        # Forward each command to the extension
        for cmd in commands:
            command_type = cmd.get("command_type", "")
            request_id = cmd.get("request_id", "")
            payload = cmd.get("payload", {})

            # Map daemon command types to extension message types
            extension_msg = {
                "type": command_type,
                "requestId": request_id,
                **payload,
            }

            try:
                write_message(extension_msg)
            except Exception as e:
                # Failed to send; report error back to daemon
                daemon_post("/browser/command/result", {
                    "request_id": request_id,
                    "error": {"type": "send_failed", "details": str(e)},
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
                "event": message.get("event"),
                "targetId": message.get("targetId"),
                "data": message.get("data"),
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

        elif msg_type == "attach_debugger_result":
            # Extension sends result for a command we forwarded from daemon
            if request_id:
                daemon_post("/browser/command/result", {
                    "request_id": request_id,
                    "result": {
                        "ok": message.get("ok"),
                        "targetId": message.get("targetId"),
                    },
                })
            write_message({"ok": True})

        elif msg_type == "detach_debugger_result":
            if request_id:
                daemon_post("/browser/command/result", {
                    "request_id": request_id,
                    "result": {
                        "ok": message.get("ok"),
                        "targetId": message.get("targetId"),
                    },
                })
            write_message({"ok": True})

        elif msg_type == "restore_group_result":
            if request_id:
                daemon_post("/browser/command/result", {
                    "request_id": request_id,
                    "result": {
                        "ok": message.get("ok"),
                        "restored": message.get("restored"),
                    },
                })
            write_message({"ok": True})

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
