# Implementation report

## Built

The repository contains a Rust workspace with a native Comptrol runtime and MCP server. The runtime implements a compact MCP surface with operate, inspect, watch, reconcile, checkpoint restore, and capabilities. Deterministic closed workflows execute through one operate call.

The core includes explicit risk classes, policy authorization, local stop latch, expiring leases, durable operation identity, an explicit prepared through committed and recovery state lifecycle, restart reconciliation, sanitized append only audit records, bounded event sequencing, file checkpoints, privacy aware traces, revisioned multi display geometry, an isolated adapter registry contract, a closed workflow representation, standard stdio progress notifications, asynchronous durable MCP Tasks with cancellation and restart-unknown handling, a local consent pairing state machine, and structured action results. Release packaging now supports explicit detached RSA signatures with public key verification and a temporary key conformance test.

The optional `command.run` route executes an explicitly allowlisted program with structured argv, an explicit working directory root, no shell, bounded output, a timeout, and exit code verification. It is disabled by default and is covered by a repository owned MCP conformance harness.

The current host observer is real. On macOS it calls the public System Events accessibility surface to read running application names when permission is available. On other platforms it reports best effort process observation.

The loopback HTTP preview validates the origin and binds only to localhost. It also serves a read only diagnostics dashboard. The standard compatibility path is MCP stdio.

The repository also includes an npm launcher package with bounded native process supervision, CI validation, a release workflow, browser target discovery, a narrow CDP route with exact target binding and duplicate submission protection, platform capability brokers, an adapter registry contract, client conformance harnesses, Homebrew formula generation, threat model, privacy notes, contribution guidance, and research notes. In the fast verified execution milestone, normal target-bound CDP and browser protocol calls reuse one mutex-protected local websocket per exact DevTools URL, with monotonic command ids and eviction on protocol failure.

The browser route now opens foreground tabs through an explicit native Chrome launcher when enabled and opens exact visible or background tabs through the configured local Chrome endpoint. On macOS, an explicit Accessibility route can reopen one exact closed saved group in the visible Chrome window and verifies that the closed group control disappeared. It reads bounded accessibility trees, moves exact live targets through history, focuses exact page elements, and closes exact live targets. The result reports the exact target identity where CDP is available and explicitly records that mouse and clipboard were untouched. Account state is preserved by using the existing browser profile. No credential or cookie transfer into a separate headless profile is attempted.

App launch is available as a separate explicit policy route through native launchers. macOS accepts an exact application name and reports process verification when Accessibility permits it. Windows uses `Start-Process` and Linux uses `gtk-launch`; both report launcher acceptance without claiming process verification.

The protocol boundary now rejects MCP and browser messages larger than one mebibyte before parsing. The privacy commands report disabled telemetry, disabled automatic update checks, redacted data classes, and the optional network routes known to this build.

Restart recovery also excludes durable unknown results from the successful idempotency cache. Browser fixture submissions can be reconciled from the fixture state endpoint by idempotency key without resubmitting them. macOS app launches and AX actions can reconcile from exact process or semantic postcondition observations without persisting typed values.

Doctor now has JSON and human readable output. Platform diagnostics label each broker as available, degraded, unavailable, requires human consent, or unsupported. Release packaging verifies archive checksums and metadata SBOMs before publication.

## Verification

Rust format check passed.

Clippy with warnings denied passed.

The current source passes 43 workspace tests, including deterministic route-plan rationale, verified-trace compilation with redaction and preconditions, stale workflow rejection, unavailable-route rejection, and a concurrent delayed-event regression proving EventBus waiters block on notification rather than polling every 5 ms. Browser fixture conformance also covers semantic locators in an open shadow root and a same-origin iframe, while retaining strict ambiguity refusal.

The live MCP harness passed initialize, tool discovery, readiness operation, real macOS desktop observation, unauthorized mutation refusal, idempotent replay, a permitted macOS notification with an explicitly unverified effect, stop and resume latch behavior, loopback HTTP origin rejection plus acceptance, durable completed replay, browser target discovery, verified fixture submission, browser duplicate protection, fixture CDP evaluation over websocket, real dedicated headless Chrome CDP navigation, background target creation, same profile state continuity, verified history navigation, exact target close, explicit closed group refusal, DOM verification, sandbox upload, filename postcondition verification, allowlisted argv command execution, native Chrome launcher policy and strict background refusal, stdio progress notification ordering, durable MCP Tasks across restart, process level restart reconciliation without duplicate mutation, trace record and fixture replay, platform capability conformance, and the repository owned Codex, Claude Code, and Cursor profiles. A headed Chrome profile acceptance run also passed in an isolated visible profile. An opt in live macOS application launch acceptance run passed for Finder. The native Chrome acceptance harness is present and skips before launch when Chrome Automation access cannot respond.

The README forbidden punctuation check passed.

The npm package dry run and launcher restart conformance passed. Browser fixture conformance passed with an explicit transport assertion of one page websocket and one browser websocket across repeated target-bound CDP operations.

The loopback HTTP conformance source now covers random session assignment, required session reuse, session deletion, finite server sent event responses, ordered progress events, invalid JSON, incomplete bodies, origin rejection, method rejection, and oversized requests. The current host cannot execute this updated harness because rebuilding the native binary is blocked by the local Xcode license gate.

The native daemon mode now has a bounded versioned Unix socket protocol on macOS and Linux and a bounded local named pipe protocol on Windows. It provides health checks, forwards MCP messages, preserves progress event ordering, applies local socket permissions, rejects stale path collisions, and bounds every frame. The Windows source path and shared conformance harness are present but await a Windows runner.

The npm launcher now has an opt in daemon mode on Unix and Windows. It preserves the external MCP stdio contract, supervises daemon startup, reconnects after transport loss, bounds restart attempts, and does not replay a request that was already sent without a response. The dedicated daemon launcher conformance test passes on this host.

The optimized Rust build, format check, Clippy with warnings denied, workspace tests, README lint, and browser fixture conformance pass in WSL. Native headed Chrome acceptance remains host-dependent and is not claimed here.

The `v0.1.18` GitHub release workflow completed successfully across all five native targets, published 16 release assets and the npm package, and the package became visible on the public registry after npm processing. The `v0.1.23` workflow is the current release target and includes bounded post-publish registry visibility verification, event-driven history verification, target-list caching for repeated read calls, and a clear WSL-versus-Windows binary diagnostic in browser conformance.

## Platform matrix

macOS has real basic process observation through System Events, exact LaunchServices application launch with process verification, and a semantic AX press and value route with exact application, window, role, and control matching. The controlled Cocoa fixture and conformance harness are present. The live host correctly reported missing Accessibility permission, so the fixture action was not falsely marked verified.

The macOS semantic route is implemented and policy gated. It supports exact application, window, role, and control matching with bounded AppleScript provider calls. Value changes read the value back before reporting verified. Press actions report unverified when no explicit postcondition is supplied. This host returned the correct assistive access refusal during live probing, so no UI action was claimed as verified.

Windows has an opt in UI Automation route for exact process and element binding. Press uses InvokePattern and value changes use ValuePattern with bounded PowerShell provider calls and postcondition checks. The repository includes a WPF fixture and a Windows only conformance harness. This host cannot execute that matrix.

Linux has an opt in AT SPI route for exact process and accessible name binding through action and editable text interfaces. X11 and Wayland capability brokers remain separate and no raw input path is claimed. The repository includes a GTK fixture and a Linux only conformance harness. This host cannot execute that matrix.

## Not built yet

Remote mutual TLS transport, four production application adapters, portable CDP closed group restoration, screenshots and visual recovery, and native Windows and Linux live fixture validation remain open work. Streamable HTTP has concurrent long lived GET streams, bounded event history, `Last-Event-ID` replay, persistent session state, safe restart loading, session deletion, and a connection ceiling. The browser session cache is implemented for normal CDP calls; upload and download still use bounded transaction-local channels pending migration. Event sequencing, bounded waits, checkpoints, record and replay, dashboard diagnostics, browser discovery, fixture CDP mutation, target-bound fill, focus, and wait operations, accessibility snapshots, history navigation, exact live tab close, sandbox upload and download verification, sandbox restricted copy with checkpoint and hash verification, client integration proposals with atomic JSON apply and undo, asynchronous MCP task execution with cancellation and restart reconciliation, cross platform build validation, release packaging, and dedicated headless Chrome validation are implemented. Native real-Chrome profile acceptance remains pending on this host.

## Security decisions

The default policy is read only. The agent cannot grant itself mutation authority. Arbitrary shell, arbitrary paths, credentials, lock screen input, remote listeners, and raw input are absent.

The implementation preserves the difference between delivery, effect, and verification. An accepted transport message is not treated as proof of a side effect.

## Benchmark result

The local benchmark measured 25 warm MCP ping calls over stdio with p50 0.049 ms and p95 0.061 ms in WSL. This is a local transport measurement only. The browser fixture additionally verified one page websocket and one browser websocket for repeated target-bound calls; no cross-platform browser latency advantage is claimed.

## Distribution status

The `comptrolling@0.1.18` npm package is publicly visible and was verified after registry propagation. The `v0.1.23` package adds deterministic route planning, verified-trace workflow compilation, event-driven browser history verification, target-list caching for repeated reads, post-publish npm visibility verification, and a clear WSL-versus-Windows binary diagnostic in browser conformance. The portable local plugin package and repo marketplace are fixture-validated, but ChatGPT developer-mode registration and live ChatGPT installation remain user-side acceptance steps. Detached signing remains opt-in because no operator signing identity was supplied. The fast verified work is pushed to `feat/fast-verified-execution-v3`; the release tag points to the latest released commit. GitHub Dependency Graph is disabled for this repository, so dependency review reports a warning while RustSec `cargo-audit` remains enforced by the security workflow.

## Remaining issues ordered by impact

1. Finish native Windows and Linux validation on their operating systems.
2. Validate the CDP websocket route against an explicitly selected real user Chrome profile and add browser state delta coverage.
3. Add remote mutual TLS transport and application adapters.
4. Publish signed release artifacts only after independent release verification and an operator signing identity is available.
