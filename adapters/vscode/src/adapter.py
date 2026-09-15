#!/usr/bin/env python3
import os
import socket
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402


def handler(request):
    method = request.get("method")
    socket_path = os.environ.get("COMPTROL_VSCODE_BRIDGE_SOCKET")
    if method == "handshake":
        return response(request, True, "available", {"adapter": "comptrol.vscode", "bridge": "official_extension_api"})
    if method == "capabilities":
        return response(request, True, "available", {"source": "adapter.toml"})
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    if not socket_path:
        return response(request, False, "requires_consent", error={"code": "bridge_not_configured", "message": "Install and explicitly start the local VS Code bridge"})
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(2)
            connection.connect(socket_path)
            # The extension bridge uses newline framed JSON and repeats the request identity.
            import json
            connection.sendall((json.dumps({"version": 1, "request": request}) + "\n").encode())
            data = b""
            while not data.endswith(b"\n"):
                chunk = connection.recv(65536)
                if not chunk:
                    break
                data += chunk
            if not data:
                raise RuntimeError("VS Code bridge closed without a response")
            bridge = json.loads(data)
            return response(request, bool(bridge.get("ok")), bridge.get("health", "available"), bridge.get("payload", {}), bridge.get("error"))
    except (OSError, ValueError, RuntimeError) as exc:
        return response(request, False, "unhealthy", error={"code": "bridge_unavailable", "message": str(exc)})


serve(handler)

