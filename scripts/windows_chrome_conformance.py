#!/usr/bin/env python3
"""Run a real Windows Chrome CDP acceptance test using an isolated profile."""

import json
import os
import platform
import subprocess
import tempfile
import time
from pathlib import Path


if platform.system() != "Windows":
    print("Windows Chrome conformance skipped because the host is not Windows")
    raise SystemExit(0)

root = Path(__file__).resolve().parents[1]
binary = os.environ.get("COMPTROL_BIN", str(root / "target" / "release" / "comptrol.exe"))
with tempfile.TemporaryDirectory(prefix="comptrol-live-chrome-") as state:
    profile = Path(state) / "chrome-profile"
    chrome = subprocess.Popen(
        [
            os.environ.get("COMPTROL_PYTHON", "python"),
            str(root / "scripts" / "start_windows_chrome_cdp.py"),
            "--profile",
            str(profile),
            "--url",
            "https://example.com",
        ],
        cwd=root,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    runtime = None
    try:
        line = chrome.stdout.readline()
        if not line:
            raise RuntimeError(chrome.stderr.read() or "Chrome launcher exited without an endpoint")
        launch = json.loads(line)
        endpoint = launch["endpoint"]
        chrome_pid = str(launch["pid"])
        environment = {
            **os.environ,
            "COMPTROL_CDP_ENDPOINT": endpoint,
            "COMPTROL_ALLOW_BROWSER_CDP": "1",
            "COMPTROL_STATE_DIR": str(Path(state) / "comptrol-state"),
        }
        runtime = subprocess.Popen(
            [binary, "mcp"],
            cwd=root,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

        def call(identifier, method, params):
            runtime.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
            runtime.stdin.flush()
            deadline = time.time() + 20
            while time.time() < deadline:
                line = runtime.stdout.readline()
                if not line:
                    raise RuntimeError(runtime.stderr.read() or "Comptrol exited during Chrome acceptance")
                response = json.loads(line)
                if response.get("id") == identifier:
                    return response
            raise TimeoutError(f"Comptrol response timeout for request {identifier}")

        call(1, "initialize", {})
        browser = call(2, "tools/call", {"name": "inspect", "arguments": {"kind": "browser"}})
        targets = browser["result"]["structuredContent"]["targets"]
        target = next(item for item in targets if item.get("url", "").startswith("http"))
        params = {
            "target_id": target["id"],
            "browser_context_id": target.get("browser_context_id"),
            "revision": target["revision"],
            "url": "https://example.com",
            "url_contains": "example.com",
            "timeout_ms": 15000,
        }
        result = call(3, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.navigate", "idempotency_key": "windows-live-chrome-navigation", "params": params}})
        structured = result["result"]["structuredContent"]
        assert structured["verification"] == "verified", structured
        assert "example.com" in structured["data"]["final_url"], structured
        print("Windows real Chrome CDP conformance passed")
    finally:
        if runtime is not None:
            runtime.terminate()
            runtime.wait(timeout=10)
        subprocess.run(["taskkill", "/PID", chrome_pid, "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        chrome.wait(timeout=10)
