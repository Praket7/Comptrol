# Platform brokers

The capability broker reports the active operating system route instead of claiming universal support.

On macOS it checks the public System Events accessibility surface. On Windows it reports the UI Automation route only when the platform backend explicitly declares readiness. On Linux it reports AT SPI, X11, and Wayland presence from the active session environment.

Detection is not actuation. A detected display or bus does not make semantic mutation available. `platform.broker.observe` returns the broker descriptor and the explicit read only boundary. The action capability stays unavailable until its adapter and verification suite pass.
