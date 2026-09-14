#!/usr/bin/env python3
"""Exercise visible and background tab creation against an explicitly selected Chrome profile."""

import json
import os
import tempfile
import urllib.parse
import urllib.request
import subprocess


endpoint = os.environ.get("COMPTROL_CDP_ENDPOINT")
if not endpoint:
    if os.environ.get("COMPTROL_REQUIRE_VISIBLE_CHROME") == "1":
        raise SystemExit("COMPTROL_CDP_ENDPOINT is required for visible Chrome conformance")
    print("visible Chrome conformance skipped because no explicit endpoint is configured")
    raise SystemExit(0)


def call(process, identifier, method, params):
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    if "error" in response:
        raise RuntimeError(response)
    return response


def close_target(target_id):
    path = f"/json/close/{urllib.parse.quote(target_id, safe='')}"
    try:
        with urllib.request.urlopen(endpoint + path, timeout=2):
            pass
    except Exception:
        pass


binary = os.environ.get("COMPTROL_BIN", "target/debug/comptrol")
opened = []
with tempfile.TemporaryDirectory(prefix="comptrol-visible-chrome-") as state:
    runtime = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "COMPTROL_ALLOW_BROWSER_CDP": "1", "COMPTROL_STATE_DIR": state},
    )
    try:
        call(runtime, 1, "initialize", {})
        for identifier, background in ((2, False), (3, True)):
            result = call(
                runtime,
                identifier,
                "tools/call",
                {
                    "name": "operate",
                    "arguments": {
                        "intent": "browser.cdp.open_tab",
                        "idempotency_key": f"visible-profile-tab-{identifier}",
                        "params": {"url": "about:blank", "background": background},
                    },
                },
            )
            structured = result["result"]["structuredContent"]
            assert structured["verification"] == "verified"
            data = structured["data"]
            assert data["profile"] == "attached_existing_browser"
            assert data["account_state"] == "same_browser_profile"
            assert data["mouse"] == "untouched"
            assert data["clipboard"] == "untouched"
            assert data["visibility"] == ("background" if background else "foreground")
            opened.append(data["target"]["id"])
        print("visible Chrome profile conformance passed")
    finally:
        runtime.terminate()
        runtime.wait(timeout=5)
        for target_id in opened:
            close_target(target_id)
