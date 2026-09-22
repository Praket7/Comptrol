#!/usr/bin/env python3
"""Static and protocol conformance checks for the packaged Browser Bridge."""

import json
import os
import pathlib
import shutil
import struct
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
BRIDGE = ROOT / "extensions" / "comptrol-browser-bridge"


def require(path: pathlib.Path) -> None:
    if not path.is_file():
        raise SystemExit(f"missing Browser Bridge asset: {path.relative_to(ROOT)}")


def main() -> None:
    manifest_path = BRIDGE / "manifest.json"
    require(manifest_path)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("manifest_version") != 3:
        raise SystemExit("Browser Bridge must use Manifest V3")
    permissions = set(manifest.get("permissions", []))
    required_permissions = {
        "debugger",
        "tabs",
        "tabGroups",
        "sessions",
        "storage",
        "alarms",
        "nativeMessaging",
        "downloads",
    }
    missing_permissions = required_permissions - permissions
    if missing_permissions:
        raise SystemExit(f"Browser Bridge missing permissions: {sorted(missing_permissions)}")

    referenced = [
        manifest["background"]["service_worker"],
        manifest["action"]["default_popup"],
        *manifest.get("icons", {}).values(),
    ]
    for relative in referenced:
        require(BRIDGE / relative)

    service_worker = BRIDGE / manifest["background"]["service_worker"]
    node = shutil.which("node")
    if not node:
        raise SystemExit("node is required for Browser Bridge conformance")
    subprocess.run([node, "--check", str(service_worker)], check=True)

    native_host = BRIDGE / "native_host.py"
    installer = BRIDGE / "install.py"
    require(native_host)
    require(installer)
    subprocess.run(
        [sys.executable, "-m", "py_compile", str(native_host), str(installer)],
        check=True,
    )

    # Exercise the native-messaging framing contract without needing a daemon.
    env = {
        **os.environ,
        "COMPTROL_DAEMON_URL": "http://127.0.0.1:1",
    }
    process = subprocess.Popen(
        [sys.executable, str(native_host)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    request = json.dumps(
        {
            "type": "handshake",
            "protocol": "comptrol.browser.bridge/0.1.0",
        },
        separators=(",", ":"),
    ).encode("utf-8")
    assert process.stdin is not None
    assert process.stdout is not None
    process.stdin.write(struct.pack("<I", len(request)) + request)
    process.stdin.flush()

    raw_length = process.stdout.read(4)
    if len(raw_length) != 4:
        process.kill()
        raise SystemExit("native host did not emit a framed handshake response")
    length = struct.unpack("<I", raw_length)[0]
    response = json.loads(process.stdout.read(length).decode("utf-8"))
    if response.get("type") != "handshake_ack":
        process.kill()
        raise SystemExit(f"unexpected native host handshake response: {response}")
    process.stdin.close()
    process.wait(timeout=5)

    print(
        json.dumps(
            {
                "manifest": "valid",
                "javascript": "syntax_clean",
                "python": "syntax_clean",
                "native_messaging_handshake": "passed",
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
