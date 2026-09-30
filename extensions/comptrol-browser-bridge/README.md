# Comptrol Browser Bridge

This Manifest V3 extension connects the Comptrol daemon to the user's existing Chrome session through Chrome Native Messaging. It uses the `debugger` API for the currently supported page protocol operations and never starts Chrome with a remote-debugging port.

## Permission scope

The extension requests `debugger`, `tabs`, `tabGroups`, `sessions`, `storage`, `alarms`, `nativeMessaging`, and `downloads`, which are used by the service worker. It does not request `activeTab`, `scripting`, or website-wide host permissions. The debugger permission still grants broad page-inspection and page-control ability; Chrome displays its own installation warning, and the user must review and authorize the extension in Chrome.

The native host is separately authorized for one exact extension ID through its `allowed_origins` entry. It authenticates to the local Comptrol daemon with a per-user token and uses a loopback HTTP endpoint. Command delivery uses an authenticated, bounded 800 ms long poll (with a 20 ms daemon-side queue check), rather than sleeping 200 ms before every request. Daemon failures use capped backoff. Registration state alone is not evidence that the extension is installed, connected, authorized, or ready.

## Setup boundary

1. Load the unpacked extension from the directory printed by `comptrol-browser-setup --print-extension-path` using Chrome's Extensions page. Select the extension folder itself, whose top level contains `manifest.json`, not its parent. Chrome rejects Python-generated `__pycache__` directories and `.pyc` files; when loading from a source checkout, prepare a clean copy that excludes `__pycache__`, `.pyc`, and `.pyo` files. The source `.gitignore` excludes them from Git, but Chrome still sees them on disk.
2. Copy the resulting exact extension ID from Chrome and run `comptrol-browser-setup --extension-id <ID>` in the intended user's environment.
3. Restart the extension service worker or Chrome, then use Comptrol's bridge health/doctor route to verify the authenticated handshake and an ordinary web target.
4. Use `comptrol-browser-setup --help` for the setup options. A verified rollback command remains a release-gate requirement and is not implemented yet.

The installer changes the current user's native-host registration and local bridge files. It must not be run as part of fixture tests. This repository contains no installed extension ID or native-host token. Extension installation, permission acceptance, live profile authorization, restart recovery, and revocation have not yet been verified in this task.

Protected Chrome pages, store pages, browser policy, and any fresh browser permission prompt may remain unavailable. The optional direct-CDP route retains Chrome's actual consent behavior and does not bypass it.
