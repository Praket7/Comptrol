#!/usr/bin/env python3
"""Check that platform capabilities report only the active host route."""

import json
import os
import subprocess
import tempfile


with tempfile.TemporaryDirectory(prefix="comptrol-platform-") as state:
    environment = dict(os.environ, COMPTROL_STATE_DIR=state)
    capabilities = json.loads(subprocess.check_output(["target/debug/comptrol", "capabilities"], env=environment))
    names = {item["name"]: item for item in capabilities}
    assert names["platform.broker.observe"]["available"] is True
    diagnostics = json.loads(subprocess.check_output(["target/debug/comptrol", "doctor"], env=environment))
    assert set((diagnostics["platform"]["brokers"] or {})) >= {"windows_uia", "linux_atspi", "linux_x11", "linux_wayland"}
    assert diagnostics["mcp_adapter"]["transport"] == "stdio"
    assert {item["client"] for item in diagnostics["client_configuration"]} >= {"codex", "claude-code", "cursor"}
    platform = os.uname().sysname.lower()
    assert names["daemon.ipc"]["available"] is True
    if platform == "darwin":
        assert names["platform.windows.uia"]["available"] is False
        assert names["platform.linux.atspi"]["available"] is False
    elif platform == "linux":
        assert names["platform.windows.uia"]["available"] is False
    assert names["browser.cdp"]["available"] is False
    print("platform capability conformance passed")
