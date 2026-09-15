#!/usr/bin/env python3
"""Verify the stateless MCP 2026-07-28 HTTP path."""

import http.client
import json
import os
import socket
import subprocess
import tempfile
import time

BIN = os.environ.get("COMPTROL_BIN", "target/debug/comptrol")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(port, method, body=None, session=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    headers = {"Origin": "http://127.0.0.1", "Accept": "application/json", "MCP-Protocol-Version": "2026-07-28"}
    if body is not None:
        headers["Content-Type"] = "application/json"
    if session:
        headers["MCP-Session-Id"] = session
    connection.request(method, "/mcp", body=json.dumps(body).encode() if body is not None else None, headers=headers)
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    return response.status, response.getheader("mcp-session-id"), json.loads(payload) if payload else {}


port = free_port()
with tempfile.TemporaryDirectory(prefix="comptrol-mcp-http-current-") as state:
    process = subprocess.Popen([BIN, "serve-http", str(port)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env={**os.environ, "COMPTROL_STATE_DIR": state})
    try:
        # Release builds on shared macOS/Windows runners can take longer to
        # bind their listener while the runtime initializes SQLite state.
        # This is a bounded startup allowance, not a request poll loop.
        deadline = time.time() + 30
        while time.time() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=1):
                    break
            except OSError:
                time.sleep(0.05)
        else:
            raise RuntimeError("current MCP HTTP server did not start")
        status, session, initialize = request(port, "POST", {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2026-07-28"}})
        assert status == 200 and session is None
        assert initialize["result"]["protocolVersion"] == "2026-07-28"
        assert initialize["result"]["comptrol"]["protocol_mode"] == "stateless"
        status, session, ping = request(port, "POST", {"jsonrpc": "2.0", "id": 2, "method": "ping"})
        assert status == 200 and session is None and ping["result"] == {}
        status, _, error = request(port, "GET")
        assert status == 405 and error["error"] == "stateless_protocol_has_no_get_stream"
        status, _, error = request(port, "DELETE")
        assert status == 405 and error["error"] == "stateless_protocol_has_no_session"
        print("MCP 2026 current HTTP conformance passed")
    finally:
        process.terminate()
        process.wait(timeout=5)
