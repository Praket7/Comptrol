#!/usr/bin/env python3
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402


def script_for(payload):
    intent = payload.get("intent")
    if intent == "blender.scene.object.list":
        return "import bpy, json; print(json.dumps({'objects':[o.name for o in bpy.context.scene.objects]}))"
    if intent == "blender.scene.object.create":
        name = str(payload.get("name", ""))
        if not name or len(name) > 120 or any(char in name for char in "\r\n\x00"):
            raise ValueError("invalid object name")
        return f"import bpy, json; bpy.ops.mesh.primitive_cube_add(); o=bpy.context.object; o.name={name!r}; print(json.dumps({{'created':o.name}}))"
    if intent == "blender.scene.object.transform":
        name = str(payload.get("name", ""))
        location = payload.get("location")
        if not name or not isinstance(location, list) or len(location) != 3 or not all(isinstance(value, (int, float)) for value in location):
            raise ValueError("name and numeric three-element location are required")
        return f"import bpy, json; o=bpy.data.objects.get({name!r}); o.location={tuple(location)!r} if o else (_ for _ in ()).throw(RuntimeError('object missing')); print(json.dumps({{'name':o.name,'location':list(o.location)}}))"
    if intent == "blender.project.save":
        path = Path(str(payload.get("path", ""))).resolve()
        if path.suffix.lower() != ".blend":
            raise ValueError("project save requires a .blend path")
        return f"import bpy, json; bpy.ops.wm.save_as_mainfile(filepath={str(path)!r}); print(json.dumps({{'saved':True,'path':str(bpy.data.filepath)}}))"
    raise ValueError("unsupported intent")


def handler(request):
    method = request.get("method")
    if method == "handshake":
        return response(request, True, "available", {"adapter": "comptrol.blender", "route": "typed_background_script"})
    if method == "capabilities":
        return response(request, True, "available", {"backend": "blender_background_python"})
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    executable = os.environ.get("COMPTROL_BLENDER_BIN") or shutil.which("blender") or shutil.which("blender.exe")
    if not executable:
        return response(request, False, "unsupported", error={"code": "blender_not_found", "message": "Blender executable is not available"})
    try:
        script = script_for(request.get("payload", {}))
        with tempfile.NamedTemporaryFile("w", suffix=".py", delete=False, encoding="utf-8") as handle:
            handle.write(script)
            script_path = handle.name
        try:
            completed = subprocess.run([executable, "--background", "--python", script_path], capture_output=True, text=True, timeout=30, check=False)
        finally:
            os.unlink(script_path)
        if completed.returncode != 0:
            return response(request, False, "degraded", error={"code": "blender_failed", "message": completed.stderr[-2000:]})
        return response(request, True, "available", {"stdout": completed.stdout[-4000:], "verified_process_exit": True})
    except Exception as exc:
        return response(request, False, "unhealthy", error={"code": "blender_request_failed", "message": str(exc)})


serve(handler)

