#!/usr/bin/env python3
"""Exercise the macOS AX route against the controlled fixture when permission allows."""

import json
import os
import pathlib
import platform
import shutil
import subprocess
import tempfile
import time


if platform.system() != "Darwin":
    print("macOS AX conformance skipped because the host is not macOS")
    raise SystemExit(0)

swiftc = shutil.which("swiftc") or "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc"
if not pathlib.Path(swiftc).exists():
    if os.environ.get("COMPTROL_REQUIRE_MACOS_AX") == "1":
        raise SystemExit("swiftc is required for macOS AX conformance")
    print("macOS AX conformance skipped because swiftc is unavailable")
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
with tempfile.TemporaryDirectory(prefix="comptrol-ax-") as temporary:
    temporary_path = pathlib.Path(temporary)
    fixture = temporary_path / "comptrol_ax_fixture"
    compile_result = subprocess.run(
        [swiftc, "-framework", "Cocoa", str(root / "fixtures" / "macos" / "AccessibilityFixture.swift"), "-o", str(fixture)],
        capture_output=True,
        text=True,
    )
    if compile_result.returncode != 0:
        if "license agreements" in compile_result.stderr and os.environ.get("COMPTROL_REQUIRE_MACOS_AX") != "1":
            print("macOS AX conformance skipped because the Apple SDK license is unavailable")
            raise SystemExit(0)
        raise SystemExit(compile_result.stderr)
    app = subprocess.Popen([str(fixture)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    runtime = None
    try:
        time.sleep(0.75)
        state = temporary_path / "state"
        runtime = subprocess.Popen(
            [binary, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            env={**os.environ, "COMPTROL_ALLOW_MACOS_AX": "1", "COMPTROL_STATE_DIR": str(state)},
        )
        call(runtime, 1, "initialize", {})
        result = call(runtime, 2, "tools/call", {"name": "operate", "arguments": {
            "intent": "macos.ax.press",
            "idempotency_key": "ax-fixture-press",
            "target": {"kind": "application", "name": "comptrol_ax_fixture"},
            "params": {"app": "comptrol_ax_fixture", "control": "Submit", "role": "button"},
            "postcondition": {"attribute": "name", "equals": "Submitted"},
        }})
        structured = result["result"]["structuredContent"]
        if structured.get("error", {}).get("code") == "permission_required":
            if os.environ.get("COMPTROL_REQUIRE_MACOS_AX") == "1":
                raise SystemExit("macOS Accessibility permission is required")
            print("macOS AX conformance skipped because Accessibility permission is unavailable")
        else:
            assert structured["verification"] == "verified"
            print("macOS AX conformance passed")
    finally:
        if runtime is not None:
            runtime.terminate()
            runtime.wait(timeout=5)
        app.terminate()
        app.wait(timeout=5)
