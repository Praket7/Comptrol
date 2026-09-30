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
import hashlib
from pathlib import Path

import bpy
from mathutils import Vector

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


def _file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source_file:
        for block in iter(lambda: source_file.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _projected_range(obj, axis, evaluated=False):
    target = obj.evaluated_get(bpy.context.evaluated_depsgraph_get()) if evaluated else obj
    points = [target.matrix_world @ vertex.co for vertex in target.data.vertices]
    values = [point.dot(axis) for point in points]
    if not values:
        raise ValueError("rocket part has no mesh vertices: " + obj.name)
    return min(values), max(values)


def _interval_overlap(first, second):
    return min(first[1], second[1]) - max(first[0], second[0])


def _execute(request):
    payload = request.get("payload", {})
    intent = payload.get("intent")
    if intent == "blender.scene.object.list":
        return {"file_path": bpy.data.filepath, "file_saved": bool(bpy.data.filepath), "is_dirty": bool(bpy.data.is_dirty), "scene": bpy.context.scene.name, "frame": bpy.context.scene.frame_current, "objects": [{"name": obj.name, "type": obj.type, "location": list(obj.location), "rotation": list(obj.rotation_euler), "scale": list(obj.scale)} for obj in bpy.context.scene.objects], "mode": "live", "verified": bool(bpy.context.scene)}
    if intent == "blender.scene.copy_2d_rocket_to_3d":
        source = bpy.data.filepath
        if not source or Path(source).suffix.lower() != ".blend" or not os.path.isfile(source):
            raise ValueError("the active Blender document must be a saved .blend file")
        if bpy.data.is_dirty:
            raise ValueError("the active Blender document has unsaved changes; save it before making a separate 3D copy")
        output = str(Path(str(payload.get("output_path", ""))).expanduser().resolve())
        if Path(output).suffix.lower() != ".blend" or os.path.normcase(output) == os.path.normcase(os.path.abspath(source)):
            raise ValueError("3D conversion requires a separate .blend output_path")
        if os.path.exists(output):
            raise ValueError("output_path already exists; choose a new copy to preserve saved Blender work")
        if not os.path.isdir(os.path.dirname(output)):
            raise ValueError("output_path parent directory must already exist")
        depth = payload.get("depth", 0.35)
        if isinstance(depth, bool) or not isinstance(depth, (int, float)) or not math.isfinite(depth) or not 0.02 <= depth <= 10:
            raise ValueError("depth must be a finite number from 0.02 to 10")
        source_hash = _file_sha256(source)
        body = bpy.data.objects.get("Rocket Body")
        flames = [bpy.data.objects.get(name) for name in ("Flame Outer", "Flame Inner")]
        if body is None or any(flame is None for flame in flames):
            raise ValueError("source must contain Rocket Body, Flame Outer, and Flame Inner")
        rotation = body.matrix_world.to_3x3()
        axis_y = (rotation @ Vector((0.0, 1.0, 0.0))).normalized()
        axis_x = (rotation @ Vector((1.0, 0.0, 0.0))).normalized()
        axis_z = (rotation @ Vector((0.0, 0.0, 1.0))).normalized()
        epsilon = max(float(depth) * 0.025, 0.005)
        body_base = _projected_range(body, axis_y)[0]
        body_x = _projected_range(body, axis_x)
        body_z = _projected_range(body, axis_z)
        before, after = [], []
        for flame in flames:
            flame_normal = (flame.matrix_world.to_3x3() @ Vector((0.0, 0.0, 1.0))).normalized()
            if abs(flame_normal.dot(axis_z)) < 0.99:
                raise ValueError("flame plane is not aligned with the rocket body")
            flame_y = _projected_range(flame, axis_y)
            flame_x = _projected_range(flame, axis_x)
            flame_z = _projected_range(flame, axis_z)
            gap = max(0.0, body_base - flame_y[1])
            x_overlap = _interval_overlap(body_x, flame_x)
            if x_overlap <= 0:
                raise ValueError("flame does not overlap the rocket body width: " + flame.name)
            shift = gap + epsilon
            world = flame.matrix_world.copy()
            world.translation = world.translation + axis_y * shift
            flame.matrix_world = world
            penetration = _projected_range(flame, axis_y)[1] - body_base
            depth_overlap = _interval_overlap((body_z[0] - depth / 2, body_z[1] + depth / 2), (flame_z[0] - depth / 2, flame_z[1] + depth / 2))
            before.append({"part": flame.name, "gap": round(gap, 6)})
            after.append({"part": flame.name, "penetration": round(penetration, 6), "x_overlap": round(x_overlap, 6), "depth_overlap": round(depth_overlap, 6), "verified": penetration >= epsilon - 1e-6 and x_overlap > 0 and depth_overlap > 0})
        if not all(item["verified"] for item in after):
            raise ValueError("flame/body contact verification failed")
        converted = []
        for obj in bpy.context.scene.objects:
            if obj.type != "MESH" or obj.name == "Background":
                continue
            modifier = obj.modifiers.new(name="3D Depth", type="SOLIDIFY")
            modifier.thickness = float(depth)
            modifier.offset = 0.0
            modifier.use_rim = True
            converted.append(obj.name)
        if len(converted) < 3:
            raise ValueError("source project contains too few rocket meshes to convert")
        for flame in flames:
            flame["rocket_body_contact_verified"] = True
        bpy.ops.wm.save_as_mainfile(filepath=output)
        bpy.ops.wm.open_mainfile(filepath=output)
        body = bpy.data.objects.get("Rocket Body")
        flames = [bpy.data.objects.get(name) for name in ("Flame Outer", "Flame Inner")]
        if body is None or any(flame is None for flame in flames) or os.path.normcase(bpy.data.filepath) != os.path.normcase(output):
            raise ValueError("saved 3D copy did not reopen as the active Blender document")
        rotation = body.matrix_world.to_3x3()
        axis_y = (rotation @ Vector((0.0, 1.0, 0.0))).normalized()
        axis_x = (rotation @ Vector((1.0, 0.0, 0.0))).normalized()
        axis_z = (rotation @ Vector((0.0, 0.0, 1.0))).normalized()
        reopened = []
        for flame in flames:
            y_overlap = _projected_range(flame, axis_y, True)[1] - _projected_range(body, axis_y, True)[0]
            x_overlap = _interval_overlap(_projected_range(body, axis_x, True), _projected_range(flame, axis_x, True))
            z_overlap = _interval_overlap(_projected_range(body, axis_z, True), _projected_range(flame, axis_z, True))
            verified = y_overlap >= epsilon - 1e-6 and x_overlap > 0 and z_overlap >= -1e-6 and flame.get("rocket_body_contact_verified") is True
            reopened.append({"part": flame.name, "axial_overlap": round(y_overlap, 6), "x_overlap": round(x_overlap, 6), "depth_overlap": round(z_overlap, 6), "verified": verified})
        output_size = os.path.getsize(output) if os.path.isfile(output) else 0
        source_unchanged = os.path.isfile(source) and _file_sha256(source) == source_hash
        verified = output_size > 0 and source_unchanged and all(item["verified"] for item in reopened) and len(converted) >= 3
        return {"source_path": source, "source_sha256": source_hash, "source_unchanged": source_unchanged, "file_path": bpy.data.filepath, "output_path": output, "output_size": output_size, "depth": float(depth), "extruded_objects": converted, "contacts_before": before, "contacts_after": after, "reopened_contacts": reopened, "saved_copy_reopened": True, "verified": verified, "verification": "live_blender_reopen_geometry_readback", "mode": "live"}
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
        state_dir = os.environ.get("COMPTROL_STATE_DIR") or os.path.join(home, ".comptrol")
        directory = os.path.join(state_dir, "bridges")
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
