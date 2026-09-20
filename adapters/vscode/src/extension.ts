import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import * as vscode from "vscode";

// This bridge deliberately exposes a closed allowlist of official VS Code APIs.
const allowed = new Set([
  "vscode.workspace.list",
  "vscode.setting.get",
  "vscode.setting.set",
  "vscode.document.open",
  "vscode.document.save",
]);

const TOKEN_SECRET_KEY = "comptrol.bridgeToken";

function stateDir(): string {
  const override = process.env.COMPTROL_STATE_DIR;
  if (override) return override;
  return path.join(os.homedir(), ".comptrol");
}

function defaultEndpoint(): string {
  if (process.platform === "win32") return "pipe:comptrol-vscode-bridge";
  const dir = path.join(stateDir(), "bridges");
  fs.mkdirSync(dir, { recursive: true });
  return path.join(dir, "vscode.sock");
}

function publishDescriptor(endpoint: string, instance: Record<string, unknown>): void {
  // The endpoint only. The token lives in SecretStorage (or the launch
  // environment) and is never written to disk.
  const dir = path.join(stateDir(), "bridges");
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  const descriptor = path.join(dir, "vscode.json");
  fs.writeFileSync(
    descriptor,
    JSON.stringify({ version: 1, endpoint, instance }),
    { mode: 0o600 },
  );
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const configuredEndpoint =
    process.env.COMPTROL_VSCODE_BRIDGE_SOCKET ?? defaultEndpoint();
  let bridgeToken =
    process.env.COMPTROL_VSCODE_BRIDGE_TOKEN ??
    (await context.secrets.get(TOKEN_SECRET_KEY));
  if (!bridgeToken) {
    // First activation pairs this instance: generate one token, keep it in
    // platform SecretStorage, and publish the endpoint for daemon discovery.
    // The daemon must be given the same token once through `comptrol setup`.
    bridgeToken = crypto.randomBytes(32).toString("hex");
    await context.secrets.store(TOKEN_SECRET_KEY, bridgeToken);
  }
  const token: string = bridgeToken;
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
          if (envelope.version !== 1 || envelope.token !== token || typeof envelope.nonce !== "string") {
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
  const listenTarget =
    configuredEndpoint.startsWith("pipe:")
      ? `\\\\.\\pipe\\${configuredEndpoint.slice("pipe:".length)}`
      : configuredEndpoint;
  if (process.platform === "win32" && !configuredEndpoint.startsWith("pipe:")) {
    // A bare env path on Windows is treated as a named pipe name.
    server.listen(`\\\\.\\pipe\\${path.basename(configuredEndpoint)}`);
  } else {
    server.listen(listenTarget);
  }
  publishDescriptor(configuredEndpoint, {
    workspaceFolders: (vscode.workspace.workspaceFolders ?? []).map((folder) => folder.uri.toString()),
    windowId: `${vscode.env.sessionId}`,
  });
  context.subscriptions.push({ dispose: () => server.close() });
}

async function execute(intent: string, payload: Record<string, unknown>): Promise<Record<string, unknown>> {
  switch (intent) {
    case "vscode.workspace.list":
      return { folders: (vscode.workspace.workspaceFolders ?? []).map((folder) => folder.uri.toString()), verified: true, verification: "vscode_workspace_readback" };
    case "vscode.setting.get": {
      const key = String(payload.key ?? "");
      if (!key || key.includes("..")) throw new Error("invalid setting key");
      return { key, value: vscode.workspace.getConfiguration().get(key), verified: true, verification: "vscode_configuration_readback" };
    }
    case "vscode.setting.set": {
      const key = String(payload.key ?? "");
      if (!key || key.includes("..")) throw new Error("invalid setting key");
      await vscode.workspace.getConfiguration().update(key, payload.value, vscode.ConfigurationTarget.Workspace);
      return { key, value: vscode.workspace.getConfiguration().get(key), verified: true, verification: "vscode_configuration_readback" };
    }
    case "vscode.document.open": {
      const uri = vscode.Uri.parse(String(payload.uri ?? ""));
      const document = await vscode.workspace.openTextDocument(uri);
      return { uri: document.uri.toString(), languageId: document.languageId, isDirty: document.isDirty, verified: true, verification: "vscode_document_readback" };
    }
    case "vscode.document.save": {
      const uri = vscode.Uri.parse(String(payload.uri ?? ""));
      const document = await vscode.workspace.openTextDocument(uri);
      await document.save();
      // Persisted-artifact verification: the saved file must exist on disk
      // with nonzero size, not merely report a clean buffer.
      const stat = await vscode.workspace.fs.stat(document.uri);
      return { uri: document.uri.toString(), saved: true, size: stat.size, mtime: stat.mtime, isDirty: document.isDirty, verified: !document.isDirty && stat.size > 0, verification: "vscode_persisted_artifact_stat" };
    }
    default: throw new Error("unsupported intent");
  }
}
