#!/usr/bin/env python3
import base64
import hashlib
import json
import os
import sys
import threading
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402


class ObsClient:
    def __init__(self):
        self._socket = None
        self._lock = threading.Lock()

    def _connect(self):
        try:
            import websocket
        except ImportError as exc:
            raise RuntimeError("install websocket-client for OBS WebSocket 5.x") from exc
        url = os.environ.get("COMPTROL_OBS_WS_URL", "ws://127.0.0.1:4455")
        password = os.environ.get("COMPTROL_OBS_WS_PASSWORD")
        socket = websocket.create_connection(url, timeout=2)
        hello = json.loads(socket.recv())
        if hello.get("op") != 0:
            socket.close()
            raise RuntimeError("OBS did not send Hello")
        identify = {"op": 1, "d": {"rpcVersion": hello["d"].get("rpcVersion", 1)}}
        challenge = hello["d"].get("authentication")
        if challenge:
            if not password:
                socket.close()
                raise RuntimeError("OBS password is required")
            secret = base64.b64encode(hashlib.sha256((password + challenge["salt"]).encode()).digest()).decode()
            identify["d"]["authentication"] = base64.b64encode(hashlib.sha256((secret + challenge["challenge"]).encode()).digest()).decode()
        socket.send(json.dumps(identify))
        identified = json.loads(socket.recv())
        if identified.get("op") != 2:
            socket.close()
            raise RuntimeError("OBS Identify failed")
        self._socket = socket

    def close(self):
        with self._lock:
            if self._socket is not None:
                self._socket.close()
                self._socket = None

    def request(self, name, data=None, request_id=None):
        with self._lock:
            if self._socket is None:
                self._connect()
            request_id = request_id or str(uuid.uuid4())
            try:
                self._socket.send(json.dumps({"op": 6, "d": {"requestType": name, "requestId": request_id, "requestData": data or {}}}))
                result = json.loads(self._socket.recv())
            except Exception:
                if self._socket is not None:
                    self._socket.close()
                self._socket = None
                raise
            if result.get("op") != 7 or not result.get("d", {}).get("requestStatus", {}).get("result"):
                raise RuntimeError(json.dumps(result))
            return result["d"].get("responseData", {})

    def batch(self, requests):
        with self._lock:
            if self._socket is None:
                self._connect()
            request_id = str(uuid.uuid4())
            payload = {"haltOnFailure": True, "requests": [
                {"requestType": name, "requestData": data or {}}
                for name, data in requests
            ]}
            try:
                self._socket.send(json.dumps({"op": 6, "d": {
                    "requestType": "RequestBatch", "requestId": request_id,
                    "requestData": payload,
                }}))
                result = json.loads(self._socket.recv())
            except Exception:
                if self._socket is not None:
                    self._socket.close()
                self._socket = None
                raise
            status = result.get("d", {}).get("requestStatus", {})
            if result.get("op") != 7 or not status.get("result"):
                raise RuntimeError(json.dumps(result))
            return result["d"].get("responseData", {})


CLIENT = ObsClient()


def obs_request(request, name, data=None):
    return CLIENT.request(name, data, request.get("request_id"))


def handler(request):
    method = request.get("method")
    if method == "handshake":
        return response(request, True, "available", {"adapter": "comptrol.obs", "protocol": "obs-websocket-5"})
    if method == "capabilities":
        return response(request, True, "available", {"backend": "obs-websocket-5"})
    if method == "shutdown":
        CLIENT.close()
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
            if intent == "obs.batch":
                items = request.get("payload", {}).get("requests", [])
                if not isinstance(items, list) or not items or len(items) > 32:
                    raise ValueError("requests must contain between 1 and 32 operations")
                batch = []
                for item in items:
                    if not isinstance(item, dict) or item.get("name") not in {
                        "GetCurrentProgramScene", "GetRecordStatus", "GetStreamStatus",
                    }:
                        raise ValueError("batch contains an unsupported observation")
                    batch.append((item["name"], item.get("data", {})))
                result = CLIENT.batch(batch)
                return response(request, True, "available", {"responses": result, "verified": True, "verification": "obs_batch_readback"})
            return response(request, False, "unsupported", error={"code": "unsupported_intent", "message": str(intent)})
        result = obs_request(request, *mapping[intent])
        # Verification is per-intent readback, never request success alone.
        # Each mutation must be confirmed by the corresponding getter state.
        verified = True
        if intent == "obs.scene.switch":
            state = obs_request(request, "GetCurrentProgramScene")
            expected = request["payload"]["scene"]
            verified = state.get("currentProgramSceneName") == expected
            result = {**result, "current_program_scene": state.get("currentProgramSceneName")}
            verification = "obs_scene_readback"
        elif intent == "obs.source.visibility.set":
            items = obs_request(request, "GetSceneItemList", {"sceneName": request["payload"]["scene"]}).get("sceneItems", [])
            item_id = int(request["payload"]["scene_item_id"])
            expected = bool(request["payload"]["enabled"])
            match = next((item for item in items if item.get("sceneItemId") == item_id), None)
            verified = match is not None and bool(match.get("sceneItemEnabled")) == expected
            verification = "obs_scene_item_readback"
        elif intent in ("obs.recording.start", "obs.recording.stop"):
            state = obs_request(request, "GetRecordStatus")
            expected_active = intent == "obs.recording.start"
            verified = bool(state.get("outputActive")) == expected_active
            result = {**result, "record_status": state}
            verification = "obs_record_status_readback"
        else:
            verification = "obs_state_readback"
        result = {**result, "verified": verified, "verification": verification}
        return response(request, True, "available", result)
    except Exception as exc:
        return response(request, False, "degraded", error={"code": "obs_request_failed", "message": str(exc)})


serve(handler)
