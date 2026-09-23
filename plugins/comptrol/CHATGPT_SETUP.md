# Install Comptrol on your computer

These steps build the current Comptrol runtime from this repository and connect it to local Codex. It runs over standard input/output and does not need an API key, cloud account, or tunnel.

## 1. Install the runtime

Install Git and Rust using the instructions at [rustup.rs](https://rustup.rs/), then build Comptrol from this repository:

```sh
git clone https://github.com/Praket7/Comptrol.git
cd Comptrol
cargo build --release
```

The executable is `target/release/comptrol` on macOS/Linux and `target/release/comptrol.exe` on Windows. The latest npm package may lag behind this repository; building this checkout ensures you get the code and adapters on its current branch.

## 2. Install the plugin in Codex

In `plugins/comptrol/.mcp.json`, set `command` to the absolute path of the executable you just built and leave `args` as `["mcp"]`. Then add the local plugin at `plugins/comptrol` from Codex's plugin manager. If you use the portable manifest, make the same change in `mcp.json`. Restart Codex after installing or changing the configuration. The runtime finds the bundled `adapters/` directory beside this repository's `target/` build automatically.

## 3. Allow macOS Accessibility access

This permission is required for Comptrol's supported semantic UI actions in Mac apps. App launching and other routes that do not use Accessibility can work without it.

1. Use the full path of the native executable you built: `<repository>/target/release/comptrol`.
2. Open **System Settings → Privacy & Security → Accessibility**.
3. Click **+**, select the `comptrol` executable at that path, and turn its switch on. Authenticate with a Mac administrator account if macOS requests it.
4. Restart Codex so the local MCP process picks up the permission.

Add the Comptrol executable itself. Granting permission to Terminal, `uv`, Codex, or ChatGPT does not grant it to the separate Comptrol process. Do not try to bypass a Mac administrator prompt; an administrator must approve the change. If you cannot grant it, Accessibility-based actions will be unavailable, while other permitted routes may still work.

## 4. Check the connection

In Codex, ask Comptrol to report its capabilities. Check that the local runtime is connected and that the routes you need are available. Capabilities depend on your operating system, installed applications, app-specific bridges, and permissions. For example, browser page control needs a Comptrol Browser Bridge session or a configured local Chrome DevTools endpoint; merely opening a URL does not enable page control.

PowerPoint file editing uses an optional Python dependency. From the repository root, install it into Comptrol's isolated adapter environment:

```sh
python3 -m venv ~/.comptrol/venv
~/.comptrol/venv/bin/python -m pip install -r adapters/powerpoint/requirements.txt
```

## ChatGPT website limitation

This local stdio plugin is for Codex. ChatGPT in a browser cannot launch a program on your computer from this configuration; ChatGPT custom apps connect to a remote MCP server. Using Comptrol from ChatGPT web requires a separately configured remote connection such as Secure MCP Tunnel, plus a plan that supports the requested MCP actions. [OpenAI's current plan guidance](https://help.openai.com/en/articles/12584461-developer-mode-and-mcp-apps-in-chatgpt) says full write-capable MCP support is rolling out to Business, Enterprise, and Edu; check that page for current availability. This repository setup does not configure or publish that connection.
