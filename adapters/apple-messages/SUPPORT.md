# Support boundary

Staged message drafts, sends to an exact participant handle or chat GUID, one attachment per message from under `COMPTROL_MAIL_ASSETS_ROOT`, and client-nonce duplicate-send protection per chat are supported. Bodies are capped at 2000 characters. Group-chat creation, tapbacks and reactions, message deletion or editing, reading chat history beyond the verification readback, arbitrary AppleScript, `do shell script`, and any non-macOS platform are unsupported. Drafts never leave the local state directory; only `message.send` transmits.
