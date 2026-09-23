#!/usr/bin/env python3
"""Exercise the Windows UI Automation fixture when a Windows host is available."""

import json
import os
import pathlib
import platform
import shutil
import subprocess
import time


if platform.system() != "Windows":
    print("Windows UI Automation conformance skipped because the host is not Windows")
    raise SystemExit(0)

powershell = shutil.which("powershell.exe")
if not powershell:
    if os.environ.get("COMPTROL_REQUIRE_WINDOWS_UIA") == "1":
        raise SystemExit("powershell.exe is required for Windows UI Automation conformance")
    print("Windows UI Automation conformance skipped because PowerShell is unavailable")
    raise SystemExit(0)


def call(process, identifier, method, params):
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    if "error" in response:
        raise RuntimeError(response)
    return response


root = pathlib.Path(__file__).resolve().parents[1]
binary = os.environ.get("COMPTROL_BIN", str(root / "target" / "debug" / "comptrol.exe"))
fixture = subprocess.Popen([powershell, "-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File", str(root / "fixtures" / "windows" / "UIAutomationFixture.ps1")])
runtime = None
try:
    time.sleep(1)
    if fixture.poll() is not None:
        raise RuntimeError("Windows UI Automation fixture exited before it could be inspected")
    runtime = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={
            **os.environ,
            "COMPTROL_ALLOW_WINDOWS_UIA": "1",
            "COMPTROL_STATE_DIR": str(root / "target" / "windows-uia-state"),
        },
    )
    call(runtime, 1, "initialize", {})
    result = call(runtime, 2, "tools/call", {"name": "operate", "arguments": {
        "intent": "windows.uia.press",
        "idempotency_key": "windows-uia-press",
        "params": {"process_id": fixture.pid, "name": "Submit", "role": "button"},
        "postcondition": {"attribute": "name", "equals": "Submitted"},
    }})
    structured = result["result"]["structuredContent"]
    if structured.get("error"):
        raise RuntimeError(structured)
    assert structured["verification"] == "verified", structured
    assert structured["route"] == "windows_uia_direct", structured
    print("Windows UI Automation conformance passed")
finally:
    if runtime is not None:
        runtime.terminate()
        runtime.wait(timeout=5)
    fixture.terminate()
    fixture.wait(timeout=5)
