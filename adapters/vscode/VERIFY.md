# Verification contract

`vscode.workspace.list` reads the active workspace folders from the extension API and returns the bridge workspace revision.

`vscode.setting.get` reads the setting through `workspace.getConfiguration`.

`vscode.setting.set` writes only a named configuration key, then reads it back through the API. Persistence is reported only after the extension API confirms the update.

`vscode.document.open` and `vscode.document.save` use the official workspace APIs and return the document URI plus dirty state after the operation.

