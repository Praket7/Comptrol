# How Comptrol is checked

Comptrol has local test pages for repeatable browser tasks. The benchmark runner starts a fresh runtime, runs each task, then checks the result independently. A returned tool response alone does not count as success.

## Current browser result

The current local run completed three browser tasks three times each. All nine runs passed. The record includes timing, tool calls, retries, wrong target events, false success reports, foreground changes.

[Open the full result](../bench/results/audit_browser_fixture_20260923.json)

The result file records that the changes were uncommitted at measurement time.

This is a small browser fixture on one macOS ARM64 computer. It does not measure every website, browser profile, operating system, or adapter. It is not a promise about future speed.

## Run the browser check

Build Comptrol, then run the browser fixture check.

```sh
cargo build -p comptrol
node scripts/browser_conformance.mjs
```

The check launches a local test page. It verifies page readiness, navigation, semantic clicks, target identity, uploads, downloads, recovery. No real account or user file is involved.

## Run the repeated matrix

```sh
python3 bench/runners/run_matrix.py --matrix bench/tasks/v5/browser_matrix.json --binary target/debug/comptrol --iterations 3 --output bench/results/local-browser.json
```

Results list a median and a p95 latency per task. A fair comparison needs the same machine, task matrix, initial state, number of runs, independent verifier.

The fixture proves only the behavior exercised by its tasks. Windows and Linux desktop control, live app editing, hosted ChatGPT connections need separate tests on those real systems.
