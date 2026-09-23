# Remote pairing

Comptrol now has a local pairing state machine for consent records. Use `comptrol pair show` to create a high entropy short lived code with explicit scopes. Use `comptrol pair accept CODE` once on the receiving side and `comptrol pair revoke PAIRING_ID` to revoke it immediately. `comptrol pair list` reports scope, expiry, acceptance, and revocation state without revealing stored codes.

Codes are stored as SHA 256 hashes in the local state directory. They are one time values, have bounded expiry, and never grant more scope than the caller selected. The default scope is observe. Terminal and file mutation scopes require explicit selection.

Pairing records do not enable a network listener. The opt-in `serve-mtls` transport provides certificate-authenticated HTTP/MCP using `COMPTROL_MTLS_CERT`, `COMPTROL_MTLS_KEY`, and `COMPTROL_MTLS_CLIENT_CA`. It requires an accepted pairing bound to the peer certificate fingerprint, checks the required scope for each MCP method or tool intent, and requires a fresh nonce for mutations. Unpaired certificates are refused unless `COMPTROL_MTLS_AUTO_PAIR=1` is explicitly enabled; that setting automatically grants the new identity the broad built-in scope set and is intended only for controlled private testing. The ordinary HTTP preview and browser endpoint remain loopback only.

The mTLS transport is a building block, not a hosted remote-control service: certificate provisioning and rotation, public endpoint hosting, release provenance, and end-to-end deployment guidance remain separate work.
