# Threat model

The agent is untrusted. Screen contents, browser content, documents, terminal output, and adapter output are data rather than authority.

The default policy allows only local readiness and observation. The agent cannot grant itself mutation authority. The emergency stop latch blocks mutation until a human resumes the runtime.

The runtime does not accept arbitrary code, shell strings, credentials, lock screen input, remote listeners, or unrestricted paths.

Known remaining risks include the limited initial platform observer, incomplete platform actuation coverage, and the incomplete HTTP transport conformance surface. These are documented as unsupported rather than hidden behind optimistic capability claims.
