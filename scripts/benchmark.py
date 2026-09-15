#!/usr/bin/env python3
"""Measure local MCP transport and verified task execution without external services."""

import argparse
import json
import os
import pathlib
import statistics
import subprocess
import tempfile
import time


def percentile(values, fraction):
    ordered = sorted(values)
    index = min(len(ordered) - 1, int((len(ordered) - 1) * fraction))
    return ordered[index]


def read_response(process, request_id):
    while True:
        line = process.stdout.readline()
        if not line:
            raise RuntimeError("MCP process exited before the response")
        response = json.loads(line)
        if response.get("id") == request_id:
            return response


def send(process, request):
    encoded = json.dumps(request, separators=(",", ":"))
    process.stdin.write(encoded + "\n")
    process.stdin.flush()
    return encoded, read_response(process, request["id"])


def start_process(binary, state_dir):
    environment = {**os.environ, "COMPTROL_STATE_DIR": state_dir}
    return subprocess.Popen(
        [str(pathlib.Path(binary)), "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env=environment,
    )


def run_transport(process, iterations):
    _, initialized = send(
        process,
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
    )
    if "result" not in initialized:
        raise RuntimeError(f"initialize failed: {initialized}")
    samples = []
    bytes_in = bytes_out = 0
    for index in range(iterations):
        request = {"jsonrpc": "2.0", "id": index + 2, "method": "ping"}
        started = time.perf_counter()
        encoded, response = send(process, request)
        samples.append((time.perf_counter() - started) * 1000)
        bytes_in += len(encoded.encode())
        bytes_out += len(json.dumps(response, separators=(",", ":")).encode())
    return {
        "suite": "transport",
        "iterations": len(samples),
        "p50_ms": round(statistics.median(samples), 3),
        "p95_ms": round(percentile(samples, 0.95), 3),
        "min_ms": round(min(samples), 3),
        "max_ms": round(max(samples), 3),
        "bytes_in": bytes_in,
        "bytes_out": bytes_out,
    }


def run_verified_task(process):
    _, initialized = send(
        process,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "capabilities": {"tasks": {"requests": {"tools": {"call": {}}}}}
            },
        },
    )
    if "result" not in initialized:
        raise RuntimeError(f"initialize failed: {initialized}")
    request = {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "operate",
            "arguments": {"intent": "system.ping"},
            "task": {},
        },
    }
    started = time.perf_counter()
    encoded_request, submitted = send(process, request)
    submit_ms = (time.perf_counter() - started) * 1000
    task = submitted.get("result", {}).get("task")
    if not task or task.get("status") != "queued":
        raise RuntimeError(f"task was not queued: {submitted}")
    task_id = task["taskId"]
    calls = 1
    bytes_in = len(encoded_request.encode())
    bytes_out = len(json.dumps(submitted, separators=(",", ":")).encode())
    observed = None
    for poll in range(1, 81):
        poll_request = {
            "jsonrpc": "2.0",
            "id": 100 + poll,
            "method": "tasks/get",
            "params": {"taskId": task_id},
        }
        encoded, response = send(process, poll_request)
        calls += 1
        bytes_in += len(encoded.encode())
        bytes_out += len(json.dumps(response, separators=(",", ":")).encode())
        observed = response.get("result", {})
        if observed.get("status") == "completed":
            break
        time.sleep(0.025)
    if not observed or observed.get("status") != "completed":
        raise RuntimeError(f"task did not complete: {observed}")
    result_request = {
        "jsonrpc": "2.0",
        "id": 200,
        "method": "tasks/result",
        "params": {"taskId": task_id},
    }
    encoded, result_response = send(process, result_request)
    calls += 1
    bytes_in += len(encoded.encode())
    bytes_out += len(json.dumps(result_response, separators=(",", ":")).encode())
    structured = result_response.get("result", {}).get("structuredContent", {})
    disturbance = structured.get("disturbance")
    return {
        "suite": "verified_task",
        "verified_success": structured.get("verification") == "verified",
        "verification": structured.get("verification"),
        "submit_ms": round(submit_ms, 3),
        "completion_ms": round((time.perf_counter() - started) * 1000, 3),
        "mcp_calls": calls,
        "steps": 1,
        "bytes_in": bytes_in,
        "bytes_out": bytes_out,
        "retries": 0,
        "disturbance": disturbance,
        "independent_final_verifier": "system.ping response verification",
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iterations", type=int, default=25)
    parser.add_argument("--binary", default="target/debug/comptrol")
    parser.add_argument("--suite", choices=("transport", "verified-task"), default="transport")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="comptrol-bench-") as state_dir:
        process = start_process(args.binary, state_dir)
        try:
            result = run_transport(process, args.iterations) if args.suite == "transport" else run_verified_task(process)
            print(json.dumps(result, sort_keys=True))
        finally:
            process.terminate()
            process.wait(timeout=5)


if __name__ == "__main__":
    main()

