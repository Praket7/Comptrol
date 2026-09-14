# Browser control

The repository now includes a local browser fixture with Chromium style discovery endpoints, exact target identity, browser context identity, a revision, and idempotent form submission.

The fixture does not claim to be a real Chrome DevTools Protocol websocket implementation. It is the deterministic contract test used before wiring a live CDP session. A stale target returns a refusal. A repeated idempotency key returns the first result without a second submission.

Run `node scripts/browser_conformance.mjs` from the repository root.

