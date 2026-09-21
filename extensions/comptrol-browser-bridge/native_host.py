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
"""

import json
import os
import struct
import sys
import time
import urllib.error
import urllib.request

LOCAL_DAEMON_URL = os.environ.get("COMPTROL_DAEMON_URL", "http://127.0.0.1:7317")
PROTOCOL_VERSION = "comptrol.browser.bridge/0.1.0"
NATIVE_HOST_ID = "comptrol_browser_bridge"


def read_message():
    """Read a length-prefixed JSON message from stdin."""
    raw_length = sys.stdin.buffer.read(4)
    if len(raw_length) == 0:
        return None
    length = struct.unpack("<I", raw_length)[0]
    message = sys.stdin.buffer.read(length).decode("utf-8")
    return json.loads(message)


def write_message(message):
    """Write a length-prefixed JSON message to stdout."""
    encoded = json.dumps(message, separators=(",", ":")).encode("utf-8")
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


def main():
    """Main loop: read from extension, forward to daemon, write response."""
    handshake_complete = False

    while True:
        message = read_message()
        if message is None:
            break

        if not isinstance(message, dict):
            write_message({"type": "error", "error": "invalid_message"})
            continue

        msg_type = message.get("type", "")
        # Extension sends camelCase request_id
        request_id = message.get("requestId") or message.get("request_id")

        # Validate protocol only on handshake
        if msg_type == "handshake":
            if message.get("protocol") != PROTOCOL_VERSION:
                write_message({"type": "error", "error": "invalid_protocol"})
                continue
            handshake_complete = True
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
            # Extension reports its discovered targets
            result = daemon_post("/browser/extension/targets", {
                "targets": message.get("targets", []),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "cdp_command_result":
            # Extension sends CDP command result back
            result = daemon_post("/browser/extension/cdp_result", {
                "requestId": request_id,
                "result": message.get("result"),
                "error": message.get("error"),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "debugger_event":
            # Extension sends debugger event (attached, detached, etc.)
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
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "debugger_detached":
            result = daemon_post("/browser/extension/event", {
                "event": "debugger_detached",
                "targetId": message.get("targetId"),
                "reason": message.get("reason"),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "attach_debugger_result":
            result = daemon_post("/browser/extension/event", {
                "event": "attach_result",
                "targetId": message.get("targetId"),
                "ok": message.get("ok"),
                "error": message.get("error"),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "detach_debugger_result":
            result = daemon_post("/browser/extension/event", {
                "event": "detach_result",
                "targetId": message.get("targetId"),
                "ok": message.get("ok"),
                "error": message.get("error"),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

        elif msg_type == "restore_group_result":
            result = daemon_post("/browser/extension/event", {
                "event": "restore_group_result",
                "requestId": request_id,
                "ok": message.get("ok"),
                "restored": message.get("restored"),
                "error": message.get("error"),
            })
            if request_id:
                result["requestId"] = request_id
            write_message(result)

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
