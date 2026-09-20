# Architecture

Comptrol has a small canonical protocol and a native Rust runtime. MCP is an adapter at the edge. The runtime owns policy, operation identity, audit, stop state, and route selection.

The preferred path is state first. Adapters use an application API, browser protocol, accessibility tree, or a verified workflow before resorting to pixels; the typed workflow engine and the persistent browser multiplexer described below are implemented today, not planned.

Every operation returns a route, preflight state, delivery state, effect state, verification state, disturbance state, recovery state, and a machine readable error when needed.

The initial runtime is intentionally conservative. It observes the host and exposes a sandbox write and desktop notification route only when a human changes local environment policy. It does not expose arbitrary shell, arbitrary file paths, raw input, lock screen access, or remote control.

## Workflow execution

The runtime has a typed workflow IR (`crates/comptrol-workflow`): Observe, Assert, Act, Wait, Verify, Checkpoint, Branch, Loop, ParallelRead, and Return nodes with validation, SHA-256 structural fingerprints, bounded loops, step budgets, cancellation, and clean-replay promotion gates. It never executes model-supplied scripts. Verified traces can be compiled and promoted into versioned warm skills after independent replay evidence; the trace compiler (`trace.rs`) records sanitized candidates with app/site version envelopes and keeps secrets as runtime parameters.

## Transport

The canonical transport is MCP stdio. A loopback Streamable HTTP transport is also included with origin validation, bounded input, concurrent long lived event streams, persistent session state, bounded replay, and session deletion. It remains loopback only.
