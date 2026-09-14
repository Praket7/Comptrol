# Security

Report suspected security issues privately to the repository maintainer rather than opening a public issue with exploit details.

The initial release is local only by default. Pairing records are local consent state and do not enable remote control. Remote transport is disabled until mutual TLS is implemented. Do not expose the HTTP preview beyond loopback. Browser DevTools endpoints are also restricted to loopback and require explicit local policy for mutation.
