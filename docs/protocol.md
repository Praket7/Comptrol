# Protocol

The canonical operation envelope has an intent, optional target, parameters, optional postcondition, risk, idempotency key, and dry run flag.

The result is deliberately richer than a boolean. Delivery says whether a route was dispatched. Effect says whether the local state changed. Verification says whether the requested postcondition was confirmed.

The optional `background` field accepts `strict_background`, `prefer_background`, `foreground_allowed`, or `foreground_required`. Strict background refuses application launch because it may activate the desktop. Browser opening in strict background must use the CDP background target path. Successful routes report foreground, mouse, clipboard, and posture fields in their disturbance record.

The default MCP tools are operate, inspect, watch, and capabilities. Unsupported intents return a structured error and do not mutate a target.

Stdio calls that provide an MCP progress token receive bounded start and completion progress notifications around the returned operation result. The operation id remains the durable recovery identity. The current runtime does not claim the MCP Tasks extension for long running work.

`command.run` is an optional R3 intent. It requires `COMPTROL_ALLOW_COMMANDS=1`, an explicit `COMPTROL_COMMAND_ROOT`, and an exact executable in `COMPTROL_COMMAND_ALLOWLIST`. It accepts a structured program and argv, never invokes a shell, limits output to 64 KiB per stream, bounds execution to 60 seconds, and verifies an optional `exit_code` postcondition. A timeout is durable unknown and must be observed before retrying.
