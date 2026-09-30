#!/usr/bin/env python3
"""Validate the checked-in portable Comptrol plugin and local marketplace wiring."""

import json
import pathlib
import sys


def load(path):
    with path.open(encoding="utf-8") as stream:
        return json.load(stream)


def fail(message):
    raise SystemExit(f"plugin validation failed: {message}")


def main():
    root = pathlib.Path(__file__).resolve().parents[1]
    plugin = root / "plugins" / "comptrol"
    manifest = load(plugin / "plugin.json")
    mcp = load(plugin / "mcp.json")
    codex_mcp = load(plugin / ".mcp.json")
    compatibility = load(plugin / ".codex-plugin" / "plugin.json")
    marketplace = load(root / ".agents" / "plugins" / "marketplace.json")

    if manifest.get("$schema") != "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json":
        fail("portable manifest schema is missing or unexpected")
    if manifest.get("name") != "comptrol":
        fail("portable plugin name is not comptrol")
    if manifest.get("version") != compatibility.get("version"):
        fail("portable and compatibility versions differ")
    portable_interface = manifest.get("extensions", {}).get("com.openai", {}).get("interface")
    if not isinstance(portable_interface, dict):
        fail("OpenAI interface metadata is missing")
    if compatibility.get("name") != manifest.get("name"):
        fail("portable and compatibility plugin names differ")
    if compatibility.get("description") != manifest.get("description"):
        fail("portable and compatibility descriptions differ")
    if compatibility.get("author") != manifest.get("author"):
        fail("portable and compatibility authors differ")
    if compatibility.get("interface") != portable_interface:
        fail("portable and compatibility OpenAI interfaces differ")
    server = mcp.get("mcpServers", {}).get("comptrol-local")
    if server != {
        "type": "stdio",
        "command": "node",
        "args": ["scripts/launch-comptrol-mcp.cjs"],
        "cwd": "${PLUGIN_ROOT}",
        "env": {
            "COMPTROL_ALLOW_BROWSER_CDP": "1",
            "COMPTROL_ALLOW_WINDOWS_UIA": "1",
            "COMPTROL_ALLOW_APP_LAUNCH": "1",
            "COMPTROL_ALLOW_SETTINGS": "1",
            "COMPTROL_ALLOW_CREATIVE_ADAPTERS": "1",
            "COMPTROL_WINDOWS_UIA": "1",
            "COMPTROL_AUTO_START_CHROME_CDP": "0",
            "COMPTROL_CHROME_AUTO_CONNECT": "1",
        },
    }:
        fail("plugin must declare its stdio runtime and explicit browser, Windows UIA, app-launch, settings, and creative-adapter policies")
    legacy_server = codex_mcp.get("mcpServers", {}).get("comptrol-local")
    portable_launch = {key: value for key, value in server.items() if key != "type"}
    if legacy_server != portable_launch:
        fail("Codex compatibility and portable MCP launch configs differ")
    if mcp.get("$schema") != "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json":
        fail("portable MCP schema is missing or unexpected")
    if server.get("type") != "stdio":
        fail("portable MCP server type must be stdio")
    if not (plugin / "skills" / "comptrol-verified-control" / "SKILL.md").is_file():
        fail("verified control skill is missing")
    if not (plugin / "scripts" / "launch-comptrol-mcp.cjs").is_file():
        fail("cross-platform npm MCP launcher is missing")
    entries = [entry for entry in marketplace.get("plugins", []) if entry.get("name") == "comptrol"]
    if len(entries) != 1:
        fail("marketplace must contain exactly one Comptrol entry")
    entry = entries[0]
    if entry.get("source") != {"source": "local", "path": "./plugins/comptrol"}:
        fail("marketplace source path is not repo relative")
    if entry.get("policy", {}).get("installation") != "AVAILABLE":
        fail("marketplace installation policy is not explicit")
    print(json.dumps({"plugin": manifest["name"], "version": manifest["version"], "mcp_command": server["command"], "mcp_cwd": server["cwd"], "marketplace": True}, sort_keys=True))


if __name__ == "__main__":
    main()
