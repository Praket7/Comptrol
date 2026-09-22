# Comptrol

Comptrol is a local first control layer for computer using agents.

It gives an MCP client one bounded operation surface with explicit policy, stable operation identity, audit records, recovery state, and honest verification.

The first release is a small foundation. It runs on macOS, Windows, and Linux. Basic platform observation is available on the current machine. Browser protocol control can use either an explicitly configured local CDP endpoint or the optional Browser Bridge extension for an existing signed-in Chrome profile. macOS semantic mutation is available only with Accessibility permission and explicit policy. Windows UI Automation and Linux AT SPI semantic mutation routes are implemented behind persistent platform workers and explicit local policy. They are exercised by conformance harnesses but still need live validation on real Windows and Linux hosts; event-driven target caches now back the browser and popup paths, and native accessibility event caching is tracked work.

## What V5 adds

One semantic `operate` call compiles to the fastest permitted verified route: official service APIs (Google Docs/Slides batch edits with revision control, Gmail/Graph mail, Discord bot API, Canva Connect), official application surfaces (DaVinci Resolve scripting, PowerPoint COM/Open XML, LibreOffice UNO, OBS WebSocket, Blender bridge, VS Code extension API), the permissioned signed-in browser session, native accessibility, or narrowly scoped visual fallback.

New in this branch: persistent consent broker with human-action waits (`awaiting_human_action`, never typing secrets), registry-backed app launch plus deep resource opening, typed settings registry, trusted software installation with inventory verification and native elevation handoff, popup classification that never auto-approves protected prompts, a browser session broker with revision-keyed target state cache, cross-platform adapter IPC, and first-party adapters for Resolve, Google Workspace, PowerPoint, Discord, mail, Messages, and Canva. Capability details live in `docs/adapters.md`; per-adapter setup lives in each `adapters/<name>/README.md`.

## Why it exists

Computer control becomes unsafe when delivery is confused with effect. Comptrol keeps those states separate. A request may be refused. A dispatch may be accepted without a verified effect. A repeated request with the same identity must not repeat a mutation.

The normal MCP surface has seven tools.

1. Operate runs one bounded intent
2. Inspect reads current state
3. Watch reads an operation state
4. Reconcile resolves durable unknown state without repeating a mutation
5. Restore checkpoint returns a local sandbox to a saved state
6. Capabilities reports only usable routes
7. Human action resolve records a user's decision after they respond to a native prompt

For ChatGPT web, follow [`plugins/comptrol/CHATGPT_SETUP.md`](plugins/comptrol/CHATGPT_SETUP.md) to connect this local stdio server through OpenAI Secure MCP Tunnel. This is separate from Codex's local stdio integration.

## Local privacy

Normal startup stays local. There is no required account, cloud service, model API, or telemetry service. The default policy allows observation and a readiness check. Mutating routes require local policy outside the agent tool channel.

Audit records are local JSON lines. Typed content is not recorded by default. Optional traces support privacy minimal, developer, and fixture full modes.

## Install from source

Install Rust and run the binary from this repository.

1. Run `cargo build`
2. Run `cargo run doctor`
3. Add the binary as an MCP server in the client of your choice
4. Use the binary command `cargo run mcp`

The standard input and output transport is the compatibility path for Codex, Claude Code, Cursor, and other MCP clients.

## Install the launcher

The published npm package is named `comptrolling`. Starting with version 0.1.1 it bundles the native runtime for macOS, Linux, and Windows.

```text
npm install comptrolling
npx comptrolling
```

Open an exact installed app or a URL in one command:

```text
comptrol open Blender
comptrol open https://www.espn.com/
```

App launch and Chrome URL opening are enabled by default as individual operations; no environment flags are needed. Chrome opens in the existing default profile. If a local DevTools endpoint or Browser Bridge is already connected, the same command also verifies that the page reaches `document.readyState === "complete"`; otherwise it reports that the browser accepted the URL without claiming page-load verification.

The npm release also bundles the optional Browser Bridge for controlling already-open signed-in Chrome tabs without copying a browser profile. Chrome requires the human to load/approve the extension and assigns the extension ID, so Comptrol does not silently install it during `npm install`. To set it up:

```text
npx comptrol-browser-setup --print-extension-path
# Load that folder with Chrome's "Load unpacked", copy the displayed extension ID, then:
npx comptrol-browser-setup --extension-id <EXTENSION_ID>
```

The setup command registers the native messaging host for the exact extension origin. Browser Bridge health is based on a live heartbeat and command round-trip, not merely the presence of a manifest or stale target list.

The Homebrew formula is generated for macOS Apple Silicon, macOS Intel, Linux ARM64, and Linux x64 when those native release archives exist. The current private release contains Apple Silicon and Intel macOS archives. Linux archives will be attached after native Linux CI runs. A public Homebrew install requires a public tap or public release assets. The private release formula is available as `comptrolling.rb` to authorized repository users.

## Release trust

Every native archive has a SHA256 checksum and a Cargo metadata SBOM. A signed release also includes a detached signature and a public key. The private signing key belongs to the release operator and is never committed to GitHub. Users verify the signature with the published public key before installing. A checksum detects accidental or transit corruption. A signature also authenticates the release source when the public key was obtained through a trusted channel.

## Safety

The emergency stop command creates a local latch. Mutating requests refuse while that latch exists. Resume is a human local action.

The runtime refuses unsupported routes. It does not pretend that a platform backend is complete because a protocol message was accepted.

## Current support

Basic observation and the policy core are implemented and tested.

The macOS observer can use the public System Events accessibility surface when permission is available. Exact macOS application and resource launch uses LaunchServices for `.app` bundles; because `open` is a helper process, that route reports delivery without falsely claiming destination-process verification. Direct executable routes can verify the destination PID. Windows UI Automation and Linux AT SPI semantic routes are implemented behind persistent platform workers with the same semantic matching contract, while live acceptance on physical Windows and Linux hosts is still pending. Other platforms use best effort process observation.

The runtime includes bounded event history, file checkpoints, fixture trace replay, exact browser target discovery, and a loopback diagnostics dashboard.

Privacy is local by default. `comptrol privacy status` reports telemetry and redaction defaults. The privacy network endpoint report lists optional routes. MCP and browser protocol messages are bounded to one mebibyte.

By default, macOS, Windows, and Linux can open an exact app through the native launcher, and Chrome can open a foreground tab in the existing default browser profile. These two allowlisted open actions do not grant general desktop input, app-resource access, or browser page mutation. With a local DevTools endpoint or a healthy Browser Bridge session, browser tabs can be controlled through exact target identities. The bridge persists target snapshots locally, leases commands for crash recovery, and reports active health only while the extension/native-host connection is fresh. These routes do not synthesize mouse input or touch the clipboard.

Supported browser operations can request strict background posture. Routes that cannot prove that posture refuse instead of activating another application or silently taking control of the foreground.

Dedicated user profile Chrome validation, Windows and Linux live matrix validation, signed releases, and published packages remain tracked work. Remote transport exists as an mTLS server but is not yet a complete paired remote control product. The real Chrome harness covers navigation, DOM evaluation, same profile background state, sandbox upload, and sandbox download. Restore verification waits on the persistent browser target graph instead of polling target discovery, and reconstruction after an unavailable native restore is implemented and unit tested.

## Contributing

Read the architecture and threat model in the docs directory. Keep platform claims tied to a runnable test. Preserve the difference between request, delivery, effect, and verification.

Comptrol is released under Apache 2.0.
