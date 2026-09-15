#!/usr/bin/env python3
"""Check that platform capabilities report only the active host route."""

import json
import os
import subprocess
import tempfile
from pathlib import Path


root = Path(__file__).resolve().parents[1]
binary = os.environ.get("COMPTROL_BIN", str(root / "target" / "debug" / ("comptrol.exe" if os.name == "nt" else "comptrol")))


with tempfile.TemporaryDirectory(prefix="comptrol-platform-") as state:
    environment = dict(os.environ, COMPTROL_STATE_DIR=state)
    capabilities = json.loads(subprocess.check_output([binary, "capabilities"], env=environment))
    names = {item["name"]: item for item in capabilities}
    assert names["platform.broker.observe"]["available"] is True
    diagnostics = json.loads(subprocess.check_output([binary, "doctor"], env=environment))
    assert set((diagnostics["platform"]["brokers"] or {})) >= {"windows_uia", "linux_atspi", "linux_x11", "linux_wayland"}
    assert diagnostics["mcp_adapter"]["transport"] == "stdio"
    assert {item["client"] for item in diagnostics["client_configuration"]} >= {"codex", "claude-code", "cursor"}
    platform = __import__("platform").system().lower()
    assert names["daemon.ipc"]["available"] is True
    if platform == "darwin":
        assert names["platform.windows.uia"]["available"] is False
        assert names["platform.linux.atspi"]["available"] is False
    elif platform == "linux":
        assert names["platform.windows.uia"]["available"] is False
    assert names["browser.cdp"]["available"] is False
    print("platform capability conformance passed")
