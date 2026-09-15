# Adapter boundary

The runtime exposes an adapter descriptor contract for detectors, capabilities, routes, risk, and isolation. The built in registry describes the core runtime, the loopback policy bound CDP browser adapter, the bounded macOS Accessibility and LaunchServices adapters, plus isolated VS Code, LibreOffice, OBS, and Blender adapter runners.

The trusted core registry rejects duplicate names. Community adapters are not dynamically loaded into the privileged process. The adapter SDK and host use bounded out of process RPC and receive explicit short lived capabilities.

Descriptors are also rejected when their identity, platform list, capability list, route, or isolation metadata is empty or malformed. Future community adapters must still run outside the trusted core and pass platform conformance before any mutation route is enabled.

Adapter directories are conformance validated, but live application availability still depends on the installed application, its user granted consent, and its official local bridge.
