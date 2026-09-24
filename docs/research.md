# Research notes

This page records the outside material used to guide Comptrol's safety work. Research informed the design. It does not prove that Comptrol is safe or that every app works.

## Local file safety

Rust filesystem paths can change between a check and a later write. Checking a path as text does not stop a link from pointing somewhere else. The new sandbox code opens files relative to a directory handle. It keeps reads, writes, backups, and restores under that directory.

Sources

1. [Rust filesystem documentation](https://doc.rust-lang.org/std/fs/index.html)
2. [cap std directory API](https://docs.rs/cap-std/latest/cap_std/fs/struct.Dir.html)

## MCP security

The MCP security guide calls out origin validation, local server boundaries, and authorization. Comptrol's normal setup remains local standard input and output. Its optional HTTP service binds to loopback. A session identifier is not a login credential. Remote use needs a separate authenticated deployment.

Source

1. [MCP security best practices](https://modelcontextprotocol.io/docs/tutorials/security/security_best_practices)

## Computer use evaluation

Recent computer use research separates task completion from operational reliability. A tool call by itself does not prove that the requested change happened. Comptrol therefore reports the observed result, target identity, verification state, and recovery state separately.

Sources

1. [UI CUBE paper on operational reliability](https://arxiv.org/abs/2511.17131)
2. [OSWorld Pro computer use benchmark](https://arxiv.org/abs/2506.12508)
3. [HazardAuditor research on computer use threats](https://arxiv.org/abs/2609.15134)
4. [OSGuard research on computer use safety](https://arxiv.org/abs/2606.15034)

## Product signals

The audit referenced public requests for parallel computer sessions and reports of unavailable controls despite a healthy setup screen. Those examples suggest useful product work. They are user reports, not a market survey or an independent measurement.

Sources

1. [Codex issue 20852](https://github.com/openai/codex/issues/20852)
2. [Codex issue 25139](https://github.com/openai/codex/issues/25139)
