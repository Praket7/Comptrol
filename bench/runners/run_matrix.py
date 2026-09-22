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
from typing import Any, Callable, Dict, List, Optional

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


# Verifier registry: maps verifier names to callable functions
# Each verifier receives (result_dict, step_params, expected) and returns (bool, str)
VERIFIERS: Dict[str, Callable[[dict, dict, Any], tuple[bool, str]]] = {
    "response_contains": lambda result, _, expected: (
        all(k in result for k in expected) if isinstance(expected, list) else expected in result,
        f"missing keys: {[k for k in (expected if isinstance(expected, list) else [expected]) if k not in result]}"
    ),
    "response_equals": lambda result, _, expected: (
        result == expected,
        f"expected {expected}, got {result}"
    ),
    "response_has_error_code": lambda result, _, expected: (
        result.get("error") == expected,
        f"expected error code {expected}, got {result.get('error')}"
    ),
    "result_has_field": lambda result, _, expected: (
        expected in result.get("result", {}),
        f"result missing field {expected}: {result.get('result', {})}"
    ),
    "result_field_equals": lambda result, _, expected: (
        result.get("result", {}).get(expected[0]) == expected[1],
        f"result.{expected[0]} != {expected[1]}: got {result.get('result', {}).get(expected[0])}"
    ),
    "route_available_or_unavailable": lambda result, _, __: (
        result.get("error") is None or result.get("error") == "route_unavailable",
        f"unexpected error: {result.get('error')}"
    ),
    "contains_provider_metadata": lambda result, _, __: (
        "provider" in result.get("result", {}) or "metadata" in result.get("result", {}),
        f"no provider metadata in result: {result}"
    ),
    "capabilities_non_empty": lambda result, _, __: (
        len(result.get("result", {}).get("capabilities", {})) > 0,
        f"capabilities empty: {result}"
    ),
    "descriptions_non_empty": lambda result, _, __: (
        len(result.get("result", {}).get("descriptions", {})) > 0,
        f"descriptions empty: {result}"
    ),
    "status_compiled": lambda result, _, __: (
        result.get("result", {}).get("status") == "compiled",
        f"status not compiled: {result}"
    ),
    "has_plan_id": lambda result, _, __: (
        "plan_id" in result.get("result", {}),
        f"no plan_id in result: {result}"
    ),
    "has_trace_id": lambda result, _, __: (
        "trace_id" in result.get("result", {}),
        f"no trace_id in result: {result}"
    ),
    "replayed_true_model_turns_zero": lambda result, _, __: (
        result.get("result", {}).get("replayed") is True
        and result.get("result", {}).get("model_turns") == 0,
        f"replayed != True or model_turns != 0: {result}"
    ),
    "executed_true_duplicate_zero": lambda result, _, __: (
        result.get("result", {}).get("executed") is True
        and result.get("result", {}).get("duplicate_count") == 0,
        f"executed != True or duplicate_count != 0: {result}"
    ),
    "cancelled_true_no_mutations": lambda result, _, __: (
        result.get("result", {}).get("cancelled") is True
        and result.get("result", {}).get("mutations_after_cancel", 0) == 0,
        f"cancelled != True or mutations after cancel: {result}"
    ),
    "error_unknown_intent": lambda result, _, __: (
        result.get("error") == "unknown_intent",
        f"expected unknown_intent error: {result}"
    ),
    "session_id_no_reload": lambda result, _, __: (
        "session_id" in result.get("result", {})
        and result.get("result", {}).get("reload_count", 0) == 0,
        f"no session_id or reload_count > 0: {result}"
    ),
    "result_contains_workspaces": lambda result, _, __: (
        "workspaces" in result.get("result", {}) and isinstance(result.get("result", {}).get("workspaces"), list) and len(result.get("result", {}).get("workspaces", [])) > 0,
        f"workspaces not found or empty: {result}"
    ),
    "result_contains_timelines": lambda result, _, __: (
        "timelines" in result.get("result", {}) and isinstance(result.get("result", {}).get("timelines"), list) and len(result.get("result", {}).get("timelines", [])) > 0,
        f"timelines not found or empty: {result}"
    ),
    "navigation_spa": lambda result, _, __: (
        result.get("result", {}).get("current_url", "").endswith("spa.html"),
        f"current_url not spa.html: {result}"
    ),
    "dialog_handled": lambda result, _, __: (
        result.get("result", {}).get("handled") is True,
        f"dialog not handled: {result}"
    ),
    "staged_true_buffer_positive": lambda result, _, __: (
        result.get("result", {}).get("staged") is True
        and result.get("result", {}).get("buffer_size", 0) > 0,
        f"not staged or buffer_size <= 0: {result}"
    ),
    "verified_true_bytes_match": lambda result, _, __: (
        result.get("result", {}).get("verified") is True
        and result.get("result", {}).get("bytes_match") is True,
        f"verified != True or bytes_match != True: {result}"
    ),
    "restore_refused": lambda result, _, __: (
        "error" in result.get("result", {}),
        f"restore not refused: {result}"
    ),
    "reconnected_true": lambda result, _, __: (
        result.get("result", {}).get("reconnected") is True,
        f"not reconnected: {result}"
    ),
    "resolved_true_path_nonempty": lambda result, _, __: (
        result.get("result", {}).get("resolved") is True
        and result.get("result", {}).get("path", ""),
        f"not resolved or empty path: {result}"
    ),
    "apps_array_nonempty": lambda result, _, __: (
        isinstance(result.get("result", {}).get("applications"), list)
        and len(result.get("result", {}).get("applications", [])) > 0,
        f"applications not array or empty: {result}"
    ),
    "has_status_field": lambda result, _, __: (
        "status" in result.get("result", {})
        or "status" in result.get("result", {}).get("structuredContent", {}).get("data", {}),
        f"no status field: {result}"
    ),
    "classification_in_allowed": lambda result, _, __: (
        result.get("result", {}).get("classification") in ["alert", "dialog", "notification", "menu", "informational"]
        or result.get("result", {}).get("structuredContent", {}).get("data", {}).get("popup", {}).get("class") in ["alert", "dialog", "notification", "menu", "informational"],
        f"classification not in allowed: {result}"
    ),
    "has_value": lambda result, _, __: (
        "value" in result.get("result", {}),
        f"no value field: {result}"
    ),
    "mtls_identity_refused": lambda result, _, __: (
        result.get("error") == "mtls_identity_refused",
        f"expected mtls_identity_refused: {result}"
    ),
    "nonce_replay": lambda result, _, __: (
        result.get("error") == "nonce_replay",
        f"expected nonce_replay: {result}"
    ),
}


def run_verifier(verifier_name: str, result: dict, params: dict, expected: Any) -> tuple[bool, str]:
    """Run a named verifier against the result."""
    if verifier_name not in VERIFIERS:
        return False, f"unknown verifier: {verifier_name}"
    try:
        return VERIFIERS[verifier_name](result, params, expected)
    except Exception as e:
        return False, f"verifier {verifier_name} raised: {e}"


def run_task(binary: str, task: dict, state_dir: pathlib.Path, cli_iterations: int) -> tuple[dict, List[dict]]:
    env = {**dict(__import__("os").environ), "COMPTROL_STATE_DIR": str(state_dir), "COMPTROL_ALLOW_ALL_INTENTS": "1", "COMPTROL_ALLOW_BROWSER_CDP": "1", "COMPTROL_ALLOW_BROWSER_LAUNCH": "1", "COMPTROL_ALLOW_ADAPTERS": "1", "COMPTROL_ALLOW_HIGH_CONSEQUENCE_ADAPTERS": "1", "COMPTROL_ALLOW_BROWSER_LAUNCH": "1", "COMPTROL_ALLOW_SOFTWARE_INSTALL": "1", "COMPTROL_ALLOW_SETTINGS": "1", "COMPTROL_ALLOW_SOFTWARE_INSTALL": "1", "COMPTROL_ALLOW_SOFTWARE": "1", "COMPTROL_ALLOW_DESKTOP_NOTIFY": "1", "COMPTROL_ALLOW_MACOS_AX": "1", "COMPTROL_ALLOW_APP_LAUNCH": "1", "COMPTROL_ALLOW_SANDBOX_WRITES": "1", "COMPTROL_ALLOW_DESKTOP_NOTIFY": "1", "COMPTROL_ALLOW_APP_CLOSE": "1", "COMPTROL_ALLOW_SOFTWARE_INSTALL": "1", "COMPTROL_ALLOW_SOFTWARE": "1", "COMPTROL_ALLOW_SOFTWARE_UNINSTALL": "1", "COMPTROL_ALLOW_SOFTWARE_UPDATE": "1", "COMPTROL_ALLOW_SOFTWARE_DESCRIBE": "1", "COMPTROL_ALLOW_SOFTWARE_SEARCH": "1", "COMPTROL_ALLOW_SOFTWARE_INSTALL": "1", "COMPTROL_ALLOW_SETTINGS": "1", "COMPTROL_ALLOW_POPUP": "1", "COMPTROL_ALLOW_COMMANDS": "1", "COMPTROL_ALLOW_WINDOWS_UIA": "1", "COMPTROL_ALLOW_LINUX_ATSPI": "1", "COMPTROL_ALLOW_BROWSER_FIXTURE": "1", "COMPTROL_ALLOW_BROWSER_CDP": "1", "COMPTROL_ALLOW_BROWSER_LAUNCH": "1", "COMPTROL_CDP_ENDPOINT": "http://127.0.0.1:9222"}
    proc = subprocess.Popen([str(binary), "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)
    request_id = 1
    rows = []
    start_wall = time.monotonic()
    iterations = task.get("iterations", cli_iterations)
    for attempt in range(iterations):
        row = {"iteration": attempt, "verified": False, "latency_ms": 0, "mcp_calls": 0, "internal_route_actions": 0, "screenshots": 0, "bytes_returned": 0, "retries": 0, "wrong_target_events": 0, "foreground_disturbances": 0, "false_positive_verifications": 0}
        task_start = time.monotonic()
        steps = task.get("steps", [])
        if not steps:
            row["error"] = "no steps defined in task"
            row["verified"] = False
            rows.append(row)
            continue
        try:
            step_results = []
            for step in steps:
                params = step.get("params", {})
                resp = send_mcp(proc, request_id, step["method"], params)
                request_id += 1
                row["mcp_calls"] += 1
                if "error" in resp:
                    # Error response counts as a failed iteration unless this is an expected-error task
                    expected_error = step.get("expected_error")
                    if expected_error and resp.get("error") == expected_error:
                        pass  # Expected error, continue
                    else:
                        row["retries"] += 1
                        step_results.append({"error": resp.get("error"), "verified": False})
                        continue
                step_results.append(resp)
                result = resp.get("result", {})
                row["bytes_returned"] += len(json.dumps(result).encode())
                for key in ("internal_route_actions", "target_list_reads", "screenshots", "wrong_target_events", "foreground_disturbances", "false_positive_verifications"):
                    if key in result and isinstance(result[key], (int, float)):
                        row[key] += int(result[key])
            
            # Run verifiers on the final step result
            verifier_name = task.get("verifier")
            if verifier_name and step_results:
                final_result = step_results[-1]
                ok, msg = run_verifier(
                    verifier_name,
                    final_result,
                    steps[-1].get("params", {}),
                    task.get("verifier_expected"),
                )
                row["verified"] = ok
                if not ok:
                    row["error"] = f"verifier {verifier_name} failed: {msg}"
            else:
                # No explicit verifier: require all steps succeeded (no errors)
                row["verified"] = all("error" not in sr for sr in step_results)
            
            elapsed = (time.monotonic() - task_start) * 1000
            row["latency_ms"] = round(elapsed, 2)
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
            summary, rows = run_task(args.binary, task, state_dir, args.iterations)
            summary["iterations_rows"] = rows
            results["tasks"].append(summary)
    out_path = args.output or ROOT / "bench" / "results" / f"{suite_name}-{datetime.now().strftime('%Y%m%dT%H%M%S')}.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(results, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(results, indent=2, sort_keys=True))
    if any(not task["verified_success"] for task in results["tasks"]):
        sys.exit(1)


if __name__ == "__main__":
    main()