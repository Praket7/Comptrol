#!/usr/bin/env python3
"""Verify restart reconciliation without repeating a fixture mutation."""

import json
import os
import pathlib
import socket
import subprocess
import tempfile
import time
import urllib.request


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for(url):
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                return json.load(response)
        except Exception:
            time.sleep(0.05)
    raise RuntimeError(f"timed out waiting for {url}")


def call(process, identifier, method, params):
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    if "error" in response:
        raise RuntimeError(response)
    return response


fixture_port = free_port()
fixture = subprocess.Popen(
    ["node", "scripts/browser_fixture.mjs"],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    env={**os.environ, "COMPTROL_FIXTURE_PORT": str(fixture_port)},
)
try:
    state = wait_for(f"http://127.0.0.1:{fixture_port}/state")
    target = {
        "target_id": state["targetId"],
        "browser_context_id": state["browserContextId"],
        "revision": state["revision"],
    }
    key = "restart-fixture-mutation"
    headers = {
        "Content-Type": "application/json",
        "X-Comptrol-Target-Id": target["target_id"],
        "X-Comptrol-Browser-Context": target["browser_context_id"],
        "X-Comptrol-Idempotency-Key": key,
    }
    request = urllib.request.Request(
        f"http://127.0.0.1:{fixture_port}/submit",
        data=json.dumps({"message": "already dispatched"}).encode(),
        headers=headers,
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=2) as response:
        assert json.load(response)["state"] == "submitted"

    with tempfile.TemporaryDirectory(prefix="comptrol-recovery-") as temporary:
        state_dir = pathlib.Path(temporary)
        record = {
            "operation_id": "op-restart-fixture",
            "idempotency_key": key,
            "intent": "browser.fixture.submit",
            "risk": "R1",
            "target": None,
            "state": "dispatched",
            "metadata": target,
            "result": None,
        }
        (state_dir / "operations.jsonl").write_text(json.dumps(record) + "\n", encoding="utf-8")
        environment = {
            **os.environ,
            "COMPTROL_STATE_DIR": str(state_dir),
            "COMPTROL_CDP_ENDPOINT": f"http://127.0.0.1:{fixture_port}",
            "COMPTROL_ALLOW_BROWSER_FIXTURE": "1",
        }
        process = subprocess.Popen(
            ["target/debug/comptrol", "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            env=environment,
        )
        try:
            call(process, 1, "initialize", {})
            arguments = {
                "intent": "browser.fixture.submit",
                "idempotency_key": key,
                "params": {**target, "message": "must not repeat"},
            }
            unknown = call(process, 2, "tools/call", {"name": "operate", "arguments": arguments})
            unknown_data = unknown["result"]["structuredContent"]
            assert unknown_data["error"]["code"] == "operation_unknown", unknown_data
            reconciled = call(process, 3, "tools/call", {"name": "reconcile", "arguments": {"operation_id": "op-restart-fixture"}})
            reconciled_data = reconciled["result"]["structuredContent"]
            assert reconciled_data.get("state") == "reconciled", reconciled
            replay = call(process, 4, "tools/call", {"name": "operate", "arguments": arguments})
            assert replay["result"]["structuredContent"]["recovery"] == "idempotent_replay"
            final = wait_for(f"http://127.0.0.1:{fixture_port}/state")
            assert len(final["submissions"]) == 1, final
            print("restart reconciliation conformance passed")
        finally:
            process.terminate()
            process.wait(timeout=5)
finally:
    fixture.terminate()
    fixture.wait(timeout=5)
