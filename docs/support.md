# Support matrix

What is actually verified, per route family. Comptrol separates *implemented*
from *live-verified*: a route is only marked verified here when a gate has run
it against the real app and observed the result. Everything else is labeled
honestly rather than claimed.

Status key:

- **live-verified** — a gate ran the route against the real app and read the
  result back (artifact, readback, or independent reopen).
- **compile-verified** — the route is implemented and unit-tested, and builds on
  its target OS, but the live gate has not yet run on this machine.
- **gated** — implemented but requires a credential, an installed app, or a
  human step before the live gate can run.
- **absent** — not implemented.

## Browser (Chrome)

| Route family | Status | Evidence |
| --- | --- | --- |
| CDP session + discovery | live-verified | `chrome_lazy_conformance.py` |
| Lazy windowless start (`--no-startup-window`) | live-verified | `chrome_lazy_conformance.py` (no `about:blank` window) |
| CDP navigate / click / fill / read | live-verified | `chrome_conformance.py`, `command_conformance.py` |
| Extension bridge (native messaging) | live-verified | handshake + challenge round trip, channel `active` |
| Session reuse via `DevToolsActivePort` | live-verified | `chrome_lazy_conformance.py` |

## Desktop (three OSes)

| Route family | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Semantic press / fill / inspect | live-verified (`windows_uia_conformance.py`) | compile-verified (AX, `uia.rs` parity) | compile-verified (AT-SPI, `uia.rs` parity) |
| `match_index` ordinal targeting | live-verified | compile-verified | compile-verified |
| Post-action verification readback | live-verified | compile-verified | compile-verified |
| Keyboard dispatch (`key_sequence`) | live-verified | compile-verified | compile-verified |
| ms-settings / permission surfaces | live-verified (`settings_conformance.py`) | compile-verified | compile-verified |
| Terminal (`desktop.terminal`) | live-verified (`terminal_conformance.py`) | gated (needs macOS run) | gated (needs Linux run) |
| File explorer (`desktop.explorer`) | live-verified (`terminal_conformance.py --reveal`) | gated | gated |

## Adapters

| Adapter | Status | Evidence |
| --- | --- | --- |
| Blender | live-verified | `blender_live_conformance.py` — rocket recipe, `.blend` + `.png` artifacts, independent reopen |
| PowerPoint (COM) | live-verified | `powerpoint_live_conformance.py` — save + reopen-verify, SHA readback, macro/scope refusal |
| Canva | gated | `canva_conformance.py` — honest contract always; live CRUD needs `COMPTROL_CANVA_ACCESS_TOKEN` |
| Terminal / Explorer | live-verified | `terminal_conformance.py` |
| LibreOffice, DaVinci Resolve, OBS, Fusion | gated | need the app installed; doctor reports honest state |
| Gmail, Google Workspace, Discord, Apple Mail/Messages | gated | need service tokens / macOS |

## Setup

| Capability | Status | Evidence |
| --- | --- | --- |
| `comptrol setup` one-command install | live-verified | 4-client config generator (6 unit tests), native-host registration |
| 4-client configs (Freebuff / Claude Code / Codex / OpenCode) | live-verified | `comptrol setup --print`, written configs |
| Six-task benchmark + baselines | live-verified | `bench/run_suite.mjs --self-check`, `bench/baselines.json` |

## Honest gaps

- macOS AX and Linux AT-SPI reach `uia.rs` parity in code and semantics tests,
  but their live gates self-skip off-platform. Run `macos_ax_conformance.py` on
  macOS and `linux_atspi_conformance.py` on Linux to promote them to live.
- Canva element editing has no Connect API equivalent; it honestly returns
  `design_editing_app_required` pointing at the companion App bridge (preview)
  instead of fabricating an edit.
- Adapters without a live gate report `requires_consent` /
  `implemented_not_live_verified` in `comptrol doctor` rather than claiming
  success.
