# Adapter authoring

Comptrol application adapters are untrusted code and run outside the trusted core. Each adapter directory contains a versioned `adapter.toml`, a support boundary, a threat model, and an independent verification contract.

The SDK uses bounded length prefixed JSON frames. Every request carries a protocol version, adapter instance, request id, deadline, resource scope, and method. `execute` and `verify` requests require an ephemeral token scoped to the declared adapter capability, resource, and operation. The host rejects malformed manifests, oversized frames, expired tokens, mismatched identities, and undeclared capabilities before dispatch.

The first party adapters are VS Code through an authenticated loopback bridge using the official Extension API, LibreOffice through UNO, OBS Studio through OBS WebSocket 5.x, and Blender through a closed typed operation schema using the official background entry point.

Presence of an adapter directory does not imply that the application is installed or consent has been granted. Each runner reports `unsupported`, `requires_consent`, `degraded`, or `unhealthy` instead of claiming availability.

Validate a manifest with `comptrol adapter validate adapters/vscode/adapter.toml`. Create a non-authoritative scaffold with `comptrol adapter scaffold my-adapter`. The scaffold is intentionally incomplete and must not advertise mutation capabilities until its backend, postconditions, and threat model are implemented.

