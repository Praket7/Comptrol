#!/usr/bin/env python3
"""Repeat A02 against the synthetic Windows UI Automation fixture."""

from __future__ import annotations

import argparse
import json
import math
import os
import pathlib
import platform
import queue
import shutil
import statistics
import subprocess
import threading
import time
import uuid


def send(process: subprocess.Popen[str], message: dict) -> None:
    assert process.stdin is not None
    process.stdin.write(json.dumps(message, ensure_ascii=False) + "\n")
    process.stdin.flush()


def response_reader(process: subprocess.Popen[str], lines: queue.Queue[str]) -> None:
    assert process.stdout is not None
    for line in process.stdout:
        lines.put(line)


def receive(lines: queue.Queue[str], request_id: int, timeout: float = 30) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            message = json.loads(lines.get(timeout=min(0.25, deadline - time.monotonic())))
        except queue.Empty:
            continue
        if message.get("id") == request_id:
            return message
    raise TimeoutError(f"Comptrol did not answer MCP request {request_id}")


def run_trial(root: pathlib.Path, output: pathlib.Path, label: str, index: int, case: str) -> dict:
    trial_dir = output / "artifacts" / label
    state_dir = trial_dir / "runtime-state"
    state_dir.mkdir(parents=True, exist_ok=True)
    fixture_state = trial_dir / "fixture-state.json"
    value = f"{case}-SYNTHETIC-{label.upper()}-{uuid.uuid4().hex[:12]}"
    powershell = shutil.which("powershell.exe") or shutil.which("powershell")
    if not powershell:
        raise RuntimeError("Windows PowerShell is required for A02")

    fixture = subprocess.Popen(
        [
            powershell,
            "-NoProfile",
            "-STA",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            str(root / "fixtures" / "windows" / "UIAutomationFixture.ps1"),
            "-StatePath",
            str(fixture_state),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    runtime: subprocess.Popen[str] | None = None
    started_utc = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    try:
        time.sleep(2)
        if fixture.poll() is not None:
            error = fixture.stderr.read() if fixture.stderr else ""
            raise RuntimeError(f"fixture exited before UIA action: {error}")

        env = os.environ.copy()
        env.update(
            {
                "COMPTROL_ALLOW_WINDOWS_UIA": "1",
                "COMPTROL_AUTO_START_CHROME_CDP": "0",
                "COMPTROL_STATE_DIR": str(state_dir),
            }
        )
        binary = env.get("COMPTROL_BIN") or str(
            pathlib.Path(os.environ["APPDATA"])
            / "npm"
            / "node_modules"
            / "comptrolling"
            / "native"
            / "win32-x64"
            / "comptrol.exe"
        )
        if not pathlib.Path(binary).is_file():
            raise FileNotFoundError(f"Comptrol runtime not found: {binary}")
        runtime = subprocess.Popen(
            [binary, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
            env=env,
        )
        lines: queue.Queue[str] = queue.Queue()
        threading.Thread(target=response_reader, args=(runtime, lines), daemon=True).start()
        target = {
            "process_id": fixture.pid,
            "name": "Synthetic value",
            "role": "edit",
            "value": value,
        }
        if case == "A02":
            target["automation_id"] = "SyntheticValue"
        send(
            runtime,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "windows-uia-a02-batch", "version": "1.0"},
                },
            },
        )
        initialized = receive(lines, 1)
        server = initialized.get("result", {}).get("serverInfo", {})
        send(runtime, {"jsonrpc": "2.0", "method": "notifications/initialized"})

        request_started = time.perf_counter()
        send(
            runtime,
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "operate",
                    "arguments": {
                        "intent": "windows.uia.set_value",
                        "idempotency_key": f"{case}-{label}-{fixture.pid}-{uuid.uuid4().hex}",
                        "params": target,
                        "postcondition": {"attribute": "value", "equals": value},
                    },
                },
            },
        )
        response = receive(lines, 2)
        latency_ms = (time.perf_counter() - request_started) * 1000
        structured = response.get("result", {}).get("structuredContent", {})
        independent_state = json.loads(fixture_state.read_text(encoding="utf-8"))
        independent_pass = (
            independent_state.get("field_value") == value
            and independent_state.get("status") == "Ready"
            and independent_state.get("submission_count") == 0
            and independent_state.get("duplicate_one_count") == 0
            and independent_state.get("duplicate_two_count") == 0
            and independent_state.get("disabled_count") == 0
        )
        verified = structured.get("verification") == "verified"
        completed_utc = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        return {
            "task_id": case,
            "iteration": label,
            "started_utc": started_utc,
            "ended_utc": completed_utc,
            "server_version": server.get("version"),
            "route": structured.get("route"),
            "intent": "windows.uia.set_value",
            "target_process_id": fixture.pid,
            "target_name": "Synthetic value",
            "target_automation_id": "SyntheticValue" if case == "A02" else None,
            "result_code": structured.get("error", {}).get("code") if structured.get("error") else "ok",
            "verification": structured.get("verification"),
            "independent_verifier": independent_pass,
            "latency_ms": round(latency_ms, 3),
            "tool_call_count": 1,
            "model_turns": 0,
            "retries": 0,
            "recovery_actions": 0,
            "fixture_state_artifact": str(fixture_state),
            "synthetic_value": value,
            "independent_state": independent_state,
            "success": bool(verified and independent_pass and structured.get("effect") == "changed"),
        }
    finally:
        if runtime is not None:
            if runtime.poll() is None:
                runtime.terminate()
                try:
                    runtime.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    runtime.kill()
                    runtime.wait(timeout=5)
        if fixture.poll() is None:
            fixture.terminate()
            try:
                fixture.wait(timeout=5)
            except subprocess.TimeoutExpired:
                fixture.kill()
                fixture.wait(timeout=5)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--case", choices=["A02", "A03"], default="A02")
    parser.add_argument("--iterations", type=int, default=20)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    if platform.system() != "Windows":
        raise SystemExit("A02/A03 requires an interactive Windows desktop")
    root = pathlib.Path(__file__).resolve().parents[1]
    args.output.mkdir(parents=True, exist_ok=False)
    tasks_path = args.output / "tasks.jsonl"
    results: list[dict] = []
    with tasks_path.open("w", encoding="utf-8", newline="\n") as tasks:
        for label, index in [("warmup", 0)] + [(f"measured-{i:02d}", i) for i in range(1, args.iterations + 1)]:
            try:
                result = run_trial(root, args.output, label, index, args.case)
            except Exception as error:  # preserve each attempted run in the raw log
                result = {
                    "task_id": args.case,
                    "iteration": label,
                    "result_code": type(error).__name__,
                    "error": str(error),
                    "independent_verifier": False,
                    "success": False,
                }
            tasks.write(json.dumps(result, ensure_ascii=False) + "\n")
            tasks.flush()
            if label != "warmup":
                results.append(result)
            print(json.dumps({"iteration": label, "success": result.get("success"), "verification": result.get("verification"), "latency_ms": result.get("latency_ms"), "error": result.get("error")}), flush=True)

    latencies = [float(row["latency_ms"]) for row in results if row.get("latency_ms") is not None]
    ordered = sorted(latencies)
    p95 = ordered[max(0, math.ceil(0.95 * len(ordered)) - 1)] if ordered else None
    summary = {
        "task_id": args.case,
        "scope": "direct native Comptrol MCP runtime; not Codex end-to-end",
        "package_version": "0.1.67",
        "warmup_count": 1,
        "measured_trials": len(results),
        "verified_successes": sum(bool(row.get("success")) for row in results),
        "independent_fixture_successes": sum(bool(row.get("independent_verifier")) for row in results),
        "false_verified_successes": sum(row.get("verification") == "verified" and not row.get("independent_verifier") for row in results),
        "median_runtime_latency_ms": statistics.median(latencies) if latencies else None,
        "p95_runtime_latency_ms_nearest_rank": p95,
        "min_runtime_latency_ms": min(latencies) if latencies else None,
        "max_runtime_latency_ms": max(latencies) if latencies else None,
        "all_measured_trials": results,
    }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    (args.output / "summary.md").write_text(
        f"# {args.case} Windows UI Automation result\n\n"
        f"- Package: `comptrolling@{summary['package_version']}`\n"
        f"- Scope: {summary['scope']}\n"
        f"- Verified success: {summary['verified_successes']}/{summary['measured_trials']}\n"
        f"- Independent fixture success: {summary['independent_fixture_successes']}/{summary['measured_trials']}\n"
        f"- False verified success: {summary['false_verified_successes']}\n"
        f"- Runtime latency, n={len(latencies)}: median {summary['median_runtime_latency_ms']}, p95 {p95}, min {summary['min_runtime_latency_ms']}, max {summary['max_runtime_latency_ms']} ms\n",
        encoding="utf-8",
    )
    return 0 if len(results) == args.iterations and summary["verified_successes"] == args.iterations else 1


if __name__ == "__main__":
    raise SystemExit(main())
