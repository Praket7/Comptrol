#!/usr/bin/env python3
"""Exercise native Chrome tab opening in the real macOS browser profile."""

import json
import os
import platform
import socket
import subprocess
import sys
import tempfile
import time


if platform.system() != "Darwin":
    print("native Chrome conformance skipped because the host is not macOS")
    raise SystemExit(0)

if os.environ.get("COMPTROL_RUN_LIVE_NATIVE_BROWSER_CONFORMANCE") != "1":
    print("native Chrome conformance skipped because live browser testing is opt in")
    raise SystemExit(0)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def call(process, identifier, method, params):
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    if "error" in response:
        raise RuntimeError(response)
    return response


def applescript(script):
    try:
        result = subprocess.run(
            ["osascript", "-e", script],
            capture_output=True,
            text=True,
            check=True,
            timeout=3,
        )
    except subprocess.TimeoutExpired as error:
        raise RuntimeError("Chrome Automation permission did not respond") from error
    return result.stdout.strip()


def tab_count(url):
    escaped = url.replace('\\', '\\\\').replace('"', '\\"')
    return int(applescript(f'''tell application "Google Chrome"
set matches to 0
repeat with w in windows
repeat with t in tabs of w
if URL of t is "{escaped}" then set matches to matches + 1
end repeat
end repeat
return matches
end tell'''))


def close_tab(url):
    escaped = url.replace('\\', '\\\\').replace('"', '\\"')
    applescript(f'''tell application "Google Chrome"
repeat with w in windows
repeat with i from (count tabs of w) to 1 by -1
set t to tab i of w
if URL of t is "{escaped}" then close t
end repeat
end repeat
end tell''')


binary = os.environ.get("COMPTROL_BIN", "target/debug/comptrol")
try:
    applescript('tell application "Google Chrome" to count windows')
except RuntimeError as error:
    if os.environ.get("COMPTROL_REQUIRE_NATIVE_BROWSER_CONFORMANCE") == "1":
        raise
    print(f"native Chrome conformance skipped because {error}")
    raise SystemExit(0)
port = free_port()
token = f"comptrol-native-{os.getpid()}-{time.time_ns()}"
url = f"http://127.0.0.1:{port}/{token}"
with tempfile.TemporaryDirectory(prefix="comptrol-native-browser-") as state:
    server = subprocess.Popen(
        [sys.executable, "-m", "http.server", str(port), "--bind", "127.0.0.1", "--directory", state],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    runtime = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "COMPTROL_ALLOW_BROWSER_LAUNCH": "1", "COMPTROL_STATE_DIR": state},
    )
    try:
        call(runtime, 1, "initialize", {})
        result = call(
            runtime,
            2,
            "tools/call",
            {
                "name": "operate",
                "arguments": {
                    "intent": "browser.chrome.open_tab",
                    "idempotency_key": token,
                    "background": "foreground_allowed",
                    "params": {"url": url},
                },
            },
        )
        structured = result["result"]["structuredContent"]
        assert structured["route"] == "browser_launcher", structured
        assert structured["verification"] == "unverified", structured
        assert structured["data"]["postcondition"] == "launcher_accepted", structured
        assert structured["data"]["mouse"] == "untouched", structured
        assert structured["data"]["clipboard"] == "untouched", structured
        try:
            deadline = time.time() + 10
            while time.time() < deadline and tab_count(url) == 0:
                time.sleep(0.2)
            assert tab_count(url) == 1
        except RuntimeError as error:
            if os.environ.get("COMPTROL_REQUIRE_NATIVE_BROWSER_CONFORMANCE") == "1":
                raise
            print(f"native Chrome launcher passed but tab observation skipped because {error}")
        assert subprocess.run(
            ["pgrep", "-f", "Google Chrome"],
            stdout=subprocess.DEVNULL,
            check=False,
        ).returncode == 0
        print("native Chrome conformance passed")
    finally:
        try:
            close_tab(url)
        except Exception:
            pass
        runtime.terminate()
        runtime.wait(timeout=5)
        server.terminate()
        server.wait(timeout=5)
