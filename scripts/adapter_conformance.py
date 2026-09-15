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


for name, script in ADAPTERS.items():
    manifest = tomllib.loads((ROOT / "adapters" / name / "adapter.toml").read_text(encoding="utf-8"))
    assert manifest["manifest_version"] == 1
    assert manifest["isolation"] == {"mode": "out_of_process", "network": "loopback_only", "filesystem": "declared_scopes"}
    assert manifest["capabilities"]
    process = subprocess.Popen([sys.executable, str(script)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    try:
        handshake = call(process, f"{name}-handshake", "handshake")
        capabilities = call(process, f"{name}-capabilities", "capabilities")
        assert handshake["ok"] and capabilities["ok"], (name, handshake, capabilities)
    finally:
        process.terminate()
        process.wait(timeout=5)

print(f"adapter conformance passed for {len(ADAPTERS)} isolated adapters")

