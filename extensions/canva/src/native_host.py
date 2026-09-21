#!/usr/bin/env python3
"""Comptrol Canva Companion Native Messaging Host.

This script acts as the native messaging host for the Comptrol Canva
extension. It communicates with the extension via stdin/stdout using
length-prefixed JSON messages, and forwards them to the local HTTP
bridge server (running on http://127.0.0.1:8765).

Message format (native messaging):
- 4-byte little-endian length prefix
- UTF-8 JSON message body

Protocol:
- Extension -> Host: {"protocol": "...", "request_id": "...", "token": "...", "nonce": "...", "session_id": "...", "design_id": "...", "op": {...}}
- Host -> Extension: {"ok": true, "protocol": "...", "request_id": "...", "receipt": {...}, "nonce_echo": "..."}

The host validates each message against the local bridge server and
returns correlated receipts.
"""

import json
import os
import struct
import sys
import time
import urllib.error
import urllib.request

LOCAL_BRIDGE_URL = os.environ.get("COMPTROL_CANVA_BRIDGE_URL", "http://127.0.0.1:8765")
PROTOCOL = "comptrol.canva.bridge/0.1.0"
NATIVE_HOST_ID = "comptrol_canva_native_host"

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

def forward_to_bridge(envelope):
    """Forward envelope to local HTTP bridge and return response."""
    url = f"{LOCAL_BRIDGE_URL}/v1/op/submit"
    data = json.dumps(envelope).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=data,
        headers={
            "Content-Type": "application/json",
            "X-Comptrol-Native-Host": NATIVE_HOST_ID,
        },
        method="POST"
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        return {"ok": False, "error": f"bridge_http_{e.code}", "details": e.read().decode("utf-8")}
    except urllib.error.URLError as e:
        return {"ok": False, "error": "bridge_unreachable", "details": str(e)}

def main():
    """Main loop: read from extension, forward to bridge, write response."""
    while True:
        message = read_message()
        if message is None:
            break
        
        if not isinstance(message, dict) or message.get("protocol") != PROTOCOL:
            write_message({"ok": False, "error": "invalid_protocol", "request_id": message.get("request_id")})
            continue
        
        request_id = message.get("request_id")
        response = forward_to_bridge(message)
        # Preserve request_id in response for correlation
        if request_id:
            response["request_id"] = request_id
        write_message(response)

if __name__ == "__main__":
    main()