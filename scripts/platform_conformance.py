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
    platform = os.uname().sysname.lower()
    if platform == "darwin":
        assert names["platform.windows.uia"]["available"] is False
        assert names["platform.linux.atspi"]["available"] is False
    elif platform == "linux":
        assert names["platform.windows.uia"]["available"] is False
    assert names["browser.cdp"]["available"] is False
    print("platform capability conformance passed")

