#!/usr/bin/env python3
"""Run a checked-in V4 benchmark task and emit a machine-readable result."""

import argparse
import json
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default=str(ROOT / "target" / "debug" / "comptrol"))
    parser.add_argument("--task", default="v4.transport.smoke")
    parser.add_argument("--iterations", type=int, default=25)
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()
    task_path = ROOT / "bench" / "tasks" / f"{args.task.replace('.', '_')}.json"
    if not task_path.is_file():
        raise SystemExit(f"unknown benchmark task: {args.task}")
    task = json.loads(task_path.read_text(encoding="utf-8"))
    if task.get("task_id") != args.task or not task.get("final_verifier"):
        raise SystemExit(f"invalid benchmark task contract: {task_path}")
    command = [sys.executable, str(ROOT / "scripts" / "benchmark.py"), "--binary", args.binary, "--suite", "transport", "--iterations", str(args.iterations)]
    result = json.loads(subprocess.check_output(command, cwd=ROOT, text=True))
    required_metrics = {
        "measurement_scope",
        "mcp_calls",
        "external_model_turns",
        "internal_route_actions",
        "websocket_handshakes",
        "json_list_requests",
        "screenshots",
        "retries",
        "target_mismatches",
        "duplicate_mutations",
        "disturbance_events",
    }
    missing = sorted(required_metrics.difference(result))
    if missing:
        raise SystemExit(f"benchmark result is missing required metrics: {', '.join(missing)}")
    result.update({"task_id": args.task, "final_verifier": task["final_verifier"], "verified_success": result["iterations"] == args.iterations and not missing})
    encoded = json.dumps(result, sort_keys=True, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")


if __name__ == "__main__":
    main()
