# DaVinci Resolve adapter

Typed control of the running DaVinci Resolve application through its official
local external-scripting session (`DaVinciResolveScript`, `scriptapp("Resolve")`).
The bridge locates the installed scripting modules dynamically on Windows, macOS,
and Linux without pinning any Resolve version, then exposes one typed function per
intent: project open/create/save, media import and bins, timeline list/open/create,
clip append/insert, batched timeline edits, markers, clip property get/set, and the
render preset/configure/queue/start/status/cancel lifecycle.

Payloads carry only closed scalar arguments (names, indices, paths, enums). The
adapter never builds scripts from request data and never uses exec/eval. Every
mutation is confirmed by a live readback (timeline items, markers, properties, job
status); project saves verify database persistence and render starts verify job
status plus the output artifact (exists, nonzero size, extension check).
