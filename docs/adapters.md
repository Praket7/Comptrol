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

Provider-qualified intents (`mail.*`, `presentation.slide.*`, `presentation.export`) need `params.provider`; without it the runtime refuses as ambiguous instead of guessing.

The trusted core registry rejects duplicate names. Community adapters are not dynamically loaded into the privileged process. The adapter SDK and host use bounded out of process RPC and receive explicit short lived capabilities.

Descriptors are also rejected when their identity, platform list, capability list, route, or isolation metadata is empty or malformed. Future community adapters must still run outside the trusted core and pass platform conformance before any mutation route is enabled.

Adapter directories are conformance validated, but live application availability still depends on the installed application, its user granted consent, and its official local bridge.
