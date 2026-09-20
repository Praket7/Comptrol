# Microsoft Graph Mail adapter

The adapter drives Outlook/Microsoft 365 mail through the official Microsoft Graph v1.0 mail surface (/me/messages, /me/sendMail, /me/mailFolders/sentitems/messages) with the standard library only. Run `python3 src/adapter.py` as the isolated host child; it speaks the shared framed RPC on stdio.

Authentication is a user OAuth bearer token read only from `COMPTROL_GRAPH_ACCESS_TOKEN` at request time. The token is never stored, never logged, and never copied into responses or errors. Optional `COMPTROL_GRAPH_API_BASE` override exists so verification can run against a loopback stub; production defaults to `https://graph.microsoft.com/v1.0`. Intents are `mail.draft` (R2), `mail.send` (R3), `mail.search` (R0), and `mail.read` (R0). Attachments are existing local files capped at 10 MB each, base64-encoded into fileAttachment payloads, and reported with size plus sha256.
