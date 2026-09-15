#!/usr/bin/env python3
"""Exercise the local Unix socket IPC framing and health contract."""

import json
import os
import socket
import stat
import subprocess
import tempfile
import time


if not hasattr(socket, "AF_UNIX"):
    print("IPC conformance skipped because Unix sockets are unavailable")
    raise SystemExit(0)


def frame(value):
    payload = json.dumps(value, separators=(",", ":")).encode()
    return len(payload).to_bytes(4, "big") + payload


def read_frame(connection):
    header = connection.recv(4)
    if len(header) != 4:
        raise RuntimeError("IPC response did not include a complete frame header")
    length = int.from_bytes(header, "big")
    payload = b""
    while len(payload) < length:
        chunk = connection.recv(length - len(payload))
        if not chunk:
            raise RuntimeError("IPC response ended before the complete frame")
        payload += chunk
    return json.loads(payload)


with tempfile.TemporaryDirectory(prefix="comptrol-ipc-") as state:
    path = os.path.join(state, "comptrol.sock")
    process = subprocess.Popen(
        ["target/debug/comptrol", "daemon"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env={**os.environ, "COMPTROL_STATE_DIR": state, "COMPTROL_SOCKET_PATH": path},
    )
    try:
        deadline = time.time() + 10
        while time.time() < deadline and not os.path.exists(path):
            time.sleep(0.05)
        if not os.path.exists(path):
            raise RuntimeError("daemon socket did not appear")
        assert stat.S_IMODE(os.stat(path).st_mode) == 0o600
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.connect(path)
            connection.sendall(frame({"version": 1, "id": "health", "method": "health"}))
            health = read_frame(connection)
            assert health["result"]["ready"] is True
            initialize = {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {},
            }
            connection.sendall(frame({"version": 1, "id": "mcp", "method": "mcp", "message": initialize}))
            response = read_frame(connection)
            assert response["result"]["result"]["serverInfo"]["name"] == "comptrol"
            connection.sendall(frame({"version": 2, "id": "bad", "method": "health"}))
            version_error = read_frame(connection)
            assert version_error["error"]["code"] == "protocol_version_unsupported"
        print("IPC conformance passed")
    finally:
        process.terminate()
        process.wait(timeout=5)
