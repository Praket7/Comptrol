# VS Code adapter

This adapter uses a small authenticated local bridge implemented by the official VS Code Extension API. The out of process runner never receives arbitrary extension host code or an unrestricted terminal command. The bridge must be installed and explicitly started by the user before the adapter becomes available.

Run `python3 src/adapter.py` as the adapter host child. Without `COMPTROL_VSCODE_BRIDGE_SOCKET`, probing reports `requires_consent` or `unsupported` and does not pretend that VS Code is connected.

