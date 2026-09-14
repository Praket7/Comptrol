# Browser control

The repository now includes a local browser fixture with Chromium style discovery endpoints, exact target identity, browser context identity, a revision, and idempotent form submission. The Rust runtime can also discover targets from a configured local DevTools HTTP endpoint through `COMPTROL_CDP_ENDPOINT`.

The fixture now includes a minimal websocket protocol route for `Runtime.evaluate` and `Page.navigate`. It is the deterministic contract test for exact target rebinding and verified protocol responses. A stale target returns a refusal. A repeated idempotency key returns the first result without a second submission. Discovery does not grant browser mutation authority.

The runtime can use the same narrow routes against a local Chromium DevTools endpoint when `COMPTROL_ALLOW_BROWSER_CDP=1` is set outside the agent channel. Every CDP mutation requires the exact page target, browser context, and target revision returned by discovery.

Uploads are restricted to the Comptrol sandbox and verify the selected filename. Downloads require a stable idempotency key, use a key scoped sandbox directory, wait for the expected file, and verify the resulting file. A repeated key returns an existing verified download without clicking the page again.

The CDP operation surface also provides target bound fill, click, and wait for condition routes. Fill verifies the resulting value. Click reports unverified unless a caller supplies a postcondition expression. Wait uses an allowlisted DOM property with an equals or contains condition, a bounded timeout, and no caller supplied JavaScript.

Run `node scripts/browser_conformance.mjs` from the repository root.
