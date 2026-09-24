# Local Codex setup

This folder connects the Comptrol runtime to Codex on your computer. It does not connect Comptrol to ChatGPT in a browser.

For the npm setup, macOS permission steps, browser connection, and hosted ChatGPT limits, see the [start guide](../../docs/start.md).

To build directly from this repository, install Rust, run `cargo build --release`, then set the command in `.mcp.json` to the full path of `target/release/comptrol`. Set the arguments to `mcp`. Restart Codex after saving the file.

On macOS, semantic control needs Accessibility permission for the Comptrol executable itself. Open System Settings, choose Privacy and Security, then Accessibility. Add that executable. Restart Codex afterward. Do not grant permission to Terminal in place of Comptrol.

Begin with a read only capabilities check. App control depends on the operating system, app installation, local policy, and any app specific setup. A listed route does not prove live support on your computer.
