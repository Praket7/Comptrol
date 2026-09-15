# Comptrol Windows verification prompt

Run this task on the Windows computer in a visible interactive desktop session.

Clone the private repository `Praket7/Comptrol` with the GitHub integration. If the npm package has already been published, install the package from npm and verify that its launcher resolves the checked out release. If it has not been published, use the checked out source and do not pretend that npm installation succeeded.

Use PowerShell. Keep Chrome visible when testing foreground behavior. Use a separate Chrome test profile unless the user explicitly asks to verify an existing signed in profile. Never export, copy, or print cookies, tokens, passwords, or session storage. A background tab must use the same explicitly selected Chrome profile when account continuity is requested. It must not create a second profile or silently transfer credentials.

Run these checks from the repository root.

```powershell
git status --short
rustup show active-toolchain
node --version
npm --version
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
python scripts/lint_readme.py
npm run test:browser
npm run test:launcher
npm run test:daemon-launcher
python scripts/client_conformance.py
python scripts/http_conformance.py
python scripts/ipc_conformance.py
python scripts/platform_conformance.py
python scripts/recovery_conformance.py
python scripts/progress_conformance.py
python scripts/tasks_conformance.py
python scripts/privacy_conformance.py
python scripts/command_conformance.py
python scripts/pairing_conformance.py
python scripts/release_conformance.py
python scripts/windows_uia_conformance.py
```

Start the rebuilt release binary and verify its health through the Windows named pipe daemon. Verify that a client disconnect followed by reconnect does not replay a request that was already sent. Verify that the durable HTTP session file survives a process restart and that a Streamable HTTP GET receives a later event on a separate connection. Verify `Last-Event-ID` replay after restart.

For Chrome, verify all of the following with an explicit local DevTools endpoint and the exact intended profile.

Open a new tab in the visible browser.

Open a background tab in the same profile without taking foreground focus.

Read the exact tab identity before every mutation.

Navigate, evaluate a bounded DOM query, fill a test form, and submit it once.

Repeat the same idempotency key and verify that the fixture rejects the duplicate mutation.

Close only the exact selected tab.

Verify that history back and forward preserve the selected tab identity.

Verify that Ctrl C and Ctrl V remain available to the user because the test uses CDP and semantic actions rather than synthetic global mouse or clipboard control.

If native Chrome automation or Windows UI Automation is unavailable, record the exact permission or endpoint error and mark that check blocked. Do not report it as passing and do not weaken the policy gate.

At the end report the commit, every command, every pass, every blocked check, the Chrome profile used, whether the browser stayed visible, and whether any account data was copied. The final report must say changed, tested, live verified, blocked, or unverified for each capability.
