#!/usr/bin/env python3
"""P5.5 live conformance: the PowerPoint COM adapter, end to end on real PowerPoint.

Runs the shipped `adapters/powerpoint-windows` adapter through Comptrol's own
MCP surface against a real PowerPoint install (comtypes COM), and checks the
things that make a desktop-adapter claim trustworthy:

  1. Exact-path binding.  `presentation.desktop.open` opens one exact `.pptx`
     and is verified by application-state readback of its full path. A missing
     deck is refused honestly (never fabricated).
  2. Semantic edit.  `presentation.shape.textbox.create` adds a textbox and
     `presentation.shape.text.set` writes text, both verified by reading the
     text back through the COM object.
  3. Save + reopen verification.  `presentation.save` persists the deck and
     reports `verified` only because the saved file exists with nonzero size
     and a stable SHA-256; a *separate* COM read of the file on disk confirms
     the text artifact, so the claim does not rest on the process that wrote
     it.
  4. Honest refusals.  A macro request is refused (`macro_execution_refused`),
     and an out-of-scope path is refused (`path_out_of_scope`) because
     Comptrol never runs VBA and never opens outside its presentations root.

PowerPoint is a heavy dependency, so a machine without it (or without
comtypes) skips cleanly with a printed reason (exit 0) instead of failing.

Usage:
    python scripts/powerpoint_live_conformance.py
    python scripts/powerpoint_live_conformance.py --raw
"""

import hashlib
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


BINARY = resolve_binary()
failures = []


def check(name, condition, detail=""):
    status = "ok  " if condition else "FAIL"
    line = f"[{status}] {name}"
    if detail and not condition:
        line += f" -- {detail}"
    print(line)
    if not condition:
        failures.append(name)


def comtypes_available():
    try:
        import comtypes.client  # noqa: F401
        return True
    except Exception:
        return False


def powerpoint_available():
    if os.name != "nt":
        return False
    for candidate in (
        r"C:\Program Files\Microsoft Office\root\Office16\POWERPNT.EXE",
        r"C:\Program Files (x86)\Microsoft Office\root\Office16\POWERPNT.EXE",
    ):
        if pathlib.Path(candidate).is_file():
            return True
    return bool(shutil.which("POWERPNT.EXE"))


def make_starter_deck(path):
    """Create a one-slide .pptx fixture via COM, then release PowerPoint.

    The adapter binds to *existing* decks (there is deliberately no blind
    'create new' route), so the gate supplies a real starter artifact and then
    drives every claim under test through the adapter itself.
    """
    import comtypes.client

    app = comtypes.client.CreateObject("PowerPoint.Application")
    try:
        pres = app.Presentations.Add()
        pres.Slides.Add(1, 12)  # 12 = ppLayoutBlank
        pres.SaveAs(str(path))
        pres.Close()
    finally:
        app.Quit()


def reopen_text(path):
    """A second, independent COM session reads the saved deck's text back."""
    import comtypes.client

    app = comtypes.client.CreateObject("PowerPoint.Application")
    texts = []
    try:
        pres = app.Presentations.Open(str(path), True, False, False)  # ReadOnly
        for index in range(1, int(pres.Slides.Count) + 1):
            slide = pres.Slides(index)
            for shape_index in range(1, int(slide.Shapes.Count) + 1):
                shape = slide.Shapes(shape_index)
                try:
                    texts.append(str(shape.TextFrame.TextRange.Text))
                except Exception:
                    continue
        pres.Close()
    finally:
        app.Quit()
    return texts


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
        self._send(9999, {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "powerpoint-gate", "version": "1"}}, "initialize")

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
    return ((result or {}).get("error") or {})


def data_of(result):
    # Core nests the adapter's raw payload under `data.payload`; fall back to
    # `data` itself for routes that flatten it.
    data = (result or {}).get("data") or {}
    nested = data.get("payload")
    return nested if isinstance(nested, dict) else data


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main():
    print(f"binary:     {BINARY}")
    print(f"comtypes:   {'yes' if comtypes_available() else 'no'}")
    print(f"powerpoint: {'yes' if powerpoint_available() else 'no'}")
    if os.name != "nt":
        print("SKIP powerpoint live conformance: Windows-only COM route")
        return 0
    if not comtypes_available():
        print("SKIP powerpoint live conformance: comtypes is not installed")
        return 0
    if not powerpoint_available():
        print("SKIP powerpoint live conformance: no PowerPoint on this machine")
        return 0

    with tempfile.TemporaryDirectory(prefix="comptrol-ppt-live-", ignore_cleanup_errors=True) as workspace:
        deck = pathlib.Path(workspace) / "gate.pptx"
        outside = pathlib.Path(tempfile.gettempdir()) / "comptrol-ppt-outside.pptx"
        make_starter_deck(deck)
        check("the starter deck fixture exists on disk", deck.is_file() and deck.stat().st_size > 0, f"{deck}")

        env = {
            **os.environ,
            "COMPTROL_STATE_DIR": os.path.join(workspace, "state"),
            "COMPTROL_ALLOW_ADAPTERS": "1",
            "COMPTROL_ADAPTER_ROOT": str(ROOT / "adapters"),
            "COMPTROL_PRESENTATIONS_ROOT": workspace,
        }
        os.makedirs(env["COMPTROL_STATE_DIR"], exist_ok=True)
        client = Mcp(env)
        try:
            catalog = json.dumps(client.tool("inspect", {"kind": "adapters"}))
            check("the PowerPoint adapter is in the runtime catalog", "powerpoint" in catalog)

            # 4a. Macro requests are always refused -- Comptrol never runs VBA.
            macro = client.operate(
                intent="presentation.desktop.open",
                idempotency_key="ppt-live-macro",
                params={"path": str(deck), "macro": "Auto_Open"},
            )
            macro_error = error_of(macro)
            check(
                "a macro request is refused (VBA is never executed)",
                "macro_execution_refused" in json.dumps(macro_error),
                json.dumps(macro_error)[:200],
            )

            # 4b. A path outside COMPTROL_PRESENTATIONS_ROOT is refused.
            if not outside.exists():
                make_starter_deck(outside)
            scope = client.operate(
                intent="presentation.desktop.open",
                idempotency_key="ppt-live-scope",
                params={"path": str(outside)},
            )
            scope_error = error_of(scope)
            check(
                "an out-of-scope path is refused (never opens outside the root)",
                "path_out_of_scope" in json.dumps(scope_error),
                json.dumps(scope_error)[:200],
            )

            # 1. Open one exact deck; verified by application-state readback.
            opened = client.operate(
                intent="presentation.desktop.open",
                idempotency_key="ppt-live-open",
                params={"path": str(deck)},
            )
            if RAW_OUTPUT:
                print(json.dumps(opened, indent=2)[:3000])
            state = data_of(opened)
            check("the deck opens through the adapter route", opened.get("error") is None, json.dumps(error_of(opened))[:250])
            check("the open is verified by application-state readback", opened.get("verification") == "verified", str(opened.get("verification")))
            full_name = str(state.get("full_name", ""))
            check(
                "the open deck is bound to the exact path",
                bool(full_name) and pathlib.Path(full_name).samefile(deck),
                full_name,
            )

            # 2. Add a textbox with text through the COM route.
            box = client.operate(
                intent="presentation.shape.textbox.create",
                idempotency_key="ppt-live-textbox",
                params={
                    "presentation_path": str(deck),
                    "slide": 1,
                    "left": 40,
                    "top": 40,
                    "width": 400,
                    "height": 120,
                    "text": "Comptrol PPT gate",
                    "name": "GateBox",
                },
            )
            if RAW_OUTPUT:
                print(json.dumps(box, indent=2)[:3000])
            box_data = data_of(box)
            check("a textbox is created through the adapter route", box.get("error") is None, json.dumps(error_of(box))[:250])
            # textbox.create verifies its own text readback internally (it does
            # not surface a `readback` field); the adapter's `verified` flag is
            # that readback, and the independent reopen below re-confirms it.
            check(
                "the textbox text is verified by the adapter's own readback",
                box.get("error") is None and box_data.get("verified") is True and box_data.get("shape") == "GateBox",
                f"verified={box_data.get('verified')} shape={box_data.get('shape')}",
            )

            # 2b. Set text on the named shape; verified by text readback.
            edited = client.operate(
                intent="presentation.shape.text.set",
                idempotency_key="ppt-live-textset",
                params={
                    "presentation_path": str(deck),
                    "slide": 1,
                    "shape": "GateBox",
                    "text": "Comptrol PPT verified",
                },
            )
            edit_data = data_of(edited)
            check("shape text is set through the adapter route", edited.get("error") is None, json.dumps(error_of(edited))[:250])
            check(
                "the edited text is read back through the COM object",
                edited.get("error") is None and str(edit_data.get("readback", "")) == "Comptrol PPT verified",
                str(edit_data.get("readback")),
            )

            # 3. Save; verified by persisted-artifact readback (size + SHA).
            saved = client.operate(
                intent="presentation.save",
                idempotency_key="ppt-live-save",
                params={"presentation_path": str(deck)},
            )
            if RAW_OUTPUT:
                print(json.dumps(saved, indent=2)[:3000])
            saved_data = data_of(saved)
            check("the deck saves through the adapter route", saved.get("error") is None, json.dumps(error_of(saved))[:250])
            check("the save is verified by persisted-artifact readback", saved.get("verification") == "verified", str(saved.get("verification")))
            check("the saved artifact is nonempty on disk", deck.is_file() and deck.stat().st_size > 0, f"{deck.stat().st_size if deck.is_file() else 0} bytes")
            header = deck.read_bytes()[:2] if deck.is_file() else b""
            check("the saved .pptx is a real ZIP/OOXML container", header == b"PK", repr(header))
            if deck.is_file() and deck.stat().st_size > 0:
                sha = sha256_of(deck)
                reported = str(saved_data.get("sha256_after", ""))
                check("the reported SHA-256 matches the artifact on disk", bool(reported) and reported == sha, f"reported={reported} actual={sha}")

            # 3b. Reopen verification: a separate COM session reads the text
            #     artifact back, so the claim does not rest on the writer.
            texts = reopen_text(deck)
            check(
                "an independent COM session reopens the saved deck",
                "Comptrol PPT verified" in texts,
                f"texts={texts}",
            )
        finally:
            client.close()
        if outside.exists():
            try:
                outside.unlink()
            except Exception:
                pass

    print()
    if failures:
        print(f"FAILED: {failures}")
        return 1
    print("PASS powerpoint live conformance")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
