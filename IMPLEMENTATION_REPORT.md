# Implementation report

## Built

The repository contains a Rust workspace with a native Comptrol runtime and MCP server. The runtime implements a compact MCP surface with operate, inspect, watch, and capabilities.

The core includes explicit risk classes, policy authorization, local stop latch, expiring leases, durable operation identity, restart reconciliation, sanitized append only audit records, a closed workflow representation, and structured action results.

The current host observer is real. On macOS it calls the public System Events accessibility surface to read running application names when permission is available. On other platforms it reports best effort process observation.

The loopback HTTP preview validates the origin and binds only to localhost. The standard compatibility path is MCP stdio.

The repository also includes an npm launcher package, CI validation, a release workflow, browser fixture with exact target binding and duplicate submission protection, platform capability brokers, client conformance harness, threat model, privacy notes, contribution guidance, and research notes.

## Verification

Rust format check passed.

Clippy with warnings denied passed.

Seven unit tests passed. They cover policy refusal, idempotent replay, stop latch behavior, lease expiry, workflow verification, restart reconciliation, and stale browser binding refusal.

The live MCP harness passed initialize, tool discovery, readiness operation, real macOS desktop observation, unauthorized mutation refusal, idempotent replay, a permitted macOS notification with an explicitly unverified effect, stop and resume latch behavior, loopback HTTP origin rejection plus acceptance, durable completed replay, browser fixture conformance, and the repository owned Codex, Claude Code, and Cursor profiles.

The README forbidden punctuation check passed.

The npm package dry run passed.

The optimized Rust build passed.

## Platform matrix

macOS has real basic process observation through System Events. Semantic desktop mutation is not advertised.

The macOS semantic route is implemented and policy gated. It supports exact application, window, role, and control matching. Value changes read the value back before reporting verified. Press actions report unverified because macOS does not provide a safe general postcondition for every control. This host returned the correct assistive access refusal during live probing, so no UI action was claimed as verified.

Windows has a portable runtime build path and a conservative UI Automation capability broker. UI Automation actuation is not implemented.

Linux has a portable runtime build path and conservative AT SPI, X11, and Wayland capability brokers. Linux actuation is not implemented.

## Not built yet

Live Chrome DevTools websocket control, uploads, platform accessibility fixture applications, event driven waits, checkpoints, record and replay, remote pairing, dashboard, application adapters, native packaging, signed releases, npm publication, Homebrew packaging, and real hardware matrix testing remain open work. The browser fixture, exact identity contract, and duplicate submission protection are implemented without falsely claiming live CDP control.

## Security decisions

The default policy is read only. The agent cannot grant itself mutation authority. Arbitrary shell, arbitrary paths, credentials, lock screen input, remote listeners, and raw input are absent.

The implementation preserves the difference between delivery, effect, and verification. An accepted transport message is not treated as proof of a side effect.

## Benchmark result

No performance headline is claimed. The current live check is functional and does not constitute the latency benchmark required by the larger plan.

## Distribution status

Source build is verified. The npm launcher package is prepared but not published. Homebrew is not prepared. No signed release exists. A private GitHub repository exists at `Praket7/Comptrol` with the verified main branch and three open implementation issues.

## Remaining issues ordered by impact

1. Add live Chrome DevTools websocket control.
2. Add platform accessibility fixture applications and live postcondition tests.
3. Add event driven waits, checkpoints, and record and replay.
4. Add packaging and reproducible release artifacts.
