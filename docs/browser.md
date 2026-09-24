# Browser control

The repository now includes a local browser fixture with Chromium style discovery endpoints, exact target identity, browser context identity, a revision, and idempotent form submission. The Rust runtime can also discover targets from a configured local DevTools HTTP endpoint through `COMPTROL_CDP_ENDPOINT`.

The fixture now includes a minimal websocket protocol route for `Runtime.evaluate` and `Page.navigate`. It is the deterministic contract test for exact target rebinding and verified protocol responses. A stale target returns a refusal. A repeated idempotency key returns the first result without a second submission. Discovery does not grant browser mutation authority.

`browser.cdp.open_tab` opens a visible tab through the local Chrome DevTools endpoint or creates a background tab in the existing browser profile. The background route uses `Target.createTarget` with background and focus disabled, then rediscoveries the exact target before reporting success. It does not synthesize mouse input or read or write the clipboard. Signed in account state is available because this route attaches to the existing browser profile. Comptrol does not copy cookies or credentials into a separate headless profile.

`browser.cdp.close_tab` closes one exact live page target after revalidating its target id, browser context, and revision. It waits until discovery confirms that target is gone. It does not close a guessed tab, move the pointer, or use the clipboard.

`browser.cdp.history_back` and `browser.cdp.history_forward` select the adjacent entry from the exact target navigation history, reject missing entries, and verify the resulting history index. They operate on inactive or grouped live tabs without foreground activation. A closed tab group remains outside this route because it is no longer a live DevTools target.

`browser.cdp.accessibility_snapshot` reads a bounded Chrome accessibility tree from the exact target. It is observation only and returns no mouse, keyboard, or clipboard effect.

`browser.cdp.screenshot` captures bounded visual evidence for the exact target, context, and revision. It returns a SHA-256 digest and encoded size rather than pixels by default, so recovery logic can compare the same target after a dynamic-site change without flooding MCP. Optional `clip`, `format`, and `quality` values are bounded and invalid clips are refused. A digest is evidence of the captured target state; it is not a claim that a visual action succeeded.

`browser.cdp.reopen_closed_group` remains an explicit safe refusal because a closed group is not a live DevTools target. The supported Chrome-native solution is the local Manifest V3 extension in `extensions/comptrol-closed-groups`. It records named group membership while the group is open, queries `chrome.sessions.getRecentlyClosed`, restores only a uniquely matched window or set of tabs, recreates the group, and verifies membership. Comptrol does not guess a URL or recreate a group from stale history.

On macOS, `browser.chrome.reopen_closed_group` is the explicit foreground alternative. With `COMPTROL_ALLOW_MACOS_AX=1` and Accessibility permission, it matches one exact saved group button by name, invokes its semantic Accessibility action, and verifies that the closed button is gone. The route does not synthesize mouse input or use the clipboard. It requires an exact group name and refuses strict background posture.

Run `COMPTROL_RUN_LIVE_CHROME_GROUP_CONFORMANCE=1 COMPTROL_CHROME_CLOSED_GROUP_NAME=GroupName python3 scripts/macos_chrome_group_conformance.py` only with a deliberately selected closed group. The acceptance harness leaves that group open after verification and skips when Chrome Accessibility access is unavailable.

Enable the route only for a browser endpoint the user intentionally started with local DevTools enabled, using `COMPTROL_CDP_ENDPOINT=http://127.0.0.1:PORT` and `COMPTROL_ALLOW_BROWSER_CDP=1`. The endpoint must remain loopback only.

Inactive and grouped live tabs remain addressable through exact target identity and do not need foreground focus. A closed tab or closed tab group is not a live DevTools target, so it must be reopened by the browser before Comptrol can control it. Chrome does not expose a portable closed group control surface through the route used here.

`browser.cdp.open_tab` can receive an optional `browser_context_id` when the endpoint exposes more than the default browser context. Comptrol passes that context to `Target.createTarget` and verifies the returned tab belongs to it. Omitting the field uses the endpoint's existing default profile.

The fixture page includes a nested frame, download, dialog, dynamic node, shadow root, canvas, and intentionally untrusted instruction text. The text is fixture data only and is never treated as runtime instruction.

The runtime can use the same narrow routes against a local Chromium DevTools endpoint when `COMPTROL_ALLOW_BROWSER_CDP=1` is set outside the agent channel. Every CDP mutation requires the exact page target, browser context, and target revision returned by discovery.

Uploads are restricted to the Comptrol sandbox and verify only the selected filename. Generic CDP reports the upload stage as `selected` and remains unverified until a site or application adapter proves transfer or acceptance. Downloads require a stable idempotency key, use a key scoped sandbox directory, wait for the expected file, and verify the resulting file. A repeated key returns an existing verified download without clicking the page again.

The CDP operation surface also provides target bound fill, click, and wait for condition routes. Fill verifies the resulting value. Click reports unverified unless a caller supplies a postcondition expression. Wait uses an allowlisted DOM property with an equals or contains condition, a bounded timeout, and no caller supplied JavaScript. To verify that a navigation finished loading, use `browser.cdp.wait_for` with `selector: "document"`, `property: "readyState"`, and `equals: "complete"` (or `interactive` when DOM readiness is enough). This checks the exact bound tab and returns verified only when the state is observed.

The `comptrol open <app-or-url>` CLI command is the one-command launch path. App launch and Chrome URL opening are enabled by default as individual operations. URLs use the existing default Chrome profile; when a local DevTools endpoint or healthy Browser Bridge is connected, Comptrol also waits for `document.readyState === "complete"`. Without that browser connection it reports native launcher acceptance as unverified.

`browser.cdp.semantic_click` is the dynamic-site fast path. It accepts a data-only locator such as `{ "role": "button", "name": "Submit" }`, `{ "text": "Buffalo Bills" }`, `{ "test_id": "checkout" }`, `{ "href_contains": "/team/" }`, or `{ "selector": "#known-control" }`. The runtime resolves the locator at action time, requires exactly one attached and visible match, checks that it is enabled and receives events rather than being covered by an overlay, waits for two stable animation frames, and then clicks it. If the supplied revision is stale, it refreshes the exact target once and retries; it never performs an unbounded retry or guesses another tab. The result is verified when the semantic click itself completed, while a caller can still provide a separate follow-up `browser.cdp.wait_for` postcondition for navigation or SPA state.

Locator resolution traverses the document, open shadow roots, and same-origin iframe documents on every retry. Cross-origin iframe documents remain inaccessible to the page security model and are not guessed through pixels. A locator that matches more than one semantic element is refused as `ambiguous_locator`; the engine does not silently choose the first result. Actionability still requires visibility, enabled state, event reception, and two stable animation frames.

`browser.cdp.workflow` is the low-round-trip fast path for bounded browser tasks. It accepts the exact target identity plus a closed `steps` array containing `navigate`, `click`, and `wait_url` steps. Each click still uses a fresh data-only semantic locator, target context and revision are revalidated between steps, navigation URLs are restricted to `http`, `https`, or `about`, and URL postconditions are bounded. This keeps the safety properties of individual operations while avoiding a separate MCP request, target inspection, and model turn for every click.

Normal target-bound CDP calls now reuse one local websocket per exact DevTools target URL, and a two-second event-invalidated target-list cache avoids repeating `/json/list` for adjacent read-only calls. The cache is invalidated after navigation, stale rebinding, and target lifecycle changes; failures on commands that can change target identity trigger a fresh discovery before retry. Command ids are monotonic within the session, unmatched protocol events are retained in a bounded queue, and any dispatch, read, protocol, or size failure evicts the session before the caller receives the error. Target creation, destruction, and navigation history verification wait for `Target.targetCreated`, `Target.targetDestroyed`, and page navigation events before the final identity check, removing fixed interval lifecycle polling from those waits. The next call reconnects once through the normal target identity binding path. Reuse is a transport optimization only and never bypasses target, context, revision, policy, or verification checks. Upload and download transactions retain their own bounded command scopes until they are migrated to the shared session without weakening their filesystem verification.

Run `node scripts/browser_conformance.mjs` from the repository root.
# Browser control in plain language

Comptrol names the browser, exact tab, browser profile, and current page version before it acts. A request for one tab cannot quietly switch to another tab. If the page changes after inspection, the old reference is rejected. Fixture tests check that stale and mismatched targets are refused before a command is sent.

The sections below describe the detailed browser routes. Passing a fixture check does not prove live support for every Chrome profile or website.
