#!/usr/bin/env python3
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402
from adapter_ipc import bridge_request, resolve_endpoint  # noqa: E402


def bridge_endpoint():
    return resolve_endpoint(
        os.environ.get("COMPTROL_VSCODE_BRIDGE_SOCKET"),
        descriptor="vscode",
    )


def handler(request):
    method = request.get("method")
    bridge_token = os.environ.get("COMPTROL_VSCODE_BRIDGE_TOKEN")
    if method == "handshake":
        endpoint, _ = bridge_endpoint()
        return response(request, True, "available" if bridge_token else "requires_consent", {"adapter": "comptrol.vscode", "bridge": "official_extension_api", "authenticated": bool(bridge_token), "endpoint_discovered": bool(endpoint)})
    if method == "capabilities":
        return response(request, True, "available", {"source": "adapter.toml"})
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    endpoint, source = bridge_endpoint()
    if not endpoint:
        return response(request, False, "requires_consent", error={"code": "bridge_not_configured", "message": "Install the Comptrol VS Code extension bridge once; it publishes its endpoint for discovery"})
    if not bridge_token:
        return response(request, False, "requires_consent", error={"code": "bridge_authentication_required", "message": "Configure COMPTROL_VSCODE_BRIDGE_TOKEN for the authenticated VS Code bridge"})
    try:
        # The extension bridge uses newline framed JSON and repeats the
        # request identity. Transport is a Unix socket, a Windows named
        # pipe, or authenticated loopback TCP from the descriptor.
        bridge = bridge_request(
            endpoint,
            bridge_token,
            {"request": request},
            timeout=5.0,
        )
        return response(request, bool(bridge.get("ok")), bridge.get("health", "available"), bridge.get("payload", {}), bridge.get("error"))
    except (OSError, ValueError, RuntimeError, PermissionError) as exc:
        return response(request, False, "unhealthy", error={"code": "bridge_unavailable", "message": str(exc)})


serve(handler)
