# Google Workspace adapter

The adapter drives Google Docs and Google Slides through the official HTTPS REST APIs (Docs v1, Slides v1, Drive v3 for export) with the standard library only. Run `python3 src/adapter.py` as the isolated host child; it speaks the shared framed RPC on stdio.

Authentication is a user OAuth bearer token read only from `COMPTROL_GOOGLE_ACCESS_TOKEN` at request time. The token is never stored, never logged, and never copied into responses or errors. Optional `COMPTROL_GOOGLE_DOCS_BASE`, `COMPTROL_GOOGLE_SLIDES_BASE`, and `COMPTROL_GOOGLE_DRIVE_BASE` overrides exist so verification can run against a loopback stub; production defaults to the official endpoints. Exports persist under `COMPTROL_GOOGLE_EXPORT_DIR` and return only size plus sha256, never file bytes.
