# Local permissions

Comptrol checks local rules before it changes anything. An assistant cannot give itself new permission.

## What works by default

Comptrol can answer a basic status check, observe supported computer state, open an exact installed app, and open a URL in Chrome. Opening an app does not grant permission to edit it. Opening a page does not grant permission to read or change its contents.

## Changes need permission

File writes are limited to the Comptrol sandbox. Local settings can enable sandbox writes or desktop notifications. These switches are broad. Keep them off unless you need them.

Some sensitive actions also need a saved approval for the exact capability and resource. Comptrol stores those approvals on your computer. A missing, damaged, expired, or revoked approval blocks the action. The doctor report shows when the approval store cannot be read.

The caller can ask for a stricter risk level. It cannot lower the server's own risk classification. The emergency stop blocks changes even when the caller labels them as read only.

## Separate hosted access

These rules protect the local runtime. ChatGPT on the web needs a separately configured authenticated HTTPS connection. Do not expose the local loopback service directly to the internet.
