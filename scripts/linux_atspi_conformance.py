#!/usr/bin/env python3
"""Exercise the Linux AT SPI fixture when a GTK and AT SPI host is available."""

import importlib.util
import json
import os
import pathlib
import platform
import subprocess
import sys
import time


if platform.system() != "Linux":
    print("Linux AT SPI conformance skipped because the host is not Linux")
    raise SystemExit(0)

if importlib.util.find_spec("gi") is None or not os.environ.get("AT_SPI_BUS_ADDRESS"):
    if os.environ.get("COMPTROL_REQUIRE_LINUX_ATSPI") == "1":
        raise SystemExit("GTK and AT SPI are required for Linux AT SPI conformance")
    print("Linux AT SPI conformance skipped because GTK or AT SPI is unavailable")
    raise SystemExit(0)


def call(process, identifier, method, params):
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    if "error" in response:
        raise RuntimeError(response)
    return response


root = pathlib.Path(__file__).resolve().parents[1]
binary = os.environ.get("COMPTROL_BIN", str(root / "target" / "debug" / "comptrol"))
fixture = subprocess.Popen([sys.executable, str(root / "fixtures" / "linux" / "atspi_fixture.py")])
runtime = None
try:
    time.sleep(1)
    runtime = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={
            **os.environ,
            "COMPTROL_ALLOW_LINUX_ATSPI": "1",
            "COMPTROL_STATE_DIR": str(root / "target" / "linux-atspi-state"),
        },
    )
    call(runtime, 1, "initialize", {})
    result = call(runtime, 2, "tools/call", {"name": "operate", "arguments": {
        "intent": "linux.atspi.press",
        "idempotency_key": "linux-atspi-press",
        "params": {"process_id": fixture.pid, "name": "Submit"},
        "postcondition": {"attribute": "name", "equals": "Submitted"},
    }})
    structured = result["result"]["structuredContent"]
    if structured.get("error"):
        raise RuntimeError(structured)
    assert structured["verification"] == "verified", structured
    print("Linux AT SPI conformance passed")
finally:
    if runtime is not None:
        runtime.terminate()
        runtime.wait(timeout=5)
    fixture.terminate()
    fixture.wait(timeout=5)
