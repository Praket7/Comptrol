import json
import os
import subprocess
import tempfile


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
            ["target/debug/comptrol", "mcp"],
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
            assert {"operate", "inspect", "watch", "reconcile", "capabilities"} <= names
            assert ping["result"]["structuredContent"]["verification"] == "verified"
            print(f"{client} MCP contract passed")
        finally:
            process.terminate()
            process.wait(timeout=3)

