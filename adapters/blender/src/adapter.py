#!/usr/bin/env python3
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402
from adapter_ipc import bridge_request, resolve_endpoint  # noqa: E402


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
    if intent == "blender.render":
        output = Path(str(payload.get("output_path", ""))).resolve()
        if output.suffix.lower() not in {".png", ".jpg", ".jpeg", ".exr", ".mp4", ".avi", ".mkv"}:
            raise ValueError("render output_path must be an image or video artifact")
        frame = payload.get("frame", 1)
        if not isinstance(frame, int) or frame < 0:
            raise ValueError("frame must be a nonnegative integer")
        return (
            "import bpy, json; "
            f"scene = bpy.context.scene; scene.render.filepath = {str(output)!r}; "
            f"scene.render.frame_start = {frame}; scene.render.frame_end = {frame}; "
            "bpy.ops.render.render(write_still=True); "
            "print(json.dumps({'rendered': True, 'path': bpy.path.abspath(scene.render.filepath)}))"
        )
    raise ValueError("unsupported intent")


def handler(request):
    method = request.get("method")
    live_endpoint, endpoint_source = resolve_endpoint(
        os.environ.get("COMPTROL_BLENDER_BRIDGE_SOCKET"),
        descriptor="blender",
    )
    if method == "handshake":
        modes = ["offline"]
        if live_endpoint and os.environ.get("COMPTROL_BLENDER_BRIDGE_TOKEN"):
            modes.append("live")
        return response(request, True, "available", {"adapter": "comptrol.blender", "modes": modes, "route": "typed_main_thread_bridge_or_exact_file", "endpoint_source": endpoint_source or "none"})
    if method == "capabilities":
        return response(request, True, "available", {"backend": "blender_typed_bridge", "mode": "live_or_offline", "live": bool(live_endpoint)})
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    executable = os.environ.get("COMPTROL_BLENDER_BIN") or shutil.which("blender") or shutil.which("blender.exe")
    if not executable:
        return response(request, False, "unsupported", error={"code": "blender_not_found", "message": "Blender executable is not available"})
    try:
        payload = request.get("payload", {})
        bridge_token = os.environ.get("COMPTROL_BLENDER_BRIDGE_TOKEN")
        if live_endpoint and bridge_token and payload.get("mode", "live") == "live":
            try:
                bridge = bridge_request(
                    live_endpoint,
                    bridge_token,
                    {"request_id": request.get("request_id"), "payload": payload},
                    timeout=5.0,
                )
            except (OSError, ValueError, RuntimeError, PermissionError) as exc:
                return response(request, False, "unhealthy", error={"code": "blender_bridge_unavailable", "message": str(exc)})
            if bridge.get("ok") is not True:
                return response(request, False, "degraded", error=bridge.get("error", {"code": "blender_bridge_rejected", "message": "live bridge rejected request"}))
            payload = bridge.get("payload", {})
            return response(request, True, "available", {**payload, "verified": payload.get("verified") is True, "verification": "blender_bpy_readback", "mode": "live"})
        input_path = Path(str(payload.get("input_path", ""))).resolve()
        if not input_path.is_file() or input_path.suffix.lower() != ".blend":
            return response(request, False, "unsupported", error={"code": "blender_input_required", "message": "Offline Blender operations require an exact existing input_path .blend file"})
        script = script_for(payload)
        with tempfile.NamedTemporaryFile("w", suffix=".py", delete=False, encoding="utf-8") as handle:
            handle.write(script)
            script_path = handle.name
        try:
            completed = subprocess.run([executable, "--background", str(input_path), "--python", script_path], capture_output=True, text=True, timeout=30, check=False)
        finally:
            os.unlink(script_path)
        if completed.returncode != 0:
            return response(request, False, "degraded", error={"code": "blender_failed", "message": completed.stderr[-2000:]})
        data = {"mode": "offline", "input_path": str(input_path), "stdout": completed.stdout[-4000:], "verified_process_exit": True, "live_project_modified": False}
        # Offline verification is artifact-based: the saved or rendered file
        # must exist with nonzero size. Process exit alone is never proof.
        if intent == "blender.project.save":
            saved = Path(str(payload.get("path", ""))).resolve()
            data["output_path"] = str(saved)
            data["output_size"] = saved.stat().st_size if saved.is_file() else 0
            data["verified"] = saved.is_file() and data["output_size"] > 0
            data["verification"] = "blend_file_readback"
        elif intent == "blender.render":
            artifact = Path(str(payload.get("output_path", ""))).resolve()
            data["output_path"] = str(artifact)
            data["output_size"] = artifact.stat().st_size if artifact.is_file() else 0
            data["verified"] = artifact.is_file() and data["output_size"] > 0
            data["verification"] = "render_artifact_readback"
        return response(request, True, "available", data)
    except Exception as exc:
        return response(request, False, "unhealthy", error={"code": "blender_request_failed", "message": str(exc)})


serve(handler)
