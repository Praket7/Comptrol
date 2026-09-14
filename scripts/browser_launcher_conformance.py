#!/usr/bin/env python3
"""Verify native Chrome launcher policy and strict background refusal."""

import json
import os
import subprocess
import tempfile


def result(environment, key):
    request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "operate",
            "arguments": {
                "intent": "browser.chrome.open_tab",
                "idempotency_key": key,
                "background": "strict_background",
                "params": {"url": "about:blank"},
            },
        },
    }
    process = subprocess.run(
        ["target/debug/comptrol", "mcp"],
        input=json.dumps(request) + "\n",
        text=True,
        capture_output=True,
        env=environment,
        check=True,
    )
    return json.loads(process.stdout)["result"]["structuredContent"]


with tempfile.TemporaryDirectory(prefix="comptrol-launcher-") as state:
    base = {**os.environ, "COMPTROL_STATE_DIR": state}
    assert result(base, "launcher-denied")["error"]["code"] == "policy_denied"
    enabled = {**base, "COMPTROL_ALLOW_BROWSER_LAUNCH": "1"}
    assert result(enabled, "launcher-strict")["error"]["code"] == "background_unavailable"
print("browser launcher policy conformance passed")
