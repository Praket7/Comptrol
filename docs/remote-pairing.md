# Remote pairing

Comptrol now has a local pairing state machine for consent records. Use `comptrol pair show` to create a high entropy short lived code with explicit scopes. Use `comptrol pair accept CODE` once on the receiving side and `comptrol pair revoke PAIRING_ID` to revoke it immediately. `comptrol pair list` reports scope, expiry, acceptance, and revocation state without revealing stored codes.

Codes are stored as SHA 256 hashes in the local state directory. They are one time values, have bounded expiry, and never grant more scope than the caller selected. The default scope is observe. Terminal and file mutation scopes require explicit selection.

Pairing records do not enable a network listener. Remote control remains disabled until the transport has mutual TLS, session pinning, and a native consent path. The HTTP preview and browser endpoint remain loopback only.
