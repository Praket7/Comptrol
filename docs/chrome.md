# Real Chrome conformance

Run `python3 scripts/chrome_conformance.py` on a host with Chrome installed.

The harness creates a temporary Chrome profile, serves the local fixture, discovers the exact page target, opens a background tab in that same profile, waits for its DOM without foreground focus, navigates through CDP, verifies a DOM value through `Runtime.evaluate`, and uploads a sandbox file through `DOM.setFileInputFiles` with a filename postcondition. It never attaches to the user browser profile.

The temporary headless profile is intentionally unauthenticated. Signed in accounts are preserved only by the visible profile route, which attaches to the user selected browser profile. Account cookies and credentials are never copied between profiles.

The real Chrome harness also writes a disposable fixture cookie in the initial page and verifies that a background page created in the same browser profile can observe it. This proves profile continuity without reading credentials or transferring browser secrets.

`browser.chrome.open_tab` is a separate opt in foreground launcher route for machines without a DevTools endpoint. It asks the installed Chrome launcher to open the URL in the existing default browser profile, so signed in state stays with Chrome. It reports unverified launcher acceptance because no exact target identity is available. Strict background posture refuses this route and directs callers to CDP.

The normal repository check uses the deterministic fixture websocket. Real Chrome validation is a host acceptance test because Chrome is not guaranteed on CI.

For a user visible validation, start Chrome with a loopback DevTools endpoint and set `COMPTROL_CDP_ENDPOINT` plus `COMPTROL_ALLOW_BROWSER_CDP`. The validation must use the user chosen profile intentionally. Comptrol will reuse that profile but will not copy cookies or credentials into another profile.

Run `python3 scripts/visible_chrome_conformance.py` for an opt in acceptance check. It opens one visible and one background `about:blank` tab in that profile, checks the profile and input invariants, and closes only those exact test tabs. Set `COMPTROL_REQUIRE_VISIBLE_CHROME=1` when the endpoint must be present.
