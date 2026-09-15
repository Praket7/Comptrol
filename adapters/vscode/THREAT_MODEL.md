# Threat model

The adapter is untrusted application code. The host validates the manifest, frame size, request identity, capability token, resource scope, and deadline before dispatch. The bridge must reject non-loopback peers and must not expose arbitrary JavaScript evaluation.

