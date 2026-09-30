#!/usr/bin/env python3
"""Fixture checks for Blender's offline 2D rocket generation recipe."""

import importlib.util
import json
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "adapters" / "_shared"))

# Import adapter functions without starting the long-lived adapter stdio loop.
import adapter_protocol  # noqa: E402

adapter_protocol.serve = lambda _handler: None
adapter_path = ROOT / "adapters" / "blender" / "src" / "adapter.py"
spec = importlib.util.spec_from_file_location("comptrol_blender_adapter_fixture", adapter_path)
assert spec and spec.loader
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)

out = ROOT / "fixture-output" / "rocket.blend"
script = adapter.script_for(
    {
        "intent": "blender.scene.create_2d_rocket",
        "output_path": str(out),
    }
)
for required in (
    '"Rocket Body"',
    '"Rocket Nose"',
    '"Left Fin"',
    '"Right Fin"',
    '"Window"',
    '"Flame Outer"',
    '"BLENDER_EEVEE"',
    "save_as_mainfile",
    "write_still=True",
):
    assert required in script, f"rocket recipe is missing {required!r}"
assert repr(str(out.resolve())) in script
assert repr(str(out.with_suffix(".png").resolve())) in script

for invalid in ("", "rocket.obj", "rocket.png"):
    try:
        adapter.script_for(
            {
                "intent": "blender.scene.create_2d_rocket",
                "output_path": invalid,
            }
        )
    except ValueError:
        pass
    else:
        raise AssertionError(f"invalid Blender output path should be rejected: {invalid!r}")

print("Blender 2D rocket fixture passed: bounded script, required geometry, saved blend and PNG")

# The preserved 2D rocket has measurable flame/body gaps. Verify that the
# conversion script closes those gaps, reopens the saved copy, and checks the
# evaluated geometry before it may report success.
source = ROOT.parent / "artifacts" / "blender" / "rocket-goal-20260925.blend"
copy_out = ROOT.parent / "work" / "fixture-rocket-copy.blend"
copy_script = adapter.script_for(
    {
        "intent": "blender.scene.copy_2d_rocket_to_3d",
        "input_path": str(source),
        "output_path": str(copy_out),
        "depth": 0.35,
    }
)
for required in (
    'bpy.data.objects.get("Rocket Body")',
    '("Flame Outer", "Flame Inner")',
    "contacts_before",
    "contacts_after",
    "bpy.ops.wm.open_mainfile(filepath=output)",
    "reopened_contacts",
):
    assert required in copy_script, f"3D rocket conversion is missing {required!r}"
assert source.is_file(), "fixture source .blend file is missing"
assert repr(str(copy_out.resolve())) in copy_script

# A failed artifact readback must not be surfaced as an available/successful
# adapter result, even when Blender itself exits with code 0.
fake_report = {
    "saved": str(copy_out.resolve()),
    "depth": 0.35,
    "extruded_objects": ["Rocket Body", "Rocket Nose", "Flame Outer"],
    "saved_copy_reopened": True,
    "reopened_contacts": [{"verified": True}],
    "rendered": [],
}
fake_process = __import__("subprocess").CompletedProcess(
    args=["blender"], returncode=0, stdout=json.dumps(fake_report), stderr=""
)
with patch.object(adapter, "find_blender", return_value="blender-fixture"), patch.object(
    adapter.subprocess, "run", return_value=fake_process
):
    result = adapter.handler(
        {
            "method": "operate",
            "request_id": "fixture-unverified-blender-output",
            "payload": {
                "intent": "blender.scene.copy_2d_rocket_to_3d",
                "input_path": str(source),
                "output_path": str(copy_out),
                "depth": 0.35,
            },
        }
    )
assert result["ok"] is False, "unreadable Blender output must fail the adapter operation"
assert result["health"] == "degraded"
assert result["error"]["code"] == "artifact_verification_failed"

with tempfile.TemporaryDirectory(dir=ROOT.parent / "work") as temp_dir:
    occupied_output = Path(temp_dir) / "occupied.blend"
    occupied_output.write_bytes(b"preserve this output")
    try:
        adapter.script_for(
            {
                "intent": "blender.scene.copy_2d_rocket_to_3d",
                "input_path": str(source),
                "output_path": str(occupied_output),
                "depth": 0.35,
            }
        )
    except ValueError as error:
        assert "already exists" in str(error)
    else:
        raise AssertionError("conversion must refuse to overwrite an existing Blender artifact")

print("Blender 3D conversion fixture passed: gap correction, reopen evidence, fail-closed result and no-overwrite")
