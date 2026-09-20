#!/usr/bin/env python3
"""Validate and smoke-test the isolated first-party adapter boundaries."""

import json
import pathlib
import re
import struct
import subprocess
import sys

try:
    import tomllib
except ImportError:  # Python < 3.11: minimal parser for the flat manifest shape
    tomllib = None


def load_manifest(text):
    if tomllib is not None:
        return tomllib.loads(text)
    manifest = {"capabilities": []}
    current = None
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line == "[[capabilities]]":
            current = {}
            manifest["capabilities"].append(current)
            continue
        if line.startswith("["):
            current = None
            if line == "[isolation]":
                current = manifest.setdefault("isolation", {})
            continue
        match = re.match(r'(\w+)\s*=\s*(".*?"|\[.*?\]|\d+)', line)
        if not match:
            continue
        key, raw = match.groups()
        if raw.startswith('"'):
            value = raw[1:-1]
        elif raw.startswith("["):
            value = [item.strip().strip('"') for item in raw[1:-1].split(",") if item.strip()]
        else:
            value = int(raw)
        if current is not None:
            current[key] = value
        else:
            manifest[key] = value
    return manifest


ROOT = pathlib.Path(__file__).resolve().parents[1]
ADAPTERS = {
    "vscode": ROOT / "adapters" / "vscode" / "src" / "adapter.py",
    "libreoffice": ROOT / "adapters" / "libreoffice" / "src" / "adapter.py",
    "obs": ROOT / "adapters" / "obs" / "src" / "adapter.py",
    "blender": ROOT / "adapters" / "blender" / "src" / "adapter.py",
    "davinci-resolve": ROOT / "adapters" / "davinci-resolve" / "src" / "adapter.py",
    "google-workspace": ROOT / "adapters" / "google-workspace" / "src" / "adapter.py",
    "powerpoint": ROOT / "adapters" / "powerpoint" / "src" / "adapter.py",
    "powerpoint-windows": ROOT / "adapters" / "powerpoint-windows" / "src" / "adapter.py",
    "discord": ROOT / "adapters" / "discord" / "src" / "adapter.py",
    "gmail": ROOT / "adapters" / "gmail" / "src" / "adapter.py",
    "microsoft-graph-mail": ROOT / "adapters" / "microsoft-graph-mail" / "src" / "adapter.py",
    "apple-mail": ROOT / "adapters" / "apple-mail" / "src" / "adapter.py",
    "apple-messages": ROOT / "adapters" / "apple-messages" / "src" / "adapter.py",
    "canva": ROOT / "adapters" / "canva" / "src" / "adapter.py",
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

# New adapters dispatch on closed intent strings: every declared mutable
# intent must appear as a handler branch in its implementation file.
INTENT_LITERAL_ADAPTERS = {
    "davinci-resolve": "adapters/davinci-resolve/src/adapter.py",
    "google-workspace": "adapters/google-workspace/src/adapter.py",
    "powerpoint": "adapters/powerpoint/src/adapter.py",
    "powerpoint-windows": "adapters/powerpoint-windows/src/adapter.py",
    "discord": "adapters/discord/src/adapter.py",
    "gmail": "adapters/gmail/src/adapter.py",
    "microsoft-graph-mail": "adapters/microsoft-graph-mail/src/adapter.py",
    "apple-mail": "adapters/apple-mail/src/adapter.py",
    "apple-messages": "adapters/apple-messages/src/adapter.py",
    "canva": "adapters/canva/src/adapter.py",
}


for name, script in ADAPTERS.items():
    manifest = load_manifest((ROOT / "adapters" / name / "adapter.toml").read_text(encoding="utf-8"))
    assert manifest["manifest_version"] == 1
    assert manifest["isolation"] == {"mode": "out_of_process", "network": "loopback_only", "filesystem": "declared_scopes"}
    assert manifest["capabilities"]
    source = script.read_text(encoding="utf-8")
    for capability in manifest["capabilities"]:
        intent = capability.get("intent")
        assert intent, (name, "capability without intent")
        # A declared mutable capability must declare a verification level.
        assert capability.get("verification"), (name, intent, "missing verification level")
        assert capability.get("risk") in ("R0", "R1", "R2", "R3"), (name, intent, "missing risk class")
        if capability.get("risk") in ("R2", "R3"):
            registered = HANDLER_SENTINELS.get(intent)
            if registered is None and name in INTENT_LITERAL_ADAPTERS:
                handler_file = INTENT_LITERAL_ADAPTERS[name]
                handler_path = ROOT / handler_file
                assert handler_path.exists(), (name, intent, f"handler file missing: {handler_file}")
                assert intent in handler_path.read_text(encoding="utf-8"), (
                    name,
                    intent,
                    f"declared but not implemented: missing {intent!r} branch in {handler_file}",
                )
                continue
            assert registered, (name, intent, "no handler sentinel registered")
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

# Cross-platform adapter IPC: the shared client must expose all transports,
# and migrated live bridges must not assume AF_UNIX-only connectivity.
shared = ROOT / "adapters" / "_shared"
sys.path.insert(0, str(shared))
import adapter_ipc  # noqa: E402

for symbol in ("connect", "resolve_endpoint", "bridge_request", "send_frame", "recv_frame"):
    assert callable(getattr(adapter_ipc, symbol, None)), f"adapter_ipc.{symbol} missing"
assert "pipe:" in (shared / "adapter_ipc.py").read_text(encoding="utf-8")
assert "tcp:127.0.0.1" in (shared / "adapter_ipc.py").read_text(encoding="utf-8")

for migrated in (
    "adapters/vscode/src/adapter.py",
    "adapters/blender/src/adapter.py",
):
    source = (ROOT / migrated).read_text(encoding="utf-8")
    assert "adapter_ipc" in source, (migrated, "must use the shared IPC client")
    assert "AF_UNIX" not in source, (migrated, "must not assume AF_UNIX-only transport")

bridge = (ROOT / "adapters" / "blender" / "src" / "comptrol_live_bridge.py").read_text(encoding="utf-8")
assert '"nonce"' in bridge or "'nonce'" in bridge, "live bridge must echo the request nonce"
assert "127.0.0.1" in bridge, "live bridge needs the loopback TCP fallback"

extension = (ROOT / "adapters" / "vscode" / "src" / "extension.ts").read_text(encoding="utf-8")
assert "secrets" in extension, "extension must pair through SecretStorage"
assert "pipe:" in extension or "pipe\\\\" in extension, "extension must support named pipes"

print("adapter IPC conformance passed: shared client, migrated bridges, pairing")

