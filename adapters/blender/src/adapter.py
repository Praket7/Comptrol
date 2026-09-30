#!/usr/bin/env python3
import os
import math
import json
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
    def vector(key, default):
        value = payload.get(key, default)
        if (not isinstance(value, list) or len(value) != 3
                or any(isinstance(v, bool) or not isinstance(v, (int, float))
                       or not math.isfinite(v) or abs(v) > 1_000_000 for v in value)):
            raise ValueError(key + " must be three finite numbers within +/-1000000")
        return tuple(value)

    def color(key):
        value = payload.get(key)
        if (not isinstance(value, list) or len(value) not in (3, 4)
                or any(isinstance(v, bool) or not isinstance(v, (int, float))
                       or not math.isfinite(v) or not 0 <= v <= 1 for v in value)):
            raise ValueError(key + " must contain 3 or 4 color channels from 0 to 1")
        return tuple(value) if len(value) == 4 else (*value, 1.0)

    def project_output():
        raw = payload.get("output_path")
        if not isinstance(raw, str) or not raw.strip() or Path(raw).suffix.lower() != ".blend":
            raise ValueError("offline edits require an explicit .blend output_path")
        return str(Path(raw).resolve())

    if intent == "blender.scene.create_2d_rocket":
        output = Path(str(payload.get("output_path", ""))).resolve()
        if output.suffix.lower() != ".blend":
            raise ValueError("2D rocket creation requires an explicit .blend output_path")
        render = Path(str(payload.get("render_path", output.with_suffix(".png")))).resolve()
        if render.suffix.lower() != ".png":
            raise ValueError("2D rocket render_path must end in .png")
        return f'''import bpy, json
scene = bpy.context.scene
bpy.ops.object.select_all(action="SELECT")
bpy.ops.object.delete(use_global=False)
def material(name, color):
    mat = bpy.data.materials.new(name=name)
    mat.diffuse_color = (*color, 1.0)
    mat.use_nodes = True
    nodes = mat.node_tree.nodes
    nodes.clear()
    emission = nodes.new("ShaderNodeEmission")
    emission.inputs["Color"].default_value = (*color, 1.0)
    output_node = nodes.new("ShaderNodeOutputMaterial")
    mat.node_tree.links.new(emission.outputs["Emission"], output_node.inputs["Surface"])
    return mat
def polygon(name, points, color, z):
    mesh = bpy.data.meshes.new(name + " Mesh")
    mesh.from_pydata([(x, y, z) for x, y in points], [], [tuple(range(len(points)))])
    mesh.materials.append(material(name + " Color", color))
    obj = bpy.data.objects.new(name, mesh)
    scene.collection.objects.link(obj)
    return obj
polygon("Background", [(-5,-5),(5,-5),(5,5),(-5,5)], (0.035,0.075,0.16), -0.2)
polygon("Rocket Body", [(-0.52,-1.45),(0.52,-1.45),(0.52,1.0),(-0.52,1.0)], (0.88,0.92,0.98), 0.0)
polygon("Rocket Nose", [(-0.52,1.0),(0.0,2.1),(0.52,1.0)], (0.96,0.28,0.16), 0.02)
polygon("Left Fin", [(-0.52,-0.75),(-1.08,-1.5),(-0.52,-1.32)], (0.96,0.28,0.16), 0.03)
polygon("Right Fin", [(0.52,-0.75),(1.08,-1.5),(0.52,-1.32)], (0.96,0.28,0.16), 0.03)
polygon("Window Rim", [(-0.31,0.45),(0.0,0.76),(0.31,0.45),(0.31,0.12),(0.0,-0.12),(-0.31,0.12)], (0.05,0.18,0.34), 0.04)
polygon("Window", [(-0.20,0.42),(0.0,0.61),(0.20,0.42),(0.20,0.18),(0.0,0.02),(-0.20,0.18)], (0.18,0.78,0.95), 0.05)
polygon("Flame Outer", [(-0.36,-1.48),(0.0,-2.35),(0.36,-1.48)], (1.0,0.36,0.06), 0.01)
polygon("Flame Inner", [(-0.17,-1.49),(0.0,-2.05),(0.17,-1.49)], (1.0,0.82,0.18), 0.06)
camera_data = bpy.data.cameras.new("Rocket Camera")
camera = bpy.data.objects.new("Rocket Camera", camera_data)
scene.collection.objects.link(camera)
camera.location = (0,0,20)
camera_data.type = "ORTHO"
camera_data.ortho_scale = 10
scene.camera = camera
scene.render.engine = "BLENDER_EEVEE"
scene.render.resolution_x = 1000
scene.render.resolution_y = 1000
scene.render.resolution_percentage = 100
scene.render.image_settings.file_format = "PNG"
scene.render.filepath = {str(render)!r}
scene.world.color = (0.035,0.075,0.16)
bpy.ops.wm.save_as_mainfile(filepath={str(output)!r})
bpy.ops.render.render(write_still=True)
print(json.dumps({{"saved":bpy.data.filepath,"render":scene.render.filepath,"objects":[o.name for o in scene.objects]}}))
'''

    if intent == "blender.scene.copy_2d_rocket_to_3d":
        source = Path(str(payload.get("input_path", ""))).resolve()
        output = Path(str(payload.get("output_path", ""))).resolve()
        if source.suffix.lower() != ".blend" or not source.is_file():
            raise ValueError("3D rocket conversion requires an exact existing input_path .blend file")
        if output.suffix.lower() != ".blend" or output == source:
            raise ValueError("3D rocket conversion requires a different explicit output_path .blend file")
        if output.exists():
            raise ValueError("output_path already exists; choose a new copy so saved Blender work is not overwritten")
        depth = payload.get("depth", 0.35)
        if isinstance(depth, bool) or not isinstance(depth, (int, float)) or not math.isfinite(depth) or not 0.02 <= depth <= 10:
            raise ValueError("depth must be a finite number from 0.02 to 10")
        render_evidence = payload.get("render_evidence", False)
        if not isinstance(render_evidence, bool):
            raise ValueError("render_evidence must be a boolean")
        render_paths = {
            view: output.with_name(output.stem + f".{view}.png")
            for view in ("front", "side", "oblique")
        } if render_evidence else {}
        existing_renders = [str(path) for path in render_paths.values() if path.exists()]
        if existing_renders:
            raise ValueError("evidence render already exists; choose a new output_path: " + ", ".join(existing_renders))
        return f'''import bpy, json
from mathutils import Vector
import os
source = bpy.data.filepath
output = {str(output)!r}
depth = {float(depth)!r}
render_paths = { {view: str(path) for view, path in render_paths.items()}!r}
converted = []
body = bpy.data.objects.get("Rocket Body")
flames = [bpy.data.objects.get(name) for name in ("Flame Outer", "Flame Inner")]
if body is None or any(flame is None for flame in flames):
    raise RuntimeError("source must contain Rocket Body, Flame Outer, and Flame Inner")
rotation = body.matrix_world.to_3x3()
axis_y = (rotation @ Vector((0.0, 1.0, 0.0))).normalized()
axis_x = (rotation @ Vector((1.0, 0.0, 0.0))).normalized()
axis_z = (rotation @ Vector((0.0, 0.0, 1.0))).normalized()
epsilon = max(depth * 0.025, 0.005)
def projected_range(obj, axis, evaluated=False):
    target = obj.evaluated_get(bpy.context.evaluated_depsgraph_get()) if evaluated else obj
    points = [target.matrix_world @ vertex.co for vertex in target.data.vertices]
    values = [point.dot(axis) for point in points]
    if not values:
        raise RuntimeError("rocket part has no mesh vertices: " + obj.name)
    return min(values), max(values)
def overlap(first, second):
    return min(first[1], second[1]) - max(first[0], second[0])
body_base = projected_range(body, axis_y)[0]
body_x = projected_range(body, axis_x)
body_z = projected_range(body, axis_z)
contacts_before = []
contacts_after = []
for flame in flames:
    flame_axis = (flame.matrix_world.to_3x3() @ Vector((0.0, 0.0, 1.0))).normalized()
    if abs(flame_axis.dot(axis_z)) < 0.99:
        raise RuntimeError("flame plane is not aligned with the rocket body")
    flame_y = projected_range(flame, axis_y)
    flame_x = projected_range(flame, axis_x)
    flame_z = projected_range(flame, axis_z)
    gap = max(0.0, body_base - flame_y[1])
    x_overlap = overlap(body_x, flame_x)
    if x_overlap <= 0:
        raise RuntimeError("flame does not overlap the rocket body width: " + flame.name)
    shift = gap + epsilon
    if shift > 0:
        world = flame.matrix_world.copy()
        world.translation = world.translation + axis_y * shift
        flame.matrix_world = world
    after_y = projected_range(flame, axis_y)
    depth_overlap = overlap((body_z[0] - depth / 2, body_z[1] + depth / 2),
                            (flame_z[0] - depth / 2, flame_z[1] + depth / 2))
    penetration = after_y[1] - body_base
    before = {{"body": body.name, "part": flame.name, "gap": round(gap, 6)}}
    after = {{"body": body.name, "part": flame.name, "gap": round(max(0.0, body_base - after_y[1]), 6),
             "penetration": round(penetration, 6), "x_overlap": round(x_overlap, 6),
             "depth_overlap": round(depth_overlap, 6), "verified": penetration >= epsilon - 1e-6 and x_overlap > 0 and depth_overlap > 0}}
    contacts_before.append(before)
    contacts_after.append(after)
if not all(contact["verified"] for contact in contacts_after):
    raise RuntimeError("flame/body contact verification failed: " + json.dumps(contacts_after))
for obj in bpy.context.scene.objects:
    if obj.type != "MESH" or obj.name == "Background":
        continue
    solidify = obj.modifiers.new(name="3D Depth", type="SOLIDIFY")
    solidify.thickness = depth
    solidify.offset = 0.0
    solidify.use_rim = True
    converted.append(obj.name)
if not converted:
    raise RuntimeError("source project contains no rocket mesh objects to extrude")
for flame in flames:
    flame["rocket_body_contact_verified"] = True
bpy.ops.wm.save_as_mainfile(filepath=output)
# Reopen the saved copy before reporting success so readback is from disk.
bpy.ops.wm.open_mainfile(filepath=output)
body = bpy.data.objects.get("Rocket Body")
flames = [bpy.data.objects.get(name) for name in ("Flame Outer", "Flame Inner")]
if body is None or any(flame is None for flame in flames) or bpy.data.filepath != output:
    raise RuntimeError("saved copy failed to reopen with the expected rocket parts")
reopened_contacts = []
for flame in flames:
    y_overlap = projected_range(flame, axis_y, True)[1] - projected_range(body, axis_y, True)[0]
    x_overlap = overlap(projected_range(body, axis_x, True), projected_range(flame, axis_x, True))
    z_overlap = overlap(projected_range(body, axis_z, True), projected_range(flame, axis_z, True))
    verified = y_overlap >= epsilon - 1e-6 and x_overlap > 0 and z_overlap >= -1e-6 and flame.get("rocket_body_contact_verified") is True
    reopened_contacts.append({{"body":body.name,"part":flame.name,"axial_overlap":round(y_overlap,6),"x_overlap":round(x_overlap,6),"z_overlap":round(z_overlap,6),"verified":verified}})
if not all(contact["verified"] for contact in reopened_contacts):
    raise RuntimeError("reopened copy contact readback failed: " + json.dumps(reopened_contacts))
rendered = []
if render_paths:
    scene = bpy.context.scene
    old_camera, old_path = scene.camera, scene.render.filepath
    target = Vector((0.0, 0.0, 0.0))
    views = {{"front":Vector((0.0,0.0,1.0)),"side":Vector((1.0,0.0,0.0)),"oblique":Vector((1.0,-1.0,1.0)).normalized()}}
    for view, direction in views.items():
        camera_data = bpy.data.cameras.new("Comptrol Evidence Camera")
        camera_data.type = "ORTHO"
        camera_data.ortho_scale = 7.0
        camera = bpy.data.objects.new("Comptrol Evidence Camera", camera_data)
        scene.collection.objects.link(camera)
        camera.location = target + direction * 18.0
        camera.rotation_euler = (target - camera.location).to_track_quat("-Z", "Y").to_euler()
        scene.camera = camera
        scene.render.filepath = render_paths[view]
        bpy.ops.render.render(write_still=True)
        size = os.path.getsize(render_paths[view]) if os.path.isfile(render_paths[view]) else 0
        if size <= 0:
            raise RuntimeError("evidence render was not written: " + view)
        rendered.append({{"view":view,"path":render_paths[view],"bytes":size,"verified":True}})
        bpy.data.objects.remove(camera, do_unlink=True)
        bpy.data.cameras.remove(camera_data)
    scene.camera, scene.render.filepath = old_camera, old_path
print(json.dumps({{"source":source,"saved":bpy.data.filepath,"depth":depth,"extruded_objects":converted,"mesh_count":len(converted),"contacts_before":contacts_before,"contacts_after":contacts_after,"reopened_contacts":reopened_contacts,"rendered":rendered,"saved_copy_reopened":True}}))
'''

    if intent == "blender.scene.object.list":
        return "import bpy, json; print(json.dumps({'objects':[{'name':o.name,'type':o.type,'location':list(o.location),'rotation':list(o.rotation_euler),'scale':list(o.scale)} for o in bpy.context.scene.objects]}))"
    if intent == "blender.scene.object.create":
        name = str(payload.get("name", ""))
        if not name or len(name) > 120 or any(char in name for char in "\r\n\x00"):
            raise ValueError("invalid object name")
        shape = payload.get("shape", "cube")
        operators = {"cube": "primitive_cube_add", "uv_sphere": "primitive_uv_sphere_add", "ico_sphere": "primitive_ico_sphere_add", "cylinder": "primitive_cylinder_add", "cone": "primitive_cone_add", "torus": "primitive_torus_add", "plane": "primitive_plane_add"}
        if shape not in operators:
            raise ValueError("shape must be one of " + ", ".join(operators))
        location, rotation, scale = vector("location", [0, 0, 0]), vector("rotation", [0, 0, 0]), vector("scale", [1, 1, 1])
        material = ""
        if "base_color" in payload:
            rgba = color("base_color")
            metallic = payload.get("metallic", 0.0)
            roughness = payload.get("roughness", 0.5)
            for key, value in (("metallic", metallic), ("roughness", roughness)):
                if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or not 0 <= value <= 1:
                    raise ValueError(key + " must be from 0 to 1")
            material = f"m=bpy.data.materials.new(name={name + ' Material'!r}); m.diffuse_color={rgba!r}; m.use_nodes=True; bs=m.node_tree.nodes.get('Principled BSDF'); bs.inputs['Base Color'].default_value={rgba!r}; bs.inputs['Metallic'].default_value={float(metallic)!r}; bs.inputs['Roughness'].default_value={float(roughness)!r}; o.data.materials.append(m); "
        save = f"bpy.ops.wm.save_as_mainfile(filepath={project_output()!r}); "
        return f"import bpy, json; bpy.ops.mesh.{operators[shape]}(location={location!r}, rotation={rotation!r}); o=bpy.context.object; o.name={name!r}; o.scale={scale!r}; {material}{save}print(json.dumps({{'created':o.name,'type':o.type,'location':list(o.location),'rotation':list(o.rotation_euler),'scale':list(o.scale),'saved':bpy.data.filepath}}))"
    if intent == "blender.scene.object.transform":
        name = str(payload.get("name", ""))
        if not name:
            raise ValueError("name is required")
        changes = []
        for key, prop, default in (("location", "location", [0, 0, 0]), ("rotation", "rotation_euler", [0, 0, 0]), ("scale", "scale", [1, 1, 1])):
            if key in payload:
                changes.append(f"o.{prop}={vector(key, default)!r}; ")
        material = ""
        if "base_color" in payload:
            rgba = color("base_color")
            metallic, roughness = payload.get("metallic", 0.0), payload.get("roughness", 0.5)
            for key, value in (("metallic", metallic), ("roughness", roughness)):
                if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or not 0 <= value <= 1:
                    raise ValueError(key + " must be from 0 to 1")
            material = f"m=bpy.data.materials.get({name + ' Material'!r}) or bpy.data.materials.new({name + ' Material'!r}); m.diffuse_color={rgba!r}; m.use_nodes=True; bs=m.node_tree.nodes.get('Principled BSDF'); bs.inputs['Base Color'].default_value={rgba!r}; bs.inputs['Metallic'].default_value={float(metallic)!r}; bs.inputs['Roughness'].default_value={float(roughness)!r}; o.data.materials.clear(); o.data.materials.append(m); "
        if not changes and not material:
            raise ValueError("provide location, rotation, scale, or base_color")
        save = f"bpy.ops.wm.save_as_mainfile(filepath={project_output()!r}); "
        return f"import bpy, json; o=bpy.data.objects.get({name!r}); (_ for _ in ()).throw(RuntimeError('object missing')) if o is None else None; {''.join(changes)}{material}{save}print(json.dumps({{'name':o.name,'location':list(o.location),'rotation':list(o.rotation_euler),'scale':list(o.scale),'saved':bpy.data.filepath}}))"
    if intent == "blender.scene.object.delete":
        name = str(payload.get("name", ""))
        if not name:
            raise ValueError("name is required")
        save = f"bpy.ops.wm.save_as_mainfile(filepath={project_output()!r}); "
        return f"import bpy, json; o=bpy.data.objects.get({name!r}); (_ for _ in ()).throw(RuntimeError('object missing')) if o is None else bpy.data.objects.remove(o, do_unlink=True); {save}print(json.dumps({{'deleted':{name!r},'saved':bpy.data.filepath}}))"
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


def find_blender():
    override = os.environ.get("COMPTROL_BLENDER_BIN", "").strip()
    if override and Path(override).is_file():
        return override
    for name in ("blender", "blender.exe"):
        found = shutil.which(name)
        if found:
            return found
    candidates = [
        Path("/Applications/Blender.app/Contents/MacOS/Blender"),
        Path.home() / "Applications/Blender.app/Contents/MacOS/Blender",
    ]
    if os.name == "nt":
        for base in (os.environ.get("ProgramFiles", "C:/Program Files"), os.environ.get("ProgramW6432", "C:/Program Files")):
            candidates.extend(Path(base).glob("Blender Foundation/Blender */blender.exe"))
    return next((str(path) for path in candidates if path.is_file()), None)


def handler(request):
    method = request.get("method")
    live_endpoint, endpoint_source = resolve_endpoint(
        os.environ.get("COMPTROL_BLENDER_BRIDGE_ENDPOINT"),
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
    executable = find_blender()
    if not executable:
        return response(request, False, "unsupported", error={"code": "blender_not_found", "message": "Blender executable is not available"})
    try:
        payload = request.get("payload", {})
        intent = payload.get("intent")
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
        rocket_recipe = intent == "blender.scene.create_2d_rocket"
        if not rocket_recipe and (not input_path.is_file() or input_path.suffix.lower() != ".blend"):
            return response(request, False, "unsupported", error={"code": "blender_input_required", "message": "Offline Blender operations require an exact existing input_path .blend file"})
        script = script_for(payload)
        with tempfile.NamedTemporaryFile("w", suffix=".py", delete=False, encoding="utf-8") as handle:
            handle.write(script)
            script_path = handle.name
        try:
            command = [executable, "--background"]
            command += ["--factory-startup"] if rocket_recipe else [str(input_path)]
            command += ["--python", script_path]
            completed = subprocess.run(command, capture_output=True, text=True, timeout=120 if rocket_recipe else 30, check=False)
        finally:
            os.unlink(script_path)
        script_readback = '"objects"' in completed.stdout and "Rocket Body" in completed.stdout
        if completed.returncode != 0 or (rocket_recipe and not script_readback):
            return response(request, False, "degraded", error={"code": "blender_failed" if completed.returncode != 0 else "blender_script_not_verified", "message": (completed.stderr + "\n" + completed.stdout)[-3000:]})
        data = {"mode": "offline", "input_path": str(input_path) if not rocket_recipe else None, "stdout": completed.stdout[-4000:], "stderr": completed.stderr[-2000:], "verified_process_exit": True, "live_project_modified": False}
        # Offline verification is artifact-based: the saved or rendered file
        # must exist with nonzero size. Process exit alone is never proof.
        if intent == "blender.project.save":
            saved = Path(str(payload.get("path", ""))).resolve()
            data["output_path"] = str(saved)
            data["output_size"] = saved.stat().st_size if saved.is_file() else 0
            data["verified"] = saved.is_file() and data["output_size"] > 0
            data["verification"] = "blend_file_readback"
        elif intent == "blender.scene.create_2d_rocket":
            saved = Path(str(payload.get("output_path", ""))).resolve()
            render = Path(str(payload.get("render_path", saved.with_suffix(".png")))).resolve()
            data["output_path"] = str(saved)
            data["output_size"] = saved.stat().st_size if saved.is_file() else 0
            data["render_path"] = str(render)
            data["render_size"] = render.stat().st_size if render.is_file() else 0
            data["verified"] = saved.is_file() and data["output_size"] > 0 and render.is_file() and data["render_size"] > 0
            data["verification"] = "blend_and_render_artifact_readback"
        elif intent == "blender.scene.copy_2d_rocket_to_3d":
            saved = Path(str(payload.get("output_path", ""))).resolve()
            data["output_path"] = str(saved)
            data["output_size"] = saved.stat().st_size if saved.is_file() else 0
            report = {}
            for line in reversed(completed.stdout.splitlines()):
                try:
                    candidate = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(candidate, dict) and "extruded_objects" in candidate:
                    report = candidate
                    break
            data["extruded_objects"] = report.get("extruded_objects", [])
            data["depth"] = report.get("depth")
            data["contacts_before"] = report.get("contacts_before", [])
            data["contacts_after"] = report.get("contacts_after", [])
            data["reopened_contacts"] = report.get("reopened_contacts", [])
            data["rendered"] = report.get("rendered", [])
            data["saved_copy_reopened"] = report.get("saved_copy_reopened") is True
            data["verified"] = (saved.is_file() and data["output_size"] > 0
                                and report.get("saved") == str(saved)
                                and len(data["extruded_objects"]) >= 3
                                and isinstance(data["depth"], (int, float))
                                and data["saved_copy_reopened"]
                                and bool(data["reopened_contacts"])
                                and all(contact.get("verified") is True for contact in data["reopened_contacts"])
                                and all(Path(render.get("path", "")).is_file()
                                        and Path(render.get("path", "")).stat().st_size == render.get("bytes")
                                        for render in data["rendered"]))
            data["verification"] = "copied_blend_and_extrusion_readback"
        elif intent == "blender.render":
            artifact = Path(str(payload.get("output_path", ""))).resolve()
            data["output_path"] = str(artifact)
            data["output_size"] = artifact.stat().st_size if artifact.is_file() else 0
            data["verified"] = artifact.is_file() and data["output_size"] > 0
            data["verification"] = "render_artifact_readback"
        elif intent in {"blender.scene.object.create", "blender.scene.object.transform", "blender.scene.object.delete"}:
            artifact = Path(str(payload.get("output_path", ""))).resolve()
            data["output_path"] = str(artifact)
            data["output_size"] = artifact.stat().st_size if artifact.is_file() else 0
            data["verified"] = artifact.is_file() and data["output_size"] > 0
            data["verification"] = "blend_file_readback"
        if data.get("verified") is False:
            return response(request, False, "degraded", data,
                            error={"code": "artifact_verification_failed",
                                   "message": "The requested Blender result did not pass persisted artifact readback"})
        return response(request, True, "available", data)
    except Exception as exc:
        return response(request, False, "unhealthy", error={"code": "blender_request_failed", "message": str(exc)})


serve(handler)
