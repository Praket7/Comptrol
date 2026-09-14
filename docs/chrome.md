# Real Chrome conformance

Run `python3 scripts/chrome_conformance.py` on a host with Chrome installed.

The harness creates a temporary Chrome profile, serves the local fixture, discovers the exact page target, opens a background tab in that same profile, waits for its DOM without foreground focus, navigates through CDP, verifies a DOM value through `Runtime.evaluate`, and uploads a sandbox file through `DOM.setFileInputFiles` with a filename postcondition. It never attaches to the user browser profile.

The normal repository check uses the deterministic fixture websocket. Real Chrome validation is a host acceptance test because Chrome is not guaranteed on CI.

For a user visible validation, start Chrome with a loopback DevTools endpoint and set `COMPTROL_CDP_ENDPOINT` plus `COMPTROL_ALLOW_BROWSER_CDP`. The validation must use the user chosen profile intentionally. Comptrol will reuse that profile but will not copy cookies or credentials into another profile.
