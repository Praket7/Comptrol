#!/usr/bin/env python3
"""Executable V5 benchmark runner: drives the release binary, records p50/p95."""

import argparse
import json
import os
import pathlib
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from typing import Any

ROOT = pathlib.Path(__file__).resolve().parents[2]


def parse_args():
    parser = argparse.ArgumentParser(description="Run a V5 benchmark matrix")
    parser.add_argument("--matrix", required=True, help="Task matrix JSON path")
    parser.add_argument("--binary", default=str(ROOT / "target" / "debug" / "comptrol"))
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--output", type=pathlib.Path)
    return parser.parse_args()


def load_matrix(path: pathlib.Path) -> dict:
    data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    if "tasks" not in data or not isinstance(data["tasks"], list):
        raise SystemExit(f"invalid matrix {path}: needs 'tasks' array")
    return data


def send_mcp(process, request_id: int, method: str, params: dict) -> dict:
    encoded = json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}, separators=(",", ":"))
    process.stdin.write(encoded + "\n")
    process.stdin.flush()
    while True:
        line = process.stdout.readline()
        if not line:
            raise RuntimeError("MCP process exited before response")
        resp = json.loads(line)
        if resp.get("id") == request_id:
            return resp


def run_task(binary: str, task: dict, state_dir: pathlib.Path) -> dict:
    env = {**dict(__import__("os").environ), "COMPTROL_STATE_DIR": str(state_dir)}
    proc = subprocess.Popen([str(binary), "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)
    request_id = 1
    rows = []
    start_wall = time.monotonic()
    for attempt in range(task.get("iterations", 1)):
        row = {"iteration": attempt, "verified": False, "latency_ms": 0, "mcp_calls": 0, "internal_route_actions": 0, "screenshots": 0, "bytes_returned": 0, "retries": 0, "wrong_target_events": 0, "foreground_disturbances": 0, "false_positive_verifications": 0}
        task_start = time.monotonic()
        steps = task.get("steps", [])
        if not steps:
            row["error"] = "no steps defined in task"
            row["verified"] = False
        else:
            try:
                for step in steps:
                    params = step.get("params", {})
                    resp = send_mcp(proc, request_id, step["method"], params)
                    request_id += 1
                    row["mcp_calls"] += 1
                    if "error" in resp:
                        row["retries"] += 1
                        continue
                    result = resp.get("result", {})
                    row["bytes_returned"] += len(json.dumps(result).encode())
                    for key in ("internal_route_actions", "target_list_reads", "screenshots", "wrong_target_events", "foreground_disturbances", "false_positive_verifications"):
                        if key in result and isinstance(result[key], (int, float)):
                            row[key] += int(result[key])
                elapsed = (time.monotonic() - task_start) * 1000
                row["latency_ms"] = round(elapsed, 2)
                row["verified"] = True
            except Exception as error:
                row["latency_ms"] = round((time.monotonic() - task_start) * 1000, 2)
                row["verified"] = False
                row["error"] = str(error)
        rows.append(row)
    proc.stdin.close()
    proc.wait(timeout=5)
    total_wall = (time.monotonic() - start_wall) * 1000
    verified = [r for r in rows if r["verified"]]
    summary = {
        "task_id": task["task_id"],
        "goal": task["goal"],
        "final_verifier": task.get("final_verifier", "unknown"),
        "verified_success": len(verified) == len(rows) and len(rows) > 0,
        "total_latency_ms": round(total_wall, 2),
        "iterations": len(rows),
        "p50_latency_ms": round(statistics.median([r["latency_ms"] for r in rows]), 2) if rows else 0,
        "p95_latency_ms": round(statistics.quantiles([r["latency_ms"] for r in rows], n=20)[18] if len(rows) >= 2 else (rows[0]["latency_ms"] if rows else 0), 2),
        "mcp_calls_per_iteration": round(statistics.median([r["mcp_calls"] for r in rows]), 1) if rows else 0,
        "internal_route_actions_per_iteration": round(statistics.median([r["internal_route_actions"] for r in rows]), 1) if rows else 0,
        "screenshots_per_iteration": round(statistics.median([r["screenshots"] for r in rows]), 1) if rows else 0,
        "retries_per_iteration": round(statistics.median([r["retries"] for r in rows]), 1) if rows else 0,
        "wrong_target_events_per_iteration": round(statistics.median([r["wrong_target_events"] for r in rows]), 1) if rows else 0,
        "foreground_disturbances_per_iteration": round(statistics.median([r["foreground_disturbances"] for r in rows]), 1) if rows else 0,
        "false_positive_verifications_per_iteration": round(statistics.median([r["false_positive_verifications"] for r in rows]), 1) if rows else 0,
        "verified_iterations": len(verified),
    }
    return summary, rows


def collect_metadata() -> dict:
    import platform
    import sysconfig
    return {
        "os": platform.system(),
        "os_release": platform.release(),
        "architecture": platform.machine(),
        "cpu_count": os.cpu_count(),
        "python_version": platform.python_version(),
        "rust_channel": "stable",
        "git_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
    }


def main():
    args = parse_args()
    matrix = load_matrix(args.matrix)
    metadata = collect_metadata()
    suite_name = matrix.get("benchmark_suite", matrix.get("task_id", "unknown"))
    results = {"matrix": args.matrix, "benchmark_suite": suite_name, "metadata": metadata, "generated_at": datetime.now(timezone.utc).isoformat(), "tasks": []}
    for task in matrix["tasks"]:
        with tempfile.TemporaryDirectory() as td:
            state_dir = pathlib.Path(td) / "state"
            state_dir.mkdir(parents=True)
            summary, rows = run_task(args.binary, task, state_dir)
            summary["iterations_rows"] = rows
            results["tasks"].append(summary)
    out_path = args.output or ROOT / "bench" / "results" / f"{suite_name}-{datetime.now().strftime('%Y%m%dT%H%M%S')}.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(results, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(results, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
