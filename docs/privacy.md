# Privacy and network boundaries

Comptrol does not enable telemetry or automatic update checks. Core startup does not contact a network service. Local MCP stdio, local state, audit records, and the loopback dashboard stay local.

The `comptrol privacy status` command reports these defaults and the data classes redacted from audit and privacy minimal trace records.

The `comptrol privacy network-endpoints` command reports every optional network route known to this build. Browser CDP is listed only when `COMPTROL_CDP_ENDPOINT` is configured and is used only for an explicit browser action. Pairing records can be created locally with a short lived scope and revocation state. Remote transport remains disabled until mutual TLS is configured. Update delivery and telemetry are not implemented or configured.

MCP and browser protocol messages are bounded to one mebibyte. Oversized messages are refused before JSON parsing or browser response handling.
