# Client conformance

The repository owned harness checks the same MCP stdio contract using profiles named Codex, Claude Code, and Cursor. It verifies initialize, tools list, operate, and verified readiness results.

This does not claim that proprietary client applications were automated. It proves the server side contract that each client must consume. Run `python3 scripts/client_conformance.py` after building the binary.

With Node.js and the global npm package installed, run `python scripts/npm_mcp_conformance.py` to exercise the packaged plugin launcher, resolve the actual `comptrolling` entry point, enumerate its registered tools, and verify `system.ping` through the launched server.

