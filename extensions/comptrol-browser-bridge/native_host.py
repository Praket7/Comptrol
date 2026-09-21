#!/usr/bin/env python3
"""
Comptrol Browser Bridge - Native Messaging Host

This script acts as the native messaging host for the Comptrol Browser Bridge extension.
It communicates with the extension via stdin/stdout using length-prefixed JSON messages,
and forwards commands to the local Comptrol daemon via HTTP.

Message format (native messaging):
- 4-byte little-endian length prefix
- UTF-8 JSON message body

Protocol:
- Extension -> Host: {"type": "...", ...}
- Host -> Extension: {"type": "...", ...}
"""

import json
import os
import struct
import sys
import time
import threading
import urllib.error
import urllib.request

LOCAL_DAEMON_URL = os.environ.get("COMPTROL_DAEMON_URL", "http://127.0.0.1:8765")
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

def forward_to_daemon(endpoint, method, params=None):
    """Forward request to local Comptrol daemon."""
    url = f"{LOCAL_DAEMON_URL}/v1/browser/{endpoint}"
    data = json.dumps(params or {}).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST"
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        return {"ok": False, "error": f"daemon_http_{e.code}", "details": e.read().decode("utf-8")}
    except urllib.error.URLError as e:
        return {"ok": False, "error": "daemon_unreachable", "details": str(e)}

def main():
    """Main loop: read from extension, forward to daemon, write response."""
    # Send handshake acknowledgment
    write_message({
        "type": "handshake_ack",
        "protocol": PROTOCOL_VERSION,
        "timestamp": time.time()
    })
    
    while True:
        message = read_message()
        if message is None:
            break
        
        if not isinstance(message, dict) or message.get("protocol") != PROTOCOL_VERSION:
            write_message({"type": "error", "error": "invalid_protocol"})
            continue
        
        msg_type = message.get("type")
        request_id = message.get("request_id")
        
        if msg_type == "cdp_command":
            # Forward CDP command to daemon
            result = forward_to_daemon(
                "cdp/command",
                message.get("method"),
                message.get("params")
            )
            result["request_id"] = message.get("request_id")
            write_message(result)
            
        elif msg_type == "get_targets":
            # Get targets from daemon
            result = forward_to_daemon("targets/list", {})
            result["request_id"] = request_id
            write_message(result)
            
        elif msg_type == "attach_debugger":
            # Attach debugger
            result = forward_to_daemon(
                "debugger/attach",
                {"target_id": message.get("target_id")}
            )
            result["request_id"] = request_id
            write_message(result)
            
        elif msg_type == "detach_debugger":
            result = forward_to_daemon(
                "debugger/detach",
                {"target_id": message.get("target_id")}
            )
            result["request_id"] = request_id
            write_message(result)
            
        elif msg_type == "restore_group":
            result = forward_to_daemon(
                "groups/restore",
                {"group_id": message.get("group_id")}
            )
            result["request_id"] = request_id
            write_message(result)
            
        elif msg_type == "get_targets":
            result = forward_to_daemon("targets/list", {})
            result["request_id"] = request_id
            write_message(result)
            
        elif msg_type == "get_status":
            result = forward_to_daemon("status", {})
            result["request_id"] = request_id
            write_message(result)
            
        else:
            write_message({
                "type": "error",
                "error": f"unknown_message_type: {msg_type}",
                "request_id": request_id
            })

if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        write_message({"type": "error", "error": f"host_crash: {str(e)}"})
        sys.exit(1)