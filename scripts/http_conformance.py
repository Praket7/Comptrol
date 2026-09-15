#!/usr/bin/env python3
"""Exercise the loopback MCP HTTP preview and its security boundaries."""

import http.client
import json
import os
import socket
import subprocess
import tempfile
import time


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for(port):
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("HTTP preview did not start")


def request(port, method, path, body=b"", origin="http://127.0.0.1"):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    headers = {
        "Origin": origin,
        "Accept": "application/json, text/event-stream",
        "Content-Type": "application/json",
    }
    connection.request(method, path, body=body, headers=headers)
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    return response.status, response.getheader("content-type", ""), payload


def oversized_request(port):
    with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
        connection.sendall(
            b"POST /mcp HTTP/1.1\r\n"
            b"Host: 127.0.0.1\r\n"
            b"Origin: http://127.0.0.1\r\n"
            b"Content-Length: 1048577\r\n"
            b"Connection: close\r\n\r\n"
        )
        response = connection.recv(4096)
    return response


port = free_port()
with tempfile.TemporaryDirectory(prefix="comptrol-http-") as state:
    process = subprocess.Popen(
        ["target/debug/comptrol", "serve-http", str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env={**os.environ, "COMPTROL_STATE_DIR": state},
    )
    try:
        wait_for(port)
        initialize = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}).encode()
        status, content_type, payload = request(port, "POST", "/mcp", initialize)
        assert status == 200
        assert "application/json" in content_type
        assert json.loads(payload)["result"]["serverInfo"]["name"] == "comptrol"
        status, _, payload = request(port, "POST", "/mcp", initialize, origin="http://evil.example")
        assert status == 403
        assert json.loads(payload)["error"] == "origin_denied"
        status, _, _ = request(port, "GET", "/mcp")
        assert status == 405
        status, content_type, payload = request(port, "GET", "/dashboard")
        assert status == 200
        assert "text/html" in content_type
        assert b"Comptrol" in payload
        assert b"413 Payload Too Large" in oversized_request(port)
        print("HTTP preview conformance passed")
    finally:
        process.terminate()
        process.wait(timeout=5)
