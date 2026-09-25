# Adapter boundary

The runtime exposes an adapter descriptor contract for detectors, capabilities, routes, risk, and isolation. The built in registry describes the core runtime, the loopback policy bound CDP browser adapter, the bounded macOS Accessibility and LaunchServices adapters, plus isolated first-party adapter runners.

The trusted core registry rejects duplicate names. Community adapters are not dynamically loaded into the privileged process. The adapter SDK and host use bounded out of process RPC and receive explicit short lived capabilities.

Descriptors are also rejected when their identity, platform list, capability list, route, or isolation metadata is empty or malformed. Future community adapters must still run outside the trusted core and pass platform conformance before any mutation route is enabled.

Adapter directories are conformance validated, but live application availability still depends on the installed application, its user granted consent, and its official local bridge.

## First-party adapters

All adapters run out of process with loopback-only network and declared filesystem scopes. Every mutable capability declares a verification level, and CI fails when a manifest advertises an intent with no handler.

| Adapter | Route | Verification |
|---|---|---|
| VS Code (`vscode.*`) | official extension API over cross-platform IPC (Unix socket, Windows named pipe, loopback TCP fallback); SecretStorage pairing with endpoint discovery | per-intent readback; persisted-artifact stat on save |
| LibreOffice (`libreoffice.*`) | UNO bridge | UNO readback; file readback on export |
| OBS (`obs.*`) | persistent WebSocket with per-intent getter readback | `GetCurrentProgramScene` / scene-item / `GetRecordStatus` readback |
| Blender (`blender.*`) | typed main-thread bridge or exact-file offline | bpy readback live; artifact readback offline |
| DaVinci Resolve (`video.*`) | installed scripting API (Local external scripting), capability-probed per version | timeline/marker/job re-read; render artifact probe |
| Google Workspace (`document.*`, `presentation.google.*`, `presentation.text.*`) | Docs/Slides batch APIs with `WriteControl` revision binding | revision + bounded reread; exports hashed |
| PowerPoint offline (`presentation.read/batch_edit/export`) | Open XML with checkpoint | reopen readback; hash before/after |
| PowerPoint desktop (`presentation.desktop.*`, `presentation.shape.*`, `presentation.save`, `presentation.export_pdf`) | Windows COM, exact-path binding | file readback |
| Discord (`discord.message.*`) | official Bot API; user accounts via signed-in UI only, never self-bot tokens | message id/author/target/content-hash readback |
| Gmail / Graph / Apple Mail (`mail.*` with `params.provider`) | official mail APIs or published scripting | Sent-folder readback; 202 means accepted, not delivered |
| Apple Messages (`message.*`) | published scripting surface, exact chat binding | count increment + last-message match; nonce dedupe |
| Canva (`design.*`) | Connect API; Design Editing App for canvas ops (preview) | revision readback; export artifact hash |

### Creative application coverage

The checked-in Codex plugin launches the local stdio MCP process with browser CDP and Windows UI Automation enabled, while automatic Chrome startup is disabled. Other adapter families, Gmail send, creative editing, app launching, macOS Accessibility, and typed settings remain opt-in through their respective policy flags. Discord message deletion and recording remain gated by `COMPTROL_ALLOW_HIGH_CONSEQUENCE_ADAPTERS`. Browser actions require an active Comptrol browser session or configured CDP endpoint. Gmail API and Discord Bot API credentials are never extracted from signed-in apps or stored by the plugin. The Gmail API route needs `COMPTROL_GMAIL_ACCESS_TOKEN`; the Discord API route needs a bot token in `COMPTROL_DISCORD_BOT_TOKEN`, and intentionally refuses user tokens and self-bot behavior. For signed-in personal accounts, use the local browser/app UI route.

| App | Current typed editing surface | Required live/runtime dependencies | Known ceiling |
|---|---|---|---|
| Blender | Inspect objects; create common mesh primitives; set transforms and basic Principled color/metallic/roughness; delete; save; render a frame/still. Offline mutations require a separate output `.blend`. | Blender for offline; install and run the authenticated `comptrol_live_bridge.py` add-on inside the open Blender process for live editing. | Does not yet cover mesh topology, curves/text, node graphs, animation/keyframes, rigs, simulations, compositor, or video sequencing. Blender's API is broad (`bpy.data`, operators, types), so additional actions need explicit typed handlers and readback. See [Blender Python API](https://docs.blender.org/api/current/). |
| DaVinci Resolve | Project/media/timeline operations, clip append/insert, batch timeline edits, markers, exposed clip properties, and render-job lifecycle. | Resolve running with local external scripting enabled and compatible scripting modules. | Color-page grading, Fusion node editing, Fairlight/audio effects, keyframe curves, and several page-specific controls do not have typed adapter intents. Current matrix: [Resolve adapter support](../adapters/davinci-resolve/SUPPORT.md). |
| Canva | Design/page reads, exports, and validated element edits when the Design Editing app bridge is present. | Connect API OAuth for read/export; a Canva Apps SDK app running in the Canva editor for canvas changes. | Canva describes its Design Editing API as actively developing; it supports CRUD for supported element types and supported page contexts. The local Codex process cannot create an in-editor app session from the Connect API token alone. See [Canva Design Editing API](https://www.canva.dev/docs/apps/design-editing/). |
| PowerPoint | `.pptx` text and notes; slide structure; images; basic vector/text shapes; shape position/rotation; fill/line/font color and font size; export to PPTX/PDF. The Windows COM adapter has a broader live shape surface. | `python-pptx` for cross-platform file editing; LibreOffice for PDF export. Windows COM requires Windows and PowerPoint. | Current cross-platform adapter does not edit animation effects, embedded audio/video authoring, SmartArt, or every Office feature. Open XML exposes presentation structure, but a safe high-level handler still must be implemented for each operation. See [Microsoft Open XML animation documentation](https://learn.microsoft.com/en-us/office/open-xml/presentation/working-with-animation). |

“Full editing” is not a single available switch. The app's own API and local bridge determine which operations can be expressed, and the adapter must validate each request and read the resulting state back. The current changes make the existing paths discoverable and improve typed coverage; they do not claim feature parity with every application's UI.

Provider-qualified intents (`mail.*`, `presentation.slide.*`, `presentation.export`) need `params.provider`; without it the runtime refuses as ambiguous instead of guessing.

The trusted core registry rejects duplicate names. Community adapters are not dynamically loaded into the privileged process. The adapter SDK and host use bounded out of process RPC and receive explicit short lived capabilities.

Descriptors are also rejected when their identity, platform list, capability list, route, or isolation metadata is empty or malformed. Future community adapters must still run outside the trusted core and pass platform conformance before any mutation route is enabled.

Adapter directories are conformance validated, but live application availability still depends on the installed application, its user granted consent, and its official local bridge.
