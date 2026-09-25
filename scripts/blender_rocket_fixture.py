#!/usr/bin/env python3
"""Fixture checks for Blender's offline 2D rocket generation recipe."""

import importlib.util
import sys
from pathlib import Path

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
