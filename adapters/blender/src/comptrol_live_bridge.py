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
import math

import bpy

_requests = queue.Queue()
_server = None
_endpoint = None
_token = os.environ.get("COMPTROL_BLENDER_BRIDGE_TOKEN", "")


def _reply(connection, request, ok, payload=None, error=None, authenticated=True):
    result = {
        "version": 1,
        "request_id": request.get("request_id"),
        "nonce": request.get("nonce"),
        "authenticated": bool(authenticated),
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
        return {"objects": [{"name": obj.name, "type": obj.type, "location": list(obj.location), "rotation": list(obj.rotation_euler), "scale": list(obj.scale)} for obj in bpy.context.scene.objects], "mode": "live", "verified": True}
    if intent == "blender.scene.object.create":
        name = str(payload.get("name", ""))
        if not name or len(name) > 120 or any(c in name for c in "\r\n\x00"):
            raise ValueError("invalid object name")
        operators = {"cube": "primitive_cube_add", "uv_sphere": "primitive_uv_sphere_add", "ico_sphere": "primitive_ico_sphere_add", "cylinder": "primitive_cylinder_add", "cone": "primitive_cone_add", "torus": "primitive_torus_add", "plane": "primitive_plane_add"}
        shape = payload.get("shape", "cube")
        if shape not in operators:
            raise ValueError("unsupported shape")
        def vector(key, default):
            value = payload.get(key, default)
            if not isinstance(value, list) or len(value) != 3 or any(isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) or abs(v) > 1_000_000 for v in value):
                raise ValueError(key + " must be three finite numbers within +/-1000000")
            return tuple(value)
        getattr(bpy.ops.mesh, operators[shape])(location=vector("location", [0, 0, 0]), rotation=vector("rotation", [0, 0, 0]))
        obj = bpy.context.object
        obj.name = name
        obj.scale = vector("scale", [1, 1, 1])
        if "base_color" in payload:
            rgba = payload["base_color"]
            if not isinstance(rgba, list) or len(rgba) not in (3, 4) or any(isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) or not 0 <= v <= 1 for v in rgba):
                raise ValueError("base_color must contain 3 or 4 channels from 0 to 1")
            rgba = tuple(rgba) if len(rgba) == 4 else (*rgba, 1.0)
            metallic, roughness = payload.get("metallic", 0.0), payload.get("roughness", 0.5)
            for key, value in (("metallic", metallic), ("roughness", roughness)):
                if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or not 0 <= value <= 1:
                    raise ValueError(key + " must be from 0 to 1")
            material = bpy.data.materials.new(name=name + " Material")
            material.diffuse_color = rgba
            material.use_nodes = True
            bsdf = material.node_tree.nodes.get("Principled BSDF")
            bsdf.inputs["Base Color"].default_value = rgba
            bsdf.inputs["Metallic"].default_value = float(metallic)
            bsdf.inputs["Roughness"].default_value = float(roughness)
            obj.data.materials.append(material)
        return {"created": obj.name, "type": obj.type, "location": list(obj.location), "rotation": list(obj.rotation_euler), "scale": list(obj.scale), "verified": True, "mode": "live"}
    if intent == "blender.scene.object.transform":
        name = str(payload.get("name", ""))
        if not name:
            raise ValueError("name is required")
        obj = bpy.data.objects.get(name)
        if obj is None:
            raise ValueError("object missing")
        for key, prop, default in (("location", "location", [0, 0, 0]), ("rotation", "rotation_euler", [0, 0, 0]), ("scale", "scale", [1, 1, 1])):
            if key in payload:
                value = payload[key]
                if not isinstance(value, list) or len(value) != 3 or any(isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) or abs(v) > 1_000_000 for v in value):
                    raise ValueError(key + " must be three finite numbers within +/-1000000")
                setattr(obj, prop, tuple(value))
        if "base_color" in payload:
            rgba = payload["base_color"]
            if not isinstance(rgba, list) or len(rgba) not in (3, 4) or any(isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) or not 0 <= v <= 1 for v in rgba):
                raise ValueError("base_color must contain 3 or 4 channels from 0 to 1")
            rgba = tuple(rgba) if len(rgba) == 4 else (*rgba, 1.0)
            material = bpy.data.materials.get(name + " Material") or bpy.data.materials.new(name=name + " Material")
            material.diffuse_color = rgba
            material.use_nodes = True
            bsdf = material.node_tree.nodes.get("Principled BSDF")
            bsdf.inputs["Base Color"].default_value = rgba
            for key in ("metallic", "roughness"):
                value = payload.get(key, 0.0 if key == "metallic" else 0.5)
                if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or not 0 <= value <= 1:
                    raise ValueError(key + " must be from 0 to 1")
                bsdf.inputs["Metallic" if key == "metallic" else "Roughness"].default_value = float(value)
            obj.data.materials.clear()
            obj.data.materials.append(material)
        return {"name": obj.name, "location": list(obj.location), "rotation": list(obj.rotation_euler), "scale": list(obj.scale), "verified": True, "mode": "live"}
    if intent == "blender.scene.object.delete":
        name = str(payload.get("name", ""))
        obj = bpy.data.objects.get(name)
        if obj is None:
            raise ValueError("object missing")
        bpy.data.objects.remove(obj, do_unlink=True)
        return {"deleted": name, "verified": bpy.data.objects.get(name) is None, "mode": "live"}
    if intent == "blender.project.save":
        path = str(payload.get("path", ""))
        if not path.lower().endswith(".blend"):
            raise ValueError("project save requires a .blend path")
        bpy.ops.wm.save_as_mainfile(filepath=path)
        return {"saved": True, "path": bpy.data.filepath, "mode": "live"}
    if intent == "blender.render":
        output = str(payload.get("output_path", ""))
        if not output.lower().endswith((".png", ".jpg", ".jpeg", ".exr", ".mp4", ".avi", ".mkv")):
            raise ValueError("render output_path must be an image or video artifact")
        frame = payload.get("frame", 1)
        if not isinstance(frame, int) or frame < 0:
            raise ValueError("frame must be a nonnegative integer")
        scene = bpy.context.scene
        scene.render.filepath = output
        scene.render.frame_start = frame
        scene.render.frame_end = frame
        bpy.ops.render.render(write_still=True)
        artifact = bpy.path.abspath(scene.render.filepath)
        import os as _os
        size = _os.path.getsize(artifact) if _os.path.isfile(artifact) else 0
        return {
            "rendered": True,
            "path": artifact,
            "output_size": size,
            "verified": size > 0,
            "verification": "render_artifact_readback",
            "mode": "live",
        }
    raise ValueError("unsupported intent")


def _timer():
    try:
        connection, request = _requests.get_nowait()
    except queue.Empty:
        return 0.05
    try:
        if request.get("token") != _token or not _token:
            _reply(connection, request, False, authenticated=False,
                   error=PermissionError("bridge authentication failed"))
            return 0.01
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
    global _server, _endpoint
    if _server is not None:
        return _endpoint
    path = os.environ.get("COMPTROL_BLENDER_BRIDGE_SOCKET", "")
    if not path or not _token:
        raise RuntimeError("COMPTROL_BLENDER_BRIDGE_SOCKET and COMPTROL_BLENDER_BRIDGE_TOKEN are required")
    bound_tcp = None
    try:
        _server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        _server.bind(path)
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass
        _endpoint = path
    except (OSError, AttributeError):
        # AF_UNIX is unavailable (notably Windows Blender builds): fall
        # back to authenticated loopback TCP, never a wider bind.
        if _server is not None:
            _server.close()
        _server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        _server.bind(("127.0.0.1", 0))
        bound_tcp = _server.getsockname()[1]
        _endpoint = f"tcp:127.0.0.1:{bound_tcp}"
    _server.listen(8)
    _write_descriptor(_endpoint)
    threading.Thread(target=_accept_loop, daemon=True).start()
    bpy.app.timers.register(_timer, first_interval=0.01, persistent=True)
    return _endpoint


def _write_descriptor(endpoint):
    try:
        home = os.environ.get("HOME") or os.environ.get("USERPROFILE") or "."
        directory = os.path.join(home, ".comptrol", "bridges")
        os.makedirs(directory, exist_ok=True)
        descriptor = os.path.join(directory, "blender.json")
        with open(descriptor, "w", encoding="utf-8") as handle:
            # The endpoint only. The token always travels in the environment.
            handle.write(json.dumps({"version": 1, "endpoint": endpoint}))
        try:
            os.chmod(descriptor, 0o600)
        except OSError:
            pass
    except OSError:
        pass


def stop():
    global _server
    if _server is not None:
        _server.close()
        _server = None


if __name__ == "__main__":
    start()
