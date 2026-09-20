// Comptrol Canva Companion — background service worker
// Manifest V3: forwards versioned, design-bound operation envelopes
// from the content relay to the localhost runtime via native messaging.

const PROTOCOL = "comptrol.canva.bridge/0.1.0";
const RUNTIME_PORT_NAME = "comptrol_canva_bridge";

let port = null;
const pendingRequests = new Map();
let requestCounter = 0;

function connect() {
  try {
    port = chrome.runtime.connectNative(RUNTIME_PORT_NAME);
    port.onMessage.addListener(onNativeMessage);
    port.onDisconnect.addListener(() => { port = null; });
  } catch (_) { port = null; }
}

function onNativeMessage(response) {
  const requestId = response.request_id;
  if (!requestId || !pendingRequests.has(requestId)) {
    return;
  }
  const { resolve, reject } = pendingRequests.get(requestId);
  pendingRequests.delete(requestId);
  if (response.ok) {
    resolve(response);
  } else {
    reject(new Error(response.error || "native_host_error"));
  }
}

function post(envelope) {
  return new Promise((resolve, reject) => {
    if (!port) { connect(); }
    if (!port) { reject(new Error("native_messaging_not_available")); return; }
    
    const requestId = ++requestCounter;
    const messageWithId = { ...envelope, request_id: requestId };
    
    pendingRequests.set(requestId, { resolve, reject });
    
    try {
      port.postMessage(messageWithId);
      // Timeout after 30 seconds
      setTimeout(() => {
        if (pendingRequests.has(requestId)) {
          pendingRequests.delete(requestId);
          reject(new Error("native_messaging_timeout"));
        }
      }, 30000);
    } catch (e) {
      pendingRequests.delete(requestId);
      reject(e);
    }
  });
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  const origin = sender?.url ? new URL(sender.url).origin : "";
  if (!isAllowedOrigin(origin)) { 
    sendResponse({ ok: false, error: "origin_not_allowed" }); 
    return true; 
  }
  
  try {
    const envelope = { ...message, protocol: PROTOCOL };
    post(envelope)
      .then(response => sendResponse({ ok: true, ...response }))
      .catch(error => sendResponse({ ok: false, error: error.message }));
  } catch (error) {
    sendResponse({ ok: false, error: error.message });
  }
  return true; // async response
});

function isAllowedOrigin(origin) {
  return origin === "https://www.canva.com" || origin === "https://canva.com";
}