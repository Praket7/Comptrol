# Security

Report suspected security issues privately to the repository maintainer rather than opening a public issue with exploit details.

The initial release is local only by default. Pairing records are local consent state and do not enable remote control. An opt in mutual TLS transport exists and completes mTLS before parsing HTTP, but certificate identity is not yet bound to pairing identity or per session scopes, so remote mutation must still be considered incomplete. Do not expose the HTTP preview beyond loopback. Browser DevTools endpoints are also restricted to loopback and require explicit local policy for mutation.
