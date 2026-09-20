# Architecture

Comptrol has a small canonical protocol and a native Rust runtime. MCP is an adapter at the edge. The runtime owns policy, operation identity, audit, stop state, and route selection.

The preferred path is state first. Adapters use an application API, browser protocol, accessibility tree, or a verified workflow before resorting to pixels; the typed workflow engine and the persistent browser multiplexer described below are implemented today, not planned.

Every operation returns a route, preflight state, delivery state, effect state, verification state, disturbance state, recovery state, and a machine readable error when needed.

The initial runtime is intentionally conservative. It observes the host and exposes a sandbox write and desktop notification route only when a human changes local environment policy. It does not expose arbitrary shell, arbitrary file paths, raw input, lock screen access, or remote control.

## Workflow execution

The library includes a closed workflow representation with sense, assert, set, wait, and return operations. It does not execute model supplied scripts. A later compiler can lower a verified trace into this representation after validating target identity and postconditions.

## Transport

The canonical transport is MCP stdio. A loopback Streamable HTTP transport is also included with origin validation, bounded input, concurrent long lived event streams, persistent session state, bounded replay, and session deletion. It remains loopback only.
