# Comptrol V4 implementation status

This branch records the first verified V4 foundation slice. It is not a claim that every platform integration in the V4 specification is complete.

## Implemented and verified

- V4 branch and reproducible baseline record.
- Browser, verification, and workflow crate boundaries.
- Target and frame graph primitives with generation invalidation on reconnect.
- Reduced global CDP map lock scope so WebSocket I/O is performed under a per-session lock.
- Required verification criteria cannot produce a verified report when they fail.
- Verification reports support an independent finish gate that can only preserve or lower confidence; dispatch evidence cannot upgrade a failed independent outcome.
- Typed workflow parameter lifting for trace values and cryptographic structural fingerprints.
- Protocol negotiation for legacy MCP and the 2026-07-28 current mode.
- Current stdio and HTTP behavior refuses legacy session operations instead of silently treating them as current protocol state.
- Chrome closed-groups extension moved to experiments and excluded from normal setup.
- Browser multiplexer primitives provide one WebSocket, flattened-session command correlation, and out-of-order response routing.
- Target and frame graphs consume CDP lifecycle events and execution-context changes.
- Download verification uses browser download lifecycle events and exact GUID binding.
- Typed upload and download transactions enforce ordered evidence; generic CDP reports selection/browser completion only, while transfer, application acceptance, persistence, and filesystem verification remain explicit stages.
- Browser locators have typed semantic kinds and reject empty or invalid identities before dispatch; click and fill actionability requirements are distinct.
- Typed workflow IR validation is enforced during trace compilation.
- Typed workflow executor runs compiled actions through the normal runtime, resolves lifted parameters, enforces a step budget, and supports bounded branches, loops, waits, verification, and parallel reads.
- Workflow candidates can now be promoted only after clean-fixture replay, independent verification, minimum repeated success, and fingerprint equality; promotion creates a new version without mutating the candidate.
- Durable MCP Tasks are available across stdio, stateless HTTP, and mutual-TLS HTTP with SQLite persistence and cancellation reconciliation.
- Task cancellation requests are persisted as task events and rechecked after execution, so cancellation state is not only an in-memory transport flag.
- Durable Tasks now persist a progress snapshot alongside status and result, with migration support for existing SQLite stores; reconnecting clients can inspect progress without relying on transient notifications.
- Cancellation events are accepted only for queued or running tasks; completed, failed, cancelled, and unknown tasks are not mutated by a late cancel request.
- EventBus now provides non-blocking subscriptions, bounded replay via `snapshot_since`, and sequence inspection for reconnecting consumers.
- Browser connection manager reuses one bootstrapped flattened-session WebSocket per debugger endpoint and invalidates graph state on disconnect.
- Browser manager bootstraps required CDP domains for all targets already attached in the live graph without holding graph locks across I/O.
- Browser multiplexer exposes generation- and revision-checked target commands so warm operations can fail with `stale_reference` before dispatch instead of rediscovering or cross-targeting.
- Browser-level synchronous compatibility calls now use `BlockingBrowserManager`, whose dedicated long-lived Tokio runtime owns `BrowserManager` and reuses one browser-level WebSocket per debugger endpoint. Target-specific legacy helpers still need migration to generation-bound `target_command`.
- The blocking bridge now also exposes generation- and revision-bound target commands, so migrated synchronous callers can use the live target graph without creating a per-target socket.
- Frame commands now bind an execution context to a generation- and revision-checked frame record, including OOPIF target/session routing; navigation and execution-context lifecycle events invalidate frame references.
- History, coordinate input, upload, accessibility, and download-trigger target commands now dispatch through the flattened browser multiplexer. Event-only waits now consume the shared browser connection's bounded broadcast stream; the old synchronous per-page session map has been removed.
- Runtime route history is persisted in SQLite and feeds conservative deterministic route scoring after safety gates.
- OBS uses a persistent authenticated WebSocket client.
- LibreOffice uses a persistent UNO connection and exact document identity.
- Adapter hosts enforce bounded request deadlines, reap timed-out children, and retain a bounded stderr diagnostic tail.
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

- Add event replay/cursor semantics for late subscribers and wire the shared browser event stream into the durable MCP EventHub.
- Complete universal verification wiring across every adapter and high-level operation.
- Complete target attachment/domain bootstrap coverage for all migrated compatibility helpers and remove the remaining legacy per-session socket path.
- Extend persisted route statistics with latency samples and planner feedback across adapter/application versions.
- Extend MCP cancellation propagation into every adapter/browser operation and add task progress replay coverage.
- Add workflow repair fallback and host persistence for promoted candidates; the clean replay promotion gate is implemented.
- Implement persistent native accessibility workers and event-driven caches for all three desktop platforms.
- Upgrade VS Code authentication, exact LibreOffice document identity, persistent OBS, Blender live IPC, and truthful kernel-isolation reporting.
- Add the complete benchmark matrix and collect stable performance history before hard regression thresholds.
- Build and publish platform-specific npm/Homebrew release artifacts after release CI produces fresh binaries.
- Wire the typed locator/actionability and upload/download transaction primitives into every remaining legacy browser helper.

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
