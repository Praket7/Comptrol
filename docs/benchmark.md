# Local benchmark

Run `python3 scripts/benchmark.py` after building the debug binary for the transport suite. Run `python3 scripts/benchmark.py --suite verified-task` for the end to end verified task suite.

The transport suite measures warm MCP ping latency over the local standard input and output transport. The verified task suite submits a real asynchronous `system.ping` MCP Task, waits for completion, retrieves the result, and requires the independent structured verification field to be `verified`. It reports completion latency, MCP calls, steps, bytes, retries, disturbance, false positive verification count, resource evidence, and verifier identity. These are raw local measurements only. They do not claim daemon latency, browser latency, semantic action latency, cross platform performance, or superiority over native ChatGPT computer use. Comparisons require the same initial state and final verifier.

The browser fixture exposes `targetListRequests` in its metrics. This supports a focused cache check: warm target-bound reads should reuse the persistent page websocket and avoid unnecessary `/json/list` requests, while mutations and stale identity checks invalidate the cache. It is not, by itself, a general browser latency or native-computer-use comparison.

