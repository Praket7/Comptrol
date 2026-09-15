# Implementation report

## Built

The repository contains a Rust workspace with a native Comptrol runtime and MCP server. The runtime implements a compact MCP surface with operate, inspect, watch, reconcile, checkpoint restore, and capabilities. Deterministic closed workflows execute through one operate call.

The core includes explicit risk classes, policy authorization, local stop latch, expiring leases, durable operation identity, an explicit prepared through committed and recovery state lifecycle, restart reconciliation, sanitized append only audit records, bounded event sequencing, file checkpoints, privacy aware traces, revisioned multi display geometry, an isolated adapter registry contract, a closed workflow representation, standard stdio progress notifications, a durable completed MCP Tasks subset, a local consent pairing state machine, and structured action results. Release packaging now supports explicit detached RSA signatures with public key verification and a temporary key conformance test.

The optional `command.run` route executes an explicitly allowlisted program with structured argv, an explicit working directory root, no shell, bounded output, a timeout, and exit code verification. It is disabled by default and is covered by a repository owned MCP conformance harness.

The current host observer is real. On macOS it calls the public System Events accessibility surface to read running application names when permission is available. On other platforms it reports best effort process observation.

The loopback HTTP preview validates the origin and binds only to localhost. It also serves a read only diagnostics dashboard. The standard compatibility path is MCP stdio.

The repository also includes an npm launcher package with bounded native process supervision, CI validation, a release workflow, browser target discovery, a narrow CDP websocket route for evaluation navigation and sandbox uploads, a fixture mutation route with exact target binding and duplicate submission protection, platform capability brokers, an adapter registry contract, client conformance harnesses, Homebrew formula generation, threat model, privacy notes, contribution guidance, and research notes.

The browser route now opens foreground tabs through an explicit native Chrome launcher when enabled and opens exact visible or background tabs through the configured local Chrome endpoint. On macOS, an explicit Accessibility route can reopen one exact closed saved group in the visible Chrome window and verifies that the closed group control disappeared. It reads bounded accessibility trees, moves exact live targets through history, and closes exact live targets. The result reports the exact target identity where CDP is available and explicitly records that mouse and clipboard were untouched. Account state is preserved by using the existing browser profile. No credential or cookie transfer into a separate headless profile is attempted.

App launch is available as a separate explicit policy route through native launchers. macOS accepts an exact application name and reports process verification when Accessibility permits it. Windows uses `Start-Process` and Linux uses `gtk-launch`; both report launcher acceptance without claiming process verification.

The protocol boundary now rejects MCP and browser messages larger than one mebibyte before parsing. The privacy commands report disabled telemetry, disabled automatic update checks, redacted data classes, and the optional network routes known to this build.

Restart recovery also excludes durable unknown results from the successful idempotency cache. Browser fixture submissions can be reconciled from the fixture state endpoint by idempotency key without resubmitting them. macOS app launches and AX actions can reconcile from exact process or semantic postcondition observations without persisting typed values.

Doctor now has JSON and human readable output. Platform diagnostics label each broker as available, degraded, unavailable, requires human consent, or unsupported. Release packaging verifies archive checksums and metadata SBOMs before publication.

## Verification

Rust format check passed.

Clippy with warnings denied passed.

The previous verified run passed thirty three unit tests. The current source adds two unit tests for exact Chrome group binding and restart metadata, but they cannot run on this host until the Xcode license gate is resolved.

The live MCP harness passed initialize, tool discovery, readiness operation, real macOS desktop observation, unauthorized mutation refusal, idempotent replay, a permitted macOS notification with an explicitly unverified effect, stop and resume latch behavior, loopback HTTP origin rejection plus acceptance, durable completed replay, browser target discovery, verified fixture submission, browser duplicate protection, fixture CDP evaluation over websocket, real dedicated headless Chrome CDP navigation, background target creation, same profile state continuity, verified history navigation, exact target close, explicit closed group refusal, DOM verification, sandbox upload, filename postcondition verification, allowlisted argv command execution, native Chrome launcher policy and strict background refusal, stdio progress notification ordering, durable MCP Tasks across restart, process level restart reconciliation without duplicate mutation, trace record and fixture replay, platform capability conformance, and the repository owned Codex, Claude Code, and Cursor profiles. A headed Chrome profile acceptance run also passed in an isolated visible profile. An opt in live macOS application launch acceptance run passed for Finder. The native Chrome acceptance harness is present and skips before launch when Chrome Automation access cannot respond.

The README forbidden punctuation check passed.

The npm package dry run and launcher restart conformance passed. The browser fixture conformance passed.

The loopback HTTP conformance source now covers random session assignment, required session reuse, session deletion, finite server sent event responses, ordered progress events, invalid JSON, incomplete bodies, origin rejection, method rejection, and oversized requests. The current host cannot execute this updated harness because rebuilding the native binary is blocked by the local Xcode license gate.

The native daemon mode now has a bounded versioned Unix socket protocol on macOS and Linux. It provides health checks, forwards MCP messages, preserves progress event ordering, applies local socket permissions, rejects stale path collisions, and bounds every frame. Its conformance harness is present but awaits a rebuild. Windows named pipe support and automatic client reconnect remain unimplemented.

The optimized Rust build passed before the latest semantic Chrome group route. The current rebuild is blocked by the same local Xcode license gate.

The GitHub Actions workflows are present, but the latest remote jobs were rejected before checkout because the repository account reported failed recent payments or an exceeded spending limit. This is an external runner availability failure rather than a test result.

## Platform matrix

macOS has real basic process observation through System Events, exact LaunchServices application launch with process verification, and a semantic AX press and value route with exact application, window, role, and control matching. The controlled Cocoa fixture and conformance harness are present. The live host correctly reported missing Accessibility permission, so the fixture action was not falsely marked verified.

The macOS semantic route is implemented and policy gated. It supports exact application, window, role, and control matching with bounded AppleScript provider calls. Value changes read the value back before reporting verified. Press actions report unverified when no explicit postcondition is supplied. This host returned the correct assistive access refusal during live probing, so no UI action was claimed as verified.

Windows has an opt in UI Automation route for exact process and element binding. Press uses InvokePattern and value changes use ValuePattern with bounded PowerShell provider calls and postcondition checks. The repository includes a WPF fixture and a Windows only conformance harness. This host cannot execute that matrix.

Linux has an opt in AT SPI route for exact process and accessible name binding through action and editable text interfaces. X11 and Wayland capability brokers remain separate and no raw input path is claimed. The repository includes a GTK fixture and a Linux only conformance harness. This host cannot execute that matrix.

## Not built yet

Long lived Streamable HTTP event streams, daemon supervision, persistent hot sessions, Windows named pipe transport, automatic client reconnect, remote mutual TLS transport, application specific adapters, portable CDP closed group restoration, screenshots and visual recovery, full asynchronous MCP task execution and cancellation, native Windows and Linux live fixture validation on their operating systems, signed release publication, npm publication, Homebrew publication, GitHub artifact attestation, and real user profile acceptance without an explicitly supplied DevTools endpoint remain open work. Basic HTTP session lifecycle, bounded event responses, ordered progress events, and the macOS and Linux Unix socket daemon boundary are implemented but await native runtime verification. Event sequencing, bounded waits, checkpoints, record and replay, dashboard diagnostics, browser discovery, fixture CDP mutation, target bound fill and wait operations, accessibility snapshots, history navigation, exact live tab close, sandbox upload and download verification, sandbox restricted copy with checkpoint and hash verification, client integration proposals with atomic JSON apply and undo, cross platform build validation, reproducible native archive and checksum preparation, and dedicated headless Chrome validation were implemented and previously tested. The new macOS semantic closed group reopening route has unit coverage but no live acceptance on this host. GitHub workflow security checks include Rust dependency audit and pull request dependency review.

## Security decisions

The default policy is read only. The agent cannot grant itself mutation authority. Arbitrary shell, arbitrary paths, credentials, lock screen input, remote listeners, and raw input are absent.

The implementation preserves the difference between delivery, effect, and verification. An accepted transport message is not treated as proof of a side effect.

## Benchmark result

The local benchmark measured 25 warm MCP ping calls over stdio with p50 0.011 ms and p95 0.014 ms in this environment. This is a local transport measurement only. It is not a cross platform, browser, semantic action, or daemon latency claim.

## Distribution status

An earlier source build is verified. The current source build is blocked by the local Xcode license gate. The npm launcher package is prepared but not published. Homebrew formula generation is prepared but no formula is published. Detached signing and verification are implemented, but no signed release exists because no operator signing identity was supplied. A private GitHub repository exists at `Praket7/Comptrol` with main synchronized through the latest implementation commit, six open implementation issues, and one completed browser issue.

## Remaining issues ordered by impact

1. Add a real daemon with supervised reconnect and complete Streamable HTTP session conformance.
2. Validate the CDP websocket route against an explicitly selected real user Chrome profile and add browser state delta coverage.
3. Complete native Windows and Linux live accessibility matrices and broaden the macOS AX fixture validation when permissions allow.
4. Add remote mutual TLS transport and application adapters.
5. Publish signed release artifacts and packages only after independent release verification.
