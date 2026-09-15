# Comptrol V4 implementation status

This branch records the first verified V4 foundation slice. It is not a claim that every platform integration in the V4 specification is complete.

## Implemented and verified

- V4 branch and reproducible baseline record.
- Browser, verification, and workflow crate boundaries.
- Target and frame graph primitives with generation invalidation on reconnect.
- Reduced global CDP map lock scope so WebSocket I/O is performed under a per-session lock.
- Required verification criteria cannot produce a verified report when they fail.
- Typed workflow parameter lifting for trace values and cryptographic structural fingerprints.
- Protocol negotiation for legacy MCP and the 2026-07-28 current mode.
- Current stdio and HTTP behavior refuses legacy session operations instead of silently treating them as current protocol state.
- Chrome closed-groups extension moved to experiments and excluded from normal setup.
- Browser multiplexer primitives provide one WebSocket, flattened-session command correlation, and out-of-order response routing.
- Target and frame graphs consume CDP lifecycle events and execution-context changes.
- Download verification uses browser download lifecycle events and exact GUID binding.
- Upload responses expose honest selected/transfer/accepted/persisted stages.
- Typed workflow IR validation is enforced during trace compilation.
- Typed workflow executor runs compiled actions through the normal runtime, resolves lifted parameters, enforces a step budget, and supports bounded branches, loops, waits, verification, and parallel reads.
- Durable MCP Tasks are available across stdio, stateless HTTP, and mutual-TLS HTTP with SQLite persistence and cancellation reconciliation.
- Browser connection manager reuses one bootstrapped flattened-session WebSocket per debugger endpoint and invalidates graph state on disconnect.
- OBS uses a persistent authenticated WebSocket client.
- LibreOffice uses a persistent UNO connection and exact document identity.
- VS Code bridge calls require a configured token, nonce, and authenticated response.
- Adapter host response reads have bounded I/O deadlines and terminate unresponsive children.
- Doctor reports current capability and live-verification boundaries.

## Implemented but not live-verified here

- Native Chrome recently-closed UI restore on Windows, macOS, and Linux.
- Real Chrome OOPIF and cross-origin frame execution.
- Native UIA, AX, and AT-SPI live event performance.
- VS Code, LibreOffice, OBS, Blender, and CapCut application acceptance.
- mTLS production deployment and remote mutual TLS interoperability.

## Still required for the full V4 specification

- Replace the remaining synchronous CDP session implementation with the full single-reader/single-writer flattened-session multiplexer.
- Complete universal verification wiring across every adapter and high-level operation.
- Wire the extracted async browser connection into the remaining synchronous compatibility facade and enable full target attachment/domain bootstrap.
- Persist route statistics and add the V2 route planner score and hard gates.
- Extend MCP cancellation propagation into every adapter/browser operation and add task progress replay coverage.
- Add clean workflow replay promotion and repair fallback; the typed state-machine executor and bounded branches/loops/parallel reads are implemented.
- Implement persistent native accessibility workers and event-driven caches for all three desktop platforms.
- Upgrade VS Code authentication, exact LibreOffice document identity, persistent OBS, Blender live IPC, adapter deadlines, and truthful kernel-isolation reporting.
- Add the complete benchmark matrix and collect stable performance history before hard regression thresholds.
- Build and publish platform-specific npm/Homebrew release artifacts after release CI produces fresh binaries.

## Verification run

On the V4 branch, the following currently pass:

```text
cargo fmt --all
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
scripts/mcp_current_conformance.py
scripts/adapter_conformance.py
scripts/client_conformance.py
scripts/http_conformance.py
```

The repository's pre-existing untracked `node_modules/`, `pnpm-lock.yaml`, and `work/` artifacts were intentionally not staged.
