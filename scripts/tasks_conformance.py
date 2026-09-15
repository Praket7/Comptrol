#!/usr/bin/env python3
"""Verify durable completed MCP Tasks handles across a process restart."""

import json
import os
import subprocess
import tempfile
import time


def send(process, message):
    process.stdin.write(json.dumps(message) + "\n")
    process.stdin.flush()


def read(process):
    line = process.stdout.readline()
    if not line:
        raise RuntimeError("MCP server closed before replying")
    return json.loads(line)


def initialize(process, identifier):
    send(
        process,
        {
            "jsonrpc": "2.0",
            "id": identifier,
            "method": "initialize",
            "params": {"capabilities": {"extensions": {"io.modelcontextprotocol/tasks": {}}}},
        },
    )
    response = read(process)
    assert response["result"]["capabilities"]["tasks"]["requests"]["tools"]["call"] == {}


with tempfile.TemporaryDirectory(prefix="comptrol-tasks-") as state:
    environment = {**os.environ, "COMPTROL_STATE_DIR": state}
    process = subprocess.Popen(
        ["target/debug/comptrol", "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env=environment,
    )
    try:
        initialize(process, 1)
        send(
            process,
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "operate",
                    "task": {"ttl": 60_000},
                    "_meta": {"progressToken": "task-progress"},
                    "arguments": {"intent": "system.ping", "idempotency_key": "task-ping"},
                },
            },
        )
        progress_start = read(process)
        progress_end = read(process)
        created = read(process)
        assert progress_start["method"] == "notifications/progress"
        assert progress_end["params"]["progress"] == 1
        task = created["result"]["task"]
        assert created["result"]["resultType"] == "task"
        task_id = task["taskId"]
        deadline = time.time() + 10
        while task["status"] not in {"completed", "failed", "cancelled", "unknown"}:
            assert time.time() < deadline, task
            send(process, {"jsonrpc": "2.0", "id": 10, "method": "tasks/get", "params": {"taskId": task_id}})
            task_response = read(process)
            task = task_response["result"]
            if task["status"] not in {"completed", "failed", "cancelled", "unknown"}:
                time.sleep(task.get("pollIntervalMs", 50) / 1000)
        assert task["status"] == "completed", task
    finally:
        process.terminate()
        process.wait(timeout=3)

    process = subprocess.Popen(
        ["target/debug/comptrol", "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env=environment,
    )
    try:
        initialize(process, 3)
        send(process, {"jsonrpc": "2.0", "id": 4, "method": "tasks/get", "params": {"taskId": task_id}})
        restored = read(process)
        assert restored["result"]["status"] == "completed"
        send(process, {"jsonrpc": "2.0", "id": 5, "method": "tasks/result", "params": {"taskId": task_id}})
        result = read(process)
        assert result["result"]["structuredContent"]["verification"] == "verified"
        print("MCP Tasks conformance passed")
    finally:
        process.terminate()
        process.wait(timeout=3)
