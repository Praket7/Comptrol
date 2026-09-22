#!/usr/bin/env node
if (process.env.COMPTROL_DAEMON === "1") {
  require("./comptrol-daemon-mcp.js");
} else {
const { spawn } = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const { ensureChromeCdp, closeOwnedChrome } = require("./chrome-cdp.js");

function bundledBinary() {
  const platform = `${process.platform}-${process.arch}`;
  return path.join(__dirname, "..", "native", platform, process.platform === "win32" ? "comptrol.exe" : "comptrol");
}

const binary = process.env.COMPTROL_BIN || (fs.existsSync(bundledBinary()) ? bundledBinary() : undefined);
process.env.COMPTROL_STATE_DIR ||= path.join(os.homedir(), ".comptrol");
const bridgeMarker = path.join(process.env.COMPTROL_STATE_DIR, "browser-bridge.enabled");
let bridgeSidecar;

function postJson(port, endpoint, body, headers = {}) {
  const encoded = Buffer.from(JSON.stringify(body));
  return new Promise(resolve => {
    const request = http.request({
      host: "127.0.0.1",
      port,
      path: endpoint,
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "Content-Length": String(encoded.length),
        ...headers,
      },
      timeout: 350,
    }, response => {
      let responseBody = "";
      response.setEncoding("utf8");
      response.on("data", chunk => { responseBody += chunk; });
      response.on("end", () => {
        try {
          resolve({ status: response.statusCode || 0, body: JSON.parse(responseBody) });
        } catch {
          resolve({ status: response.statusCode || 0, body: null });
        }
      });
    });
    request.once("timeout", () => {
      request.destroy();
      resolve({ status: 0, body: null });
    });
    request.once("error", () => resolve({ status: 0, body: null }));
    request.end(encoded);
  });
}

function signedBridgeHeaders(token, method, endpoint, body) {
  const encoded = Buffer.from(JSON.stringify(body));
  const nonce = crypto.randomBytes(32).toString("hex");
  const bodyHash = crypto.createHash("sha256").update(encoded).digest("hex");
  const signingInput = Buffer.from(
    "comptrol.browser.bridge/0.1.0\0" +
    method + "\0" +
    endpoint + "\0" +
    nonce + "\0" +
    bodyHash + "\0",
    "ascii",
  );
  return {
    "X-Comptrol-Bridge-Nonce": nonce,
    "X-Comptrol-Bridge-Signature": crypto
      .createHmac("sha256", token)
      .update(signingInput)
      .digest("hex"),
  };
}

async function browserBridgeReady(port) {
  const tokenPath = path.join(process.env.COMPTROL_STATE_DIR, "browser-bridge.token");
  let token;
  try {
    token = fs.readFileSync(tokenPath, "utf8").trim();
  } catch {
    return false;
  }
  if (!/^[0-9a-fA-F]{64,}$/.test(token)) return false;

  const nonce = crypto.randomBytes(32).toString("hex");
  const challenge = await postJson(port, "/browser-auth/challenge", { nonce });
  if (
    challenge.status !== 200 ||
    challenge.body?.ok !== true ||
    challenge.body?.protocol !== "comptrol.browser.bridge/0.1.0" ||
    typeof challenge.body?.proof !== "string"
  ) {
    return false;
  }
  const expected = crypto
    .createHmac("sha256", token)
    .update("comptrol.browser.bridge/0.1.0")
    .update(Buffer.from([0]))
    .update(nonce)
    .digest();
  let received;
  try {
    received = Buffer.from(challenge.body.proof, "hex");
  } catch {
    return false;
  }
  if (received.length !== expected.length || !crypto.timingSafeEqual(received, expected)) {
    return false;
  }

  const statusBody = {};
  const status = await postJson(
    port,
    "/browser/status",
    statusBody,
    signedBridgeHeaders(token, "POST", "/browser/status", statusBody),
  );
  return status.status === 200 && status.body?.ok === true;
}

async function ensureBrowserBridgeSidecar() {
  if (!fs.existsSync(bridgeMarker)) return false;
  process.env.COMPTROL_AUTO_START_CHROME_CDP = "0";
  const port = Number(process.env.COMPTROL_DAEMON_PORT || 7317);
  if (await browserBridgeReady(port)) return true;
  bridgeSidecar = spawn(binary, ["serve-http", String(port)], {
    stdio: ["ignore", "ignore", "inherit"],
    env: process.env,
    windowsHide: true,
    shell: process.platform === "win32" && binary.toLowerCase().endsWith(".cmd"),
  });
  bridgeSidecar.on("error", error => {
    console.error(`Comptrol Browser Bridge sidecar failed to start: ${error.message}`);
  });
  for (let attempt = 0; attempt < 40; attempt += 1) {
    if (await browserBridgeReady(port)) return true;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  bridgeSidecar.kill();
  bridgeSidecar = undefined;
  console.error(`Comptrol Browser Bridge sidecar did not become ready on 127.0.0.1:${port}`);
  return false;
}

if (!binary) {
  console.error(`Comptrol native binary is missing for ${process.platform}-${process.arch}; reinstall the package or set COMPTROL_BIN`);
  process.exitCode = 1;
  return;
}
const args = ["mcp", ...process.argv.slice(2)];
const maxRestarts = 3;
const pending = [];
let pendingBytes = 0;
let child;
let restarts = 0;
let stopping = false;
let restartTimer;

function flushPending() {
  while (pending.length && child?.stdin.writable) {
    const chunk = pending.shift();
    pendingBytes -= chunk.length;
    child.stdin.write(chunk);
  }
}

function scheduleRestart(reason) {
  if (stopping || restarts >= maxRestarts) {
    console.error(`Comptrol stopped after ${restarts} restarts ${reason}`);
    process.exitCode = 1;
    process.stdin.destroy();
    return;
  }
  const delay = 100 * 2 ** restarts;
  restarts += 1;
  console.error(`Comptrol connection lost ${reason} reconnecting`);
  restartTimer = setTimeout(start, delay);
}

function start() {
  const current = spawn(binary, args, {
    stdio: ["pipe", "pipe", "inherit"],
    env: process.env,
    shell: process.platform === "win32" && binary.toLowerCase().endsWith(".cmd"),
  });
  child = current;
  let handled = false;
  current.stdout.pipe(process.stdout);
  current.on("error", (error) => {
    if (handled) return;
    handled = true;
    if (child === current) child = undefined;
    scheduleRestart(`because ${error.message}`);
  });
  current.stdin.on("error", (error) => {
    if (handled || stopping) return;
    handled = true;
    if (child === current) child = undefined;
    scheduleRestart(`because the child input closed ${error.message}`);
  });
  current.on("exit", (code, signal) => {
    if (handled) return;
    handled = true;
    if (child === current) child = undefined;
    if (stopping) {
      process.exitCode = code ?? 1;
      if (signal) console.error(`Comptrol stopped with ${signal}`);
      return;
    }
    scheduleRestart(`with ${signal || `exit ${code ?? 1}`}`);
  });
  flushPending();
}

process.stdin.on("data", (chunk) => {
  if (child?.stdin.writable) {
    child.stdin.write(chunk);
    return;
  }
  pending.push(chunk);
  pendingBytes += chunk.length;
  if (pendingBytes > 1024 * 1024) {
    console.error("Comptrol input exceeded the reconnect buffer limit");
    process.exitCode = 1;
    stopping = true;
    child?.kill();
  }
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    stopping = true;
    clearTimeout(restartTimer);
    process.stdin.destroy();
    child?.kill(signal);
    bridgeSidecar?.kill(signal);
    closeOwnedChrome();
  });
}

ensureBrowserBridgeSidecar()
  .catch(error => {
    console.error(`Comptrol Browser Bridge sidecar setup failed: ${error.message}`);
    return false;
  })
  .then(() => ensureChromeCdp())
  .finally(() => start());
}
