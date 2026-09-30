# Comptrol — Fulfilling the Mission: Research-Backed Roadmap

**Date:** 2026-09-27 · Companion to `COMPTROL_BUILD_PLAN.md` (execution) and `docs/DEPLOY_V2_STEPS.md` (deployment).
**Mission:** control the entire computer with commands — background-first, one-command-per-task, four MCP clients, three OSes.

---

## 1. Where the state of the art is (research summary)

| Finding | Source | Implication for Comptrol |
|---|---|---|
| Hybrid UIA + vision beats screenshot-only agents on robustness across 20+ Windows apps; UIA trees give cleaner, more reliable data than pixels | Microsoft **UFO2: The Desktop AgentOS** (arXiv 2504.14603, 2025) | Comptrol's semantic-first, accessibility-driven architecture is validated as the right bet. Double down: UIA everywhere possible, vision only as fallback, never the primary route. |
| **Hybrid GUI–API action layer**: for supported apps, native APIs (COM, add-ins, CLI) are more robust than GUI manipulation; the agent should dynamically choose API vs GUI per step | UFO2 hybrid actions docs | Comptrol's adapter program (Blender bridge, PowerPoint COM, Fusion add-in) matches this exactly — accelerate Stage E. Every adapter should be exposed as a *route option*, not a separate tool. |
| **Speculative multi-action planning** reduces per-step LLM overhead dramatically; batch-planned actions amortize round trips | UFO2 | Comptrol already has the substrate: the workflow executor. Next step: let the model submit *optimistic step batches* with per-step postconditions and automatic rollback/reconcile on failure (the dedupe ledger already prevents double-execution). |
| Agents hit high benchmark scores (OSWorld ~85%) but fail most *real* workflows; efficiency (steps/wall-time) is now the frontier — **OSWorld-Human** (MLSys 2026) benchmarks agents against human step counts | OSWorld-Human paper; process-centric GUI agent benchmarks | Success metric = fewest commands + least wall time with honest verification — exactly Comptrol's design goal (one-command tasks, ≤2-call Classroom gate). Adopt OSWorld-Human's efficiency framing in the benchmark suite. |
| Screenshot + accessibility tree + **Set-of-Mark** are the three standard observation modes; a11y-tree-only agents are more reliable but need vision fallback for canvas/custom-drawn UI | Process-centric benchmark (ACL 2026 findings); UGround | Comptrol's compact_snapshot (a11y-based) is primary; add an optional SoM overlay *screenshot* mode for canvas-heavy apps (Canva, Blender viewport) — visual grounding only where semantics don't exist. |
| browser-use dropped Playwright for raw CDP for speed/capability; chrome-devtools-mcp shapes tools around debugging; Playwright MCP around page interaction | browser-use "Closer to the Metal" (2025); MCP comparisons 2026 | Comptrol's bridge is already CDP-shaped end-to-end. Keep zero new deps; the extension route remains the only way to reach *signed-in, real-profile, background* tabs — that's the differentiator no Playwright-style tool offers. |
| Community consensus (2025–26): UIA-tree automation is more reliable than pixel automation; the debate is settled for standard controls — vision is needed only for custom-drawn surfaces | Windows-Use discussions; UFO2 adoption | Continue refusing pixel-only routes for standard controls; disclose honestly when a target has no semantic tree (canvas). |

---

## 2. What v2 already delivers against this bar

- **Channel truth & self-healing** (round-trip probes, wake bus, offscreen keeper) — no mainstream competitor handles MV3 SW suspension at all; they sidestep it by launching separate browser instances and giving up the user's real profile.
- **Background-tab actuation** (rAF-free stability checks, background postures, disclosed focus hops) — Playwright MCP and chrome-devtools-mcp both require attached/foregrounded pages by design.
- **Honest verification labels** (actor-attested vs independent second-channel readback) — beyond what any surveyed tool reports; most report raw success only.
- **One-command tasks** (`browser.ensure_session`, workflow executor, promoted-recipe replay) — directly targets the OSWorld-Human efficiency frontier.
- **Multi-client resident runtime** (detached daemon, attach protocol) — four MCP clients share one state store; competitor tools are single-client.

## 3. Ranked next moves (highest mission-impact first)

### Tier 1 — close the efficiency gap (the OSWorld-Human frontier)

1. **Speculative step batches.** Extend the workflow executor so an agent can submit N steps with per-step postconditions in one `operate` call; on a failed step, return the durable state of every executed step + reconciliation guidance. Expected: Classroom-class tasks in 1 call, not 2. (Evidence: UFO2's multi-action planning.)
2. **Recipe promotion into the catalog.** Promote the Classroom/ESPN/Settings/Calculator flows into versioned, parameter-lifted recipes selectable by `recipe.run` with operands — replay with zero discovery steps.
3. **Observation deltas.** `compact_snapshot` currently returns full lists; add `since_revision` to return only changed controls. Shrinks tokens per step and speeds model loops.

### Tier 2 — breadth of surfaces (the mission's "entire computer")

4. **Set-of-Mark fallback for canvas surfaces** (Canva web, Blender viewport, Fusion canvas): screenshot + numbered overlays, but only when the a11y tree exposes nothing actionable — keep semantic-first discipline. Vision grounding is a fallback, not the driver (per research).
5. **Adapter SDK + API-first routes per UFO2's hybrid action layer**: PowerPoint COM (open/read/edit/save/reopen), Fusion add-in (dimensioned solid, save, reopen), Blender live bridge end-to-end verification. Each adapter = one more app class where Comptrol beats pixel agents on reliability.
6. **Terminal intent** (`desktop.terminal`): allowlisted-executable command run inside a visible terminal with output readback — file explorer + terminal + settings deep-links (`ms-settings:`) close the "system control" surface.
7. **macOS AX + Linux AT-SPI parity** so the semantic-first architecture is a three-OS guarantee, not a Windows-only trait.

### Tier 3 — trust, operations, scale

8. **Trace spans surfaced through MCP** (`operate` → `trace_id`, `inspect kind:events` → spans): latency attribution becomes a first-class answer, accelerating the benchmark loop.
9. **Benchmarks wired to OSWorld-Human efficiency framing**: extend `bench/run_suite.mjs` with step-count and wall-time per task, native-CU baseline pairs where comparable (`bench/baselines.json`).
10. **`comptrol setup` one-command installer** (Stage F): build → stage extension with pinned ID → register native host (3 OS paths) → doctor ping → write MCP configs for Freebuff/Codex/Claude Code/OpenCode. The mission's seamlessness depends on this last mile.
11. **Crash/visibility telemetry v2**: journal events for every wake, retry, lease requeue, and postcondition failure — operational introspection for the user, and training data for future recipe promotion.

## 4. Honest limits that remain

- Rendering-dependent verification in hidden tabs still needs a disclosed `activate_tab` hop — Chrome does not render hidden pages, full stop; the fix is disclosure, not deception.
- The debugger infobar is a *consent signal*; keep it (idle sweep now prevents accumulation) rather than hiding automation from the user.
- Canvas-only surfaces without any accessibility tree will never be semantic-clickable; SoM vision fallback is the honest answer there.
- "Every app instantly" remains aspirational; the supported-matrix and `unsupported_surface` honesty codes are the product telling the truth.

## 5. What to do after the user's 3 reload steps

1. Run `node bench/run_suite.mjs` — all three tasks should pass with `round_trip_active: true`; this is the post-reload Gate A evidence.
2. Re-run the Classroom benchmark end-to-end via MCP (≤2 operate calls target).
3. Pick Tier 1 item 1 (speculative step batches) as the next work session — it compounds every future task's efficiency.
