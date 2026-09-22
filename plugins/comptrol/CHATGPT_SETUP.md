# Use Comptrol in ChatGPT Plus

This guide connects the Comptrol process on your Mac to a private developer-mode app in ChatGPT web. It uses OpenAI Secure MCP Tunnel; it does not expose a public listener or use the Codex MCP configuration.

## Requirements

- ChatGPT Plus with Developer mode available on the web.
- Access to OpenAI Platform Tunnels and Runtime API keys.
- A local `comptrol` executable. Build it from the repository with `cargo build --release`, or use an installed Comptrol release.
- The official [`tunnel-client`](https://github.com/openai/tunnel-client) binary.

The ChatGPT plan and Platform tunnel permissions are separate. Platform access must let you create or use a tunnel and create a Restricted runtime key with Tunnels **Read** and **Use** permissions. Do not use an admin key as the long-running runtime key.

## Install the official tunnel client

On macOS, install from OpenAI's Homebrew tap:

```sh
brew install openai/tools/tunnel-client
tunnel-client --version
```

## Create the tunnel and restricted runtime key

In the [OpenAI Platform Tunnels settings](https://platform.openai.com/settings/organization/tunnels), create a tunnel and copy its ID. In [Runtime API keys](https://platform.openai.com/settings/organization/api-keys), create a **Restricted** key with Tunnels **Read** and **Use** only. The tunnel ID is an identifier; the runtime API key is a secret.

If the Platform UI does not offer these controls to your account, stop here and ask an organization owner/admin to grant access. Do not work around the restriction with a broad admin key.

## Configure and start Comptrol through the tunnel

Build the runtime from the repository, or use the full path to an installed `comptrol` executable. The tunnel profile launches the existing local stdio MCP server:

```sh
export CONTROL_PLANE_TUNNEL_ID="<your-tunnel-id>"
tunnel-client init \
  --sample sample_mcp_stdio_local \
  --profile comptrol-chatgpt \
  --tunnel-id "$CONTROL_PLANE_TUNNEL_ID" \
  --mcp-command "comptrol mcp"
```

If `comptrol` is not on `PATH`, replace `comptrol mcp` with `/absolute/path/to/comptrol mcp`.

Enter the restricted runtime key without echoing it into the terminal, then start the tunnel:

```sh
read -s "CONTROL_PLANE_API_KEY?Restricted runtime API key: "
export CONTROL_PLANE_API_KEY
printf '\n'
tunnel-client doctor --profile comptrol-chatgpt --explain
tunnel-client run --profile comptrol-chatgpt
```

Leave this process running while you use the ChatGPT app. It opens an outbound connection to OpenAI and forwards MCP requests to the local Comptrol process. Stop it with `Ctrl-C`; remove the tunnel or revoke the runtime key in Platform to revoke access.

## Add it to ChatGPT web

1. In ChatGPT, open **Settings → Security and login** and enable **Developer mode**.
2. Open **Plugins** (or **Apps**, depending on the current interface), select **+**, and create a developer-mode app.
3. Name it **Comptrol** and choose **Tunnel** for the connection method.
4. Select the tunnel created above, then save the app.
5. In a new ChatGPT web conversation, open the **+** menu, choose **Developer mode**, and select **Comptrol**.

Start with `capabilities` or `inspect` and confirm the returned machine is your Mac. Mutations remain subject to Comptrol's local policy and native human-approval gates. A tool call being accepted is not proof that its requested effect happened; check the returned verification state or inspect the result.

## Limits

- This is a private, single-user developer-mode connection. It does not publish a Plugin Directory listing.
- It requires the tunnel process, local Mac, and Comptrol runtime to remain available.
- The local MCP preview remains loopback-only. Do not expose it through a public unauthenticated URL.
- Public distribution requires a separately hosted, per-user OAuth service and a paired outbound local-agent design; a single personal tunnel cannot serve as a shared public endpoint.
