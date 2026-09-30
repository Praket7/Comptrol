# Comptrol Build Plan — V3
**Date:** 2026-09-27 · **Author:** Buffy, from a full-codebase analysis pass + the 2026-09-27 live debugging session
**Mission (user's words, hardened into requirements):** one local runtime that Freebuff, Claude Code, Codex, and OpenCode drive through MCP to control the *entire computer* — Chrome (background + foreground), desktop apps (Blender, Settings, Calculator, file explorer, terminal), hidden/headless processes — with as few commands as possible, faster and more reliable than native computer-use agents, on Windows, macOS, and Linux, with seamless setup (Chrome extension + MCP load = done).
**Companion docs:** `COMPTROL_BUILD_PLAN.md` (v2, execution plan), `docs/FUTURE_MISSION_ROADMAP.md` (research), `docs/DEPLOY_V2_STEPS.md` (deployment).

---

## 0. Method — how this plan was built

- **Structural analysis of the entire tree**: 14 crates (~28.7k lines Rust), the extension (`service_worker.js` 1,294 lines, `native_host.py` 502, offscreen keeper, popup, manifest), 15 adapter directories, 60+ conformance scripts, MCP launchers, bench harness.
- **Deep line-level dives** on the files that carry product risk: `service_worker.js` (complete), `browser.rs` semantic/verification paths, `browser_bridge.rs`, `workflow/lib.rs`, `uia.rs` press path, MCP launchers, manifest, main.rs HTTP surface.
- **Live evidence from the 2026-09-27 session**: v2 deployed, verified (bench 3/3, channel round trip 8–47 ms, `browser.ensure_session` verified end-to-end, real 7-tab discovery), then a real Classroom task was attempted — and the failures it produced are the single most valuable input to this plan. Every T-issue below was *observed live*, not theorized.
- Honesty note: this is a systematic full-tree pass with line-level depth on ~15k lines of product-critical code, not a literal line-by-line of all 30k. The conformance script inventory was cataloged, not individually read.

---

## 1. Scorecard — where Comptrol stands today (verified live)

| Capability | Status | Evidence |
|---|---|---|
| SW keep-alive (offscreen keeper + alarms) | ✅ working | channel survived idle; wake path revived it twice |
| Channel truth (round-trip health) | ✅ working | healthz/doctor honestly showed degradation (23 s RTT) instead of lying |
| `browser.ensure_session` (one-call readiness) | ✅ verified | returned real surface: 7 tabs, signed-in profile, 13 ms |
| Background tab discovery | ✅ verified | `/browser/discovery`: 7 real tabs incl. Classroom, 216 ms |
| Background tab *opening* | ✅ verified | Classroom opened background, 518 ms, verified |
| One-command binary deploys | ✅ working | `scripts/deploy.sh` rename-swap, no process kills |
| Background semantic **click on a live SPA** | ❌ **broken** | every debugger-backed command on the SPA wedged → timeout (T2) |
| Background text readback | ❌ honest-but-unusable | `wait_text` correctly reports it can't read hidden tabs (T3) |
| Revision binding on SPAs | ❌ races | `stale_reference` on every static revision (T1) |
| Resident daemon across 4 clients | ⚠️ built, unused | `comptrol-daemon-mcp.js` + `attachSocket` exist; all configs run `COMPTROL_DAEMON=0` |
| Desktop UIA background actuation | ⚠️ partial | input lease done; geometry gate still blocks semantic-first (W1/W2 open) |
| Adapters (Canva/Blender/PPT/Fusion/terminal) | ⚠️ skeletons | catalog exists; live routes unverified (AD1–AD4) |
| macOS / Linux | ❌ stubs | platform crates exist, parity not implemented (X3) |
| One-command setup | ❌ absent | F1 not started |

**Conclusion:** the foundation (channel truth, keep-alive, wake bus, honest verification culture) is right and now *proven*. The product breaks exactly where today's session broke it: **in-page command reliability on real SPAs**. Fix that, then climb the stack.

---

## 2. Complete issue register

### 2.1 T-issues — live regressions/limitations found 2026-09-27 (new, highest priority)

| # | Issue | Evidence (live) | Root cause | Severity |
|---|---|---|---|---|
| **T1** | **Revision binding races live SPAs.** Classroom bumps its tab generation continuously; any operation that pins a static `revision` is refused `stale_reference` before dispatch (3 consecutive refusals). Omitting revision loses TOCTOU protection. | 3× `stale_reference` refusals at 12–445 ms | Binding validates `generation` captured at *snapshot* time; SPA navigation between snapshot and dispatch invalidates it. No "revalidate-at-dispatch" mode. | **Critical** (blocks every real task) |
| **T2** | **Debugger-backed commands wedge after SPA navigation.** Click/snapshot/evaluate on the Classroom tab timed out at 10–24 s, *three times*, including on a freshly reloaded SW. Light commands (discovery, ping) stayed healthy. | 3× `browser_bridge_timeout`; RTT spiked to 23 s; post-reload repeat = deterministic | `chrome.debugger.sendCommand` has **no timeout and no cancel**; `Target.setAutoAttach` (flatten, iframe filter) fires an auto-attach storm on SPA cross-origin iframe churn; per-child-session init promises + in-flight tab-level command collide; the tab-level promise never resolves. `lastUsedAt` is refreshed by the event storm, starving the 90 s idle sweep. Wake reconnect does not clear in-flight debugger promises. | **Critical** (the single biggest defect) |
| **T3** | **Background text readback is impossible by design** (Chrome does not render hidden tabs). `wait_text` in a workflow correctly refuses after 11 s — honest, but it makes the canonical Classroom recipe unusable in strict background. | `verification_failed: "The visible page did not contain the requested text: Invest"` | `wait_text` probes *rendered visibility*; hidden tabs have no visible text. DOM-presence probing not offered as the background-safe alternative. | High |
| **T4** | **Wake handler bounces healthy sessions.** `wakeAndReconnect` unconditionally disconnects+reconnects the native port, even when the SW is connected and healthy — contributing to churn during the wedge, and doing nothing to clear wedged *debugger* state. | SW source (`wakeAndReconnect`); RTT 4.5 s during sequence | No "already connected → no-op" guard; no debugger-state remediation on wake. | High |
| **T5** | **Health doesn't measure the debugger path.** Channel alive (pings fine) while every in-page command hung. Doctor/healthz said `alive` throughout the incident. | healthz `alive` at all times | Health = ping RTT only. No per-class success tracking (no debugger op vs debugger op), no wedge detector. | High |
| **T6** | **Timed-out commands leave no recovery path.** After `browser_bridge_timeout`, the command stays `requires_reconciliation`; the agent must manually re-inspect and re-issue with a new idempotency key. No auto-requeue-on-SW-recover, no explicit reconcile verb surfaced. | 3× `recovery: requires_reconciliation` | Timeout path ends at the HTTP response; nothing watches for SW recovery to requeue or to emit a structured reconcile plan. | Medium |
| **T7** | **Policy allowlist and capability gate can drift.** `browser.ensure_session` was capability-gated `true` but absent from every env-gate allowlist → `policy_denied` from real clients (fixed today under `COMPTROL_ALLOW_BROWSER_CDP`, but by hand). | Live `policy_denied` → fix → verified | Two independent gates (authorize + capability) with no conformance test tying them together. | Medium (recurring foot-gun) |
| **T8** | **Extension staging is still a manual step.** Rust deploys are one command; extension changes still require running the sync script and a user extension reload — undocumented in deploy.sh. | Deploy flow used today | `scripts/deploy.sh` doesn't stage the extension or detect "extension changed → tell the user to reload." | Medium |
| **T9** | **Dedupe ledger can pin a wedged command.** `beginDedupedCommand` marks `inflight` in storage; if the SW restarts mid-wedge, the entry persists and replays `requires_reconciliation` instead of clearing on a health reset. | Source (`beginDedupedCommand`) + today's repeats | No inflight-entry TTL tied to SW instance lifetime. | Medium |

### 2.2 Carried-forward issues from v2 plan — status after v2

**Fixed ✅:** E1/E2 (keep-alive), E3 (wake bus), E5→partial (90 s idle sweep), E8/S1 (pinned key), N2 (cached HMAC identity), N4 (wake drain), R1 (condvar wait), R2 (process-wide store), R4 (rAF fix), R9/S3 (honest doctor + channel block), A6 (canonical config), A7, A8, B2, B3, B5 (activate_tab), B6 (idle sweep), C3-partial (schemas published), C4-partial (`max_windows`), C5-partial (actor-attested labeling + second-channel verify for some postconditions), C6 (ensure_session), D2 (input lease), F4 (bench harness), S1.

**Still open ❌ (IDs from v2 plan):**

| ID | Issue | Where it lives now | Severity |
|---|---|---|---|
| E4-partial | No screenshot handler (`tabs.captureVisibleTab`), no `tabs.highlight`, screenshot intent requires active tab | service_worker.js | Medium |
| E6/R10 | Frame scoping via bridge exists in SW (`cdp_frame_command` + frame maps) but the Rust side still refuses bridge frame calls — **and the auto-attach machinery is the T2 wedge source** | browser.rs `cdp_frame_call` | High |
| E7 | `attachedTargets`/session maps reset on SW restart → orphaned debugger attachments possible | SW | Medium |
| E9 | No JS dialog / permission-prompt manager in SW | SW | Medium |
| N1/N3 | Host poll loop remains (6 s probe / 4 s wake); no persistent stream; poll floor still on every command | native_host.py | High |
| N5/R8 | Second config file (`native_host_config.json`) still exists beside the canonical one | extension dir | Low |
| N6 | Two+ runtimes share SQLite with identity flapping; daemon mode built but unused (configs say `COMPTROL_DAEMON=0`) | launchers | High |
| R3 | Event-driven revalidation exists in part; `wait_for_url` bridge path still polls discovery | browser.rs | Medium |
| R6 | `browser.chrome.open_tab` (launcher route) still returns unverified | browser.rs | Medium |
| R7 | Command timeouts still tight for cold heavy pages (10 s) | browser.rs | Medium |
| M1 | `lib.rs` 12,398 lines — monolith risk now worse | comptrol-core | High (velocity) |
| M4 | Schema gaps remain for some intents (snapshot schemas demand params the model can't know — live `invalid_input` on `limit`/`revision`) | intent_schema.rs | Medium |
| M5/M6 | `capabilities` payload still large; no delta mode | lib.rs | Medium |
| M7 | Trace spans still not surfaced through MCP | trace.rs | Medium |
| F1/F2/F3 | Two workflow systems persist (template store vs ad-hoc executor); no workflow deadline/cancellation across steps; step cap 32 | workflow/lib.rs, browser.rs | High |
| V1 | Verification crate (154 lines) still unwired; verification remains ad-hoc in browser.rs | comptrol-verification | High |
| W1/W2 | UIA geometry gate still filters press candidates before semantic patterns; background actuation not semantic-first | uia.rs:621–785 | **Critical (desktop)** |
| W3 | `UIA_E_ELEMENTNOTAVAILABLE` on filtered inspect; HWND binding refresh needed | uia.rs | Medium |
| W4 | No worker-process isolation for hung UIA providers | uia.rs | High |
| W5 | Per-monitor DPI foreground checks absent | windows.rs | Medium |
| A1/A2 | PID≠window identity; fixed 750 ms settle instead of window-event wait | launch.rs | High |
| A3 | `app.open_resource` unverified (0/11 live) | registry/launch | High |
| A4 | No hidden/offscreen launch (window-style, virtual desktop) | launch.rs | Medium |
| D1 | UIA worker isolation (same as W4) | — | High |
| D3–D6 | Launch→window correlation, hidden-state control, full pattern coverage, doctor app smoke tests | — | High |
| E1–E8 (adapters) | Blender live unverified; Canva two-route absent; Fusion absent; PPT COM unverified; terminal intent absent; file explorer absent; ms-settings deep-links absent | adapters/* | High |
| F1–F3 | `comptrol setup` absent; 4-client conformance absent; macOS/Linux parity stubs | — | High |
| F5 | README quickstart/docs for 4 clients | docs | Medium |
| X1 | Telemetry events exist in bridge_meta; no surfaced timeline via MCP | events.rs | Medium |
| X2 | Bench = 3 tasks; six-task suite + baselines not wired | bench/ | High |
| X3 | macOS AX + Linux AT-SPI parity unimplemented | platform-macos/linux | High |

---

## 3. Research foundation (what the evidence says to build)

From `docs/FUTURE_MISSION_ROADMAP.md` (UFO2/AgentOS 2025, OSWorld-Human MLSys 2026, browser-use CDP pivot, chrome-devtools-mcp / Playwright MCP comparisons, SoM/a11y-tree benchmarking) plus today's live data:

1. **Semantic-first + hybrid API routes win.** UIA/a11y trees beat pixels on reliability; vision is a fallback for canvas-only surfaces (Canva viewport, Blender). Keep refusing pixel-first routes.
2. **Efficiency is the frontier.** Benchmarks now score steps/wall-time against human baselines, not just success rate. Comptrol's one-command workflow bet is the correct strategic target; every stage below is judged on commands-per-task.
3. **The signed-in real-profile background route is the moat.** No Playwright-style tool reaches the user's actual signed-in Chrome in the background. This is why fixing the extension (not bypassing it) is the whole game — and why T2 is priority zero.
4. **Speculative multi-action execution** (UFO2) is the proven pattern for step-count reduction: submit N steps with per-step postconditions, reconcile on failure. Comptrol's dedupe ledger already provides the safety substrate.
5. **In-page agent surfaces beat CDP round-trips for hot loops** (browser-use's "closer to the metal"): install a small evaluated surface once per page, then drive it via fast messages instead of full `Runtime.evaluate` round trips.
6. **Today's live session adds a negative result worth publishing:** Chrome's `chrome.debugger` + `Target.setAutoAttach` on SPA/OOPIF-heavy pages can wedge tab-level commands indefinitely when the wrapper lacks per-command deadlines — an unrecovered-await failure mode that generic harnesses don't surface. Defensive design (deadlines, serialization, watchdogs) is mandatory, not optional.

---

## 4. Architecture V3 — six pillars

### P1 — Extension reliability kernel (fixes T1–T5, T9; E6/E7; R7)
The SW becomes a *supervised kernel*, not a message relay:
- **K1 Per-command deadline + race-loser discipline (T2).** Every `chrome.debugger.sendCommand` wrapped in `withDeadline(ms, op)` (Promise.race + explicit tag). On deadline: do NOT cancel the CDP op (Chrome has no cancel) — *tag it orphaned*, record `orphaned_ops` count per attachment, and **detach+reattach the debugger for that target** when orphans exceed 1, clearing the stuck session. Result flows back as `{ok:false, code:"sw_deadline"}` so the host can requeue.
- **K2 Auto-attach serialization (T2).** One in-flight `Target.setAutoAttach` init per attachment (promise memoized); child-session init queue capped (init N sessions max, rest lazily on first use); attach storm on navigation handled by *canceling pending child inits on `Page.frameNavigated`* of the main frame.
- **K3 Idle-sweep starvation fix (T2).** Track `lastUsedAt` only for *commands*, never for events; sweep uses command-time. Optional force-sweep when `orphaned_ops > 0`.
- **K4 Generation-tolerant binding (T1).** SW accepts `{target_id, revision?}`; when revision omitted → bind at dispatch; when revision provided and stale → re-read current tab state and *auto-revalidate* if URL prefix matches (class of safe ops: click/fill/snapshot) — refuse only on target-gone or URL-class change. Rust side mirrors this: `revision` becomes an *optimistic hint*, not a gate, for R1/R2-class ops; hard gate only for destructive ops with `consent` (coordinate_click keeps screenshot-proof binding).
- **K5 Two-tier health (T5).** SW tracks `last_debugger_op_ok_ms` per target-class; ping result includes it; healthz reports `channel` + `page_ops` tiers; doctor shows both. Wedge detector: ping fine + page-ops failing → `degraded_page_ops` state, which triggers K6.
- **K6 Smart wake (T4).** `wake` handler: if connected → no reconnect; instead run a *probe command* on the most recent attachment; if it deadlines → detach/reattach that target (K1) and report `page_ops_recovered`. If disconnected → current reconnect path.
- **K7 Background text readback (T3).** `wait_text` gains a `dom: true` mode (default for background): text presence via DOM (a11y tree / textContent), no visibility requirement. Visible-variant stays available and honestly labeled `requires_rendering`.
- **K8 In-page fast surface (research #5).** On attach, evaluate a tiny `__comptrol` page surface once (locator resolution, geometry read, click dispatch, text probe). Hot ops call it via one evaluate (or `chrome.scripting` for optional-permission pages) instead of 3–5 CDP round trips. Expected: click p50 from ~800 ms to <150 ms; also removes rAF-stability dependence.
- **K9 SW-restart state rebuild (E7, T9).** On init: `chrome.debugger.getTargets()` to discover *already-attached* targets (orphaned from previous SW life) and adopt-or-detach them; inflight ledger entries get `instanceId`; entries from a previous instance are expired on startup.
- **K10 Screenshot + dialogs (E4, E9).** `capture_tab` (visible tabs) + JS dialog interposer (Page.javascriptDialogOpening auto-answer policy: report-and-hold, never silent-answer).

### P2 — Push transport (fixes N1/N3, R3, R7; completes B1/B8)
- **P2.1 Host↔sidecar stream.** native_host.py opens one persistent chunked HTTP connection to the sidecar: commands pushed instantly (no 6 s poll), results streamed, wake multiplexed. Keep poll as fallback.
- **P2.2 Sidecar↔SW is already push** (native messaging) — keep.
- **P2.3 End-to-end latency budget:** operate→dispatch p95 ≤ 50 ms warm (Rust), SW op p50 ≤ 150 ms with K8, host→SW ≤ 30 ms. Gate B numbers finally reachable.
- **P2.4 Requeue-on-recover (T6).** Sidecar watches channel health; a command that timed out with `sw_deadline`/`bridge_timeout` is requeued once automatically when `page_ops` recovers, with the same request_id (dedupe ledger makes this safe), and the operation result carries `auto_requeued: true`.

### P3 — Resident runtime for 4 clients (fixes N6, M2/M3)
- **P3.1** Enable daemon mode: `comptrol-mcp.js` (COMPTROL_DAEMON=1 path) attaches via port-file `~/.comptrol/daemon.port`; spawns the daemon if absent (user-session process, survives client exit); all 4 clients share it. Config generator writes the right mode per client.
- **P3.2** Identity arbitration: single daemon owns sidecar + SQLite writers; stdio children become thin proxies (already supported).
- **P3.3** Doctor reports daemon ownership (`resident: true, clients: 4`).

### P4 — One-command tasks & verification (fixes F1–F3, V1, C1/C2/C5/C7/C8; M4)
- **P4.1 Single executor.** Move the browser workflow executor into `comptrol-workflow` (typed IR already exists); `browser.rs` dispatches to it. One workflow system, one name.
- **P4.2 Speculative batches.** New intent `workflow.speculate`: N steps, per-step postconditions, per-step rollback hints; executor runs optimistically, on failure returns durable state of executed steps + reconcile plan. Deadline + cancellation between steps (P4.1 wires `run_with_cancel` to the HTTP handler). This is the UFO2-backed efficiency lever: Classroom = 1 call.
- **P4.3 Independent verification, wired.** comptrol-verification owns postcondition evaluation: every mutation returns `verified_by: actor_dispatch` unless a *second channel* readback passes (fresh discovery + K7 DOM probe, or separate read-only evaluate). Verification crate becomes the only code that can emit `verification: verified`.
- **P4.4 Recipes.** Classroom/ESPN/Settings/Calculator as promoted, parameter-lifted recipes (`recipe.classroom_open_class(account, class)`), replay-gated, stored in the workflow host. Gate C: Classroom in ≤2 calls ≤8 s; recipes make it ≤1 call warm.
- **P4.5 Schema completeness + conformance test (M4, T7).** Test asserts: every intent in the catalog has a schema; every capability-gated intent appears in ≥1 env-gate allowlist; every schema example validates against itself. Kills both drift classes permanently.
- **P4.6 Trace spans via MCP (M7, C8).** `operate` returns `trace_id`; `inspect kind:events` shows spans; benchmark harness records per-step latency attribution.

### P5 — Desktop & surfaces (fixes W1–W5, A1–A4, D1/D3–D6, E1–E8 adapters, X3)
- **P5.1 UIA semantic-first completion (W1/W2).** Press path order: semantic patterns (Invoke/Toggle/SelectionItem/ExpandCollapse/Value/ScrollItem) → only if none: physical click with foreground lease (D2 exists). Remove geometry pre-filter from the semantic path entirely.
- **P5.2 UIA worker isolation (W4/D1).** UIA calls execute in a supervised child process with deadlines + restart; a hung provider can never stall the daemon. (This is also the V3 prerequisite for trusting desktop ops in long workflows.)
- **P5.3 Launch→window correlation (A1/A2/A3).** `IApplicationActivationManager` PID → `EVENT_OBJECT_SHOW` wait → per-monitor-DPI-aware window identity (AUMID + class + title) → surface ref. `app.open_resource` finally verified (target: 100% of launches return verified surface).
- **P5.4 Hidden-state control (A4).** Launch with ShowWindow state / virtual-desktop targeting for "background windows"; Blender/Settings altered while Chrome is foreground.
- **P5.5 Adapters (E1–E8).** One SDK contract (handshake, capability negotiation, heartbeat, restart — SDK crate exists). Order by user value: **Canva** (web route via bridge + app route via UIA), **PowerPoint COM** (open/read/edit/save/reopen-verify), **Blender live** (verify the existing bridge end-to-end + save/reopen gate), **Fusion add-in**, **terminal** (`desktop.terminal`: visible terminal, typed command, output readback), **file explorer**, **ms-settings:** deep links with UIA readback.
- **P5.6 macOS AX + Linux AT-SPI parity (X3).** Implement platform crates to uia.rs semantics (inspect/press/fill/dispatch). Semantic-first is a three-OS guarantee.

### P6 — Setup, benchmarks, docs (fixes F1–F5, S2/S4/S5, X2, T8)
- **P6.1 `comptrol setup`** (the mission's seamlessness): build → stage extension with pinned ID → register native host (3 OS paths) → doctor ping → write MCP configs for Freebuff/Codex/Claude Code/OpenCode (daemon mode) → verify. Fresh machine to working = one command + one extension click.
- **P6.2 Benchmarks to OSWorld-Human framing (X2).** Six-task suite + generalization cases; per-task step count + wall time; native-CU baselines where comparable; runs.jsonl + trend tracking; CI-wired.
- **P6.3 Deploy pipeline completes (T8):** deploy.sh stages the extension and prints "extension changed — reload once" when its hash differs; N5/R8 second config file removed.
- **P6.4 Docs (F5).** README quickstart ≤20 lines; per-client pages; honest support matrix.

---

## 5. Staged plan (each stage ends with a live gate on this machine)

### Stage 1 — Extension reliability kernel *(P1: K1–K9; T1–T5, T9, E7, R7)*
**Files:** service_worker.js, browser.rs binding/timeout paths, browser_bridge.rs health.
The single highest-leverage stage: makes real SPA tasks work in the background.
1. K1 withDeadline + orphan tracking + detach/reattach remediation
2. K2 attach serialization + K3 sweep fix
3. K4 generation-tolerant binding (SW + Rust mirror)
4. K5 two-tier health + K6 smart wake
5. K7 DOM text probe (background readback)
6. K9 restart state rebuild + T9 ledger instanceId
7. R7: heavy-op timeout 10 s → 20 s with progress events
**Gate 1:** on the live Classroom tab (background, never focused): ensure_session → click class card → URL verified, ≤3 operate calls, zero focus disturbance, p95 in-page op < 1.5 s, and a forced SW-reload mid-task recovers via K6/K9 without user help. Repeat on a Docs tab and a Reddit tab (SPA/OOPIF variety).

### Stage 2 — Push transport + resident daemon *(P2, P3; N1/N3/N6, M2/M3, T6)*
**Gate 2:** warm `operate` round trip p50 ≤ 80 ms / p95 ≤ 300 ms; two clients share one daemon (verified by doctor); killed client leaves daemon running; a `sw_deadline` command auto-requeues and completes on recovery.

### Stage 3 — Workflow engine + speculative batches + verification *(P4)*
**Gate 3:** Classroom account→class in **1 operate call** (speculative batch) with independent verification, warm ≤8 s; a deliberately-broken step returns executed-state + reconcile plan; verification crate is sole issuer of `verified`.

### Stage 4 — Desktop: UIA semantic-first + isolation + launch correlation *(P5.1–P5.4)*
**Gate 4 (Gate D from v2):** Calculator 9×11=99 cold-launch in one operate call, display verified; Blender scene read while Chrome is foreground; Settings toggle flipped in background with readback.

### Stage 5 — Adapters *(P5.5)*
**Gate 5:** Canva web (open template → read → replace text → export) and one desktop adapter (PowerPoint COM save/reopen-verify) live; terminal + ms-settings intents working.

### Stage 6 — Cross-platform parity + setup + benchmarks *(P5.6, P6)*
**Gate 6 (release gate):** fresh machine: `comptrol setup` + one extension click → all 4 clients each run one browser + one desktop task; six-task benchmark passes with recorded baselines; macOS + Linux each run the browser suite + one desktop task.

**Sequencing rationale:** 1 unblocks the product (today's proof), 2 makes it fast and multi-client, 3 makes it one-command, 4–5 broaden the computer, 6 makes it universal and effortless. Stages 1–3 are the minimum bar for "significantly better than native computer use" on the browser half of the mission; 4–6 are the "entire computer" half.

---

## 6. Definition of done (release gate, honest)

- Four clients, one resident runtime, three OSes; fresh-machine setup = 1 command + 1 click.
- Browser: open/activate/close/navigate/snapshot/click/fill/fill-verify **in background on real SPAs**, p95 in-page < 1.5 s, zero infobar accumulation, zero silent failures — every unknown is labeled and auto-reconciled.
- Desktop: background semantic actuation (no focus) for pattern-bearing apps; disclosed lease only for physical input; launch→verified-surface 100%.
- Tasks: Classroom ≤1 call warm; benchmark suite green with step/wall-time records vs native-CU baselines.
- Verification: `verified` only from the independent channel; everything else honestly labeled.
- No issue in §2 remains open without a documented reason.

## 7. Risks & mitigations
| Risk | Mitigation |
|---|---|
| Chrome breaks keep-alive/native messaging assumptions in an update | Two-tier health detects it in ≤30 s; K6 wake + alarm resurrection; conformance suite run on every Chrome major in CI |
| Detach/reattach remediation (K1) shows infobar churn | Only on orphan; single infobar per tab already; telemetry counts remediations; user-visible only in doctor |
| Optimistic batches double-execute on retry | Dedupe ledger (already durable) + per-step postconditions + idempotency keys; speculative mode is opt-in per intent |
| Daemon mode regressions single-client setups | Config generator keeps stdio fallback; daemon attach has version handshake + fallback to child mode |
| Scope creep across 15 adapters | Stage 5 order locked by user value; each adapter gated on live demo before the next starts |
| Monolith (M1) slows every stage | Stage 3 includes the lib.rs split along dispatch routes; new code lands in crates, never in lib.rs |
