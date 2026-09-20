#!/usr/bin/env python3
"""Shared cross-platform adapter IPC.

One framing, three transports:

- Unix domain socket on macOS/Linux (endpoint is a filesystem path).
- Windows named pipe (endpoint ``pipe:NAME`` maps to ``\\\\.\\pipe\\NAME``).
- Authenticated loopback TCP as a controlled fallback (endpoint
  ``tcp:127.0.0.1:PORT``). The token envelope is mandatory on every
  transport, and TCP never binds or connects beyond loopback.

Endpoint discovery order: explicit argument, then the ``COMPTROL_*``
environment variable, then the user-local descriptor file written by the
paired extension or bridge (``~/.comptrol/bridges/<name>.json``). The
descriptor carries the endpoint only, never the token; the token always
comes from the process environment.
"""

import json
import os
import secrets
import socket
from pathlib import Path

FRAME_LIMIT = 1024 * 1024


def state_dir() -> Path:
    override = os.environ.get("COMPTROL_STATE_DIR")
    if override:
        return Path(override)
    home = os.environ.get("HOME") or os.environ.get("USERPROFILE") or "."
    return Path(home) / ".comptrol"


def descriptor_path(name: str) -> Path:
    return state_dir() / "bridges" / f"{name}.json"


def resolve_endpoint(explicit=None, *, env_var=None, descriptor=None):
    """Return (endpoint, source) or (None, reason)."""
    if explicit:
        return explicit, "explicit"
    if env_var:
        value = os.environ.get(env_var)
        if value:
            return value, "environment"
    if descriptor:
        path = descriptor_path(descriptor)
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None, f"bridge descriptor {path} is missing or invalid"
        endpoint = data.get("endpoint")
        if endpoint:
            return endpoint, f"descriptor:{path}"
        return None, f"bridge descriptor {path} has no endpoint"
    return None, "no bridge endpoint configured"


def write_descriptor(name: str, endpoint: str, extra=None):
    """Publish the local endpoint for adapter discovery. Never writes tokens."""
    path = descriptor_path(name)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {"version": 1, "endpoint": endpoint}
    if extra:
        payload.update({k: v for k, v in extra.items() if k != "token"})
    path.write_text(json.dumps(payload), encoding="utf-8")
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass
    return str(path)


class _PipeConnection:
    """Minimal duplex byte-stream wrapper around a Windows named pipe handle."""

    def __init__(self, handle):
        self._handle = handle

    def settimeout(self, timeout):
        return None

    def sendall(self, data: bytes):
        view = memoryview(data)
        while view:
            # os.write on a pipe handle opened in binary mode transfers bytes.
            written = os.write(self._handle, view)
            view = view[written:]

    def recv(self, size: int) -> bytes:
        try:
            chunk = os.read(self._handle, size)
        except OSError:
            return b""
        return chunk or b""

    def close(self):
        try:
            os.close(self._handle)
        except OSError:
            pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def connect(endpoint: str, timeout: float = 5.0):
    """Open a duplex byte stream to a bridge endpoint."""
    if endpoint.startswith("tcp:"):
        host_port = endpoint[4:]
        host, _, port = host_port.rpartition(":")
        if host not in ("127.0.0.1", "localhost", "::1"):
            raise ValueError("loopback TCP endpoints only")
        connection = socket.create_connection((host, int(port)), timeout=timeout)
        return connection
    if endpoint.startswith("pipe:") or endpoint.startswith("\\\\.\\pipe\\"):
        if os.name != "nt":
            raise RuntimeError("named pipe endpoints require Windows")
        name = endpoint[5:] if endpoint.startswith("pipe:") else endpoint[len("\\\\.\\pipe\\"):]
        # Opening the pipe path connects a client endpoint (CreateFile).
        handle = os.open(f"\\\\.\\pipe\\{name}", os.O_RDWR | os.O_BINARY)
        return _PipeConnection(handle)
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.settimeout(timeout)
    connection.connect(endpoint)
    return connection


def send_frame(connection, obj) -> None:
    connection.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def recv_frame(connection, limit: int = FRAME_LIMIT):
    data = b""
    while not data.endswith(b"\n") and len(data) < limit:
        chunk = connection.recv(65536)
        if not chunk:
            break
        data += chunk
    if not data:
        raise RuntimeError("bridge closed without a response")
    return json.loads(data.decode("utf-8"))


def bridge_request(endpoint, token, envelope, timeout: float = 5.0):
    """Send one authenticated envelope and validate the nonce reply."""
    if not token:
        raise PermissionError("bridge token is required")
    nonce = secrets.token_hex(16)
    message = {"version": 1, "nonce": nonce, "token": token}
    message.update(envelope)
    with connect(endpoint, timeout=timeout) as connection:
        send_frame(connection, message)
        reply = recv_frame(connection)
    if reply.get("nonce") != nonce or reply.get("authenticated") is not True:
        raise PermissionError("bridge authentication or nonce validation failed")
    return reply
