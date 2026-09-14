# Policy

Policy runs below the MCP adapter. The agent cannot grant itself a new capability.

The default policy allows system ping and desktop observation. Setting `COMPTROL_ALLOW_SANDBOX_WRITES=1` enables writes below the Comptrol sandbox. Setting `COMPTROL_ALLOW_DESKTOP_NOTIFY=1` enables the notification route on macOS.

Those environment switches are intentionally coarse for the foundation release. A file based policy with per application and per path scopes is required before broader desktop mutation is enabled.

