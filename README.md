# Comptrol

Comptrol is a local first control layer for computer using agents.

It gives an MCP client one bounded operation surface with explicit policy, stable operation identity, audit records, recovery state, and honest verification.

The first release is a small foundation. It runs on macOS, Windows, and Linux. Basic platform observation is available on the current machine. Semantic desktop mutation, browser control, remote pairing, and application adapters are not advertised until their platform tests exist.

## Why it exists

Computer control becomes unsafe when delivery is confused with effect. Comptrol keeps those states separate. A request may be refused. A dispatch may be accepted without a verified effect. A repeated request with the same identity must not repeat a mutation.

The normal MCP surface has four tools.

1. Operate runs one bounded intent
2. Inspect reads current state
3. Watch reads an operation state
4. Capabilities reports only usable routes

## Local privacy

Normal startup stays local. There is no required account, cloud service, model API, or telemetry service. The default policy allows observation and a readiness check. Mutating routes require local policy outside the agent tool channel.

Audit records are local JSON lines. Typed content is not recorded by default.

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

The browser, semantic input, remote, dashboard, packaging, and signed release paths remain tracked work.

## Contributing

Read the architecture and threat model in the docs directory. Keep platform claims tied to a runnable test. Preserve the difference between request, delivery, effect, and verification.

Comptrol is released under Apache 2.0.
