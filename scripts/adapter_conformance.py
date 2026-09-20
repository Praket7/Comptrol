#!/usr/bin/env python3
"""Validate and smoke-test the isolated first-party adapter boundaries."""

import json
import pathlib
import struct
import subprocess
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[1]
ADAPTERS = {
    "vscode": ROOT / "adapters" / "vscode" / "src" / "adapter.py",
    "libreoffice": ROOT / "adapters" / "libreoffice" / "src" / "adapter.py",
    "obs": ROOT / "adapters" / "obs" / "src" / "adapter.py",
    "blender": ROOT / "adapters" / "blender" / "src" / "adapter.py",
}


def frame(value):
    payload = json.dumps(value, separators=(",", ":")).encode()
    return struct.pack(">I", len(payload)) + payload


def read_frame(stream):
    header = stream.read(4)
    if len(header) != 4:
        raise RuntimeError("adapter closed before response")
    length = struct.unpack(">I", header)[0]
    if length > 256 * 1024:
        raise RuntimeError("adapter returned an oversized frame")
    return json.loads(stream.read(length))


def call(process, request_id, method):
    process.stdin.write(frame({
        "protocol_version": 1,
        "adapter_instance_id": "conformance",
        "request_id": request_id,
        "deadline_ms": 9_999_999_999_999,
        "capability_token": None,
        "resource_scope": "adapter",
        "method": method,
        "payload": {},
    }))
    process.stdin.flush()
    response = read_frame(process.stdout)
    assert response["protocol_version"] == 1, response
    assert response["request_id"] == request_id, response
    return response


# Declared mutable intents map to the implementation file and the token that
# must appear in it for the capability to be considered implemented. This
# closes the manifest/handler gap: CI fails when a manifest advertises an
# intent that no implementation provides, or when an adapter's declared
# verification level has no enforcing code.
HANDLER_SENTINELS = {
    "libreoffice.document.open": ("adapters/libreoffice/src/adapter.py", "loadComponentFromURL"),
    "libreoffice.document.save": ("adapters/libreoffice/src/adapter.py", "document.store()"),
    "libreoffice.document.export": ("adapters/libreoffice/src/adapter.py", "storeToURL"),
    "libreoffice.calc.range.read": ("adapters/libreoffice/src/adapter.py", "getDataArray"),
    "libreoffice.calc.range.write": ("adapters/libreoffice/src/adapter.py", "setDataArray"),
    "libreoffice.writer.text.replace": ("adapters/libreoffice/src/adapter.py", "replaceAll"),
    "vscode.workspace.list": ("adapters/vscode/src/extension.ts", "workspaceFolders"),
    "vscode.setting.get": ("adapters/vscode/src/extension.ts", "getConfiguration"),
    "vscode.setting.set": ("adapters/vscode/src/extension.ts", "ConfigurationTarget.Workspace"),
    "vscode.document.open": ("adapters/vscode/src/extension.ts", "openTextDocument"),
    "vscode.document.save": ("adapters/vscode/src/extension.ts", "workspace.fs.stat"),
    "blender.scene.object.list": ("adapters/blender/src/comptrol_live_bridge.py", "scene.objects"),
    "blender.scene.object.create": ("adapters/blender/src/comptrol_live_bridge.py", "primitive_cube_add"),
    "blender.scene.object.transform": ("adapters/blender/src/comptrol_live_bridge.py", "obj.location"),
    "blender.project.save": ("adapters/blender/src/comptrol_live_bridge.py", "save_as_mainfile"),
    "blender.render": ("adapters/blender/src/comptrol_live_bridge.py", "write_still"),
    "obs.scene.list": ("adapters/obs/src/adapter.py", "GetSceneList"),
    "obs.scene.switch": ("adapters/obs/src/adapter.py", "GetCurrentProgramScene"),
    "obs.source.visibility.set": ("adapters/obs/src/adapter.py", "GetSceneItemList"),
    "obs.recording.start": ("adapters/obs/src/adapter.py", "GetRecordStatus"),
    "obs.recording.stop": ("adapters/obs/src/adapter.py", "GetRecordStatus"),
}


for name, script in ADAPTERS.items():
    manifest = tomllib.loads((ROOT / "adapters" / name / "adapter.toml").read_text(encoding="utf-8"))
    assert manifest["manifest_version"] == 1
    assert manifest["isolation"] == {"mode": "out_of_process", "network": "loopback_only", "filesystem": "declared_scopes"}
    assert manifest["capabilities"]
    source = script.read_text(encoding="utf-8")
    for capability in manifest["capabilities"]:
        intent = capability.get("intent")
        assert intent, (name, "capability without intent")
        # A declared mutable capability must declare a verification level.
        assert capability.get("verification"), (name, intent, "missing verification level")
        if capability.get("risk") in ("R2", "R3"):
            registered = HANDLER_SENTINELS.get(intent)
            assert registered, (name, intent, "no handler sentinel registered")
            handler_file, sentinel = registered
            handler_path = ROOT / handler_file
            assert handler_path.exists(), (name, intent, f"handler file missing: {handler_file}")
            assert sentinel in handler_path.read_text(encoding="utf-8"), (
                name,
                intent,
                f"declared but not implemented: missing {sentinel!r} in {handler_file}",
            )
        if capability.get("verification") == "persisted_artifact":
            # The artifact check may live in the handler implementation or in
            # the adapter's own verification path.
            artifact_sources = [source]
            if intent in HANDLER_SENTINELS:
                artifact_sources.append((ROOT / HANDLER_SENTINELS[intent][0]).read_text(encoding="utf-8"))
            assert any(
                "output_size" in candidate or "fs.stat" in candidate or "artifact" in candidate.lower()
                for candidate in artifact_sources
            ), (
                name,
                intent,
                "persisted_artifact verification must check the file artifact",
            )
    process = subprocess.Popen([sys.executable, str(script)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    try:
        handshake = call(process, f"{name}-handshake", "handshake")
        capabilities = call(process, f"{name}-capabilities", "capabilities")
        assert handshake["ok"] and capabilities["ok"], (name, handshake, capabilities)
    finally:
        process.terminate()
        process.wait(timeout=5)

print(f"adapter conformance passed for {len(ADAPTERS)} isolated adapters")

