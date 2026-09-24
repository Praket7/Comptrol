# Start with Comptrol

Comptrol runs on your computer. It connects an assistant to a small set of computer actions. Start with the local setup when using a desktop assistant.

## Install the local runtime

Install Node.js with npm, then install the `comptrolling` package.

The npm release for these changes is version 0.1.67. For local Codex setup, follow [the local Codex guide](../plugins/comptrol/CHATGPT_SETUP.md).

```sh
npm install -g comptrolling
```

In your assistant's local MCP settings, set the command to `comptrolling` with no arguments. The exact settings screen differs by assistant. See [client setup](client-integration.md) for examples.

## Run two harmless checks

Open a fresh assistant session. Ask it to use Comptrol's inspect tool with `kind` set to `doctor`. The report separates local configuration from routes that have actually passed a live check. It labels untouched routes as not tested.

Next, ask it to run the `system.ping` operation. A working connection returns `ready` set to true with verification set to `verified`. This confirms the assistant reached Comptrol. It does not test an app or grant permission to change anything.

## Give macOS permission

On macOS, open System Settings. Choose Privacy and Security, then Accessibility. Add the Comptrol runtime shown by your assistant settings. Restart the assistant after changing this permission.

Only grant Accessibility when you want Comptrol to read or change controls in apps. App launch and browser opening use narrower system features.

## Use Chrome

Comptrol can open a URL in the default browser. That alone does not let it read or change a page. For browser control, connect a local Chrome debugging endpoint or install the optional Browser Bridge extension.

The Browser Bridge extension works with your existing signed in Chrome profile. Chrome asks you to load the extension yourself. Comptrol does not install or silently approve it.

## Connect another app

Each app has its own support and setup. Read [adapter support](adapters.md), then open that app's guide in the `adapters` folder. A listed feature may need an app installed, a local permission, or a service token.

## ChatGPT on the web

The local runtime cannot be reached by ChatGPT on the web. ChatGPT needs a separate HTTPS endpoint that can be reached from the internet. You must configure that connection and its access controls. A local server address is not a secure remote connection.

## Keep control

Comptrol records actions on your computer. Review the permissions before enabling changes. Use the local emergency stop when needed. Restarting after a stop requires a person at the computer.
