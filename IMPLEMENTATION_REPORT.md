# Implementation report

## Built

The repository contains a Rust workspace with a native Comptrol runtime and MCP server. The runtime implements a compact MCP surface with operate, inspect, watch, reconcile, checkpoint restore, and capabilities. Deterministic closed workflows execute through one operate call.

The core includes explicit risk classes, policy authorization, local stop latch, expiring leases, durable operation identity, restart reconciliation, sanitized append only audit records, bounded event sequencing, file checkpoints, privacy aware traces, revisioned multi display geometry, an isolated adapter registry contract, a closed workflow representation, and structured action results.

The current host observer is real. On macOS it calls the public System Events accessibility surface to read running application names when permission is available. On other platforms it reports best effort process observation.

The loopback HTTP preview validates the origin and binds only to localhost. It also serves a read only diagnostics dashboard. The standard compatibility path is MCP stdio.

The repository also includes an npm launcher package, CI validation, a release workflow, browser target discovery, a narrow CDP websocket route for evaluation navigation and sandbox uploads, a fixture mutation route with exact target binding and duplicate submission protection, platform capability brokers, an adapter registry contract, client conformance harness, Homebrew formula generation, threat model, privacy notes, contribution guidance, and research notes.

## Verification

Rust format check passed.

Clippy with warnings denied passed.

Sixteen unit tests passed. They cover policy refusal, idempotent replay, stop latch behavior, lease expiry, workflow verification, one call workflow execution, restart reconciliation, stale browser binding refusal, browser identity parsing, loopback enforcement, event deduplication, checkpoint restore, trace redaction, mixed scale display geometry, adapter registry isolation, and audit redaction.

The live MCP harness passed initialize, tool discovery, readiness operation, real macOS desktop observation, unauthorized mutation refusal, idempotent replay, a permitted macOS notification with an explicitly unverified effect, stop and resume latch behavior, loopback HTTP origin rejection plus acceptance, durable completed replay, browser target discovery, verified fixture submission, browser duplicate protection, fixture CDP evaluation over websocket, real dedicated headless Chrome CDP navigation, DOM verification, sandbox upload, and filename postcondition verification, trace record and fixture replay, platform capability conformance, and the repository owned Codex, Claude Code, and Cursor profiles.

The README forbidden punctuation check passed.

The npm package dry run passed.

The optimized Rust build passed.

## Platform matrix

macOS has real basic process observation through System Events and a semantic AX press and value route with exact application, window, role, and control matching. The controlled Cocoa fixture and conformance harness are present. The live host correctly reported missing Accessibility permission, so the fixture action was not falsely marked verified.

The macOS semantic route is implemented and policy gated. It supports exact application, window, role, and control matching with bounded AppleScript provider calls. Value changes read the value back before reporting verified. Press actions report unverified because macOS does not provide a safe general postcondition for every control. This host returned the correct assistive access refusal during live probing, so no UI action was claimed as verified.

Windows has a portable runtime build path and a conservative UI Automation capability broker. UI Automation actuation is not implemented.

Linux has a portable runtime build path and conservative AT SPI, X11, and Wayland capability brokers. Linux actuation is not implemented.

## Not built yet

Real Chrome profile validation beyond the dedicated headless harness, platform accessibility fixture applications, remote pairing, application adapters, native packaging, signed releases, npm publication, Homebrew publication, and real hardware matrix testing remain open work. Event sequencing, bounded waits, checkpoints, record and replay, dashboard diagnostics, browser discovery, fixture CDP mutation, target bound fill and wait operations, sandbox upload and download verification, client integration proposals with atomic JSON apply and undo, and dedicated headless Chrome validation are implemented and tested.

## Security decisions

The default policy is read only. The agent cannot grant itself mutation authority. Arbitrary shell, arbitrary paths, credentials, lock screen input, remote listeners, and raw input are absent.

The implementation preserves the difference between delivery, effect, and verification. An accepted transport message is not treated as proof of a side effect.

## Benchmark result

The local benchmark measured 25 warm MCP ping calls over stdio with p50 0.011 ms and p95 0.014 ms in this environment. This is a local transport measurement only. It is not a cross platform, browser, semantic action, or daemon latency claim.

## Distribution status

Source build is verified. The npm launcher package is prepared but not published. Homebrew formula generation is prepared but no formula is published. No signed release exists. A private GitHub repository exists at `Praket7/Comptrol` with the verified main branch, six open implementation issues, and one completed browser issue.

## Remaining issues ordered by impact

1. Validate the CDP websocket route against real user Chrome profiles and download behavior.
2. Add platform accessibility fixture applications and live postcondition tests.
3. Add remote pairing and application adapters.
4. Add signed release artifacts and publish only after independent release verification.
