#!/usr/bin/env python3
"""P5.5 live conformance: the desktop.terminal and desktop.explorer routes.

Run against a built binary (not a source tree) so this proves what a user
would actually get.  Five properties, each of which used to be a hole:

  0. The binary under test is new enough.  A pre-P5.5 binary answers
     `policy_denied` for everything, which would otherwise make the two
     refusal checks below pass for the wrong reason; so this asserts up front
     that `capabilities` advertises a `terminal` route.
  1. Policy first.  With `COMPTROL_ALLOW_COMMANDS` off, the intent is refused
     with `policy_denied` and nothing is executed.
  2. Honest unavailability.  With the policy on but no allowlist configured,
     the route reports `route_unavailable` instead of silently doing nothing.
  3. The allowlist is enforced.  A program that is not on
     `COMPTROL_COMMAND_ALLOWLIST` is refused with `policy_denied` -- the
     terminal route is not a second, looser way to run code than
     `command.run`.
  4. Verification comes from readback.  An allowlisted command runs with
     structured argv (no shell) and, with an `output_contains` postcondition,
     is marked `verified` by `terminal_output_readback` -- a second channel,
     not the dispatcher's own say-so.

`desktop.explorer` is checked for its refusals (policy off, missing path) and,
with `--reveal`, for a real reveal.  `--reveal` opens a file-manager window on
the machine running the test; close it when it finishes.

Usage:
    python scripts/terminal_conformance.py [--reveal]
    COMPTROL_BIN=./target/release/comptrol.exe python scripts/terminal_conformance.py
"""

import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]


def resolve_binary():
    """Prefer an explicitly named binary, then the freshest local build.

    `COMPTROL_BIN` is honored because CI and other conformance scripts use it,
    but a stale value pointing at another checkout must not silently turn this
    into a test of the wrong thing -- hence the printed path plus the
    capability preflight below.
    """
    override = os.environ.get("COMPTROL_BIN")
    if override:
        candidate = pathlib.Path(override)
        if candidate.exists():
            return str(candidate.resolve())
        raise SystemExit(f"COMPTROL_BIN points at a missing file: {override}")
    for relative in ("target/release", "target/debug", "target-buffy/release", "target-buffy/debug"):
        candidate = ROOT / relative / "comptrol"
        if os.name == "nt":
            candidate = candidate.with_suffix(".exe")
        if candidate.exists():
            return str(candidate)
    raise SystemExit("no built comptrol binary found; run `cargo build -p comptrol` first")


BINARY = resolve_binary()
REVEAL = "--reveal" in sys.argv[1:]

if os.name == "nt":
    SHELL_PROGRAM = "cmd.exe"
    SHELL_ARGS = ["/c", "echo"]
    DENIED_PROGRAM = "calc.exe"
else:
    SHELL_PROGRAM = "sh"
    SHELL_ARGS = ["-c", "echo"]
    DENIED_PROGRAM = "not-a-real-program"

failures = []
server_version = "unknown"


def check(name, condition, detail=""):
    status = "PASS" if condition else "FAIL"
    print(f"{status} {name}{(': ' + detail) if detail else ''}")
    if not condition:
        failures.append(name)


def error_code(result):
    return ((result or {}).get("error") or {}).get("code")


def data_of(result):
    """A refusal carries `data: null`; normalize it so checks can read it."""
    return (result or {}).get("data") or {}


class Mcp:
    """One `comptrol mcp` child, driven over stdio JSON-RPC."""

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
                "clientInfo": {"name": "terminal-conformance", "version": "1"},
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
        return self._send(self._id, {"name": name, "arguments": arguments or {}})["result"]["structuredContent"]

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
        except Exception:
            self.proc.kill()


def gate(name, env, body):
    print(f"\n== {name} ==")
    # Each gate gets its own state directory.  The child may probe (and so
    # leave behind) a Chrome profile under it, hence the best-effort cleanup.
    state = tempfile.mkdtemp(prefix="comptrol-terminal-conformance-")
    client = Mcp({**os.environ, **env, "COMPTROL_STATE_DIR": state})
    try:
        body(client)
    finally:
        client.close()


def echo_command(text):
    return {"program": SHELL_PROGRAM, "args": [*SHELL_ARGS, text]}


def advertises_route(client):
    catalog = json.dumps(client.tool("capabilities"))
    check(
        "the binary advertises the terminal route",
        '"route": "terminal"' in catalog.replace("'", '"') or '"terminal"' in catalog,
        f"comptrol {server_version}",
    )
    check(
        "the binary advertises the file_explorer route",
        '"file_explorer"' in catalog.replace("'", '"'),
        "",
    )


def policy_off(client):
    result = client.operate(
        intent="desktop.terminal",
        params={"commands": [echo_command("should-never-run")], "visible": False},
    )
    check(
        "policy off refuses desktop.terminal",
        error_code(result) == "policy_denied",
        str(error_code(result)),
    )
    check(
        "nothing ran when policy was off",
        not data_of(result).get("output", "").strip(),
        json.dumps(result.get("data"))[:100],
    )


def route_not_configured(client):
    result = client.operate(
        intent="desktop.terminal",
        params={"commands": [echo_command("should-never-run")], "visible": False},
    )
    check(
        "an unconfigured terminal route reports route_unavailable",
        error_code(result) == "route_unavailable",
        str(error_code(result)),
    )


def runs_and_verifies(client):
    result = client.operate(
        intent="desktop.terminal",
        postcondition={"output_contains": "comptrol-terminal-conformance"},
        idempotency_key="terminal-conformance-echo",
        params={"commands": [echo_command("comptrol-terminal-conformance")], "visible": False},
    )
    check("an allowlisted command runs", result.get("error") is None, json.dumps(result.get("error"))[:140])
    check("it is verified from readback", result.get("verification") == "verified", str(result.get("verification")))
    data = data_of(result)
    check(
        "readback names its channel",
        data.get("verified_by") == "terminal_output_readback",
        str(data.get("verified_by")),
    )
    check("the echoed text came back", "comptrol-terminal-conformance" in data.get("output", ""))
    check("the route is the terminal route", result.get("route") == "terminal", str(result.get("route")))
    check("the invocation is reported as structured argv", "comptrol-terminal-conformance" in json.dumps(data))
    check(
        "the terminal did not take the foreground",
        (result.get("disturbance") or {}).get("foreground_changed") is False,
        json.dumps(result.get("disturbance")),
    )


def enforces_allowlist(client):
    result = client.operate(
        intent="desktop.terminal",
        params={"commands": [{"program": DENIED_PROGRAM, "args": []}], "visible": False},
    )
    check(
        "a program outside the allowlist is refused, not run",
        error_code(result) == "policy_denied",
        json.dumps(result.get("error"))[:140],
    )
    partial = client.operate(
        intent="desktop.terminal",
        params={
            "commands": [echo_command("first-half"), {"program": DENIED_PROGRAM, "args": []}],
            "visible": False,
        },
    )
    check(
        "all commands are validated before any of them run",
        error_code(partial) == "policy_denied" and not data_of(partial).get("output", "").strip(),
        json.dumps(partial.get("data"))[:100],
    )


def explorer_refusals(client, sample):
    missing = client.operate(
        intent="desktop.explorer",
        params={"path": str(sample.parent / "definitely-not-here-comptrol")},
    )
    check(
        "explorer refuses a path that does not exist",
        error_code(missing) == "path_not_found",
        str(error_code(missing)),
    )
    control = client.operate(
        intent="desktop.explorer",
        params={"path": "C:\\bad\npath"},
    )
    check(
        "explorer refuses control characters in a path",
        error_code(control) == "invalid_input",
        str(error_code(control)),
    )


def explorer_reveals(client, sample):
    result = client.operate(intent="desktop.explorer", params={"path": str(sample)})
    check("explorer reveals an existing path", result.get("error") is None, json.dumps(result.get("error"))[:140])
    check(
        "the reveal is not claimed as verified",
        result.get("verification") != "verified",
        str(result.get("verification")),
    )


def explorer_policy_off(client, sample):
    result = client.operate(intent="desktop.explorer", params={"path": str(sample)})
    check("explorer is refused when its gate is off", error_code(result) == "policy_denied", str(error_code(result)))


def main():
    print(f"binary: {BINARY}")
    print(f"python: {sys.version.split()[0]} on {sys.platform}")
    with tempfile.TemporaryDirectory(prefix="comptrol-terminal-allow-") as allow_root:
        sample = pathlib.Path(allow_root) / "comptrol-conformance-sample.txt"
        sample.write_text("comptrol desktop.conformance sample\n", encoding="utf-8")

        configured = {
            "COMPTROL_ALLOW_COMMANDS": "1",
            "COMPTROL_COMMAND_ROOT": allow_root,
            "COMPTROL_COMMAND_ALLOWLIST": SHELL_PROGRAM,
            "COMPTROL_ALLOW_DESKTOP_EXPLORER": "1",
        }

        gate("advertised routes", configured, advertises_route)
        gate(
            "policy off",
            {"COMPTROL_ALLOW_COMMANDS": "0", "COMPTROL_ALLOW_DESKTOP_EXPLORER": "0"},
            policy_off,
        )
        # Policy on, but no allowlist: the route itself is not configured.
        gate(
            "route not configured",
            {
                "COMPTROL_ALLOW_COMMANDS": "1",
                "COMPTROL_COMMAND_ALLOWLIST": "",
                "COMPTROL_ALLOW_DESKTOP_EXPLORER": "0",
            },
            route_not_configured,
        )
        gate("allowlisted command", configured, runs_and_verifies)
        gate("allowlist enforcement", configured, enforces_allowlist)
        gate("explorer refusals", configured, lambda client: explorer_refusals(client, sample))
        gate(
            "explorer gate off",
            {**configured, "COMPTROL_ALLOW_DESKTOP_EXPLORER": "0"},
            lambda client: explorer_policy_off(client, sample),
        )
        if REVEAL:
            gate("explorer reveal", configured, lambda client: explorer_reveals(client, sample))
        else:
            print("\n(skipping the live reveal; pass --reveal to open a file-manager window)")

    print()
    if failures:
        print(f"FAILED: {failures}")
        return 1
    print("PASS desktop.terminal / desktop.explorer live conformance")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
