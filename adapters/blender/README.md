# Blender adapter

The first route is an offline typed-operation runner using Blender's official `--background --python` entry point. It generates a closed script from validated scene operations and never executes model-supplied Python. A live add-on bridge also exists (`comptrol_live_bridge.py`): it runs an authenticated local socket inside the already-open Blender process, queues every bpy mutation through `bpy.app.timers` onto Blender's main thread, and reports live readback with artifact verification for saves and renders. Both routes verify saved or rendered files by existence and nonzero size; process exit alone is never treated as proof.

