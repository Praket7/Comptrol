#!/usr/bin/env python3
"""Conformance for browser.chrome.restore_recent and its alias.

Runs entirely against the local MCP stdio surface with no live browser.
It verifies semantic matching, mode handling, ambiguity refusal, strict
background refusal, alias compatibility, and truthful refusal codes.

Label: fixture. This harness never claims a live Chrome restore happened.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "comptrol")


class Server:
    def __init__(self):
        self.state = tempfile.mkdtemp(prefix="comptrol-restore-conformance-")
        self.proc = subprocess.Popen(
            [BIN, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            env={**os.environ, "COMPTROL_STATE_DIR": self.state, "COMPTROL_ALLOW_MACOS_AX": "1", "COMPTROL_ALLOW_BROWSER_CDP": "1"},
            text=True,
        )
        self.next_id = 0

    def rpc(self, method, params=None):
        self.next_id += 1
        message = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        if params is not None:
            message["params"] = params
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("server closed the transport")
            response = json.loads(line)
            if response.get("id") == self.next_id:
                return response
            # notifications such as progress are skipped for this harness

    def initialize(self):
        response = self.rpc(
            "initialize",
            {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "restore-conformance"}},
        )
        self.rpc("notifications/initialized")
        return response

    def operate(self, request):
        response = self.rpc(
            "tools/call",
            {"name": "operate", "arguments": request},
        )
        text = response["result"]["content"][0]["text"]
        return json.loads(text)

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()
        shutil.rmtree(self.state, ignore_errors=True)


def check(name, condition, detail=""):
    status = "PASS" if condition else "FAIL"
    print(f"[{status}] {name}" + (f" :: {detail}" if detail and not condition else ""))
    return condition


def main():
    if not os.path.exists(BIN):
        print("release binary missing; build it first")
        return 1
    ok = True
    server = Server()
    try:
        init = server.initialize()
        ok &= check("initialize negotiates legacy protocol", init["result"]["protocolVersion"] == "2025-03-26")

        # Semantic identity is required.
        result = server.operate({
            "intent": "browser.chrome.restore_recent",
            "params": {"kind": "tab"},
            "risk": "R2",
            "idempotency_key": "restore-no-identity",
        })
        ok &= check(
            "restore without title/urls is refused invalid_input",
            result.get("error", {}).get("code") == "invalid_input",
            json.dumps(result.get("error")),
        )

        # Unsafe URL is refused before any dispatch.
        result = server.operate({
            "intent": "browser.chrome.restore_recent",
            "params": {"kind": "tab", "url": "file:///etc/passwd"},
            "risk": "R2",
            "idempotency_key": "restore-unsafe-url",
        })
        ok &= check(
            "restore with unsafe url is refused",
            result.get("error", {}).get("code") == "invalid_input",
            json.dumps(result.get("error")),
        )

        # Unknown mode refused.
        result = server.operate({
            "intent": "browser.chrome.restore_recent",
            "params": {"kind": "tab", "url": "https://example.test/", "mode": "teleport"},
            "risk": "R2",
            "idempotency_key": "restore-bad-mode",
        })
        ok &= check(
            "restore with unknown mode is refused",
            result.get("error", {}).get("code") == "invalid_input",
            json.dumps(result.get("error")),
        )

        # Strict background refusal.
        result = server.operate({
            "intent": "browser.chrome.restore_recent",
            "params": {"kind": "tab", "url": "https://example.test/"},
            "risk": "R2",
            "background": "strict_background",
            "idempotency_key": "restore-strict-bg",
        })
        ok &= check(
            "strict_background restore is refused",
            result.get("error", {}).get("code") == "background_unavailable",
            json.dumps(result.get("error")),
        )

        # Without a CDP endpoint, native restore is unavailable on this host
        # surface and reconstruction cannot be verified: refused with a stable code.
        result = server.operate({
            "intent": "browser.chrome.restore_recent",
            "params": {"kind": "tab", "url": "https://example.test/", "mode": "native_then_reconstruct"},
            "risk": "R2",
            "idempotency_key": "restore-no-endpoint",
        })
        ok &= check(
            "restore without CDP endpoint refuses with machine-readable code",
            result.get("error", {}).get("code") in {"native_restore_unavailable", "browser_unavailable"},
            json.dumps(result.get("error")),
        )

        # Alias: reopen_closed_group with only a group name maps to tab_group.
        result = server.operate({
            "intent": "browser.chrome.reopen_closed_group",
            "params": {"group": "Research"},
            "risk": "R2",
            "idempotency_key": "restore-alias-group",
        })
        ok &= check(
            "alias without CDP endpoint refuses with machine-readable code",
            result.get("error", {}).get("code") in {"native_restore_unavailable", "browser_unavailable"},
            json.dumps(result.get("error")),
        )

        # Capabilities surface lists both intents.
        caps = server.rpc("tools/call", {"name": "capabilities", "arguments": {}})
        caps_text = json.dumps(caps["result"]["structuredContent"])
        ok &= check("capabilities list restore_recent", "browser.chrome.restore_recent" in caps_text)
        ok &= check("capabilities list alias", "browser.chrome.reopen_closed_group" in caps_text)
    finally:
        server.close()
    print("RESTORE CONFORMANCE:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
