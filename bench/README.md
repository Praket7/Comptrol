# Comptrol V4 benchmarks

Benchmark tasks describe the goal and independent verifier without prescribing the internal route. Results are JSON artifacts and must distinguish verified success from dispatch success.

The checked-in smoke task can be run with:

```text
python3 scripts/benchmark_v4.py --binary target/debug/comptrol --task v4.transport.smoke
```

Browser, desktop, adapter, and native acceptance tasks should be added only with an executable fixture and an independent final verifier. Unsupported live-platform tasks remain documented as pending rather than being reported as passing.

The transport fixture also emits a measurement scope and zero-valued non-applicable counters such as model turns, screenshots, WebSocket handshakes, and target-list requests. These are explicit fixture observations, not claims about live browser performance.

## Six-task acceptance suite (`run_suite.mjs`)

`node bench/run_suite.mjs` runs the six-task acceptance suite and appends one
JSON record per task to `bench/runs.jsonl` (step count + wall time per record):

1. `bridge_channel_alive` - the service worker answers a bridge_ping round trip
2. `ensure_session_one_call` - channel readiness in one call
3. `classroom_background_click` - background discovery of a real signed-in SPA
4. `terminal_echo_readback` - allowlisted command verified from output readback
5. `app_list_indexed` - the desktop app registry lists installed applications
6. `workflow_execute_verified` - one bounded workflow call, independently verified

Generalization cases (`--generalization`) prove tasks 4 and 6 are not tied to
one magic input. `--trend` prints per-task p50/p95 wall time, step counts, and
speedup against `bench/baselines.json`. Baselines are honest: entries state
`measured: true|false` with provenance in `native_cu_source`, and infrastructure
probes with no human equivalent are marked `not_comparable` instead of invented.
`--self-check` is the offline integrity gate wired into `scripts/check.sh`.
