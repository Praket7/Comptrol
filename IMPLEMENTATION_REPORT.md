# Implementation report

## Built

The repository contains a Rust workspace with a native Comptrol runtime and MCP server. The runtime implements a compact MCP surface with operate, inspect, watch, and capabilities.

The core includes explicit risk classes, policy authorization, local stop latch, expiring leases, operation identity, in process idempotent replay, sanitized append only audit records, a closed workflow representation, and structured action results.

The current host observer is real. On macOS it calls the public System Events accessibility surface to read running application names when permission is available. On other platforms it reports best effort process observation.

The loopback HTTP preview validates the origin and binds only to localhost. The standard compatibility path is MCP stdio.

The repository also includes an npm launcher package, CI validation, a release workflow, threat model, privacy notes, contribution guidance, and research notes.

## Verification

Rust format check passed.

Clippy with warnings denied passed.

Five unit tests passed. They cover policy refusal, idempotent replay, stop latch behavior, lease expiry, and workflow verification.

The live MCP harness passed initialize, tool discovery, readiness operation, real macOS desktop observation, unauthorized mutation refusal, idempotent replay, a permitted macOS notification with an explicitly unverified effect, stop and resume latch behavior, and loopback HTTP origin rejection plus acceptance.

The README forbidden punctuation check passed.

The npm package dry run passed.

The optimized Rust build passed.

## Platform matrix

macOS has real basic process observation through System Events. Semantic desktop mutation is not advertised.

Windows has a portable runtime build path and conservative unsupported results for platform actuation. UI Automation is not implemented.

Linux has a portable runtime build path and conservative unsupported results for platform actuation. AT SPI, X11, and Wayland adapters are not implemented.

## Not built yet

Browser CDP control, exact tab binding, uploads, semantic desktop input, platform accessibility action adapters, durable crash reconciliation, event driven waits, checkpoints, record and replay, remote pairing, dashboard, application adapters, native packaging, signed releases, npm publication, Homebrew packaging, and real hardware matrix testing remain open work.

## Security decisions

The default policy is read only. The agent cannot grant itself mutation authority. Arbitrary shell, arbitrary paths, credentials, lock screen input, remote listeners, and raw input are absent.

The implementation preserves the difference between delivery, effect, and verification. An accepted transport message is not treated as proof of a side effect.

## Benchmark result

No performance headline is claimed. The current live check is functional and does not constitute the latency benchmark required by the larger plan.

## Distribution status

Source build is verified. The npm launcher package is prepared but not published. Homebrew is not prepared. No signed release exists. A private GitHub repository exists at `Praket7/Comptrol` with the verified main branch and three open implementation issues.

## Remaining issues ordered by impact

1. Add durable operation storage and reconciliation after process restart.
2. Build the macOS semantic adapter with exact identity and postcondition tests.
3. Add Windows and Linux capability brokers with truthful per desktop results.
4. Add browser protocol support and a local fixture site.
5. Add cross client conformance tests.
6. Add packaging and reproducible release artifacts.
