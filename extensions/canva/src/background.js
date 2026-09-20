// Comptrol Canva Companion — background service worker
// Manifest V3: forwards versioned, design-bound operation envelopes
// from the content relay to the localhost runtime via native messaging.

const PROTOCOL = "comptrol.canva.bridge/0.1.0";
const RUNTIME_PORT_NAME = "comptrol_canva_bridge";

let port = null;

function connect() {
  try {
    port = chrome.runtime.connectNative(RUNTIME_PORT_NAME);
    port.onDisconnect.addListener(() => { port = null; });
  } catch (_) { port = null; }
}

function post(envelope) {
  if (!port) { connect(); }
  if (!port) { throw new Error("native_messaging_not_available"); }
  try { port.postMessage(envelope); } catch (_) { connect(); }
}

chrome.runtime.onMessageExternal.addListener((message, sender, sendResponse) => {
  const origin = sender?.url ?? "";
  if (!isAllowedOrigin(origin)) { sendResponse({ ok: false, error: "origin_not_allowed" }); return true; }
  try {
    const envelope = { ...message, protocol: PROTOCOL };
    post(envelope);
    sendResponse({ ok: true });
  } catch (error) {
    sendResponse({ ok: false, error: error.message });
  }
  return true;
});

function isAllowedOrigin(origin) {
  return origin === "https://www.canva.com" || origin === "https://canva.com";
}
