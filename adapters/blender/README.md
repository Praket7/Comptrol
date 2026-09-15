# Blender adapter

The first route is an offline typed-operation runner using Blender's official `--background --python` entry point. It generates a closed script from validated scene operations and never executes model-supplied Python. A live add-on bridge can be added later using the same RPC contract.

