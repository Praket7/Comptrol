# Client conformance

The repository owned harness checks the same MCP stdio contract using profiles named Codex, Claude Code, and Cursor. It verifies initialize, tools list, operate, and verified readiness results.

This does not claim that proprietary client applications were automated. It proves the server side contract that each client must consume. Run `python3 scripts/client_conformance.py` after building the binary.

