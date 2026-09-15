# OBS adapter

The adapter uses OBS WebSocket 5.x over an explicitly configured loopback endpoint. It performs Identify and protocol negotiation before any request. Recording and streaming mutations are high consequence and require an explicit capability token; starting a public stream is not advertised.

