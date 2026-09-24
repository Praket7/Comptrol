# Current support

Checked on 23 September 2026. Version 0.1.66 is listed on npm. The matching public GitHub release contains runtime archives for all five listed processor and operating system combinations. This checks package availability. It does not prove every adapter works on every computer.

| Computer | Runtime package | Computer controls | Evidence in this checkout |
|---|---|---|---|
| macOS Apple silicon | Available | App and browser actions work through supported routes. Semantic app control needs Accessibility permission. | Workspace tests and local browser fixture passed on macOS ARM64. |
| macOS Intel | Available | Same permission boundary as Apple silicon. | Release archive is published. No Intel live run was performed here. |
| Windows x64 | Available | UI Automation needs an active desktop session plus local permission. | Release archive is published. A Windows junction test is in the test suite. |
| Linux x64 | Available | Accessibility control needs an active desktop accessibility session. | Release archive is published. No Linux desktop session was tested here. |
| Linux ARM64 | Available | Same session requirement as Linux x64. | Release archive is published. No Linux desktop session was tested here. |

The npm package requires Node.js 18 or newer. It starts the matching native runtime. App controls depend on installed apps, local permission, each app's own connection settings. The 0.1.66 npm release predates the changes in this GitHub update. A new npm release is required to receive these fixes through npm.

## What live means

The doctor report has separate fields for local environment availability, local policy permission, operation attempts, verified successes. An available route is ready to try. It is not proof that an external app is installed. A verified success means the current runtime observed a successful result at least once.

Comptrol binds browser operations to an exact tab, browser context, revision. It rejects stale references before sending a command. A previous success does not prove that another tab, browser profile, app version, or account will work.

## ChatGPT in a browser

Installing the npm package does not connect ChatGPT on the web to your computer. Hosted ChatGPT needs an HTTPS service reachable from the internet plus its own authentication setup. The local runtime in this release is not a complete public remote control service. The local HTTP preview must stay bound to loopback.

## Verified package references

The release lists macOS ARM64, macOS Intel, Windows x64, Linux ARM64, Linux x64 archives. The [GitHub release](https://github.com/Praket7/Comptrol/releases/tag/v0.1.66) is public. The [npm package](https://www.npmjs.com/package/comptrolling/v/0.1.66) lists the same package version.
