#!/usr/bin/env python3
"""Exercise the trusted-core to isolated-adapter boundary without an app install."""

import json
import os
import pathlib
import subprocess
import tempfile


root = pathlib.Path(__file__).resolve().parents[1]
binary = os.environ.get("COMPTROL_BIN", str(root / "target" / "debug" / "comptrol"))
if os.name == "nt" and not binary.lower().endswith(".exe"):
    if os.environ.get("COMPTROL_REQUIRE_NATIVE_ADAPTER_RUNTIME") == "1":
        raise SystemExit("COMPTROL_BIN points to a WSL/Linux binary. Run this conformance test from WSL or set COMPTROL_BIN to a Windows .exe")
    print("adapter runtime conformance skipped because Windows Python was given a WSL/Linux binary; run it from WSL")
    raise SystemExit(0)
with tempfile.TemporaryDirectory(prefix="comptrol-adapter-runtime-") as state:
    process = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={
            **os.environ,
            "COMPTROL_ALLOW_ADAPTERS": "1",
            "COMPTROL_AUTO_START_CHROME_CDP": "0",
            "COMPTROL_ADAPTER_ROOT": str(root / "adapters"),
            "COMPTROL_STATE_DIR": state,
        },
    )

    def call(identifier, method, params):
        process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
        process.stdin.flush()
        while True:
            value = json.loads(process.stdout.readline())
            if value.get("id") == identifier:
                return value

    try:
        assert "result" in call(1, "initialize", {})
        result = call(2, "tools/call", {"name": "operate", "arguments": {
            "intent": "vscode.workspace.list",
            "idempotency_key": "adapter-runtime-conformance",
            "params": {"resource": "workspace"},
        }})
        structured = result["result"]["structuredContent"]
        assert structured["verification"] == "not_attempted", structured
        assert structured["error"]["code"] == "adapter_execution_failed", structured
        assert "bridge" in structured["error"]["message"], structured
        assert structured["route"] == "adapter.vscode", structured
        print("adapter runtime conformance passed with truthful bridge refusal")
    finally:
        process.terminate()
        process.wait(timeout=5)
