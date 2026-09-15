# Comptrol Closed Groups extension

This Manifest V3 extension is the Chrome-native bridge for closed tab-group restoration. CDP cannot read Chrome's recently closed session list, so the extension uses the supported `sessions` API and restores only a uniquely matched session.

## Install in the signed-in profile

1. Open `chrome://extensions` in the profile that owns the groups.
2. Enable Developer mode.
3. Choose Load unpacked and select this directory.
4. Leave the extension installed while groups are open so it can record their members.
5. Open the extension action to select a named group for restoration.

The extension requests only `sessions`, `tabs`, `tabGroups`, `storage`, and `alarms`. It does not read page contents, cookies, passwords, or credentials. It refuses missing, ambiguous, or membership-mismatched restores.

The snapshot history is profile-local Chrome extension storage and is capped at 25 entries.
