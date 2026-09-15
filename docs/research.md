# Research notes

The design was checked against current MCP transport documentation, Chromium DevTools Protocol documentation, GitHub source from CUA Driver, and recent GUI workflow compilation papers.

MCP documents stdio as a standard local transport and Streamable HTTP as a separate transport with origin validation requirements. Comptrol therefore keeps stdio as the canonical first path and treats the loopback HTTP server as a bounded local preview. The preview assigns an in memory session id after initialization, validates it on later requests, supports a finite server sent event readiness response, and refuses invalid origins.

CUA Driver source emphasizes exact tab binding, refusal when a route cannot preserve background posture, and independent verification. Comptrol carries those ideas into its result model without copying code.

ActionEngine reports a state machine and programmatic execution design with one or a few planning calls instead of one model call per GUI step. TraceCompiler and PreAct describe related compilation and replay ideas. Comptrol uses the narrower safe version of that idea, a closed workflow representation with explicit assertions and no arbitrary model supplied code.

Sources

1. Model Context Protocol transport specification
2. Chromium DevTools Protocol documentation
3. CUA Driver MCP documentation
4. ActionEngine paper with identifier 2602.20502
5. TraceCompiler paper with identifier 2608.02680
