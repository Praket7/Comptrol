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
    return json.loads(line)


with tempfile.TemporaryDirectory(prefix="comptrol-mcp-current-") as state:
    process = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env=dict(os.environ, COMPTROL_STATE_DIR=state, COMPTROL_AUTO_START_CHROME_CDP="0"),
    )
    try:
        response = call(process, {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2026-07-28"},
        })
        assert response["result"]["protocolVersion"] == "2026-07-28", response
        assert response["result"]["comptrol"]["protocol_mode"] == "stateless", response
        print("MCP 2026 current stdio conformance passed")
    finally:
        process.terminate()
        process.wait(timeout=3)
