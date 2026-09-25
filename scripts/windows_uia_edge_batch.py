#!/usr/bin/env python3
"""Run the refusal, stale-target, idempotency, and restart UIA cases."""

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


class McpClient:
    def __init__(self, binary: str, state_dir: pathlib.Path):
        env = os.environ.copy()
        env.update(
            {
                "COMPTROL_ALLOW_WINDOWS_UIA": "1",
                "COMPTROL_AUTO_START_CHROME_CDP": "0",
                "COMPTROL_STATE_DIR": str(state_dir),
            }
        )
        self.process = subprocess.Popen(
            [binary, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
            env=env,
        )
        self.lines: queue.Queue[str] = queue.Queue()
        threading.Thread(target=self._read_lines, daemon=True).start()
        self.request_id = 0
        init = self.request(
            "initialize",
            {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "windows-uia-edge-batch", "version": "1.0"},
            },
        )
        if init.get("error") or init.get("result", {}).get("serverInfo", {}).get("name") != "comptrol":
            raise RuntimeError(f"Comptrol MCP initialization failed: {init}")
        self.server_info = init["result"]["serverInfo"]
        self.notify("notifications/initialized")

    def _read_lines(self) -> None:
        assert self.process.stdout is not None
        for line in self.process.stdout:
            self.lines.put(line)

    def request(self, method: str, params: dict) -> dict:
        self.request_id += 1
        request_id = self.request_id
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                message = json.loads(self.lines.get(timeout=min(0.25, deadline - time.monotonic())))
            except queue.Empty:
                continue
            if message.get("id") == request_id:
                return message
        raise TimeoutError(f"Comptrol did not answer MCP request {request_id}")

    def send(self, message: dict) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message, ensure_ascii=False) + "\n")
        self.process.stdin.flush()

    def notify(self, method: str) -> None:
        self.send({"jsonrpc": "2.0", "method": method})

    def operate(self, intent: str, key: str, params: dict, postcondition: dict | None = None) -> tuple[dict, float]:
        arguments = {"intent": intent, "idempotency_key": key, "params": params}
        if postcondition is not None:
            arguments["postcondition"] = postcondition
        started = time.perf_counter()
        response = self.request("tools/call", {"name": "operate", "arguments": arguments})
        return unpack(response), (time.perf_counter() - started) * 1000

    def close(self) -> None:
        if self.process.poll() is None:
            if self.process.stdin:
                self.process.stdin.close()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                try:
                    self.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)


def unpack(response: dict) -> dict:
    result = response.get("result", {})
    structured = result.get("structuredContent")
    if isinstance(structured, dict):
        return structured
    for item in result.get("content", []):
        text = item.get("text")
        if text:
            try:
                return json.loads(text)
            except json.JSONDecodeError:
                pass
    return {"transport_error": response.get("error") or result}


def launch_fixture(script: pathlib.Path, state_path: pathlib.Path, powershell: str) -> subprocess.Popen[str]:
    state_path.parent.mkdir(parents=True, exist_ok=True)
    return subprocess.Popen(
        [powershell, "-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File", str(script), "-StatePath", str(state_path)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )


def stop_process(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def state(path: pathlib.Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def ready_state() -> dict:
    return {
        "status": "Ready",
        "submission_count": 0,
        "field_value": "",
        "duplicate_one_count": 0,
        "duplicate_two_count": 0,
        "disabled_count": 0,
    }


def submitted_state() -> dict:
    expected = ready_state()
    expected["status"] = "Submitted"
    expected["submission_count"] = 1
    return expected


def refusal_code(result: dict) -> str | None:
    error = result.get("error")
    return error.get("code") if isinstance(error, dict) else None


def trial(root: pathlib.Path, output: pathlib.Path, case: str, label: str, binary: str, powershell: str) -> dict:
    artifacts = output / "artifacts" / label
    artifacts.mkdir(parents=True, exist_ok=True)
    state_dir = artifacts / "runtime-state"
    fixture_script = root / "fixtures" / "windows" / "UIAutomationFixture.ps1"
    fixture_state = artifacts / "fixture-state.json"
    main: subprocess.Popen[str] | None = None
    decoy: subprocess.Popen[str] | None = None
    runtime: McpClient | None = None
    call_latencies: list[float] = []
    start_utc = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    response_summaries: list[dict] = []
    recovery_ms: float | None = None
    try:
        main = launch_fixture(fixture_script, fixture_state, powershell)
        time.sleep(2)
        if main.poll() is not None:
            raise RuntimeError("main fixture exited before action")
        runtime = McpClient(binary, state_dir)
        main_pid = main.pid

        if case == "A04":
            result, latency = runtime.operate(
                "windows.uia.press",
                f"{case}-{label}-{uuid.uuid4().hex}",
                {"process_id": main_pid, "name": "Duplicate", "role": "button"},
            )
            call_latencies.append(latency)
            fixture = state(fixture_state)
            code = refusal_code(result)
            safe = code == "target_ambiguous" and fixture == ready_state()
            response_summaries.append({"result_code": code, "effect": result.get("effect"), "recovery": result.get("recovery")})
            independent = fixture == ready_state()
            success = safe

        elif case == "A05":
            decoy_state = artifacts / "decoy-state.json"
            decoy = launch_fixture(root / "fixtures" / "windows" / "UIAutomationDecoyFixture.ps1", decoy_state, powershell)
            time.sleep(1)
            if decoy.poll() is not None:
                raise RuntimeError("decoy fixture exited before wrong-process test")
            result, latency = runtime.operate(
                "windows.uia.press",
                f"{case}-{label}-{uuid.uuid4().hex}",
                {"process_id": decoy.pid, "name": "Submit", "automation_id": "SubmitButton", "role": "button"},
            )
            call_latencies.append(latency)
            main_state, decoy_value = state(fixture_state), state(decoy_state)
            code = refusal_code(result)
            independent = main_state == ready_state() and decoy_value == {"status": "Ready", "decoy_count": 0}
            success = code in ("target_gone", "target_missing") and independent
            response_summaries.append({"result_code": code, "effect": result.get("effect"), "recovery": result.get("recovery")})

        elif case == "A06":
            result, latency = runtime.operate(
                "windows.uia.press",
                f"{case}-{label}-{uuid.uuid4().hex}",
                {"process_id": main_pid, "name": "Disabled", "automation_id": "DisabledButton", "role": "button"},
            )
            call_latencies.append(latency)
            fixture = state(fixture_state)
            code = refusal_code(result)
            independent = fixture == ready_state()
            success = code in ("not_actionable", "target_gone", "target_missing") and independent
            response_summaries.append({"result_code": code, "effect": result.get("effect"), "recovery": result.get("recovery")})

        elif case == "A07":
            recovery_started = time.perf_counter()
            old_pid = main_pid
            stop_process(main)
            main = launch_fixture(fixture_script, artifacts / "reopened-fixture-state.json", powershell)
            time.sleep(2)
            if main.poll() is not None:
                raise RuntimeError("reopened fixture exited before stale target test")
            new_state_path = artifacts / "reopened-fixture-state.json"
            if main.pid == old_pid:
                raise RuntimeError("operating system reused the stale fixture process ID")
            stale, latency = runtime.operate(
                "windows.uia.press",
                f"{case}-stale-{label}-{uuid.uuid4().hex}",
                {"process_id": old_pid, "name": "Submit", "automation_id": "SubmitButton", "role": "button"},
                {"attribute": "name", "equals": "Submitted"},
            )
            call_latencies.append(latency)
            stale_state = state(new_state_path)
            fresh_value = f"A07-FRESH-{uuid.uuid4().hex[:12]}"
            fresh, latency = runtime.operate(
                "windows.uia.set_value",
                f"{case}-fresh-{label}-{uuid.uuid4().hex}",
                {"process_id": main.pid, "name": "Synthetic value", "automation_id": "SyntheticValue", "role": "edit", "value": fresh_value},
                {"attribute": "value", "equals": fresh_value},
            )
            call_latencies.append(latency)
            recovery_ms = (time.perf_counter() - recovery_started) * 1000
            final_state = state(new_state_path)
            code = refusal_code(stale)
            fresh_expected = ready_state()
            fresh_expected["field_value"] = fresh_value
            independent = stale_state == ready_state() and final_state == fresh_expected
            success = code in ("target_gone", "target_missing") and fresh.get("verification") == "verified" and independent
            response_summaries.extend(
                [
                    {"step": "stale", "result_code": code, "verification": stale.get("verification"), "effect": stale.get("effect")},
                    {"step": "fresh", "route": fresh.get("route"), "verification": fresh.get("verification"), "effect": fresh.get("effect")},
                ]
            )

        elif case == "A08":
            request_key = f"{case}-{label}-{uuid.uuid4().hex}"
            args = {"process_id": main_pid, "name": "Submit", "automation_id": "SubmitButton", "role": "button"}
            first, latency = runtime.operate(
                "windows.uia.press", request_key, args, {"attribute": "name", "equals": "Submitted"}
            )
            call_latencies.append(latency)
            replay, latency = runtime.operate(
                "windows.uia.press", request_key, args, {"attribute": "name", "equals": "Submitted"}
            )
            call_latencies.append(latency)
            fixture = state(fixture_state)
            replay_identified = replay.get("recovery") == "idempotent_replay"
            independent = fixture == submitted_state()
            first_verified = first.get("verification") == "verified" and first.get("effect") == "changed"
            success = first_verified and replay_identified and independent
            response_summaries.extend(
                [
                    {"step": "first", "verification": first.get("verification"), "effect": first.get("effect"), "verified_success": first_verified},
                    {"step": "replay", "verification": replay.get("verification"), "effect": replay.get("effect"), "recovery": replay.get("recovery")},
                ]
            )

        elif case == "A09":
            result, latency = runtime.operate(
                "windows.uia.press",
                f"{case}-{label}-{uuid.uuid4().hex}",
                {"process_id": main_pid, "name": "Submit", "automation_id": "SubmitButton", "role": "button"},
                {"attribute": "name", "equals": "Deliberately-wrong-postcondition"},
            )
            call_latencies.append(latency)
            fixture = state(fixture_state)
            independent = fixture == submitted_state()
            success = result.get("verification") != "verified" and independent
            response_summaries.append({"verification": result.get("verification"), "effect": result.get("effect"), "data": result.get("data")})

        elif case == "A10":
            request_key = f"{case}-{label}-{uuid.uuid4().hex}"
            args = {"process_id": main_pid, "name": "Submit", "automation_id": "SubmitButton", "role": "button"}
            first, latency = runtime.operate(
                "windows.uia.press", request_key, args, {"attribute": "name", "equals": "Submitted"}
            )
            call_latencies.append(latency)
            recovery_started = time.perf_counter()
            runtime.close()
            runtime = None
            runtime = McpClient(binary, state_dir)
            replay, latency = runtime.operate(
                "windows.uia.press", request_key, args, {"attribute": "name", "equals": "Submitted"}
            )
            call_latencies.append(latency)
            recovery_ms = (time.perf_counter() - recovery_started) * 1000
            fixture = state(fixture_state)
            replay_identified = replay.get("recovery") == "idempotent_replay"
            independent = fixture == submitted_state()
            success = replay_identified and independent
            response_summaries.extend(
                [
                    {"step": "before_restart", "verification": first.get("verification"), "effect": first.get("effect")},
                    {"step": "after_restart", "verification": replay.get("verification"), "effect": replay.get("effect"), "recovery": replay.get("recovery")},
                ]
            )
        else:
            raise ValueError(f"unsupported case {case}")

        return {
            "task_id": case,
            "iteration": label,
            "started_utc": start_utc,
            "ended_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "server_version": runtime.server_info.get("version") if runtime else "0.1.67",
            "route": "windows_uia_direct",
            "result_code": response_summaries[-1].get("result_code", "ok"),
            "verification": response_summaries[-1].get("verification"),
            "independent_verifier": independent,
            "safe_refusal": case in ("A04", "A05", "A06", "A07") and success,
            "replay_identified": case in ("A08", "A10") and success,
            "latencies_ms": [round(value, 3) for value in call_latencies],
            "latency_ms": round(max(call_latencies), 3) if call_latencies else None,
            "recovery_ms": round(recovery_ms, 3) if recovery_ms is not None else None,
            "tool_call_count": len(call_latencies),
            "model_turns": 0,
            "retries": 0,
            "recovery_actions": 1 if case == "A07" else (1 if case == "A10" else 0),
            "response_summary": response_summaries,
            "fixture_state_artifact": str(fixture_state),
            "success": bool(success),
        }
    finally:
        if runtime:
            runtime.close()
        stop_process(main)
        stop_process(decoy)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--case", required=True, choices=["A04", "A05", "A06", "A07", "A08", "A09", "A10"])
    parser.add_argument("--iterations", type=int, default=20)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    if platform.system() != "Windows":
        raise SystemExit("These cases require an interactive Windows desktop")
    root = pathlib.Path(__file__).resolve().parents[1]
    powershell = shutil.which("powershell.exe") or shutil.which("powershell")
    if not powershell:
        raise SystemExit("Windows PowerShell is required")
    binary = os.environ.get("COMPTROL_BIN") or str(
        pathlib.Path(os.environ["APPDATA"]) / "npm" / "node_modules" / "comptrolling" / "native" / "win32-x64" / "comptrol.exe"
    )
    if not pathlib.Path(binary).is_file():
        raise SystemExit(f"Comptrol runtime not found: {binary}")
    args.output.mkdir(parents=True, exist_ok=False)
    results = []
    with (args.output / "tasks.jsonl").open("w", encoding="utf-8", newline="\n") as tasks:
        for label in ["warmup"] + [f"measured-{i:02d}" for i in range(1, args.iterations + 1)]:
            try:
                row = trial(root, args.output, args.case, label, binary, powershell)
            except Exception as error:
                row = {"task_id": args.case, "iteration": label, "result_code": type(error).__name__, "error": str(error), "independent_verifier": False, "success": False}
            tasks.write(json.dumps(row, ensure_ascii=False) + "\n")
            tasks.flush()
            if label != "warmup":
                results.append(row)
            print(json.dumps({"iteration": label, "success": row.get("success"), "verification": row.get("verification"), "latency_ms": row.get("latency_ms"), "error": row.get("error")}), flush=True)

    latencies = [latency for row in results for latency in row.get("latencies_ms", [])]
    ordered = sorted(latencies)
    p95 = ordered[max(0, math.ceil(0.95 * len(ordered)) - 1)] if ordered else None
    summary = {
        "task_id": args.case,
        "scope": "direct native Comptrol MCP runtime; not Codex end-to-end",
        "package_version": "0.1.67",
        "warmup_count": 1,
        "measured_trials": len(results),
        "successful_trials": sum(bool(row.get("success")) for row in results),
        "safe_refusals": sum(bool(row.get("safe_refusal")) for row in results),
        "identified_replays": sum(bool(row.get("replay_identified")) for row in results),
        "independent_fixture_successes": sum(bool(row.get("independent_verifier")) for row in results),
        "false_verified_successes": sum(
            any(step.get("verification") == "verified" and not row.get("independent_verifier") for step in row.get("response_summary", []))
            for row in results
        ),
        "runtime_calls": len(latencies),
        "median_runtime_latency_ms": statistics.median(latencies) if latencies else None,
        "p95_runtime_latency_ms_nearest_rank": p95,
        "min_runtime_latency_ms": min(latencies) if latencies else None,
        "max_runtime_latency_ms": max(latencies) if latencies else None,
        "recovery_ms_by_trial": [row["recovery_ms"] for row in results if row.get("recovery_ms") is not None],
        "all_measured_trials": results,
    }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    (args.output / "summary.md").write_text(
        f"# {args.case} Windows UI Automation result\n\n"
        f"- Package: `comptrolling@{summary['package_version']}`\n"
        f"- Scope: {summary['scope']}\n"
        f"- Successful trials: {summary['successful_trials']}/{summary['measured_trials']}\n"
        f"- Safe refusals: {summary['safe_refusals']}/{summary['measured_trials']}\n"
        f"- Identified replays: {summary['identified_replays']}/{summary['measured_trials']}\n"
        f"- Independent fixture success: {summary['independent_fixture_successes']}/{summary['measured_trials']}\n"
        f"- False verified success: {summary['false_verified_successes']}\n"
        f"- Runtime latency, n={len(latencies)}: median {summary['median_runtime_latency_ms']}, p95 {p95}, min {summary['min_runtime_latency_ms']}, max {summary['max_runtime_latency_ms']} ms\n",
        encoding="utf-8",
    )
    return 0 if len(results) == args.iterations and summary["successful_trials"] == args.iterations else 1


if __name__ == "__main__":
    raise SystemExit(main())
