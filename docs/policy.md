# Policy

Policy runs below the MCP adapter. The agent cannot grant itself a new capability.

The default policy allows system ping, desktop observation, platform broker observation, opening an exact installed app with `app.launch`, and opening a URL in the existing default Chrome profile with `browser.chrome.open_tab`. The open routes are individually allowlisted; they do not grant general desktop input, app-resource access, browser DOM mutation, or file writes. Setting `COMPTROL_ALLOW_SANDBOX_WRITES=1` enables writes and regular file copies below the Comptrol sandbox. Setting `COMPTROL_ALLOW_DESKTOP_NOTIFY=1` enables the notification route on macOS.

Those environment switches are intentionally coarse for the foundation release. A file based policy with per application and per path scopes is required before broader desktop mutation is enabled.
