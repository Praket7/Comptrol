# Protocol

The canonical operation envelope has an intent, optional target, parameters, optional postcondition, risk, idempotency key, and dry run flag.

The result is deliberately richer than a boolean. Delivery says whether a route was dispatched. Effect says whether the local state changed. Verification says whether the requested postcondition was confirmed.

The optional `background` field accepts `strict_background`, `prefer_background`, `foreground_allowed`, or `foreground_required`. Strict background refuses application launch because it may activate the desktop. Browser opening in strict background must use the CDP background target path. Successful routes report foreground, mouse, clipboard, and posture fields in their disturbance record.

The default MCP tools are operate, inspect, watch, and capabilities. Unsupported intents return a structured error and do not mutate a target.

Stdio calls that provide an MCP progress token receive bounded start and completion progress notifications around the returned operation result. The operation id remains the durable recovery identity.

The stdio server also supports a durable completed task subset when the client advertises the Tasks extension and sends a task request. It persists the task handle and final result across restart and serves `tasks/get`, `tasks/result`, and `tasks/list`. Long running asynchronous execution and task cancellation remain unavailable and are refused rather than implied.

The loopback HTTP transport binds only to localhost. It assigns a random session id after initialization and requires that id for later MCP requests. It supports origin validation, session deletion, concurrent long lived server sent event streams, persistent session state under the configured state directory, bounded event replay with `Last-Event-ID`, and a bounded connection count. It remains local because it does not provide remote authentication.

The session event history is bounded to 256 messages and sessions expire after 24 hours. A stream that is idle for five minutes closes while the session remains durable. Set `COMPTROL_HTTP_STREAM_IDLE_MS` to change that bounded idle period. The server writes the session file atomically before acknowledging a new session, event, or deletion.

The native daemon mode uses a versioned length framed Unix socket on macOS and Linux, and a local named pipe on Windows. It provides health checks and forwards MCP messages through the same runtime while preserving ordered progress events. Local transports reject oversized frames.

On Windows the native daemon uses the local named pipe `\\.\pipe\comptrol`, with `COMPTROL_PIPE_NAME` available for an explicit name. The npm bridge reconnects through that pipe without replaying an already sent request.

`command.run` is an optional R3 intent. It requires `COMPTROL_ALLOW_COMMANDS=1`, an explicit `COMPTROL_COMMAND_ROOT`, and an exact executable in `COMPTROL_COMMAND_ALLOWLIST`. It accepts a structured program and argv, never invokes a shell, limits output to 64 KiB per stream, bounds execution to 60 seconds, and verifies an optional `exit_code` postcondition. A timeout is durable unknown and must be observed before retrying.
