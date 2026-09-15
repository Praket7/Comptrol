# Fast verified execution baseline

This baseline records the current implementation before the persistent browser session migration. It is intentionally explicit about measurements that are not available on this Windows host.

## Build identity

| Field | Value |
| --- | --- |
| Source | `ab65aa7f5d659d58a98cb266c02b9793bd514c9d` |
| Workspace version | `0.1.11` |
| Verification environment | WSL on Windows, x86_64 |
| Rust | `rustc 1.98.1 (48a229cea 2026-09-01)` |
| Node | `v22.22.1` |
| Chrome | Installed at `C:\Program Files\Google\Chrome\Application\chrome.exe`; native binary conformance was not run from this WSL build |

## Existing checks

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | passed |
| `cargo test --workspace` | passed, 37 tests |
| `cargo build` | passed |
| Warm MCP benchmark | 25 iterations, p50 0.049 ms, p95 0.061 ms, min 0.048 ms, max 0.214 ms |
| Linux and Windows-target Clippy | passed in the preceding CI verification on `ab65aa7` |
| GitHub validate workflow | passed on `ab65aa7` |
| GitHub security workflow | passed on `ab65aa7` |

## Browser baseline status

The current browser implementation opens a short-lived DevTools websocket for normal target operations and discovers targets through `/json/list`. The following measurements are required before the persistent session implementation can claim improvement:

| Measurement | Status |
| --- | --- |
| 20-action real browser task latency | pending native executable run |
| Websocket handshakes during 20 actions | pending instrumentation |
| `/json/list` calls during 20 actions | pending instrumentation |
| target cache invalidation after reconnect | pending implementation |
| event-backed browser waits | pending implementation |

This document is a baseline, not a performance claim. The warm MCP number measures transport only and must not be compared with browser or native computer-use task completion latency.
