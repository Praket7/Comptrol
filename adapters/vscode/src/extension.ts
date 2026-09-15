import * as net from "node:net";
import * as vscode from "vscode";

// This bridge deliberately exposes a closed allowlist of official VS Code APIs.
const allowed = new Set([
  "vscode.workspace.list",
  "vscode.setting.get",
  "vscode.setting.set",
  "vscode.document.open",
  "vscode.document.save",
]);

export function activate(context: vscode.ExtensionContext): void {
  const socketPath = process.env.COMPTROL_VSCODE_BRIDGE_SOCKET;
  const bridgeToken = process.env.COMPTROL_VSCODE_BRIDGE_TOKEN;
  if (!socketPath || !bridgeToken) return;
  const server = net.createServer((socket) => {
    let buffer = "";
    socket.on("data", async (chunk) => {
      buffer += chunk.toString("utf8");
      let newline = buffer.indexOf("\n");
      while (newline >= 0) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        newline = buffer.indexOf("\n");
        try {
          const envelope = JSON.parse(line);
          if (envelope.version !== 1 || envelope.token !== bridgeToken || typeof envelope.nonce !== "string") {
            throw new Error("bridge authentication failed");
          }
          const request = envelope.request ?? {};
          const intent = request.payload?.intent;
          if (!allowed.has(intent)) throw new Error("intent is not allowlisted");
          const payload = await execute(intent, request.payload ?? {});
          socket.write(JSON.stringify({ nonce: envelope.nonce, authenticated: true, ok: true, health: "available", payload }) + "\n");
        } catch (error) {
          socket.write(JSON.stringify({ nonce: undefined, authenticated: false, ok: false, health: "degraded", error: { code: "bridge_request_failed", message: String(error) } }) + "\n");
        }
      }
    });
  });
  server.listen(socketPath, "127.0.0.1");
  context.subscriptions.push({ dispose: () => server.close() });
}

async function execute(intent: string, payload: Record<string, unknown>): Promise<Record<string, unknown>> {
  switch (intent) {
    case "vscode.workspace.list":
      return { folders: (vscode.workspace.workspaceFolders ?? []).map((folder) => folder.uri.toString()), verified: true };
    case "vscode.setting.get": {
      const key = String(payload.key ?? "");
      if (!key || key.includes("..")) throw new Error("invalid setting key");
      return { key, value: vscode.workspace.getConfiguration().get(key), verified: true };
    }
    case "vscode.setting.set": {
      const key = String(payload.key ?? "");
      if (!key || key.includes("..")) throw new Error("invalid setting key");
      await vscode.workspace.getConfiguration().update(key, payload.value, vscode.ConfigurationTarget.Workspace);
      return { key, value: vscode.workspace.getConfiguration().get(key), verified: true };
    }
    case "vscode.document.open": {
      const uri = vscode.Uri.parse(String(payload.uri ?? ""));
      const document = await vscode.workspace.openTextDocument(uri);
      return { uri: document.uri.toString(), languageId: document.languageId, isDirty: document.isDirty, verified: true };
    }
    case "vscode.document.save": {
      const uri = vscode.Uri.parse(String(payload.uri ?? ""));
      const document = await vscode.workspace.openTextDocument(uri);
      return { uri: document.uri.toString(), saved: await document.save(), isDirty: document.isDirty, verified: !document.isDirty };
    }
    default: throw new Error("unsupported intent");
  }
}
