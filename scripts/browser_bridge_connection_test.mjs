import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { performance } from "node:perf_hooks";

const workerPath = new URL("../extensions/comptrol-browser-bridge/src/service_worker.js", import.meta.url);
const worker = await readFile(workerPath, "utf8");
const start = worker.indexOf("async function connectNative() {");
const nextComment = worker.indexOf("Schedule reconnection", start);
const end = worker.lastIndexOf("/**", nextComment);
assert(start >= 0 && end > start, "connectNative function markers must exist");
const connectSource = worker.slice(start, end);

class Listener {
  callbacks = new Set();
  addListener(callback) { this.callbacks.add(callback); }
  removeListener(callback) { this.callbacks.delete(callback); }
  emit(value) { for (const callback of [...this.callbacks]) callback(value); }
}

function harness() {
  const port = { onMessage: new Listener(), onDisconnect: new Listener(), postMessage() {}, disconnect() {} };
  const runtime = { id: "bpnakihocoimajcddkohnpgkepdmdkna", lastError: undefined, connectNative: () => port };
  let reconnects = 0;
  const create = new Function(
    "chrome", "handleNativeMessage", "scheduleReconnect", "setTimeout", "clearTimeout", "console",
    `let nativePort = null; let isConnected = false; let connectPromise = null; let reconnectAttempts = 0; let connectionStatus = "disconnected";\n` +
      `const NATIVE_HOST_NAME = "comptrol_browser_bridge";\n` +
      `const PROTOCOL_VERSION = "comptrol.browser.bridge/0.1.0";\n` +
      `${connectSource}\nreturn { connectNative, getConnected: () => isConnected, getPort: () => nativePort };`
  );
  const api = create(
    { runtime },
    () => {},
    () => { reconnects += 1; },
    globalThis.setTimeout,
    globalThis.clearTimeout,
    { warn() {}, log() {}, error() {} },
  );
  return { api, port, runtime, getReconnects: () => reconnects };
}

{
  const test = harness();
  const { api, port, runtime } = test;
  const started = performance.now();
  const pending = api.connectNative();
  queueMicrotask(() => {
    runtime.lastError = { message: "Specified native messaging host not found." };
    port.onDisconnect.emit();
    runtime.lastError = undefined;
  });
  await assert.rejects(pending, /Specified native messaging host not found/);
  assert(performance.now() - started < 1000, "missing host should fail immediately, not wait for the handshake timeout");
  assert.equal(api.getPort(), null);
  assert.equal(test.getReconnects(), 0, "a failed initial handshake must not schedule reconnect twice");
}

{
  const test = harness();
  const { api, port } = test;
  port.postMessage = message => {
    assert.equal(message.type, "handshake");
    queueMicrotask(() => port.onMessage.emit({ type: "handshake_ack", protocol: "comptrol.browser.bridge/0.1.0", extension_id: test.runtime.id, daemon_authorized: true, daemon_state: "ready" }));
  };
  await api.connectNative();
  assert.equal(api.getConnected(), true);
  port.onDisconnect.emit();
  assert.equal(api.getConnected(), false);
  assert.equal(test.getReconnects(), 1, "an established connection should schedule one reconnect");
}

for (const overrides of [
  { protocol: "wrong" }, { extension_id: "wrong" },
  { daemon_authorized: false }, { daemon_state: "not_ready" },
]) {
  const test = harness();
  test.port.postMessage = () => queueMicrotask(() => test.port.onMessage.emit({
    type: "handshake_ack", protocol: "comptrol.browser.bridge/0.1.0",
    extension_id: test.runtime.id, daemon_authorized: true, daemon_state: "ready", ...overrides,
  }));
  await assert.rejects(test.api.connectNative());
  assert.equal(test.api.getConnected(), false);
}

console.log(JSON.stringify({ missing_host_error: "immediate", handshake_failure_ms_bound: 1000, false_timeout: false }));
