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
    compatibility = load(plugin / ".codex-plugin" / "plugin.json")
    marketplace = load(root / ".agents" / "plugins" / "marketplace.json")

    if manifest.get("$schema") != "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json":
        fail("portable manifest schema is missing or unexpected")
    if manifest.get("name") != "comptrol":
        fail("portable plugin name is not comptrol")
    if manifest.get("version") != compatibility.get("version"):
        fail("portable and compatibility versions differ")
    if not isinstance(manifest.get("extensions", {}).get("com.openai", {}).get("interface"), dict):
        fail("OpenAI interface metadata is missing")
    server = mcp.get("mcpServers", {}).get("comptrol-local")
    if server != {"type": "streamable-http", "url": "http://127.0.0.1:7317/mcp"}:
        fail("local Streamable HTTP MCP mapping is not explicit")
    if not (plugin / "skills" / "comptrol-verified-control" / "SKILL.md").is_file():
        fail("verified control skill is missing")
    entries = [entry for entry in marketplace.get("plugins", []) if entry.get("name") == "comptrol"]
    if len(entries) != 1:
        fail("marketplace must contain exactly one Comptrol entry")
    entry = entries[0]
    if entry.get("source") != {"source": "local", "path": "./plugins/comptrol"}:
        fail("marketplace source path is not repo relative")
    if entry.get("policy", {}).get("installation") != "AVAILABLE":
        fail("marketplace installation policy is not explicit")
    print(json.dumps({"plugin": manifest["name"], "version": manifest["version"], "mcp_server": server["url"], "marketplace": True}, sort_keys=True))


if __name__ == "__main__":
    main()
