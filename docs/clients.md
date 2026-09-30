# Client setup

Comptrol is a local MCP server. Every client below talks to the same runtime,
Client configuration support and a live client handshake are separate checks. Run `comptrol setup` once: it
writes each client's config (preserving any file that already exists), registers
the Chrome native-messaging host, and prints the one manual step left.

The generated config starts `comptrol mcp` over stdio. Resident sharing requires
the npm daemon launcher; setting `COMPTROL_DAEMON` on the native executable alone does not enable it. To print
a config without writing it: `comptrol setup --print "<client>"`.

## Freebuff

`comptrol setup` writes `~/.freebuff/mcp.json`:

```json
{
  "mcpServers": {
    "comptrol": {
      "command": "/absolute/path/to/comptrol",
      "args": ["mcp"],
      "env": {}
    }
  }
}
```

Restart Freebuff (or reload its MCP servers), then ask it to run `system.ping`.
A working connection returns `ready: true`, `verification: verified`.

## Claude Code

`comptrol setup` writes `~/.claude.json` in the same `mcpServers` shape.
If you prefer the CLI, register it directly:

```sh
claude mcp add comptrol -- /absolute/path/to/comptrol mcp
```

Claude Code reads `~/.claude.json` too; if you keep servers there, copy the
`comptrol` block from `~/.claude.json` into its `mcpServers` object.

## Codex

`comptrol setup` writes `~/.codex/config.toml` (TOML, not JSON):

```toml
[mcp_servers.comptrol]
command = "/absolute/path/to/comptrol"
args = ["mcp"]
env = {}
```

## OpenCode

`comptrol setup` writes `~/.config/opencode/opencode.json` for OpenCode v2,
using `mcp.servers.comptrol`, `type: "local"`, and a command array containing
the executable and `mcp`. OpenCode v1 uses `mcp.comptrol` instead; move the
entry out of `servers` when using that version. A live OpenCode handshake
must still be checked on the installed client.

References: [OpenCode v2 MCP](https://opencode.ai/v2/docs/mcp-servers),
[Claude Code configuration](https://code.claude.com/docs/en/mcp), and
[Codex MCP](https://developers.openai.com/codex/mcp/).

## Verify

With any client connected, run two harmless checks:

1. `inspect` with `kind: doctor` — separates local configuration from routes
   that have actually passed a live check.
2. `system.ping` — returns `ready: true`, `verification: verified` when the
   client reached the runtime.

Neither touches an app or grants permission to change anything.

## Permissions

Comptrol starts with change routes gated off. Each `COMPTROL_ALLOW_*` gate is
listed by `comptrol capabilities` in the [support matrix](support.md).
Enable only what a task needs by setting the gate in the client config's `env`
block. On macOS, grant Accessibility (System Settings → Privacy & Security →
Accessibility) before using control-reading routes.

## Notes

- Do not point a client config at anything that grants shell access, and never
  bind the preview HTTP server to a non-loopback address.
- `comptrol integrate --list` reports known client config paths without
  changing them; `--apply` writes a merged config behind a timestamped backup,
  and `--undo` restores it. Unknown fields and unrelated tables are preserved.
