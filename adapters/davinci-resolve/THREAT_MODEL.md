# Threat model

Scripting surface: the adapter attaches only to the already-running Resolve process
through the local external-scripting session (`scriptapp("Resolve")`). It probes
module availability read-only, never enables network scripting, never passes a host
or port, and never writes configuration to turn remote control on. If Resolve is not
running or Local external scripting is disabled, every intent fails closed with
`resolve_unreachable` instead of attempting a broader channel.

Project identity: project open/create selects the current project explicitly by
validated name, and every project, media, timeline, and render call re-resolves the
current project handle first. The adapter never guesses across project libraries and
never touches multi-user collaboration state.

No arbitrary code: request payloads supply only typed scalars from closed sets
(names, 1-based track indices, file paths with allowlisted suffixes, marker colors,
property keys, render-setting keys). The bridge maps them to fixed Resolve API calls;
there is no script-text parameter, no exec/eval path, and unknown render-setting keys
are rejected before `SetRenderSettings` is called. Credentials are not handled: the
only environment input is the optional `COMPTROL_RESOLVE_SCRIPTING_DIR` override for
locating the vendor scripting modules.
