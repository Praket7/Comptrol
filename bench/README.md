# Comptrol V4 benchmarks

Benchmark tasks describe the goal and independent verifier without prescribing the internal route. Results are JSON artifacts and must distinguish verified success from dispatch success.

The checked-in smoke task can be run with:

```text
python3 scripts/benchmark_v4.py --binary target/debug/comptrol --task v4.transport.smoke
```

Browser, desktop, adapter, and native acceptance tasks should be added only with an executable fixture and an independent final verifier. Unsupported live-platform tasks remain documented as pending rather than being reported as passing.

The transport fixture also emits a measurement scope and zero-valued non-applicable counters such as model turns, screenshots, WebSocket handshakes, and target-list requests. These are explicit fixture observations, not claims about live browser performance.
