# Remote pairing

Comptrol now has a local pairing state machine for consent records. Use `comptrol pair show` to create a high entropy short lived code with explicit scopes. Use `comptrol pair accept CODE` once on the receiving side and `comptrol pair revoke PAIRING_ID` to revoke it immediately. `comptrol pair list` reports scope, expiry, acceptance, and revocation state without revealing stored codes.

Codes are stored as SHA 256 hashes in the local state directory. They are one time values, have bounded expiry, and never grant more scope than the caller selected. The default scope is observe. Terminal and file mutation scopes require explicit selection.

Pairing records do not enable a network listener. The opt in `serve-mtls` transport now provides certificate-authenticated HTTP/MCP using `COMPTROL_MTLS_CERT`, `COMPTROL_MTLS_KEY`, and `COMPTROL_MTLS_CLIENT_CA`; it must still be combined with pairing/session scope enforcement before being treated as a complete remote-control product. The ordinary HTTP preview and browser endpoint remain loopback only.
