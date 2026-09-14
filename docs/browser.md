# Browser control

The repository now includes a local browser fixture with Chromium style discovery endpoints, exact target identity, browser context identity, a revision, and idempotent form submission. The Rust runtime can also discover targets from a configured local DevTools HTTP endpoint through `COMPTROL_CDP_ENDPOINT`.

The fixture now includes a minimal websocket protocol route for `Runtime.evaluate` and `Page.navigate`. It is the deterministic contract test for exact target rebinding and verified protocol responses. A stale target returns a refusal. A repeated idempotency key returns the first result without a second submission. Discovery does not grant browser mutation authority.

`browser.cdp.open_tab` opens a visible tab through the local Chrome DevTools endpoint or creates a background tab in the existing browser profile. The background route uses `Target.createTarget` with background and focus disabled, then rediscoveries the exact target before reporting success. It does not synthesize mouse input or read or write the clipboard. Signed in account state is available because this route attaches to the existing browser profile. Comptrol does not copy cookies or credentials into a separate headless profile.

`browser.cdp.close_tab` closes one exact live page target after revalidating its target id, browser context, and revision. It waits until discovery confirms that target is gone. It does not close a guessed tab, move the pointer, or use the clipboard.

`browser.cdp.history_back` and `browser.cdp.history_forward` select the adjacent entry from the exact target navigation history, reject missing entries, and verify the resulting history index. They operate on inactive or grouped live tabs without foreground activation. A closed tab group remains outside this route because it is no longer a live DevTools target.

`browser.cdp.accessibility_snapshot` reads a bounded Chrome accessibility tree from the exact target. It is observation only and returns no mouse, keyboard, or clipboard effect.

`browser.cdp.reopen_closed_group` is an explicit safe refusal. It returns `closed_group_unsupported` because a closed group is not a live DevTools target. Comptrol does not guess a URL or recreate a group from stale history.

Enable the route only for a browser endpoint the user intentionally started with local DevTools enabled, using `COMPTROL_CDP_ENDPOINT=http://127.0.0.1:PORT` and `COMPTROL_ALLOW_BROWSER_CDP=1`. The endpoint must remain loopback only.

Inactive and grouped live tabs remain addressable through exact target identity and do not need foreground focus. A closed tab or closed tab group is not a live DevTools target, so it must be reopened by the browser before Comptrol can control it. Chrome does not expose a portable closed group control surface through the route used here.

`browser.cdp.open_tab` can receive an optional `browser_context_id` when the endpoint exposes more than the default browser context. Comptrol passes that context to `Target.createTarget` and verifies the returned tab belongs to it. Omitting the field uses the endpoint's existing default profile.

The fixture page includes a nested frame, download, dialog, dynamic node, shadow root, canvas, and intentionally untrusted instruction text. The text is fixture data only and is never treated as runtime instruction.

The runtime can use the same narrow routes against a local Chromium DevTools endpoint when `COMPTROL_ALLOW_BROWSER_CDP=1` is set outside the agent channel. Every CDP mutation requires the exact page target, browser context, and target revision returned by discovery.

Uploads are restricted to the Comptrol sandbox and verify the selected filename. Downloads require a stable idempotency key, use a key scoped sandbox directory, wait for the expected file, and verify the resulting file. A repeated key returns an existing verified download without clicking the page again.

The CDP operation surface also provides target bound fill, click, and wait for condition routes. Fill verifies the resulting value. Click reports unverified unless a caller supplies a postcondition expression. Wait uses an allowlisted DOM property with an equals or contains condition, a bounded timeout, and no caller supplied JavaScript.

Run `node scripts/browser_conformance.mjs` from the repository root.
