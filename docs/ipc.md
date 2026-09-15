# Local IPC

The native daemon exposes a Unix domain socket on macOS and Linux. The default path is the local Comptrol state directory followed by `comptrol.sock`. `COMPTROL_SOCKET_PATH` can select an explicit path. Windows uses the local named pipe `\\.\pipe\comptrol`, or the name in `COMPTROL_PIPE_NAME`.

Each frame begins with a four byte big endian length followed by one bounded JSON envelope. The envelope contains version one, an id, and a method. The health method returns daemon readiness. The mcp method forwards one JSON RPC message through the same runtime used by stdio and returns ordered event frames followed by the response frame.

The local transport is restricted to the current machine. Frames larger than the shared protocol limit are rejected before parsing. Unknown versions and methods return structured errors.

On Windows the same framing runs over the local named pipe `\\.\pipe\comptrol`, or the pipe named by `COMPTROL_PIPE_NAME`. The npm daemon bridge uses that pipe automatically on Windows.
