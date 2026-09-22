import json
import os
import subprocess
import tempfile
from pathlib import Path


root = Path(__file__).resolve().parents[1]
binary = os.environ.get("COMPTROL_BIN", str(root / "target" / "debug" / ("comptrol.exe" if os.name == "nt" else "comptrol")))


def call(process, message):
    process.stdin.write(json.dumps(message) + "\n")
    process.stdin.flush()
    line = process.stdout.readline()
    if not line:
        raise RuntimeError("MCP server closed before replying")
    response = json.loads(line)
    if "error" in response:
        raise RuntimeError(response)
    return response


for client in ("codex", "claude-code", "cursor"):
    with tempfile.TemporaryDirectory(prefix="comptrol-client-") as state:
        environment = dict(os.environ, COMPTROL_STATE_DIR=state)
        process = subprocess.Popen(
            [binary, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            env=environment,
        )
        try:
            initialize = call(process, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
            tools = call(process, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
            ping = call(process, {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "operate", "arguments": {"intent": "system.ping", "idempotency_key": client}}})
            names = {tool["name"] for tool in tools["result"]["tools"]}
            assert initialize["result"]["serverInfo"]["name"] == "comptrol"
            assert {"operate", "inspect", "watch", "reconcile", "restore_checkpoint", "capabilities", "human_action.resolve"} <= names
            annotations = {tool["name"]: tool.get("annotations", {}) for tool in tools["result"]["tools"]}
            assert annotations["inspect"].get("readOnlyHint") is True
            assert annotations["watch"].get("readOnlyHint") is True
            assert annotations["capabilities"].get("readOnlyHint") is True
            assert annotations["operate"].get("readOnlyHint") is False
            assert annotations["operate"].get("destructiveHint") is True
            assert annotations["operate"].get("openWorldHint") is True
            assert annotations["restore_checkpoint"].get("destructiveHint") is True
            assert all("readOnlyHint" in value and "destructiveHint" in value and "openWorldHint" in value for value in annotations.values())
            operate_schema = next(tool["inputSchema"] for tool in tools["result"]["tools"] if tool["name"] == "operate")
            assert set(operate_schema["properties"]["background"]["enum"]) == {
                "strict_background",
                "prefer_background",
                "foreground_allowed",
                "foreground_required",
            }
            assert ping["result"]["structuredContent"]["verification"] == "verified"
            print(f"{client} MCP contract passed")
        finally:
            process.terminate()
            process.wait(timeout=3)
