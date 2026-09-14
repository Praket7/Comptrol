# Platform brokers

The capability broker reports the active operating system route instead of claiming universal support.

On macOS it checks the public System Events accessibility surface. On Windows it reports the UI Automation route only when the platform backend explicitly declares readiness. On Linux it reports AT SPI, X11, and Wayland presence from the active session environment.

Detection is not actuation. A detected display or bus does not make semantic mutation available. `platform.broker.observe` returns the broker descriptor and the explicit read only boundary. The action capability stays unavailable until its adapter and verification suite pass.

Doctor reports broker state explicitly. A configured but read only broker is degraded. A platform permission that needs user approval is requires human consent. A broker for another operating system is unsupported.

The macOS fixture source is `fixtures/macos/AccessibilityFixture.swift`. `scripts/macos_ax_conformance.py` compiles it and exercises an AX press with a name postcondition. The harness reports a permission skip when the host has not granted Accessibility access and fails if permission is explicitly required.

`desktop.open_app` is a separate explicit policy route. On macOS it launches an exact application name through LaunchServices using an argument vector, then checks for the running process when Accessibility permits it. It does not synthesize mouse input or touch the clipboard. Other operating systems report this route as unsupported until a native launch adapter is validated.
