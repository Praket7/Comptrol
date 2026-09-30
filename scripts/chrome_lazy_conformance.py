#!/usr/bin/env python3
"""Live gate: Comptrol must not open a browser window the user did not ask for.

This is the regression gate for a bug that made Comptrol unusable in practice:
every `comptrol mcp` session -- including desktop-only, terminal-only, and
adapter-only ones -- started a visible Chrome with an `about:blank` tab, so
asking for a Blender or terminal operation threw a browser window at the user,
once per client session.

It checks four properties against the built binary:

  1. A session that never touches the browser starts no Chrome at all.
  2. A browser operation starts exactly one Chrome, with a dedicated profile,
     and that Chrome has no page target until Comptrol opens one.
  3. A later session reuses the running Chrome instead of starting a second
     (and a reusing session never stops a browser it did not start).
  4. The session that started the Chrome stops it when it ends normally.

Chrome is located the same way Comptrol locates it. On a machine without Chrome
(in or out of a Windows environment) the gate skips cleanly.

Usage:
    python scripts/chrome_lazy_conformance.py
"""

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
failures = []


def check(name, condition, detail=""):
    status = "PASS" if condition else "FAIL"
    print(f"{status} {name}{(': ' + detail) if detail else ''}")
    if not condition:
        failures.append(name)


def resolve_binary():
    override = os.environ.get("COMPTROL_BIN")
    if override:
        if not pathlib.Path(override).exists():
            raise SystemExit(f"COMPTROL_BIN points at a missing file: {override}")
        return override
    for relative in ("target/release", "target/debug"):
        candidate = ROOT / relative / "comptrol"
        if os.name == "nt":
            candidate = candidate.with_suffix(".exe")
        if candidate.exists():
            return str(candidate)
    raise SystemExit("no built comptrol binary found; run `cargo build -p comptrol` first")


def resolve_chrome():
    for variable in ("PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"):
        base = os.environ.get(variable)
        if not base:
            continue
        candidate = pathlib.Path(base) / "Google/Chrome/Application/chrome.exe"
        if candidate.is_file():
            return candidate
    return None


BINARY = resolve_binary()
CHROME = resolve_chrome()

POWERSHELL = (
    "Get-CimInstance Win32_Process -Filter \"Name = 'chrome.exe'\" | "
    "Where-Object {{ $_.CommandLine -like '*{profile}*' }} | "
    "ForEach-Object {{ \"$($_.ProcessId)`t$($_.CommandLine)\" }}"
)


def chrome_procs(profile):
    """(pid, command line) for every chrome.exe bound to one profile directory."""
    if os.name != "nt":
        return [
            (pid, "")
            for pid in subprocess.run(["pgrep", "-f", str(profile)], capture_output=True, text=True).stdout.split()
        ]
    script = POWERSHELL.format(profile=str(profile).replace("'", "''"))
    completed = subprocess.run(
        ["powershell", "-NoProfile", "-NonInteractive", "-Command", script],
        capture_output=True,
        text=True,
        timeout=90,
    )
    procs = []
    for line in completed.stdout.splitlines():
        pid, _, command = line.partition("\t")
        if pid.strip().isdigit():
            procs.append((pid.strip(), command))
    return procs


def chrome_pids(profile):
    """Chrome processes bound to one profile directory."""
    return [pid for pid, _ in chrome_procs(profile)]


def browser_pids(profile):
    """Top-level browser processes: Chrome's children carry --type= switches,
    so one Chrome instance is one non-child process no matter how many
    renderers and GPU helpers share the profile path."""
    return [pid for pid, command in chrome_procs(profile) if "--type=" not in command]


def devtools(port, path):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=3) as response:
        return json.load(response)


def profile_port(profile):
    """The DevTools port a Chrome started against this profile is using."""
    try:
        first = (profile / "DevToolsActivePort").read_text(encoding="utf-8").splitlines()[0]
        port = int(first.strip())
    except (OSError, ValueError, IndexError):
        return None
    try:
        devtools(port, "/json/version")
    except Exception:
        return None
    return port


class Session:
    """One `comptrol mcp` child over stdio JSON-RPC."""

    def __init__(self, env):
        self.proc = subprocess.Popen(
            [BINARY, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
        )
        self._id = 0
        self._send(9999, {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "chrome-gate", "version": "1"}}, "initialize")

    def _send(self, request_id, params, method="tools/call"):
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("the MCP child closed stdout")
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("id") == request_id:
                return message

    def tool(self, name, arguments=None):
        self._id += 1
        return self._send(self._id, {"name": name, "arguments": arguments or {}})["result"]["structuredContent"]

    def operate(self, **arguments):
        return self.tool("operate", arguments)

    def finish(self, timeout=30):
        """Close stdin and let the session end the way a normal client ends it."""
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            self.proc.wait(timeout=timeout)
            return "terminated"
        return "exited"


def main():
    print(f"binary: {BINARY}")
    print(f"chrome: {CHROME or '(not found)'}")
    if not CHROME:
        print("SKIP chrome lazy conformance: no Chrome on this machine")
        return 0
    if os.name != "nt":
        print("SKIP chrome lazy conformance: the local auto-start is a Windows feature")
        return 0

    workspace = pathlib.Path(tempfile.mkdtemp(prefix="comptrol-chrome-lazy-"))
    profile = workspace / "state" / "chrome-cdp-profile"
    base = {**os.environ, "COMPTROL_STATE_DIR": str(workspace / "state")}
    (workspace / "state").mkdir(parents=True, exist_ok=True)

    # A terminal-only session must not start a browser at all.
    session = Session(
        {
            **base,
            "COMPTROL_ALLOW_COMMANDS": "1",
            "COMPTROL_COMMAND_ROOT": str(workspace),
            "COMPTROL_COMMAND_ALLOWLIST": "cmd.exe",
        }
    )
    try:
        result = session.operate(
            intent="desktop.terminal",
            params={"commands": [{"program": "cmd.exe", "args": ["/c", "echo", "no-browser"]}], "visible": False},
        )
        check("the terminal operation ran", result.get("error") is None, json.dumps(result.get("error"))[:120])
    finally:
        session.finish()
    check("a session that never touches the browser starts no Chrome", not chrome_pids(profile), f"profile={profile}")

    # The first browser operation is what starts Chrome, and it starts
    # windowless. Every property below is measured while the owning session is
    # still alive, because ending that session is what stops the Chrome it
    # started (a reused browser belongs to whoever started it).
    session = Session({**base, "COMPTROL_ALLOW_ALL_INTENTS": "1", "COMPTROL_ALLOW_BROWSER_CDP": "0"})
    try:
        result = session.operate(intent="browser.cdp.discovery")
        check("the first browser operation is served", result.get("error") is None, json.dumps(result.get("error"))[:200])
        time.sleep(2)
        pids = browser_pids(profile)
        check("the browser operation started exactly one Chrome", len(pids) == 1, f"pids={pids}")
        port = profile_port(profile)
        check("that Chrome is bound to a dedicated profile and answers DevTools", port is not None, f"port={port}")
        pages = []
        if port is not None:
            pages = [target for target in devtools(port, "/json/list") if target.get("type") == "page"]
        check(
            "the auto-started Chrome shows no page the user did not ask for",
            pages == [],
            json.dumps([target.get("url") for target in pages]),
        )

        # A second session must reuse the running Chrome instead of starting
        # another one -- and ending that second session must not stop a browser
        # it did not start.
        second = Session({**base, "COMPTROL_ALLOW_ALL_INTENTS": "1"})
        try:
            second_result = second.operate(intent="browser.cdp.discovery")
        finally:
            second.finish()
        check(
            "the second session's operation is served by the running Chrome",
            second_result.get("error") is None,
            json.dumps(second_result.get("error"))[:200],
        )
        time.sleep(2)
        after = browser_pids(profile)
        check(
            "a later session reuses the running Chrome instead of starting another",
            after == pids and len(after) == 1,
            f"first={pids} after={after}",
        )
    finally:
        outcome = session.finish()

    # And the session that started the Chrome stops it when it ends normally.
    time.sleep(3)
    lingering = chrome_pids(profile)
    check(
        "a session that ends normally stops the Chrome it started",
        not lingering,
        f"exit={outcome} lingering={lingering}",
    )

    print()
    if failures:
        print(f"FAILED: {failures}")
        return 1
    print("PASS Chrome is started lazily, windowless, reused, and stopped with the session")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
