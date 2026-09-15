#!/usr/bin/env python3
"""Exercise exact closed Chrome group reopening on a permissioned macOS host."""

import json
import os
import platform
import subprocess
import tempfile


if platform.system() != "Darwin":
    print("macOS Chrome group conformance skipped because the host is not macOS")
    raise SystemExit(0)

if os.environ.get("COMPTROL_RUN_LIVE_CHROME_GROUP_CONFORMANCE") != "1":
    print("macOS Chrome group conformance skipped because live group testing is opt in")
    raise SystemExit(0)

group = os.environ.get("COMPTROL_CHROME_CLOSED_GROUP_NAME")
if not group:
    raise SystemExit("COMPTROL_CHROME_CLOSED_GROUP_NAME is required")

try:
    subprocess.run(
        [
            "osascript",
            "-e",
            'tell application "System Events" to tell process "Google Chrome" to count windows',
        ],
        capture_output=True,
        text=True,
        check=True,
        timeout=3,
    )
except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
    if os.environ.get("COMPTROL_REQUIRE_LIVE_CHROME_GROUP_CONFORMANCE") == "1":
        raise SystemExit(f"Chrome Accessibility access is unavailable: {error}")
    print("macOS Chrome group conformance skipped because Chrome Accessibility access is unavailable")
    raise SystemExit(0)


def call(process, identifier, method, params):
    process.stdin.write(
        json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n"
    )
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    if "error" in response:
        raise RuntimeError(response)
    return response


binary = os.environ.get("COMPTROL_BIN", "target/debug/comptrol")
with tempfile.TemporaryDirectory(prefix="comptrol-chrome-group-") as state:
    runtime = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "COMPTROL_ALLOW_MACOS_AX": "1", "COMPTROL_STATE_DIR": state},
    )
    try:
        call(runtime, 1, "initialize", {})
        response = call(
            runtime,
            2,
            "tools/call",
            {
                "name": "operate",
                "arguments": {
                    "intent": "browser.chrome.reopen_closed_group",
                    "idempotency_key": "macos-chrome-group-conformance",
                    "background": "foreground_allowed",
                    "params": {"group": group},
                },
            },
        )
        result = response["result"]["structuredContent"]
        assert result["verification"] == "verified", result
        assert result["data"]["postcondition"] == "closed_group_button_absent", result
        assert result["disturbance"]["mouse"] == "untouched", result
        assert result["disturbance"]["clipboard"] == "untouched", result
        print(f"macOS Chrome group conformance passed for {group}")
    finally:
        runtime.terminate()
        runtime.wait(timeout=5)
