#!/usr/bin/env python3
"""Check privacy reporting and protocol size refusal."""

import json
import os
import subprocess
import tempfile


binary = "target/debug/comptrol"
with tempfile.TemporaryDirectory(prefix="comptrol-privacy-") as state:
    environment = dict(os.environ, COMPTROL_STATE_DIR=state)
    status = json.loads(subprocess.check_output([binary, "privacy", "status"], env=environment))
    assert status["telemetry"]["enabled"] is False
    assert status["automatic_updates"]["network_on_startup"] is False
    endpoints = json.loads(
        subprocess.check_output([binary, "privacy", "network-endpoints"], env=environment)
    )
    names = {endpoint["name"] for endpoint in endpoints["endpoints"]}
    assert names == {"browser_cdp", "remote_host", "update_service", "telemetry"}
    request = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": "ping", "padding": "x" * (1024 * 1024)}
    )
    result = subprocess.run(
        [binary, "mcp"], input=request + "\n", text=True, capture_output=True, env=environment, check=True
    )
    assert "message_too_large" in result.stdout
print("privacy and protocol boundary conformance passed")
