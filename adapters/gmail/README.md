# Gmail adapter

The adapter drives Gmail through the official Gmail REST API (users.drafts, users.messages) with the standard library only. Run `python3 src/adapter.py` as the isolated host child; it speaks the shared framed RPC on stdio.

Authentication is a user OAuth bearer token read only from `COMPTROL_GMAIL_ACCESS_TOKEN` at request time. The token is never stored, never logged, and never copied into responses or errors. Optional `COMPTROL_GMAIL_API_BASE` override exists so verification can run against a loopback stub; production defaults to `https://gmail.googleapis.com`. Intents are `mail.draft` (R2), `mail.send` (R3), `mail.search` (R0), and `mail.read` (R0). Attachments are existing local files capped at 10 MB each, base64-encoded into the MIME body, and reported with size plus sha256.
