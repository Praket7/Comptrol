#!/usr/bin/env python3
"""Exercise the local CDP route against a dedicated headless Chrome profile."""

import json
import os
import pathlib
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for(url, timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                return json.load(response)
        except Exception:
            time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for {url}")


def call(process, identifier, method, params):
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    if "error" in response:
        raise RuntimeError(response)
    return response


if os.name == "nt":
    chrome_defaults = [
        pathlib.Path(os.environ.get("PROGRAMFILES", "C:/Program Files")) / "Google/Chrome/Application/chrome.exe",
        pathlib.Path(os.environ.get("PROGRAMFILES(X86)", "C:/Program Files (x86)")) / "Google/Chrome/Application/chrome.exe",
        pathlib.Path(os.environ.get("LOCALAPPDATA", "")) / "Google/Chrome/Application/chrome.exe",
    ]
    chrome = os.environ.get("COMPTROL_CHROME_BIN", next((str(path) for path in chrome_defaults if path.exists()), "chrome.exe"))
else:
    chrome = os.environ.get("COMPTROL_CHROME_BIN", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
binary = os.environ.get("COMPTROL_BIN", str(pathlib.Path(__file__).resolve().parents[1] / "target" / ("debug/comptrol.exe" if os.name == "nt" else "debug/comptrol")))
if not pathlib.Path(chrome).exists():
    if os.environ.get("COMPTROL_REQUIRE_CHROME") == "1":
        raise SystemExit("Chrome binary is required but unavailable")
    print("chrome conformance skipped because Chrome is unavailable")
    raise SystemExit(0)

fixture_port = free_port()
debug_port = free_port()
fixture = subprocess.Popen(
    ["node", "scripts/browser_fixture.mjs"],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.PIPE,
    text=True,
    env={**os.environ, "COMPTROL_FIXTURE_PORT": str(fixture_port)},
)
profile = tempfile.TemporaryDirectory(prefix="comptrol-chrome-profile-")
chrome_process = subprocess.Popen(
    [
        chrome,
        "--headless=new",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-gpu",
        "--remote-debugging-address=127.0.0.1",
        f"--remote-debugging-port={debug_port}",
        f"--user-data-dir={profile.name}",
        f"http://127.0.0.1:{fixture_port}/",
    ],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
runtime = None
try:
    wait_for(f"http://127.0.0.1:{fixture_port}/json/list")
    targets = wait_for(f"http://127.0.0.1:{debug_port}/json/list")
    target = next(item for item in targets if item.get("type") == "page" and item.get("url") == f"http://127.0.0.1:{fixture_port}/")
    state = tempfile.TemporaryDirectory(prefix="comptrol-chrome-state-")
    upload_path = pathlib.Path(state.name) / "sandbox" / "verified.txt"
    upload_path.parent.mkdir(parents=True)
    upload_path.write_text("verified upload", encoding="utf-8")
    runtime = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "COMPTROL_CDP_ENDPOINT": f"http://127.0.0.1:{debug_port}", "COMPTROL_ALLOW_BROWSER_CDP": "1", "COMPTROL_STATE_DIR": state.name},
    )
    call(runtime, 1, "initialize", {})
    capabilities = call(runtime, 20, "tools/call", {"name": "inspect", "arguments": {"kind": "capabilities"}})
    focus_available = any(item["name"] == "browser.cdp.focus" for item in capabilities["result"]["structuredContent"])
    inspection = call(runtime, 2, "tools/call", {"name": "inspect", "arguments": {"kind": "browser"}})
    assert any(item["id"] == target["id"] for item in inspection["result"]["structuredContent"]["targets"])
    opened = call(runtime, 12, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.open_tab", "idempotency_key": "chrome-background-tab", "params": {"url": f"http://127.0.0.1:{fixture_port}/", "background": True}}})
    opened_data = opened["result"]["structuredContent"]
    assert opened_data["verification"] == "verified"
    assert opened_data["data"]["visibility"] == "background"
    assert opened_data["data"]["profile"] == "attached_existing_browser"
    assert opened_data["data"]["account_state"] == "same_browser_profile"
    assert opened_data["data"]["mouse"] == "untouched"
    assert opened_data["data"]["clipboard"] == "untouched"
    opened_target = opened_data["data"]["target"]
    opened_identity = {"target_id": opened_target["id"], "browser_context_id": opened_target.get("browser_context_id", "default"), "revision": opened_target.get("revision", f"url:{opened_target['url']}")}
    opened_wait = call(runtime, 13, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.wait_for", "idempotency_key": "chrome-background-ready", "params": {**opened_identity, "selector": "#message", "property": "value", "equals": ""}}})
    assert opened_wait["result"]["structuredContent"]["verification"] == "verified"
    identity = {"target_id": target["id"], "browser_context_id": target.get("browserContextId", "default"), "revision": target.get("revision", f"url:{target['url']}")}
    profile_marker = call(runtime, 14, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.evaluate", "idempotency_key": "chrome-profile-marker", "params": {**identity, "expression": "document.cookie = 'comptrol_profile_marker=retained; path=/'; true"}}})
    assert profile_marker["result"]["structuredContent"]["verification"] == "verified"
    retained_state = call(runtime, 19, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.evaluate", "idempotency_key": "chrome-profile-state", "params": {**opened_identity, "expression": "document.cookie.includes('comptrol_profile_marker=retained')"}}})
    retained_data = retained_state["result"]["structuredContent"]
    assert retained_data["verification"] == "verified"
    assert retained_data["data"]["result"]["value"] is True
    navigation = call(runtime, 3, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.navigate", "idempotency_key": "chrome-navigation", "params": {**identity, "url": f"http://127.0.0.1:{fixture_port}/"}}})
    assert navigation["result"]["structuredContent"]["verification"] == "unverified"
    deadline = time.time() + 5
    rebound = None
    while time.time() < deadline:
        candidate = next(item for item in wait_for(f"http://127.0.0.1:{debug_port}/json/list") if item.get("id") == target["id"])
        if candidate.get("url") == f"http://127.0.0.1:{fixture_port}/":
            rebound = candidate
            break
        time.sleep(0.05)
    assert rebound is not None, rebound
    identity = {"target_id": rebound["id"], "browser_context_id": rebound.get("browserContextId", "default"), "revision": rebound.get("revision", f"url:{rebound['url']}")}
    if focus_available:
        focus = call(runtime, 21, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.focus", "idempotency_key": "chrome-focus", "params": {**identity, "selector": "#message"}}})
        focus_data = focus["result"]["structuredContent"]
        assert focus_data["verification"] == "verified", focus
        assert focus_data["data"]["postcondition"] == "verified"
        assert focus_data["disturbance"]["mouse"] == "untouched"
        assert focus_data["disturbance"]["clipboard"] == "untouched"
    evaluation = call(runtime, 4, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.evaluate", "idempotency_key": "chrome-evaluation", "params": {**identity, "expression": "(() => { const input = document.querySelector('#message'); input.value = 'real chrome'; return input.value })()"}}})
    structured = evaluation["result"]["structuredContent"]
    assert structured["verification"] == "verified", structured
    assert structured["data"]["result"]["value"] == "real chrome"
    snapshot = call(runtime, 18, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.accessibility_snapshot", "idempotency_key": "chrome-accessibility-snapshot", "params": {**identity, "depth": 8}}})
    snapshot_data = snapshot["result"]["structuredContent"]
    assert snapshot_data["verification"] == "verified"
    assert snapshot_data["data"]["snapshot"]["nodes"]
    upload = call(runtime, 5, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.upload", "idempotency_key": "chrome-upload", "params": {**identity, "selector": "#upload", "path": str(upload_path)}}})
    assert upload["result"]["structuredContent"]["verification"] == "verified"
    assert upload["result"]["structuredContent"]["data"]["verified"] is True
    fill = call(runtime, 6, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.fill", "idempotency_key": "chrome-fill", "params": {**identity, "selector": "#message", "value": "verified form"}}})
    assert fill["result"]["structuredContent"]["verification"] == "verified"
    click = call(runtime, 7, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.click", "idempotency_key": "chrome-click", "params": {**identity, "selector": "#submit"}}})
    assert click["result"]["structuredContent"]["verification"] == "unverified"
    wait = call(runtime, 8, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.wait_for", "idempotency_key": "chrome-wait", "params": {**identity, "selector": "#state", "property": "textContent", "contains": "submitted"}}})
    assert wait["result"]["structuredContent"]["verification"] == "verified"
    dialog = call(runtime, 9, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.click", "idempotency_key": "chrome-dialog-open", "params": {**identity, "selector": "#open-dialog", "verify_expression": "document.querySelector('#fixture-dialog').open"}}})
    assert dialog["result"]["structuredContent"]["verification"] == "verified"
    close_dialog = call(runtime, 10, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.click", "idempotency_key": "chrome-dialog-close", "params": {**identity, "selector": "#close-dialog"}}})
    assert close_dialog["result"]["structuredContent"]["verification"] == "unverified"
    download = call(runtime, 11, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.download", "idempotency_key": "chrome-download", "params": {**identity, "selector": "#download", "file_name": "fixture.txt"}}})
    download_data = download["result"]["structuredContent"]
    assert download_data["verification"] == "verified"
    downloaded = pathlib.Path(download_data["data"]["path"])
    assert downloaded.read_text(encoding="utf-8") == "Comptrol fixture download\n"
    navigate_blank = call(runtime, 15, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.navigate", "idempotency_key": "chrome-history-blank", "params": {**identity, "url": "about:blank"}}})
    assert navigate_blank["result"]["structuredContent"]["delivery"] == "delivered"
    deadline = time.time() + 5
    current_target = None
    while time.time() < deadline:
        current_target = next(item for item in wait_for(f"http://127.0.0.1:{debug_port}/json/list") if item.get("id") == target["id"])
        if current_target.get("url") == "about:blank":
            break
        time.sleep(0.1)
    assert current_target and current_target.get("url") == "about:blank", current_target
    current_identity = {"target_id": current_target["id"], "browser_context_id": current_target.get("browserContextId", "default"), "revision": current_target.get("revision", f"url:{current_target['url']}")}
    history_back = call(runtime, 16, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.history_back", "idempotency_key": "chrome-history-back", "params": current_identity}})
    assert history_back["result"]["structuredContent"]["verification"] == "verified", history_back
    assert history_back["result"]["structuredContent"]["data"]["direction"] == "back"
    close = call(runtime, 17, "tools/call", {"name": "operate", "arguments": {"intent": "browser.cdp.close_tab", "idempotency_key": "chrome-close-background-tab", "params": opened_identity}})
    assert close["result"]["structuredContent"]["verification"] == "verified"
    assert close["result"]["structuredContent"]["data"]["closed"] is True
    assert close["result"]["structuredContent"]["data"]["mouse"] == "untouched"
    assert close["result"]["structuredContent"]["data"]["clipboard"] == "untouched"
    print("real Chrome CDP conformance passed")
finally:
    if runtime is not None:
        runtime.terminate()
        runtime.wait(timeout=5)
    chrome_process.terminate()
    chrome_process.wait(timeout=5)
    fixture.terminate()
    fixture.wait(timeout=5)
    profile.cleanup()
    if "state" in locals():
        state.cleanup()
