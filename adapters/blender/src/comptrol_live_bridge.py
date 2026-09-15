"""Typed Comptrol bridge for the already-open Blender process.

Install this file as an add-on or run it from Blender's text editor. The
socket thread only frames authenticated requests; every bpy mutation is
queued and executed by bpy.app.timers on Blender's main thread.
"""
import json
import os
import queue
import socket
import threading

import bpy

_requests = queue.Queue()
_server = None
_token = os.environ.get("COMPTROL_BLENDER_BRIDGE_TOKEN", "")


def _reply(connection, request, ok, payload=None, error=None):
    result = {
        "version": 1,
        "request_id": request.get("request_id"),
        "authenticated": True,
        "ok": ok,
        "payload": payload or {},
    }
    if error:
        result["error"] = {"code": "blender_bridge_request_failed", "message": str(error)}
    connection.sendall((json.dumps(result) + "\n").encode("utf-8"))


def _execute(request):
    payload = request.get("payload", {})
    intent = payload.get("intent")
    if intent == "blender.scene.object.list":
        return {"objects": [obj.name for obj in bpy.context.scene.objects], "mode": "live"}
    if intent == "blender.scene.object.create":
        name = str(payload.get("name", ""))
        if not name or len(name) > 120 or any(c in name for c in "\r\n\x00"):
            raise ValueError("invalid object name")
        bpy.ops.mesh.primitive_cube_add()
        obj = bpy.context.object
        obj.name = name
        return {"created": obj.name, "mode": "live"}
    if intent == "blender.scene.object.transform":
        name = str(payload.get("name", ""))
        location = payload.get("location")
        if not name or not isinstance(location, list) or len(location) != 3:
            raise ValueError("name and three-element location are required")
        if not all(isinstance(value, (int, float)) for value in location):
            raise ValueError("location must be numeric")
        obj = bpy.data.objects.get(name)
        if obj is None:
            raise ValueError("object missing")
        obj.location = tuple(location)
        return {"name": obj.name, "location": list(obj.location), "mode": "live"}
    if intent == "blender.project.save":
        path = str(payload.get("path", ""))
        if not path.lower().endswith(".blend"):
            raise ValueError("project save requires a .blend path")
        bpy.ops.wm.save_as_mainfile(filepath=path)
        return {"saved": True, "path": bpy.data.filepath, "mode": "live"}
    raise ValueError("unsupported intent")


def _timer():
    try:
        connection, request = _requests.get_nowait()
    except queue.Empty:
        return 0.05
    try:
        if request.get("token") != _token or not _token:
            raise PermissionError("bridge authentication failed")
        _reply(connection, request, True, _execute(request))
    except Exception as exc:
        _reply(connection, request, False, error=exc)
    finally:
        connection.close()
    return 0.01


def _accept_loop():
    while _server is not None:
        try:
            connection, _ = _server.accept()
            data = b""
            while not data.endswith(b"\n") and len(data) < 1024 * 1024:
                chunk = connection.recv(65536)
                if not chunk:
                    break
                data += chunk
            if data:
                _requests.put((connection, json.loads(data.decode("utf-8"))))
            else:
                connection.close()
        except OSError:
            break
        except Exception:
            if 'connection' in locals():
                connection.close()


def start():
    global _server
    if _server is not None:
        return
    path = os.environ.get("COMPTROL_BLENDER_BRIDGE_SOCKET", "")
    if not path or not _token:
        raise RuntimeError("COMPTROL_BLENDER_BRIDGE_SOCKET and COMPTROL_BLENDER_BRIDGE_TOKEN are required")
    _server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    _server.bind(path)
    os.chmod(path, 0o600)
    _server.listen(8)
    threading.Thread(target=_accept_loop, daemon=True).start()
    bpy.app.timers.register(_timer, first_interval=0.01, persistent=True)


def stop():
    global _server
    if _server is not None:
        _server.close()
        _server = None


if __name__ == "__main__":
    start()
