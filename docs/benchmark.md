# Comptrol V5 Benchmarks

## Overview

V5 benchmarks are driven by an **executable** runner (`bench/runners/run_matrix.py`) that spawns the release `comptrol` binary over stdio MCP and records per-task latency distributions with p50/p95. Every task is defined as JSON in `bench/tasks/v5/`. Results are written to `bench/results/` with full metadata.

## Running

```bash
python3 bench/runners/run_matrix.py --matrix bench/tasks/v5/browser_matrix.json --binary target/debug/comptrol --iterations 5
python3 bench/runners/run_matrix.py --matrix bench/tasks/v5/adapter_matrix.json --binary target/release/comptrol --iterations 3
```

All scripts must run with `/opt/homebrew/bin/python3.11` **and** `/usr/bin/python3` (3.10; no `tomllib`).

## Task Matrix Contract

Each `bench/tasks/v5/<name>.json` contains:
- `task_id`: unique identifier
- `benchmark_suite`: category
- `goal`: plain-language goal
- `tasks`: array of task objects, each with:
  - `task_id`, `goal`, `method`, `params`, `iterations`, `final_verifier`
  - Optional `skip_reason` for live-platform tasks that need unavailable permissions
- `final_verifier`: overall matrix verification string

## Measurement Scope

Each row records:
- `verified`: whether the independent verifier passed
- `latency_ms`: wall-clock per iteration
- `mcp_calls`, `internal_route_actions`, `screenshots`, `bytes_returned`
- `retries`, `wrong_target_events`, `foreground_disturbances`, `false_positive_verifications`

The runner computes p50/p95 per task over N iterations and writes a full results JSON with per-iteration rows.

## Transport Ping vs Task Performance

The transport ping (`v4.transport.smoke`) is a **connectivity check only**. It must never be presented as task performance. Task performance requires an independent final verifier and a real fixture or live platform.

## Results Schema

Results JSON contains:
- `matrix`, `benchmark_suite`, `metadata` (os, arch, cpu, python, rust, git head)
- `generated_at`
- `tasks`: array of per-task summaries with p50/p95 and full iteration rows

## Matched Comparison

Comparing two runs requires:
1. Same initial state (fresh state directory)
2. Same independent verifier
3. Same iteration count
4. Transport ping excluded from performance claims
