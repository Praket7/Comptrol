# Support boundary

Mail drafts, sends, subject/sender searches in a named mailbox, and single-message reads by mailbox plus subject are supported. At most 10 attachments of at most 10 MB each are accepted per compose, and only from under `COMPTROL_MAIL_ASSETS_ROOT`. Arbitrary AppleScript, `do shell script`, mailbox moves and deletes, rule management, and any non-macOS platform are unsupported. Sent-mailbox names vary by provider and may not be scriptable; when the readback cannot observe the sent message, the adapter reports honest delivery-only state instead of claiming verification.
