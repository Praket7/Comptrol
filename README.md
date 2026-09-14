# Comptrol

Comptrol is a local first control layer for computer using agents.

It gives an MCP client one bounded operation surface with explicit policy, stable operation identity, audit records, recovery state, and honest verification.

The first release is a small foundation. It runs on macOS, Windows, and Linux. Basic platform observation is available on the current machine. Browser CDP control is available only for an explicitly configured local endpoint. macOS semantic mutation is available only with Accessibility permission and explicit policy. Windows and Linux currently expose read only capability brokers.

## Why it exists

Computer control becomes unsafe when delivery is confused with effect. Comptrol keeps those states separate. A request may be refused. A dispatch may be accepted without a verified effect. A repeated request with the same identity must not repeat a mutation.

The normal MCP surface has six tools.

1. Operate runs one bounded intent
2. Inspect reads current state
3. Watch reads an operation state
4. Reconcile resolves durable unknown state without repeating a mutation
5. Restore checkpoint returns a local sandbox to a saved state
6. Capabilities reports only usable routes

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

## Safety

The emergency stop command creates a local latch. Mutating requests refuse while that latch exists. Resume is a human local action.

The runtime refuses unsupported routes. It does not pretend that a platform backend is complete because a protocol message was accepted.

## Current support

Basic observation and the policy core are implemented and tested.

The macOS observer can use the public System Events accessibility surface when permission is available. Other platforms use best effort process observation.

The runtime includes bounded event history, file checkpoints, fixture trace replay, exact browser target discovery, and a loopback diagnostics dashboard.

Privacy is local by default. `comptrol privacy status` reports telemetry and redaction defaults. The privacy network endpoint report lists optional routes. MCP and browser protocol messages are bounded to one mebibyte.

With explicit local app launch policy, macOS, Windows, and Linux can open an exact app through their native launcher. Browser tabs can be opened in the existing Chrome profile through the local DevTools endpoint. These routes do not synthesize mouse input or touch the clipboard.

Dedicated user profile Chrome validation, Windows and Linux semantic actuation, remote pairing, signed releases, and published packages remain tracked work. The real Chrome harness covers navigation, DOM evaluation, sandbox upload, and sandbox download.

## Contributing

Read the architecture and threat model in the docs directory. Keep platform claims tied to a runnable test. Preserve the difference between request, delivery, effect, and verification.

Comptrol is released under Apache 2.0.
