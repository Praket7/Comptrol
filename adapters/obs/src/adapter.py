#!/usr/bin/env python3
import base64
import hashlib
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402


def obs_request(request, name, data=None):
    try:
        import websocket
    except ImportError as exc:
        raise RuntimeError("install websocket-client for OBS WebSocket 5.x") from exc
    url = os.environ.get("COMPTROL_OBS_WS_URL", "ws://127.0.0.1:4455")
    password = os.environ.get("COMPTROL_OBS_WS_PASSWORD")
    socket = websocket.create_connection(url, timeout=2)
    try:
        hello = json.loads(socket.recv())
        if hello.get("op") != 0:
            raise RuntimeError("OBS did not send Hello")
        identify = {"op": 1, "d": {"rpcVersion": hello["d"].get("rpcVersion", 1)}}
        challenge = hello["d"].get("authentication")
        if challenge:
            if not password:
                raise RuntimeError("OBS password is required")
            secret = base64.b64encode(hashlib.sha256((password + challenge["salt"]).encode()).digest()).decode()
            identify["d"]["authentication"] = base64.b64encode(hashlib.sha256((secret + challenge["challenge"]).encode()).digest()).decode()
        socket.send(json.dumps(identify))
        identified = json.loads(socket.recv())
        if identified.get("op") != 2:
            raise RuntimeError("OBS Identify failed")
        request_id = request.get("request_id", "request")
        socket.send(json.dumps({"op": 6, "d": {"requestType": name, "requestId": request_id, "requestData": data or {}}}))
        result = json.loads(socket.recv())
        if result.get("op") != 7 or not result.get("d", {}).get("requestStatus", {}).get("result"):
            raise RuntimeError(json.dumps(result))
        return result["d"].get("responseData", {})
    finally:
        socket.close()


def handler(request):
    method = request.get("method")
    if method == "handshake":
        return response(request, True, "available", {"adapter": "comptrol.obs", "protocol": "obs-websocket-5"})
    if method == "capabilities":
        return response(request, True, "available", {"backend": "obs-websocket-5"})
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    try:
        intent = request.get("payload", {}).get("intent")
        mapping = {
            "obs.scene.list": ("GetSceneList", {}),
            "obs.scene.switch": ("SetCurrentProgramScene", {"sceneName": request["payload"]["scene"]}),
            "obs.source.visibility.set": ("SetSceneItemEnabled", {"sceneName": request["payload"]["scene"], "sceneItemId": int(request["payload"]["scene_item_id"]), "sceneItemEnabled": bool(request["payload"]["enabled"])}),
            "obs.recording.status": ("GetRecordStatus", {}),
            "obs.recording.start": ("StartRecord", {}),
            "obs.recording.stop": ("StopRecord", {}),
        }
        if intent not in mapping:
            return response(request, False, "unsupported", error={"code": "unsupported_intent", "message": str(intent)})
        result = obs_request(request, *mapping[intent])
        if intent.startswith("obs.recording.") and intent != "obs.recording.status":
            result = {**result, "verified_status": obs_request(request, "GetRecordStatus")}
        result = {**result, "verified": True}
        return response(request, True, "available", result)
    except Exception as exc:
        return response(request, False, "degraded", error={"code": "obs_request_failed", "message": str(exc)})


serve(handler)
