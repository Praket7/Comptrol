# ChatGPT and Codex plugin

The repository contains a portable Agent Plugins package at `plugins/comptrol`. Its root `plugin.json` uses the Agent Plugins schema, its root `mcp.json` points at the local Streamable HTTP endpoint, its compatibility manifest remains under `.codex-plugin/plugin.json`, and its skill explains the verified-control contract.

The repo marketplace is `.agents/plugins/marketplace.json`. Validate the package with `python3 scripts/validate_plugin_package.py` and the Codex compatibility manifest with the plugin creator validator.

For ChatGPT web on a personal Plus account, follow [`plugins/comptrol/CHATGPT_SETUP.md`](../plugins/comptrol/CHATGPT_SETUP.md). It connects the existing stdio server through OpenAI Secure MCP Tunnel; ChatGPT cannot reach the local `127.0.0.1` endpoint directly. Codex continues to use its local stdio integration.

The portable package intentionally keeps its local endpoint explicit. It is not a public ChatGPT submission: public distribution needs a stable HTTPS service, per-user identity and authorization, and an outbound paired local-agent design. The local preview must never be exposed as an unauthenticated public listener.

Verified workflows can be compiled from a privacy-aware trace and validated before replay:

```text
comptrol workflow compile trace.jsonl bills-article > workflow.json
comptrol workflow validate workflow.json '{"target_id":"page-1","revision":"r1"}'
```

Validation rejects a changed workflow fingerprint or stale target precondition. The compiler emits closed JSON IR only; it does not embed arbitrary code or private typed content.
