# Apple Mail adapter

The adapter drives Apple Mail on macOS through fixed AppleScript templates executed as subprocess argv (`/usr/bin/osascript -e`, never a shell). Run `python3 src/adapter.py` as the isolated host child; it speaks the shared framed RPC on stdio.

The model never supplies script source. Payloads carry typed data only (account name, recipients, single-line subject, body, attachment paths), which the adapter quotes and escapes into closed template slots; any script-source keys are refused. Attachments must resolve under `COMPTROL_MAIL_ASSETS_ROOT`, must already exist, are capped at 10 MB each, and are reported with size plus sha256. Intents are `mail.draft` (R2), `mail.send` (R3), `mail.search` (R0), and `mail.read` (R0). Outside macOS every intent returns unsupported. Denied Automation permission returns `automation_permission_required` with a remediation hint.
