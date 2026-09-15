# ChatGPT and Codex plugin

The repository contains a current portable Agent Plugins package at `plugins/comptrol`. Its root `plugin.json` uses the Agent Plugins schema, its root `mcp.json` points at the local Streamable HTTP endpoint, its compatibility manifest remains under `.codex-plugin/plugin.json`, and its skill explains the verified-control contract.

The repo marketplace is `.agents/plugins/marketplace.json`. Validate the package with `python3 scripts/validate_plugin_package.py` and the Codex compatibility manifest with the plugin creator validator.

Start the local endpoint with `comptrol-http` after installing the npm package, or with `comptrol serve-http` from a native checkout. The endpoint remains loopback-only. A ChatGPT desktop user must enable developer mode, register the local MCP connection, approve the connection, and then install the local marketplace plugin. The repository does not invent or embed a `plugin_asdk_app` registration id, and the local package is not claimed to be publicly submitted or live-tested in ChatGPT.

The portable MCP schema supports remote HTTPS servers for public submission. This package intentionally keeps the local endpoint explicit so it cannot silently turn local computer control into an unauthenticated remote listener.
