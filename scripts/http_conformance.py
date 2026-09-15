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


def request(port, method, path, body=b"", origin="http://127.0.0.1", session=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    headers = {
        "Origin": origin,
        "Accept": "application/json, text/event-stream",
        "Content-Type": "application/json",
    }
    if session:
        headers["MCP-Session-Id"] = session
    connection.request(method, path, body=body, headers=headers)
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    return (
        response.status,
        response.getheader("content-type", ""),
        response.getheader("mcp-session-id"),
        payload,
    )


def oversized_request(port):
    with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
        connection.sendall(
            b"POST /mcp HTTP/1.1\r\n"
            b"Host: 127.0.0.1\r\n"
            b"Origin: http://127.0.0.1\r\n"
            b"Content-Length: 1048577\r\n"
            b"Connection: close\r\n\r\n"
        )
        response = b""
        while True:
            chunk = connection.recv(4096)
            if not chunk:
                break
            response += chunk
    return response


def open_stream(port, session, last_event_id=None):
    connection = socket.create_connection(("127.0.0.1", port), timeout=3)
    headers = [
        "GET /mcp HTTP/1.1",
        "Host: 127.0.0.1",
        "Origin: http://127.0.0.1",
        "Accept: text/event-stream",
        f"MCP-Session-Id: {session}",
    ]
    if last_event_id is not None:
        headers.append(f"Last-Event-ID: {last_event_id}")
    connection.sendall(("\r\n".join(headers) + "\r\n\r\n").encode())
    response = b""
    while b"\r\n\r\n" not in response:
        response += connection.recv(4096)
    header, body = response.split(b"\r\n\r\n", 1)
    assert b"200 OK" in header
    assert b"text/event-stream" in header
    return connection, body


def read_until(connection, body, marker):
    while marker not in body:
        body += connection.recv(4096)
    return body


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
        status, content_type, session, payload = request(port, "POST", "/mcp", initialize)
        assert status == 200
        assert "application/json" in content_type
        assert session and len(session) == 48
        assert json.loads(payload)["result"]["serverInfo"]["name"] == "comptrol"
        status, _, _, payload = request(port, "POST", "/mcp", initialize, origin="http://evil.example")
        assert status == 403
        assert json.loads(payload)["error"] == "origin_denied"
        status, _, _, payload = request(
            port,
            "POST",
            "/mcp",
            json.dumps({"jsonrpc": "2.0", "id": 2, "method": "ping"}).encode(),
        )
        assert status == 400
        assert json.loads(payload)["error"] == "session_required"
        status, _, _, payload = request(
            port,
            "POST",
            "/mcp",
            json.dumps({"jsonrpc": "2.0", "id": 2, "method": "ping"}).encode(),
        )
        assert status == 400
        status, _, _, payload = request(
            port,
            "POST",
            "/mcp",
            json.dumps({"jsonrpc": "2.0", "id": 2, "method": "ping"}).encode(),
            session=session,
        )
        assert status == 200
        assert json.loads(payload)["result"] == {}
        stream, stream_body = open_stream(port, session)
        stream_body = read_until(stream, stream_body, b"event: ready")
        assert b"event: ready" in stream_body
        status, content_type, _, payload = request(
            port,
            "POST",
            "/mcp",
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {
                        "name": "operate",
                        "arguments": {
                            "intent": "system.ping",
                            "idempotency_key": "http-progress",
                        },
                        "_meta": {"progressToken": "http-progress"},
                    },
                }
            ).encode(),
            session=session,
        )
        assert status == 200
        assert "text/event-stream" in content_type
        assert payload.find(b'"message":"operation_started"') < payload.find(
            b'"message":"operation_completed"'
        )
        stream_body = read_until(stream, stream_body, b'"message":"operation_completed"')
        assert b"event: message" in stream_body
        event_ids = [line.split(b":", 1)[1].strip() for line in stream_body.splitlines() if line.startswith(b"id:")]
        assert event_ids and int(event_ids[-1]) >= 2
        stream.close()
        replay, replay_body = open_stream(port, session, last_event_id=1)
        replay_body = read_until(replay, replay_body, b'"message":"operation_completed"')
        assert b"id: 2" in replay_body
        replay.close()
        process.terminate()
        process.wait(timeout=5)
        process = subprocess.Popen(
            ["target/debug/comptrol", "serve-http", str(port)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env={**os.environ, "COMPTROL_STATE_DIR": state},
        )
        wait_for(port)
        resumed, resumed_body = open_stream(port, session, last_event_id=1)
        resumed_body = read_until(resumed, resumed_body, b'"message":"operation_completed"')
        assert b"id: 2" in resumed_body
        resumed.close()
        status, _, _, _ = request(port, "DELETE", "/mcp", session=session)
        assert status == 204
        status, _, _, payload = request(port, "GET", "/mcp", session=session)
        assert status == 404
        assert json.loads(payload)["error"] == "session_not_found"
        status, content_type, _, payload = request(port, "GET", "/dashboard")
        assert status == 200
        assert "text/html" in content_type
        assert b"Comptrol" in payload
        assert b"413 Payload Too Large" in oversized_request(port)
        print("HTTP preview conformance passed")
    finally:
        process.terminate()
        process.wait(timeout=5)
