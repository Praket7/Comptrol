#!/usr/bin/env python3
"""Exercise the local daemon IPC framing and health contract."""

import json
import os
import pathlib
import socket
import stat
import subprocess
import sys
import tempfile
import time


is_windows = sys.platform == "win32"
if not is_windows and not hasattr(socket, "AF_UNIX"):
    print("IPC conformance skipped because Unix sockets are unavailable")
    raise SystemExit(0)


def frame(value):
    payload = json.dumps(value, separators=(",", ":")).encode()
    return len(payload).to_bytes(4, "big") + payload


def read_exact(connection, length):
    payload = b""
    while len(payload) < length:
        chunk = connection.read(length - len(payload)) if is_windows else connection.recv(length - len(payload))
        if not chunk:
            raise RuntimeError("IPC response ended before the complete frame")
        payload += chunk
    return payload


def read_frame(connection):
    header = read_exact(connection, 4)
    if len(header) != 4:
        raise RuntimeError("IPC response did not include a complete frame header")
    length = int.from_bytes(header, "big")
    return json.loads(read_exact(connection, length))


def send_frame(connection, value):
    payload = frame(value)
    if is_windows:
        connection.write(payload)
        connection.flush()
    else:
        connection.sendall(payload)


def connect_pipe(path, timeout=10):
    deadline = time.monotonic() + timeout
    while True:
        try:
            return open(path, "r+b", buffering=0)
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


with tempfile.TemporaryDirectory(prefix="comptrol-ipc-") as state:
    path = os.environ.get("COMPTROL_PIPE_NAME", r"\\.\pipe\comptrol") if is_windows else os.path.join(state, "comptrol.sock")
    root = pathlib.Path(__file__).resolve().parents[1]
    binary = pathlib.Path(os.environ.get("COMPTROL_BIN", str(root / "target" / "debug" / ("comptrol.exe" if is_windows else "comptrol"))))
    process = subprocess.Popen(
        [str(binary), "daemon"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env={**os.environ, "COMPTROL_STATE_DIR": state, "COMPTROL_SOCKET_PATH": path, "COMPTROL_PIPE_NAME": path},
    )
    try:
        deadline = time.time() + 10
        while time.time() < deadline and (is_windows or not os.path.exists(path)):
            time.sleep(0.05)
        if not is_windows and not os.path.exists(path):
            raise RuntimeError("daemon socket did not appear")
        if not is_windows:
            assert stat.S_IMODE(os.stat(path).st_mode) == 0o600
            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connection.connect(path)
        else:
            connection = connect_pipe(path)
        try:
            send_frame(connection, {"version": 1, "id": "health", "method": "health"})
            health = read_frame(connection)
            assert health["result"]["ready"] is True
            initialize = {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {},
            }
            send_frame(connection, {"version": 1, "id": "mcp", "method": "mcp", "message": initialize})
            response = read_frame(connection)
            assert response["result"]["result"]["serverInfo"]["name"] == "comptrol"
            send_frame(connection, {"version": 2, "id": "bad", "method": "health"})
            version_error = read_frame(connection)
            assert version_error["error"]["code"] == "protocol_version_unsupported"
        finally:
            connection.close()
        oversized = connect_pipe(path) if is_windows else socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        if not is_windows:
            oversized.settimeout(2)
            oversized.connect(path)
        try:
            header = (1024 * 1024 + 1).to_bytes(4, "big")
            if is_windows:
                oversized.write(header)
                oversized.flush()
            else:
                oversized.sendall(header)
            try:
                read_frame(oversized)
            except (OSError, RuntimeError, ValueError):
                pass
            else:
                raise AssertionError("daemon accepted an oversized IPC frame")
        finally:
            oversized.close()
        survivor = connect_pipe(path) if is_windows else socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        if not is_windows:
            survivor.settimeout(2)
            survivor.connect(path)
        try:
            send_frame(survivor, {"version": 1, "id": "health-after-oversize", "method": "health"})
            assert read_frame(survivor)["result"]["ready"] is True
        finally:
            survivor.close()
        print("IPC conformance passed")
    finally:
        process.terminate()
        process.wait(timeout=5)
