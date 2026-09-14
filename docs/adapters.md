# Adapter boundary

The runtime exposes an adapter descriptor contract for detectors, capabilities, routes, risk, and isolation. The built in registry describes the core runtime, the loopback policy bound CDP browser adapter, the bounded macOS Accessibility adapter, and the explicit macOS LaunchServices adapter.

The trusted core registry rejects duplicate names. Community adapters are not dynamically loaded into the privileged process. A future adapter host must use bounded out of process RPC and receive explicit capabilities.

Descriptors are also rejected when their identity, platform list, capability list, route, or isolation metadata is empty or malformed. Future community adapters must still run outside the trusted core and pass platform conformance before any mutation route is enabled.

No application specific adapter is advertised yet.
