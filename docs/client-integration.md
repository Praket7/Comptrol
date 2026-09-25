# Client integration

For the npm installation, configure the MCP command as `comptrolling` with no arguments. The launcher starts the bundled native runtime and keeps the stdio connection alive. If you build from source, configure the full path to the compiled `comptrol` binary with the argument `mcp` instead.

The server speaks newline delimited JSON on standard input and standard output. Logs go to standard error. The same binary can be used by Codex, Claude Code, Cursor, and any other client that supports MCP stdio.

The npm launcher keeps the client pipe open across a bounded number of unexpected native process exits. Durable operation state remains in the configured Comptrol state directory, so a client can retry an idempotent request after reconnect. The launcher stops after three restarts and reports the loss on standard error.

Set `COMPTROL_DAEMON=1` to route the launcher through the local versioned daemon transport. macOS and Linux use the Unix socket; Windows uses the local named pipe. The launcher starts the daemon, keeps the MCP stdio contract for the client, reconnects within the same bounded restart budget, and does not replay a request that was already sent without a response.

Do not add a client configuration that grants shell access or points the preview HTTP server at a non loopback address.

`comptrol integrate --list` reports known client config locations without changing them. Cursor and Claude Code JSON configs can be proposed with `comptrol integrate --client cursor --config PATH` or `comptrol integrate --client claude-code --config PATH`. Codex TOML can be proposed with `comptrol integrate --client codex --config PATH`. Add `--apply` to create a timestamped backup and atomically write the merged configuration. Use `comptrol integrate --undo --config PATH` to restore the newest backup. Unknown JSON fields and unrelated TOML tables are preserved. Symlinked configs are refused.
