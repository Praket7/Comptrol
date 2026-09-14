# Adapter boundary

The runtime exposes an adapter descriptor contract for detectors, capabilities, routes, risk, and isolation.

The trusted core registry rejects duplicate names. Community adapters are not dynamically loaded into the privileged process. A future adapter host must use bounded out of process RPC and receive explicit capabilities.

No application specific adapter is advertised yet.

