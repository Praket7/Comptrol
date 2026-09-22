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
import urllib.request
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


def structured_content(response: dict) -> Any:
    payload = response.get("result", {})
    if isinstance(payload, dict) and "structuredContent" in payload:
        return payload.get("structuredContent")
    return payload


def result_data(response: dict) -> Any:
    structured = structured_content(response)
    if isinstance(structured, dict) and "data" in structured:
        return structured.get("data")
    return structured


def result_error_code(response: dict) -> Optional[str]:
    error = response.get("error")
    if isinstance(error, str):
        return error
    if isinstance(error, dict):
        return error.get("code") or error.get("message")
    structured = structured_content(response)
    if isinstance(structured, dict):
        error = structured.get("error")
        if isinstance(error, str):
            return error
        if isinstance(error, dict):
            return error.get("code") or error.get("message")
    return None


def result_verification(response: dict) -> Optional[str]:
    structured = structured_content(response)
    if isinstance(structured, dict):
        value = structured.get("verification")
        return value if isinstance(value, str) else None
    return None


def dict_data(response: dict) -> dict:
    value = result_data(response)
    return value if isinstance(value, dict) else {}


# Verifier registry: all public MCP tool results normalize through structuredContent.
VERIFIERS: Dict[str, Callable[[dict, dict, Any], tuple[bool, str]]] = {
    "response_contains": lambda result, _, expected: (
        all(k in result for k in expected) if isinstance(expected, list) else expected in result,
        f"missing response keys in {result}",
    ),
    "response_equals": lambda result, _, expected: (
        result == expected,
        f"expected {expected}, got {result}",
    ),
    "response_has_error_code": lambda result, _, expected: (
        result_error_code(result) == expected,
        f"expected error code {expected}, got {result_error_code(result)}: {result}",
    ),
    "result_has_field": lambda result, _, expected: (
        isinstance(result_data(result), dict) and expected in result_data(result),
        f"result data missing field {expected}: {result}",
    ),
    "result_field_equals": lambda result, _, expected: (
        isinstance(result_data(result), dict)
        and result_data(result).get(expected[0]) == expected[1],
        f"result data {expected[0]} != {expected[1]}: {result}",
    ),
    "route_available_or_unavailable": lambda result, _, __: (
        result_error_code(result) in (None, "route_unavailable"),
        f"unexpected error: {result_error_code(result)}: {result}",
    ),
    "contains_provider_metadata": lambda result, _, __: (
        isinstance(result_data(result), dict)
        and ("provider" in result_data(result) or "metadata" in result_data(result)),
        f"no provider metadata in result: {result}",
    ),
    "capabilities_non_empty": lambda result, _, __: (
        bool(structured_content(result)),
        f"capabilities empty: {result}",
    ),
    "descriptions_non_empty": lambda result, _, __: (
        bool(result_data(result)),
        f"descriptions empty: {result}",
    ),
    "status_compiled": lambda result, _, __: (
        dict_data(result).get("status") == "compiled",
        f"status not compiled: {result}",
    ),
    "has_plan_id": lambda result, _, __: (
        "plan_id" in dict_data(result),
        f"no plan_id in result: {result}",
    ),
    "has_trace_id": lambda result, _, __: (
        "trace_id" in dict_data(result),
        f"no trace_id in result: {result}",
    ),
    "replayed_true_model_turns_zero": lambda result, _, __: (
        dict_data(result).get("replayed") is True
        and dict_data(result).get("model_turns") == 0,
        f"replayed != True or model_turns != 0: {result}",
    ),
    "executed_true_duplicate_zero": lambda result, _, __: (
        dict_data(result).get("executed") is True
        and dict_data(result).get("duplicate_count") == 0,
        f"executed != True or duplicate_count != 0: {result}",
    ),
    "cancelled_true_no_mutations": lambda result, _, __: (
        dict_data(result).get("cancelled") is True
        and dict_data(result).get("mutations_after_cancel", 0) == 0,
        f"cancelled != True or mutations after cancel: {result}",
    ),
    "error_unknown_intent": lambda result, _, __: (
        result_error_code(result) in ("unknown_intent", "method_not_found", "unsupported_capability"),
        f"expected unknown-intent error: {result}",
    ),
    "session_id_no_reload": lambda result, _, __: (
        result_verification(result) == "verified"
        and dict_data(result).get("satisfied") is True
        and isinstance(dict_data(result).get("ensure_state"), dict),
        f"ensure_state was not independently verified: {result}",
    ),
    "result_contains_workspaces": lambda result, _, __: (
        isinstance(dict_data(result).get("workspaces"), list)
        and len(dict_data(result).get("workspaces", [])) > 0,
        f"workspaces not found or empty: {result}",
    ),
    "result_contains_timelines": lambda result, _, __: (
        isinstance(dict_data(result).get("timelines"), list)
        and len(dict_data(result).get("timelines", [])) > 0,
        f"timelines not found or empty: {result}",
    ),
    "navigation_spa": lambda result, _, __: (
        str(dict_data(result).get("final_url") or dict_data(result).get("current_url") or "").endswith("spa.html")
        and result_verification(result) == "verified",
        f"final URL not verified as spa.html: {result}",
    ),
    "dialog_handled": lambda result, _, __: (
        result_verification(result) == "verified"
        and dict_data(result).get("verification") == "cdp_dialog_handled",
        f"dialog not verified as handled: {result}",
    ),
    "staged_true_buffer_positive": lambda result, _, __: (
        dict_data(result).get("staged") is True
        and dict_data(result).get("buffer_size", 0) > 0,
        f"not staged or buffer_size <= 0: {result}",
    ),
    "verified_true_bytes_match": lambda result, _, __: (
        dict_data(result).get("verified") is True
        and dict_data(result).get("bytes_match") is True,
        f"verified != True or bytes_match != True: {result}",
    ),
    "restore_refused": lambda result, _, __: (
        result_error_code(result) is not None,
        f"restore not refused: {result}",
    ),
    "reconnected_true": lambda result, _, __: (
        dict_data(result).get("reconnected") is True,
        f"not reconnected: {result}",
    ),
    "resolved_true_path_nonempty": lambda result, _, __: (
        isinstance(dict_data(result).get("app"), dict)
        and bool(dict_data(result)["app"].get("id"))
        and bool(dict_data(result)["app"].get("executable")),
        f"app identity/executable not resolved: {result}",
    ),
    "apps_array_nonempty": lambda result, _, __: (
        isinstance(dict_data(result).get("apps"), list)
        and len(dict_data(result).get("apps", [])) > 0,
        f"apps array empty: {result}",
    ),
    "has_status_field": lambda result, _, __: (
        result_verification(result) == "verified"
        and isinstance(result_data(result), dict)
        and bool(result_data(result).get("platform"))
        and any(
            key in result_data(result)
            for key in (
                "accessibility_trusted",
                "windows_uia_policy",
                "linux_atspi_bus",
                "macos_ax_policy",
            )
        ),
        f"permission observation is not a verified status payload: {result}",
    ),
    "classification_in_allowed": lambda result, _, __: (
        dict_data(result).get("popup", {}).get("class")
        in ["alert", "dialog", "notification", "menu", "informational"],
        f"classification not in allowed set: {result}",
    ),
    "has_value": lambda result, _, __: (
        "value" in dict_data(result),
        f"no value field: {result}",
    ),
    "mtls_identity_refused": lambda result, _, __: (
        result_error_code(result) == "mtls_identity_refused",
        f"expected mtls_identity_refused: {result}",
    ),
    "nonce_replay": lambda result, _, __: (
        result_error_code(result) == "nonce_replay",
        f"expected nonce_replay: {result}",
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


def run_task(
    binary: str,
    task: dict,
    state_dir: pathlib.Path,
    cli_iterations: int,
    extra_env: Optional[dict] = None,
) -> tuple[dict, List[dict]]:
    env = {
        **os.environ,
        "COMPTROL_STATE_DIR": str(state_dir),
        "COMPTROL_ALLOW_ALL_INTENTS": "1",
        "COMPTROL_ALLOW_BROWSER_CDP": "1",
        "COMPTROL_ALLOW_BROWSER_LAUNCH": "1",
        "COMPTROL_ALLOW_ADAPTERS": "1",
        "COMPTROL_ALLOW_HIGH_CONSEQUENCE_ADAPTERS": "1",
        "COMPTROL_ALLOW_SOFTWARE_INSTALL": "1",
        "COMPTROL_ALLOW_SETTINGS": "1",
        "COMPTROL_ALLOW_SOFTWARE": "1",
        "COMPTROL_ALLOW_DESKTOP_NOTIFY": "1",
        "COMPTROL_ALLOW_MACOS_AX": "1",
        "COMPTROL_ALLOW_APP_LAUNCH": "1",
        "COMPTROL_ALLOW_SANDBOX_WRITES": "1",
        "COMPTROL_ALLOW_APP_CLOSE": "1",
        "COMPTROL_ALLOW_SOFTWARE_UNINSTALL": "1",
        "COMPTROL_ALLOW_SOFTWARE_UPDATE": "1",
        "COMPTROL_ALLOW_SOFTWARE_DESCRIBE": "1",
        "COMPTROL_ALLOW_SOFTWARE_SEARCH": "1",
        "COMPTROL_ALLOW_POPUP": "1",
        "COMPTROL_ALLOW_COMMANDS": "1",
        "COMPTROL_ALLOW_WINDOWS_UIA": "1",
        "COMPTROL_ALLOW_LINUX_ATSPI": "1",
        "COMPTROL_ALLOW_BROWSER_FIXTURE": "1",
    }
    env.update(extra_env or {})
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


def wait_for_url(url: str, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                if response.status == 200:
                    return
        except Exception:
            time.sleep(0.05)
    raise RuntimeError(f"timed out waiting for fixture {url}")


def start_suite_fixture(suite_name: str):
    if suite_name != "browser":
        return None, {}
    port = 17417
    process = subprocess.Popen(
        ["node", "scripts/browser_fixture.mjs"],
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        env={**os.environ, "COMPTROL_FIXTURE_PORT": str(port)},
    )
    try:
        wait_for_url(f"http://127.0.0.1:{port}/json/version")
    except Exception:
        process.terminate()
        process.wait(timeout=5)
        raise
    return process, {"COMPTROL_CDP_ENDPOINT": f"http://127.0.0.1:{port}"}


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
    fixture, suite_env = start_suite_fixture(suite_name)
    try:
        for task in matrix["tasks"]:
            with tempfile.TemporaryDirectory() as td:
                state_dir = pathlib.Path(td) / "state"
                state_dir.mkdir(parents=True)
                summary, rows = run_task(
                    args.binary,
                    task,
                    state_dir,
                    args.iterations,
                    suite_env,
                )
                summary["iterations_rows"] = rows
                results["tasks"].append(summary)
    finally:
        if fixture is not None:
            fixture.terminate()
            fixture.wait(timeout=5)
    out_path = args.output or ROOT / "bench" / "results" / f"{suite_name}-{datetime.now().strftime('%Y%m%dT%H%M%S')}.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(results, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(results, indent=2, sort_keys=True))
    if any(not task["verified_success"] for task in results["tasks"]):
        sys.exit(1)


if __name__ == "__main__":
    main()