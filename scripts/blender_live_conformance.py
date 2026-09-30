#!/usr/bin/env python3
"""P5.5 live conformance: the Blender adapter, end to end on real Blender.

Runs the shipped `adapters/blender` adapter through Comptrol's own MCP surface
against a real Blender install, and checks the two things that make a Blender
route trustworthy:

  1. Artifact verification.  `blender.scene.create_2d_rocket` launches Blender
     for real, saves a `.blend`, renders a `.png`, and must report
     `verified` only because both files exist with nonzero size.
  2. Reopen verification.  A *separate* Blender process reopens the saved
     `.blend` and lists its objects, so the claim does not rest on the process
     that wrote the file.

Blender is a heavy dependency, so a machine without it skips cleanly with a
printed reason (exit 0) instead of failing.  On a machine that has it, this is
the Stage 5 desktop-adapter gate.

Usage:
    python scripts/blender_live_conformance.py
    COMPTROL_BLENDER_BIN=/path/to/blender python scripts/blender_live_conformance.py
"""

import glob
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
RAW_OUTPUT = "--raw" in sys.argv[1:]


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


def resolve_blender():
    override = os.environ.get("COMPTROL_BLENDER_BIN", "").strip()
    if override and pathlib.Path(override).is_file():
        return override
    for name in ("blender", "blender.exe"):
        found = shutil.which(name)
        if found:
            return found
    candidates = []
    if os.name == "nt":
        for base in (os.environ.get("ProgramFiles", "C:/Program Files"), os.environ.get("ProgramW6432", "C:/Program Files")):
            candidates.extend(glob.glob(str(pathlib.Path(base) / "Blender Foundation" / "Blender *" / "blender.exe")))
    candidates.extend(
        [
            "/Applications/Blender.app/Contents/MacOS/Blender",
            str(pathlib.Path.home() / "Applications/Blender.app/Contents/MacOS/Blender"),
            "/usr/bin/blender",
            "/usr/local/bin/blender",
            "/snap/bin/blender",
        ]
    )
    return next((path for path in candidates if pathlib.Path(path).is_file()), None)


BINARY = resolve_binary()
BLENDER = resolve_blender()
failures = []


def check(name, condition, detail=""):
    status = "PASS" if condition else "FAIL"
    print(f"{status} {name}{(': ' + detail) if detail else ''}")
    if not condition:
        failures.append(name)


class Mcp:
    def __init__(self, env):
        self.proc = subprocess.Popen(
            [BINARY, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=env,
        )
        self._id = 0
        self._send(9999, {"protocolVersion": "0.1", "capabilities": {}, "clientInfo": {"name": "blender-gate", "version": "1"}}, "initialize")

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


def error_of(result):
    return ((result or {}).get("error") or {}).get("code")


def data_of(result):
    return (result or {}).get("data") or {}


LIST_OBJECTS = "import bpy, json; print('COMPTROL_OBJECTS=' + json.dumps(sorted(o.name for o in bpy.context.scene.objects)))"


def reopen_and_list(blend_path):
    """A second, independent Blender process reads the artifact back."""
    completed = subprocess.run(
        [BLENDER, "--background", str(blend_path), "--python-expr", LIST_OBJECTS],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    names = []
    for line in completed.stdout.splitlines():
        if line.startswith("COMPTROL_OBJECTS="):
            names = json.loads(line[len("COMPTROL_OBJECTS=") :])
    return names, completed


def main():
    print(f"binary:  {BINARY}")
    print(f"blender: {BLENDER or '(not found)'}")
    if not BLENDER:
        print("SKIP blender live conformance: no Blender executable on this machine")
        return 0

    # The MCP child probes Chrome and may leave a live profile under the state
# directory, which makes strict cleanup fail on Windows; the artifacts we
# verified are read before this block exits, so a best-effort cleanup is fine.
    with tempfile.TemporaryDirectory(prefix="comptrol-blender-live-", ignore_cleanup_errors=True) as workspace:
        env = {
            **os.environ,
            "COMPTROL_STATE_DIR": os.path.join(workspace, "state"),
            "COMPTROL_ALLOW_ADAPTERS": "1",
            "COMPTROL_ADAPTER_ROOT": str(ROOT / "adapters"),
            "COMPTROL_BLENDER_BIN": BLENDER,
        }
        os.makedirs(env["COMPTROL_STATE_DIR"], exist_ok=True)
        blend = pathlib.Path(workspace) / "rocket.blend"
        render = pathlib.Path(workspace) / "rocket.png"
        client = Mcp(env)
        try:
            catalog = json.dumps(client.tool("inspect", {"kind": "adapters"}))
            check("the Blender adapter is in the runtime catalog", '"comptrol.blender"' in catalog)

            rocket = client.operate(
                intent="blender.scene.create_2d_rocket",
                idempotency_key="blender-live-rocket",
                params={"output_path": str(blend), "render_path": str(render)},
            )
            if RAW_OUTPUT:
                print(json.dumps(rocket, indent=2)[:6000])
            check("the rocket recipe runs on real Blender", rocket.get("error") is None, json.dumps(rocket.get("error"))[:300])
            check("it is verified by artifact readback", rocket.get("verification") == "verified", str(rocket.get("verification")))
            check("the .blend artifact exists and is nonempty", blend.is_file() and blend.stat().st_size > 0, f"{blend.stat().st_size if blend.is_file() else 0} bytes")
            check("the render artifact exists and is nonempty", render.is_file() and render.stat().st_size > 0, f"{render.stat().st_size if render.is_file() else 0} bytes")
            header = blend.read_bytes()[:7] if blend.is_file() else b""
            # Blender 5.x saves a zstd-compressed container by default, so the
            # uncompressed "BLENDER" magic is not the only honest answer.
            check(
                "the .blend is a real Blender container",
                header.startswith(b"BLENDER")
                or header.startswith(b"\x28\xb5\x2f\xfd")
                or header.startswith(b"\x1f\x8b"),
                repr(header),
            )

            if blend.is_file():
                names, completed = reopen_and_list(blend)
                expected = {"Rocket Body", "Rocket Nose", "Left Fin", "Right Fin", "Window", "Flame Outer"}
                missing = expected - set(names)
                check("a separate Blender reopens the saved .blend", completed.returncode == 0 and bool(names), f"rc={completed.returncode} objects={len(names)}")
                check("the reopened scene still has the rocket geometry", not missing, f"missing={sorted(missing)}")

            listing = client.operate(
                intent="blender.scene.object.list",
                idempotency_key="blender-live-list",
                params={"input_path": str(blend)},
            )
            check("object listing works through the adapter route", listing.get("error") is None, json.dumps(listing.get("error"))[:200])
            check(
                "the adapter's own listing agrees with the independent reopen",
                "Rocket Body" in json.dumps(data_of(listing)),
            )
        finally:
            client.close()

    print()
    if failures:
        print(f"FAILED: {failures}")
        return 1
    print("PASS Blender adapter live conformance")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
