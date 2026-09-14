#!/usr/bin/env python3
"""Exercise the allowlisted argv command route without a shell."""

import json
import os
import pathlib
import subprocess
import tempfile


root = pathlib.Path(__file__).resolve().parents[1]
binary = os.environ.get("COMPTROL_BIN", str(root / "target" / "debug" / "comptrol"))


def call(process, identifier, method, params):
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    if "error" in response:
        raise RuntimeError(response)
    return response


with tempfile.TemporaryDirectory(prefix="comptrol-command-") as temporary:
    state = pathlib.Path(temporary) / "state"
    environment = {
        **os.environ,
        "COMPTROL_STATE_DIR": str(state),
        "COMPTROL_ALLOW_COMMANDS": "1",
        "COMPTROL_COMMAND_ROOT": str(root),
        "COMPTROL_COMMAND_ALLOWLIST": str(pathlib.Path(binary).resolve()),
    }
    runtime = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env=environment,
    )
    try:
        call(runtime, 1, "initialize", {})
        result = call(
            runtime,
            2,
            "tools/call",
            {
                "name": "operate",
                "arguments": {
                    "intent": "command.run",
                    "idempotency_key": "command-version",
                    "params": {
                        "program": str(pathlib.Path(binary).resolve()),
                        "args": ["version"],
                        "cwd": str(root),
                    },
                    "postcondition": {"kind": "exit_code", "value": 0},
                },
            },
        )
        structured = result["result"]["structuredContent"]
        assert structured["verification"] == "verified", structured
        assert structured["data"]["exit_code"] == 0, structured
        assert "0" not in structured["data"]["stderr"], structured
        failed = call(
            runtime,
            3,
            "tools/call",
            {
                "name": "operate",
                "arguments": {
                    "intent": "command.run",
                    "idempotency_key": "command-wrong-postcondition",
                    "params": {
                        "program": str(pathlib.Path(binary).resolve()),
                        "args": ["version"],
                        "cwd": str(root),
                    },
                    "postcondition": {"kind": "exit_code", "value": 7},
                },
            },
        )
        failed_structured = failed["result"]["structuredContent"]
        assert failed_structured["verification"] == "failed", failed_structured
        print("command argv conformance passed")
    finally:
        runtime.terminate()
        runtime.wait(timeout=5)
