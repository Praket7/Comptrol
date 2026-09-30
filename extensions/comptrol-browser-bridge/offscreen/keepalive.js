/**
 * Comptrol offscreen keep-alive.
 *
 * Chrome suspends MV3 service workers after ~30 s idle, which silently kills
 * the native messaging port and all browser-bridge commands. A connected
 * extension port resets the SW idle timer on every message; this document —
 * which Chrome keeps running independently of the SW — reconnects its port
 * whenever the SW restarts and pings it every KEEPALIVE_PING_INTERVAL_MS.
 *
 * If a ping goes unacked for several intervals the SW is wedged or suspended:
 * a fresh chrome.runtime.connect wakes it immediately (connection attempts
 * are delivered as startup events), and the SW's onConnect listener re-binds.
 */
const KEEPALIVE_PORT_NAME = "comptrol-keepalive";
const PING_INTERVAL_MS = 20000;
const RECONNECT_CHECK_MS = 5000;

let port = null;
let lastAckAt = 0;
let pingsSent = 0;

function status() {
  return {
    connected: Boolean(port),
    lastAckAt,
    pingsSent,
    msSinceAck: lastAckAt ? Date.now() - lastAckAt : null
  };
}

function connect() {
  try {
    port = chrome.runtime.connect({ name: KEEPALIVE_PORT_NAME });
    pingsSent = 0;
    port.onMessage.addListener(message => {
      if (message?.type === "keepalive-ack") lastAckAt = Date.now();
    });
    port.onDisconnect.addListener(() => {
      port = null;
    });
    // First ping immediately: proves the fresh SW is listening.
    ping();
  } catch (error) {
    port = null;
  }
}

function ping() {
  if (!port) {
    connect();
    return;
  }
  try {
    port.postMessage({ type: "keepalive-ping", at: Date.now() });
    pingsSent += 1;
  } catch (error) {
    port = null;
    connect();
  }
}

setInterval(ping, PING_INTERVAL_MS);
// Watchdog: if acks stop arriving, drop and reconnect (wakes a suspended SW).
setInterval(() => {
  if (port && pingsSent > 2 && Date.now() - lastAckAt > PING_INTERVAL_MS * 3) {
    try { port.disconnect(); } catch {}
    port = null;
  }
  if (!port) connect();
}, RECONNECT_CHECK_MS);

connect();
