# Platform brokers

The capability broker reports the active operating system route instead of claiming universal support.

On macOS it checks the public System Events accessibility surface. On Windows it reports the UI Automation route only when the platform backend explicitly declares readiness. On Linux it reports AT SPI, X11, and Wayland presence from the active session environment.

Detection is not actuation. A detected display or bus does not make semantic mutation available. `platform.broker.observe` returns the broker descriptor and the explicit read only boundary. The action capability stays unavailable until its adapter and verification suite pass.

Windows semantic routes use UI Automation control patterns. `windows.uia.press` requires an exact process id plus a name or automation id and uses InvokePattern. `windows.uia.set_value` uses ValuePattern and reads the value back before reporting verified. The route is bounded and does not synthesize mouse or keyboard input.

Linux semantic routes use AT SPI when the session exposes the bus. `linux.atspi.press` resolves one accessible by process id and name and invokes its action interface. `linux.atspi.set_value` uses the editable text interface when available and verifies the resulting text. Wayland input injection remains separate and unsupported until a portal and libei consent path is validated.

Doctor reports broker state explicitly. A configured but read only broker is degraded. A platform permission that needs user approval is requires human consent. A broker for another operating system is unsupported.

The macOS fixture source is `fixtures/macos/AccessibilityFixture.swift`. `scripts/macos_ax_conformance.py` compiles it and exercises an AX press with a name postcondition. The Windows fixture is `fixtures/windows/UIAutomationFixture.ps1` and the Linux fixture is `fixtures/linux/atspi_fixture.py`. Their conformance harnesses skip on other operating systems and fail when explicitly required but unavailable.

`desktop.open_app` is a separate explicit policy route. On macOS it launches an exact application name through LaunchServices and checks for the running process when Accessibility permits it. On Windows it uses `Start-Process` with an environment bound argument. On Linux it uses `gtk-launch` with an application desktop id. Non macOS launchers report accepted by the native launcher without claiming process verification. None of these routes synthesize mouse input or touch the clipboard.
