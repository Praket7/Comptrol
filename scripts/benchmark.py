#!/usr/bin/env python3
"""Measure warm local MCP ping latency without external services."""

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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iterations", type=int, default=25)
    parser.add_argument("--binary", default="target/debug/comptrol")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="comptrol-bench-") as state_dir:
        environment = {**os.environ, "COMPTROL_STATE_DIR": state_dir}
        process = subprocess.Popen(
            [str(pathlib.Path(args.binary)) , "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            env=environment,
        )
        try:
            process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}) + "\n")
            process.stdin.flush()
            process.stdout.readline()
            samples = []
            for index in range(args.iterations):
                started = time.perf_counter()
                process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": index + 2, "method": "ping"}) + "\n")
                process.stdin.flush()
                response = json.loads(process.stdout.readline())
                if response.get("id") != index + 2:
                    raise RuntimeError("unexpected MCP response")
                samples.append((time.perf_counter() - started) * 1000)
            print(json.dumps({"iterations": len(samples), "p50_ms": round(statistics.median(samples), 3), "p95_ms": round(percentile(samples, 0.95), 3), "min_ms": round(min(samples), 3), "max_ms": round(max(samples), 3)}))
        finally:
            process.terminate()
            process.wait(timeout=5)


if __name__ == "__main__":
    main()

