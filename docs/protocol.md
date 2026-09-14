# Protocol

The canonical operation envelope has an intent, optional target, parameters, optional postcondition, risk, idempotency key, and dry run flag.

The result is deliberately richer than a boolean. Delivery says whether a route was dispatched. Effect says whether the local state changed. Verification says whether the requested postcondition was confirmed.

The default MCP tools are operate, inspect, watch, and capabilities. Unsupported intents return a structured error and do not mutate a target.

`command.run` is an optional R3 intent. It requires `COMPTROL_ALLOW_COMMANDS=1`, an explicit `COMPTROL_COMMAND_ROOT`, and an exact executable in `COMPTROL_COMMAND_ALLOWLIST`. It accepts a structured program and argv, never invokes a shell, limits output to 64 KiB per stream, bounds execution to 60 seconds, and verifies an optional `exit_code` postcondition. A timeout is durable unknown and must be observed before retrying.
