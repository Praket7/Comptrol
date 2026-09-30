#!/usr/bin/env python3
"""Live gate: the ms-settings / OS permission-surface intents work and stay honest.

Gate 5 of the build plan requires "terminal + ms-settings intents working".
This exercises `permission.request` -- the intent that opens the documented OS
settings surface (on Windows: `ms-settings:privacy-*`) -- against the built
binary:

  1. Policy first.  With the settings gate off, the intent is refused and no
     surface is opened.
  2. An unknown permission is refused honestly (unsupported_permission).
  3. A real permission opens the exact documented surface and reports
     `awaiting_human_action` with verification `unverified` -- it never claims
     a grant it did not observe.
  4. `settings.get` answers with either a value or a documented refusal.

Usage:
    python scripts/settings_conformance.py
"""

import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
failures = []
server_version = "unknown"


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


BINARY = resolve_binary()

PLATFORM = {"nt": "windows", "darwin": "macos"}.get(os.name, "linux")

# The schema enum for permission.request is [accessibility, screen_recording,
# automation, notification]; "notification" is the documented value.
EXPECTED_SURFACE = {
    "windows": "ms-settings:privacy-notifications",
    "macos": "x-apple.systempreferences:com.apple.preference.notifications",
    "linux": "gnome-control-center notifications",
}


def settings_processes():
    """Running OS settings apps (0 when none), so a gate can tell whether it
    started one and leave no window behind either way."""
    if os.name == "nt":
        script = (
            "Get-CimInstance Win32_Process -Filter \"Name = 'SystemSettings.exe'\" | "
            "Measure-Object | Select-Object -ExpandProperty Count"
        )
    elif sys.platform == "darwin":
        script = None
        return int(
            subprocess.run(
                ["pgrep", "-x", "System Settings"], capture_output=True, text=True
            ).stdout.strip()
            or "0"
        )
    else:
        return int(
            subprocess.run(["pgrep", "-x", "gnome-control-c"], capture_output=True, text=True)
            .stdout.strip()
            or "0"
        )
    completed = subprocess.run(
        ["powershell", "-NoProfile", "-NonInteractive", "-Command", script],
        capture_output=True,
        text=True,
        timeout=60,
    )
    return int(completed.stdout.strip() or "0")


def close_settings_if_i_started_it(started_before):
    """Never leave a window the user did not ask for: if this gate started the
    OS settings app, this gate stops it again."""
    if os.name == "nt" and started_before == 0 and settings_processes() > 0:
        subprocess.run(
            ["taskkill", "/IM", "SystemSettings.exe", "/F"],
            capture_output=True,
            timeout=60,
        )


class Mcp:
    def __init__(self, env):
        global server_version
        self.proc = subprocess.Popen(
            [BINARY, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=env,
        )
        self._id = 0
        handshake = self._send(
            9999,
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "settings-conformance", "version": "1"},
            },
            method="initialize",
        )
        info = ((handshake or {}).get("result") or {}).get("serverInfo") or {}
        server_version = info.get("version", "unknown")

    def _send(self, request_id, params, method="tools/call"):
        self.proc.stdin.write(
            json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n"
        )
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
        return self._send(self._id, {"name": name, "arguments": arguments or {}})["result"][
            "structuredContent"
        ]

    def operate(self, **arguments):
        return self.tool("operate", arguments)

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def gate(name, env, body):
    print(f"\n== {name} ==")
    state = tempfile.mkdtemp(prefix="comptrol-settings-conformance-")
    client = Mcp({**os.environ, **env, "COMPTROL_STATE_DIR": state})
    try:
        body(client)
    finally:
        client.close()


def data_of(result):
    return result.get("data") or {}


def error_code_of(result):
    error = result.get("error")
    return (error.get("code") if isinstance(error, dict) else error) or None


def policy_off(client):
    before = settings_processes()
    result = client.operate(intent="permission.request", params={"permission": "notifications"})
    check(
        "policy off refuses permission.request",
        error_code_of(result) == "policy_denied",
        json.dumps(result.get("error"))[:120],
    )
    check(
        "policy off opens no settings surface",
        settings_processes() == before,
        f"processes before={before} after={settings_processes()}",
    )


def surfaces_open_and_stay_honest(client):
    result = client.operate(
        intent="permission.request",
        params={"permission": "comptrol-nonexistent-permission"},
    )
    code = error_code_of(result)
    check(
        "an unknown permission is refused with a documented code",
        code in ("invalid_input", "unsupported_permission"),
        str(code),
    )

    before = settings_processes()
    result = client.operate(
        intent="permission.request",
        params={"permission": "notification"},
        idempotency_key="settings-conformance-notification",
    )
    check("a known permission request is delivered", result.get("error") is None, json.dumps(result.get("error"))[:120])
    data = data_of(result)
    expected = EXPECTED_SURFACE.get(PLATFORM, "")
    check(
        "it names the exact documented OS surface",
        data.get("surface") == expected,
        f"{data.get('surface')} != {expected}",
    )
    check(
        "it reports awaiting_human_action, never a guessed grant",
        data.get("state") == "awaiting_human_action",
        str(data.get("state")),
    )
    check(
        "its verification is honestly unverified",
        result.get("verification") == "unverified",
        str(result.get("verification")),
    )
    check(
        "the agent is forbidden from entering the secret",
        data.get("agent_must_not_enter_secret") is True,
        str(data.get("agent_must_not_enter_secret")),
    )
    after = settings_processes()
    check(
        "the OS settings surface actually opened",
        after > before or after > 0,
        f"processes before={before} after={after}",
    )
    close_settings_if_i_started_it(before)


def settings_get_answers_honestly(client):
    result = client.operate(intent="settings.get", params={"key": "settings.audio.output_device"})
    error = result.get("error")
    if error is None:
        data = data_of(result)
        check(
            "settings.get returns a structured answer",
            "key" in json.dumps(data) or "value" in json.dumps(data),
            json.dumps(data)[:120],
        )
    else:
        code = error.get("code") if isinstance(error, dict) else error
        message = error.get("message") if isinstance(error, dict) else ""
        check(
            "settings.get refuses with a documented code and message",
            bool(code) and bool(message),
            f"{code}: {message}",
        )


def main():
    print(f"binary: {BINARY}")
    if os.name not in ("nt", "darwin") and not os.environ.get("DISPLAY"):
        print("SKIP settings conformance: no desktop session on this machine")
        return 0

    gate(
        "policy off",
        {"COMPTROL_ALLOW_SETTINGS": "0"},
        policy_off,
    )
    gate(
        "permission surfaces",
        {"COMPTROL_ALLOW_SETTINGS": "1"},
        surfaces_open_and_stay_honest,
    )
    gate(
        "settings read",
        {"COMPTROL_ALLOW_SETTINGS": "1"},
        settings_get_answers_honestly,
    )

    print()
    if failures:
        print(f"FAILED: {failures}")
        return 1
    print(f"PASS permission surface / settings conformance (comptrol {server_version})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
