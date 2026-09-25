#!/usr/bin/env python3
"""Exercise the installed npm launcher through MCP initialize, tools/list, and ping."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path


def call(process: subprocess.Popen[str], message: dict) -> dict:
    assert process.stdin is not None and process.stdout is not None
    process.stdin.write(json.dumps(message) + "\n")
    process.stdin.flush()
    line = process.stdout.readline()
    if not line:
        stderr = process.stderr.read() if process.stderr else ""
        raise RuntimeError(f"npm launcher closed before replying (exit={process.poll()}): {stderr}")
    response = json.loads(line)
    if "error" in response:
        raise RuntimeError(response)
    return response


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    plugin_root = root / "plugins" / "comptrol"
    config = json.loads((plugin_root / "mcp.json").read_text(encoding="utf-8"))
    server = config["mcpServers"]["comptrol-local"]
    node = shutil.which(server["command"])
    if not node:
        raise SystemExit(f"Node.js executable not found: {server['command']}")
    launcher_script = plugin_root / server["args"][0]
    if not launcher_script.is_file():
        raise SystemExit(f"Packaged Comptrol launcher is missing: {launcher_script}")

    with tempfile.TemporaryDirectory(prefix="comptrol-npm-mcp-") as state_dir:
        environment = dict(os.environ, **server["env"])
        environment.update(
            COMPTROL_STATE_DIR=state_dir,
            PLUGIN_ROOT=str(plugin_root),
            PLUGIN_DATA=state_dir,
        )
        process = subprocess.Popen(
            [node, *server["args"]],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            env=environment,
            cwd=plugin_root,
        )
        try:
            initialized = call(
                process,
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {"name": "comptrol-npm-conformance", "version": "1.0"},
                    },
                },
            )
            assert process.stdin is not None
            process.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
            process.stdin.flush()
            tools = call(process, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
            ping = call(
                process,
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {
                        "name": "operate",
                        "arguments": {"intent": "system.ping", "idempotency_key": "npm-launcher-conformance"},
                    },
                },
            )
            names = {tool["name"] for tool in tools["result"]["tools"]}
            required = {"operate", "inspect", "watch", "reconcile", "capabilities"}
            assert initialized["result"]["serverInfo"]["name"] == "comptrol", initialized
            assert initialized["result"]["protocolVersion"] == "2025-06-18", initialized
            assert required <= names, sorted(names)
            assert ping["result"]["structuredContent"]["verification"] == "verified", ping
            print(
                json.dumps(
                    {
                        "launcher": str(launcher_script),
                        "server": initialized["result"]["serverInfo"],
                        "registered_tools": sorted(names),
                        "system_ping": ping["result"]["structuredContent"]["verification"],
                    },
                    sort_keys=True,
                )
            )
        finally:
            if os.name == "nt":
                subprocess.run(
                    ["taskkill.exe", "/PID", str(process.pid), "/T", "/F"],
                    check=False,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
            else:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            time.sleep(0.25)


if __name__ == "__main__":
    main()
