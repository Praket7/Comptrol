#!/usr/bin/env python3
"""Exercise exact application launch against an installed macOS app."""

import json
import os
import platform
import subprocess
import tempfile


if platform.system() != "Darwin":
    print("macOS app conformance skipped because the host is not macOS")
    raise SystemExit(0)

if os.environ.get("COMPTROL_RUN_LIVE_APP_CONFORMANCE") != "1":
    print("macOS app conformance skipped because live app testing is opt in")
    raise SystemExit(0)


def call(process, identifier, method, params):
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    if "error" in response:
        raise RuntimeError(response)
    return response


binary = os.environ.get("COMPTROL_BIN", "target/debug/comptrol")
app = os.environ.get("COMPTROL_APP_CONFORMANCE_NAME", "Finder")
with tempfile.TemporaryDirectory(prefix="comptrol-app-state-") as state:
    runtime = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "COMPTROL_ALLOW_APP_LAUNCH": "1", "COMPTROL_STATE_DIR": state},
    )
    try:
        call(runtime, 1, "initialize", {})
        result = call(runtime, 2, "tools/call", {"name": "operate", "arguments": {
            "intent": "desktop.open_app",
            "idempotency_key": "macos-app-conformance",
            "background": "foreground_allowed",
            "params": {"app": app},
        }})
        structured = result["result"]["structuredContent"]
        if structured.get("error"):
            raise RuntimeError(structured)
        assert structured["verification"] == "verified", structured
        assert structured["data"]["postcondition"] == "process_present", structured
        assert structured["data"]["mouse"] == "untouched", structured
        assert structured["data"]["clipboard"] == "untouched", structured
        print(f"macOS app conformance passed for {app}")
    finally:
        runtime.terminate()
        runtime.wait(timeout=5)
