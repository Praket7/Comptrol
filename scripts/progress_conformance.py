#!/usr/bin/env python3
"""Verify MCP stdio progress notifications around one bounded call."""

import json
import os
import subprocess
import tempfile


with tempfile.TemporaryDirectory(prefix="comptrol-progress-") as state:
    process = subprocess.Popen(
        ["target/debug/comptrol", "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "COMPTROL_STATE_DIR": state},
    )
    try:
        requests = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "operate",
                    "_meta": {"progressToken": "progress-check"},
                    "arguments": {"intent": "system.ping", "idempotency_key": "progress-check"},
                },
            },
        ]
        for request in requests:
            process.stdin.write(json.dumps(request) + "\n")
            process.stdin.flush()
        messages = [json.loads(process.stdout.readline()) for _ in range(4)]
        assert messages[0]["id"] == 1
        assert messages[1]["method"] == "notifications/progress"
        assert messages[1]["params"] == {
            "progressToken": "progress-check",
            "progress": 0,
            "total": 1,
            "message": "operation_started",
        }
        assert messages[2]["method"] == "notifications/progress"
        assert messages[2]["params"]["progress"] == 1
        assert messages[3]["id"] == 2
        assert messages[3]["result"]["structuredContent"]["verification"] == "verified"
        print("MCP progress conformance passed")
    finally:
        process.terminate()
        process.wait(timeout=3)
