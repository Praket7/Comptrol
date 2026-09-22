# Comptrol plugin setup for Codex

Comptrol is configured here as a local Codex MCP/plugin. This repository does not connect it to ChatGPT web. Codex launches the local MCP process itself; it does not need a separate HTTP server or platform tunnel.

## Requirements

- Comptrol's local executable available as `comptrol` on the environment PATH.
- For bundled application operations, install the repository release/package so `adapters/` ships with the executable. The plugin opts into only Blender, Resolve, Canva, and presentation adapter intents.

The plugin starts `comptrol mcp` over stdio and opts into creative app adapters, app launching, and the native macOS Accessibility route. macOS still requires the user to grant Accessibility access in System Settings. The runtime does not opt into email, messaging, or recording adapters. On first use, inspect `capabilities` and verify the actual local machine and detected adapter routes.

For PowerPoint file editing, install its optional open-source Python dependency into the adapter environment from the repository root:

```sh
python3 -m venv ~/.comptrol/venv
~/.comptrol/venv/bin/python -m pip install -r adapters/powerpoint/requirements.txt
```

The runtime automatically uses `~/.comptrol/venv` for isolated Python adapters when present.

The `comptrol` executable must be visible to the Codex app's process PATH. If it is installed outside PATH, set the `command` field in `.mcp.json` to its absolute path. Restart Codex after updating the plugin/runtime so it reloads the MCP process.
