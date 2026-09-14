# Client integration

Run the compiled binary with the command `comptrol mcp` as the MCP server command.

The server speaks newline delimited JSON on standard input and standard output. Logs go to standard error. The same binary can be used by Codex, Claude Code, Cursor, and any other client that supports MCP stdio.

Do not add a client configuration that grants shell access or points the preview HTTP server at a non loopback address.

`comptrol integrate --list` reports known client config locations without changing them. Cursor and Claude Code JSON configs can be proposed with `comptrol integrate --client cursor --config PATH` or `comptrol integrate --client claude-code --config PATH`. Add `--apply` to create a timestamped backup and atomically write the merged JSON. Use `comptrol integrate --undo --config PATH` to restore the newest backup. Codex TOML is detected but deliberately refused by this JSON only writer.
