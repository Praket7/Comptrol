#!/usr/bin/env python3
"""Exercise the built Comptrol MCP binary against real Windows apps.

The probe uses a temporary Comptrol state directory, leaves existing user data
alone, disables automatic Chrome startup, and launches/reuses only named apps.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import tempfile
import time
from pathlib import Path


def rpc_call(process: subprocess.Popen[str], request_id: int, method: str, params: dict) -> dict:
    assert process.stdin is not None and process.stdout is not None
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    line = process.stdout.readline()
    if not line:
        raise RuntimeError(f"Comptrol exited before replying to {method}; stderr={process.stderr.read() if process.stderr else ''}")
    response = json.loads(line)
    if response.get("id") != request_id:
        raise RuntimeError(f"unexpected MCP response: {response}")
    return response


def unwrap_tool(response: dict) -> dict:
    result = response.get("result", {})
    structured = result.get("structuredContent")
    if isinstance(structured, dict):
        return structured
    for block in result.get("content", []):
        if block.get("type") == "text":
            try:
                return json.loads(block["text"])
            except (KeyError, json.JSONDecodeError):
                continue
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--app", action="append", required=True, help="exact registered app name; repeat for additional apps")
    parser.add_argument("--readiness-timeout-ms", type=int, default=10_000)
    parser.add_argument("--output", type=Path, help="write the full sanitized live report to this JSON file")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    report = {"binary": str(binary), "sha256": digest, "results": []}
    with tempfile.TemporaryDirectory(prefix="comptrol-stage2-live-") as state_dir:
        env = os.environ.copy()
        env.update({
            "COMPTROL_STATE_DIR": state_dir,
            "COMPTROL_ALLOW_APP_LAUNCH": "1",
            "COMPTROL_AUTO_START_CHROME_CDP": "0",
            "COMPTROL_UIA_WORKER_DEADLINE_MS": "3000",
        })
        process = subprocess.Popen(
            [str(binary), "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, encoding="utf-8", bufsize=1, env=env,
        )
        try:
            initialized = rpc_call(process, 1, "initialize", {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": {"name": "comptrol-stage2-live-probe", "version": "1"},
            })
            if "error" in initialized:
                raise RuntimeError(f"MCP initialize failed: {initialized['error']}")
            inventory_response = rpc_call(process, 2, "tools/call", {
                "name": "operate",
                "arguments": {"intent": "app.list", "params": {}, "risk": "R0"},
            })
            inventory = unwrap_tool(inventory_response)
            apps = inventory.get("data", {}).get("apps", [])
            report["matching_registry_entries"] = [
                {key: app.get(key) for key in ("id", "name", "executable", "aumid") if key in app}
                for app in apps
                if str(app.get("name", "")).casefold() in {"calculator", "settings"}
                or "immersivecontrolpanel" in str(app.get("id", "")).casefold()
            ]
            call_id = 3
            for app in args.app:
                calls = []
                for attempt in ("first", "warm"):
                    started = time.perf_counter()
                    response = rpc_call(process, call_id, "tools/call", {
                        "name": "operate",
                        "arguments": {
                            "intent": "app.launch",
                            "params": {
                                "app": app,
                                "instance_policy": "reuse_unique",
                                "readiness_timeout_ms": args.readiness_timeout_ms,
                            },
                            "risk": "R1",
                            "idempotency_key": f"stage2-{os.getpid()}-{app}-{attempt}",
                            "background": "prefer_background",
                        },
                    })
                    elapsed_ms = (time.perf_counter() - started) * 1000
                    result = unwrap_tool(response)
                    calls.append({
                        "attempt": attempt,
                        "wall_ms": round(elapsed_ms, 2),
                        "preflight": result.get("preflight"),
                        "delivery": result.get("delivery"),
                        "effect": result.get("effect"),
                        "verification": result.get("verification"),
                        "route": result.get("route"),
                        "error": result.get("error"),
                        "data": result.get("data"),
                    })
                    call_id += 1
                report["results"].append({"app": app, "calls": calls})
        finally:
            if process.stdin:
                process.stdin.close()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            if process.stderr:
                report["stderr"] = process.stderr.read()[-4000:]
    if args.output:
        output = args.output.resolve()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        summary = {
            "binary": report["binary"],
            "sha256": report["sha256"],
            "results_file": str(output),
            "results": [
                {
                    "app": item["app"],
                    "calls": [
                        {
                            "attempt": call["attempt"],
                            "wall_ms": call["wall_ms"],
                            "route": call["route"],
                            "verification": call["verification"],
                            "readiness": (call.get("data") or {}).get("readiness"),
                        }
                        for call in item["calls"]
                    ],
                }
                for item in report["results"]
            ],
        }
        print(json.dumps(summary, indent=2))
    else:
        print(json.dumps(report, indent=2))
    return 0 if all(
        call.get("verification") == "verified"
        and call.get("data", {}).get("readiness", {}).get("ready") is True
        for item in report["results"] for call in item["calls"]
    ) else 2


if __name__ == "__main__":
    raise SystemExit(main())
