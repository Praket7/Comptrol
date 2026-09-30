# Detailed setup

Start with the [setup steps in the README](../README.md#set-up-comptrol). This page explains where setup saves assistant settings and what each system needs.

## Assistant settings

Comptrol can add its entry to the settings for Freebuff, Claude Code, Codex, and OpenCode. It leaves an existing settings file unchanged. If a file already exists, use the matching example in [Client settings](clients.md) and add the Comptrol entry to that file.

If an assistant already has a settings file, print its Comptrol entry with the commands below. Replace `Codex` with the name of your assistant.

On Windows run this in PowerShell.

```powershell
target\release\comptrol.exe setup --print Codex
```

On macOS or Linux run this in Terminal.

```sh
./target/release/comptrol setup --print Codex
```

## Windows

Setup registers the Chrome connection for your Windows user. Keep Chrome, Comptrol, and your assistant open under the same user account.

## macOS

Setup saves the Chrome connection under your user account. To let Comptrol read app controls, open System Settings, then Privacy and Security, then Accessibility. Add the Comptrol program from your checkout. Reopen your assistant after changing permission.

## Linux

Setup saves the Chrome connection under your user account. App control needs a signed in desktop session that provides accessibility information. A terminal only session may build and start Comptrol, but it cannot prove that app controls work.

## Check your setup

Start a new assistant session and run `inspect` with `kind` set to `doctor`. Then run `system.ping`. A ready connection reports `ready` as `true` and `verification` as `verified`.

For Chrome control, leave Chrome open with a normal web page. Check the Browser Bridge status and confirm that it reports an active connection. The Chrome extension cannot control special browser pages.

A successful ping only checks the connection between the assistant and Comptrol. Check the [support guide](support.md) for evidence about each feature.
