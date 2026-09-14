# Client integration

Run the compiled binary with the command `comptrol mcp` as the MCP server command.

The server speaks newline delimited JSON on standard input and standard output. Logs go to standard error. The same binary can be used by Codex, Claude Code, Cursor, and any other client that supports MCP stdio.

Do not add a client configuration that grants shell access or points the preview HTTP server at a non loopback address.

