# Local Codex setup

This folder connects the Comptrol runtime to Codex on your computer. It does not connect Comptrol to ChatGPT in a browser.

For the npm setup, macOS permission steps, browser connection, and hosted ChatGPT limits, see the [start guide](../../docs/start.md).

Install the runtime with `npm install -g comptrolling`. From the repository root, register and install the Codex plugin with `codex plugin marketplace add .` and `codex plugin add comptrol@personal`. Check `codex plugin list` for `installed, enabled`, then restart the Codex desktop app so the plugin MCP tools load in new tasks. Enable the bundled server in `~/.codex/config.toml` if it is not already enabled:

```toml
[plugins."comptrol@personal".mcp_servers.comptrol-local]
enabled = true
```

The package opts into browser CDP and Windows UI Automation, and disables automatic Chrome startup. UI Automation remains limited to Windows and the exact process and control supplied in each request.

If you build directly from this repository, install Rust and run `cargo build --release`. Update both `mcp.json` and `.mcp.json` to launch the full path to `target/release/comptrol` with `args` set to [`mcp`]; retain `"type": "stdio"` in portable `mcp.json`.

On macOS, semantic control needs Accessibility permission for the Comptrol executable itself. Open System Settings, choose Privacy and Security, then Accessibility. Add that executable. Restart Codex afterward. Do not grant permission to Terminal in place of Comptrol.

Begin with a read only capabilities check. App control depends on the operating system, app installation, local policy, and any app specific setup. A listed route does not prove live support on your computer.
