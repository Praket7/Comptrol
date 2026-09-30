/**
 * Comptrol Browser Bridge - Production Service Worker (v2 keep-alive)
 *
 * Bridges the Comptrol runtime with existing signed-in Chrome sessions.
 * It uses chrome.debugger to attach to existing tabs, chrome.tabGroups for
 * group management, chrome.sessions for closed tab/group recovery, and native
 * messaging for daemon communication.
 *
 * v2 adds the three things live failure analysis proved missing:
 *  1. KEEP-ALIVE: an offscreen document holds a long-lived extension port that
 *     pings the service worker every 20 s. Messages arriving over that port
 *     reset the SW idle timer, so the worker (and the native messaging host it
 *     spawned) stay alive while Chrome runs. chrome.alarms remains as a
 *     resurrection backstop.
 *  2. WAKE: a `wake` message from the native host (drained from the daemon's
 *     /browser/wake endpoint) triggers reconnect + targets replay, so a worker
 *     that did die recovers within ~1 s of the next command arriving.
 *  3. DEBUGGER HYGIENE: idle debugger attachments detach after 90 s, so the
 *     per-tab "started debugging this browser" infobar does not accumulate.
 */

// Protocol version for native messaging handshake
const PROTOCOL_VERSION = "comptrol.browser.bridge/0.1.0";
const NATIVE_HOST_NAME = "comptrol_browser_bridge";
const RECONNECT_BASE_DELAY_MS = 1000;
const RECONNECT_MAX_DELAY_MS = 60000;
const RECONNECT_MAX_EXPONENT = 6;

// Native messaging port
let nativePort = null;
let reconnectAttempts = 0;
let isConnected = false;
let reconnectTimer = null;
let connectPromise = null;
let listenersRegistered = false;
let profileId = null;
let profileSelected = false;
let connectionStatus = "profile_not_selected";

// K9: one identity per service-worker life. The durable command ledger stamps
// entries with it so a restart can tell its own in-flight work apart from a
// dead instance's pinned entries (which then expire honestly instead of
// blocking replays forever).
const swInstanceId = crypto.randomUUID();

// K1: per-command deadline discipline. chrome.debugger.sendCommand has no
// cancellation; a lost response previously hung the command chain until the
// host's timeout fired (observed live on SPA/OOPIF-heavy pages). On deadline
// the op is tagged orphaned and the attachment is remediated so the next
// command attaches a clean session. A late resolution of the raced promise is
// dropped by design.
class SwDeadlineError extends Error {
  constructor(label) {
    super(`sw_deadline: ${label}`);
    this.code = "sw_deadline";
    this.label = label;
  }
}

function withDeadline(promise, ms, label) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new SwDeadlineError(label)), ms);
    promise.then(
      value => { clearTimeout(timer); resolve(value); },
      error => { clearTimeout(timer); reject(error); }
    );
  });
}

// K5 (SW half): page-op health, reported with every bridge_ping result so the
// daemon can distinguish "channel alive" from "page ops alive". Live incident:
// pings stayed healthy while every debugger-backed command hung.
const pageOps = {
  lastOkAt: 0,
  lastFailAt: 0,
  orphaned: 0,
  remediations: 0
};

function pageOpsSnapshot() {
  return {
    last_ok_at_ms: pageOps.lastOkAt || null,
    last_fail_at_ms: pageOps.lastFailAt || null,
    orphaned: pageOps.orphaned,
    remediations: pageOps.remediations,
    instance_id: swInstanceId
  };
}

// ---------------------------------------------------------------------------
// Keep-alive: offscreen document heartbeat
// ---------------------------------------------------------------------------
const KEEPALIVE_PORT_NAME = "comptrol-keepalive";
const KEEPALIVE_PING_INTERVAL_MS = 20000;
const OFFSCREEN_KEEPALIVE_PATH = "offscreen/keepalive.html";
let keepaliveEnsuring = null;

async function hasOffscreenKeepalive() {
  try {
    const contexts = await chrome.runtime.getContexts({
      contextTypes: ["OFFSCREEN_DOCUMENT"],
      documentUrls: [chrome.runtime.getURL(OFFSCREEN_KEEPALIVE_PATH)]
    });
    return Array.isArray(contexts) && contexts.length > 0;
  } catch (error) {
    // getContexts unavailable on very old Chrome; assume absent.
    return false;
  }
}

async function ensureOffscreenKeepalive() {
  if (keepaliveEnsuring) return keepaliveEnsuring;
  keepaliveEnsuring = (async () => {
    try {
      if (await hasOffscreenKeepalive()) return;
      await chrome.offscreen.createDocument({
        url: OFFSCREEN_KEEPALIVE_PATH,
        reasons: ["IFRAME_SCRIPTING"],
        justification:
          "Keeps the Comptrol bridge service worker alive so browser automation commands keep working in background tabs."
      });
    } catch (error) {
      // createDocument throws if one already exists (race with another SW
      // instance); that is fine.
      if (!/single|already|only one/i.test(String(error?.message || error))) {
        console.warn("Offscreen keepalive setup failed:", error?.message || error);
      }
    } finally {
      keepaliveEnsuring = null;
    }
  })();
  return keepaliveEnsuring;
}

// A connected extension port delivered via chrome.runtime.connect resets the
// service-worker idle timer on every message. The offscreen document owns this
// port (it outlives SW suspension) and reconnects it after every SW restart.
chrome.runtime.onConnect.addListener(port => {
  if (port?.name !== KEEPALIVE_PORT_NAME) return;
  port.onMessage.addListener(message => {
    if (message?.type === "keepalive-ping") {
      try { port.postMessage({ type: "keepalive-ack", at: Date.now() }); } catch {}
    }
  });
  port.onDisconnect.addListener(() => {
    // The offscreen doc (or its port) went away; recreate it.
    void ensureOffscreenKeepalive();
  });
});

// ---------------------------------------------------------------------------
// Wake bus
// ---------------------------------------------------------------------------

/**
 * Reconnect the native port and replay fresh targets. Called on wake requests
 * from the native host and whenever the keeper notices disconnection.
 */
async function wakeAndReconnect(reason) {
  console.warn("Comptrol bridge wake:", reason || "unspecified");
  if (!profileSelected) return;
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  if (nativePort) {
    try { nativePort.disconnect(); } catch {}
    nativePort = null;
  }
  isConnected = false;
  try {
    await connectNative();
    await sendTargetsToNative();
  } catch (error) {
    connectionStatus = "daemon_unavailable";
    scheduleReconnect();
  }
}

function classifyConnectionFailure(error) {
  const reason = String(error?.message || error);
  if (/specified native messaging host not found/i.test(reason)) {
    connectionStatus = "host_registration_required";
    return true;
  }
  if (/access to the specified native messaging host is forbidden|not authorized/i.test(reason)) {
    connectionStatus = "authorization_required";
    return true;
  }
  if (/extension identity mismatch/i.test(reason)) {
    connectionStatus = "extension_identity_mismatch";
    return true;
  }
  if (/daemon_http_409|another Chrome profile is currently selected/i.test(reason)) {
    connectionStatus = "profile_in_use";
    return true;
  }
  connectionStatus = "daemon_unavailable";
  return false;
}

async function loadProfileState() {
  const stored = await chrome.storage.local.get(["comptrol_profile_id", "comptrol_profile_selected"]);
  profileId = stored.comptrol_profile_id;
  if (!profileId) {
    profileId = crypto.randomUUID();
    await chrome.storage.local.set({ comptrol_profile_id: profileId, comptrol_profile_selected: false });
  }
  profileSelected = stored.comptrol_profile_selected === true;
  return profileId;
}

function parseLocalTargetId(value) {
  const prefix = `${profileId}:`;
  if (typeof value !== "string" || !value.startsWith(prefix)) throw new Error("Target belongs to another Chrome profile");
  const tabId = Number(value.slice(prefix.length));
  if (!Number.isInteger(tabId) || tabId < 0) throw new Error("Invalid browser target ID");
  return tabId;
}

const tabGenerations = new Map();
const TAB_GENERATIONS_KEY = "comptrol_tab_generations";

async function loadTabGenerations() {
  const stored = await chrome.storage.local.get(TAB_GENERATIONS_KEY);
  for (const [key, generation] of Object.entries(stored[TAB_GENERATIONS_KEY] || {})) {
    if (key.startsWith(`${profileId}:`) && Number.isInteger(generation) && generation >= 0) tabGenerations.set(key, generation);
  }
}

function targetKey(tabId) { return `${profileId}:${tabId}`; }

async function bumpTabGeneration(tabId) {
  const key = targetKey(tabId);
  const generation = (tabGenerations.get(key) || 0) + 1;
  tabGenerations.set(key, generation);
  const stored = await chrome.storage.local.get(TAB_GENERATIONS_KEY);
  const values = stored[TAB_GENERATIONS_KEY] || {};
  values[key] = generation;
  await chrome.storage.local.set({ [TAB_GENERATIONS_KEY]: Object.fromEntries(Object.entries(values).slice(-2000)) });
  return generation;
}

// Debugger attachment state
// targetId -> { tabId, attachedAt, lastUsedAt, generation, sessions, frames, executionContexts }
const attachedTargets = new Map();
const DEBUGGER_IDLE_DETACH_MS = 90000;
const DEBUGGER_ORPHAN_DETACH_MS = 30000;
const DEBUGGER_COMMAND_TIMEOUT_MS = 8000;
const CHILD_INIT_TIMEOUT_MS = 3000;
const CHILD_INIT_MAX_PENDING = 16;

// K2: attach serialization + bounded child-session init. Concurrent commands
// previously each ran the full attach sequence against the same tab, and
// every auto-attached OOPIF session fired four unsupervised CDP commands —
// on SPA pages those storms collided with tab-level commands and wedged them.
const attachInflight = new Map();
const childInitQueue = [];
let childInitRunning = false;

function enqueueChildInit(tabId, sessionId, generation) {
  if (childInitQueue.length >= CHILD_INIT_MAX_PENDING) return;
  childInitQueue.push({ tabId, sessionId, generation });
  void drainChildInitQueue();
}

async function drainChildInitQueue() {
  if (childInitRunning) return;
  childInitRunning = true;
  try {
    while (childInitQueue.length) {
      const job = childInitQueue.shift();
      const attachment = attachedTargets.get(`${profileId}:${job.tabId}`);
      // Skip sessions queued before a top-frame navigation: their frames are
      // gone and re-initializing them stalls the queue for nothing.
      if (!attachment || attachment.initGeneration !== job.generation) continue;
      try {
        const child = { tabId: job.tabId, sessionId: job.sessionId };
        await withDeadline(chrome.debugger.sendCommand(child, "Page.enable"), CHILD_INIT_TIMEOUT_MS, "child.Page.enable");
        await withDeadline(chrome.debugger.sendCommand(child, "Runtime.enable"), CHILD_INIT_TIMEOUT_MS, "child.Runtime.enable");
        await withDeadline(chrome.debugger.sendCommand(child, "DOM.enable"), CHILD_INIT_TIMEOUT_MS, "child.DOM.enable");
        await withDeadline(enableRecursiveFrameAttach(child, attachment), CHILD_INIT_TIMEOUT_MS, "child.setAutoAttach");
      } catch (error) {
        console.warn("Could not initialize attached frame session:", error.message);
      }
    }
  } finally {
    childInitRunning = false;
  }
}

/**
 * K1 remediation: drop a wedged attachment so the next command attaches a
 * fresh session. The orphaned CDP op may still complete inside Chrome later;
 * its result is discarded (single-response discipline).
 */
async function remediateAttachment(targetId) {
  const attachment = attachedTargets.get(String(targetId));
  if (!attachment) return;
  attachedTargets.delete(String(targetId));
  attachInflight.delete(String(targetId));
  try { await chrome.debugger.detach({ tabId: attachment.tabId }); } catch {}
  pageOps.remediations += 1;
}
const inflightCommandIds = new Set();
const COMMAND_CACHE_KEY = "comptrol_command_results";
const COMMAND_CACHE_TTL_MS = 10 * 60 * 1000;
const COMMAND_CACHE_MAX_ENTRIES = 256;
const COMMAND_CACHE_MAX_RESULT_BYTES = 64 * 1024;
const DEDUPED_COMMAND_TYPES = new Set([
  "cdp_command",
  "bridge_ping",
  "open_tab",
  "close_tab",
  "history",
  "download_file",
  "attach_debugger",
  "detach_debugger",
  "restore_group",
  "dom_click",
  "cdp_frame_command"
]);
let commandLedgerTail = Promise.resolve();

function withCommandLedgerLock(callback) {
  const run = commandLedgerTail.then(callback, callback);
  commandLedgerTail = run.then(
    () => undefined,
    () => undefined
  );
  return run;
}

// Alarms: periodic health check, group observation, debugger hygiene sweep.
const HEALTH_CHECK_ALARM = "comptrol-health-check";
const GROUP_OBSERVE_ALARM = "comptrol-observe-groups";
const DEBUGGER_SWEEP_ALARM = "comptrol-debugger-sweep";

/**
 * Establish native messaging connection to Comptrol daemon
 */
async function connectNative() {
  if (nativePort && isConnected) return;
  if (connectPromise) return connectPromise;

  connectPromise = new Promise((resolve, reject) => {
    let port;
    let handshakeTimer;
    let handshakeListener;
    let settled = false;
    const fail = error => {
      if (settled) return;
      settled = true;
      clearTimeout(handshakeTimer);
      if (handshakeListener && port) {
        port.onMessage.removeListener(handshakeListener);
      }
      reject(error instanceof Error ? error : new Error(String(error)));
    };

    try {
      port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
      nativePort = port;

      port.onMessage.addListener(handleNativeMessage);
      port.onDisconnect.addListener(() => {
        // Chrome exposes the native-host launch/registration failure through
        // lastError only during this callback. Read it here and reject the
        // pending handshake immediately instead of hiding it behind a timeout.
        const disconnectReason = chrome.runtime.lastError?.message;
        const wasConnected = isConnected;
        if (nativePort === port) nativePort = null;
        isConnected = false;
        if (!settled) {
          fail(new Error(disconnectReason || "Native messaging host disconnected before handshake"));
          return;
        }
        console.warn("Native messaging disconnected", disconnectReason || "");
        if (wasConnected) scheduleReconnect();
      });

      // Budget: the native host fails its daemon probes fast (2 s identity +
      // 2 s heartbeat worst case), so 8 s leaves comfortable margin even on a
      // cold host start while still failing promptly on a dead registration.
      handshakeTimer = setTimeout(() => {
        if (!isConnected) {
          fail(new Error("Handshake timeout"));
          try { port.disconnect(); } catch {}
        }
      }, 8000);

      handshakeListener = msg => {
        if (msg.type !== "handshake_ack") return;
        if (settled) return;
        if (msg.protocol !== PROTOCOL_VERSION || msg.extension_id !== chrome.runtime.id) {
          connectionStatus = "extension_identity_mismatch";
          fail(new Error("Browser bridge extension identity mismatch"));
          try { port.disconnect(); } catch {}
          return;
        }
        if (msg.daemon_authorized !== true || msg.daemon_state !== "ready") {
          if (/daemon_http_409|another Chrome profile/i.test(String(msg.daemon_state))) connectionStatus = "profile_in_use";
          else connectionStatus = msg.daemon_state === "daemon_identity_unverified" ? "authorization_required" : "daemon_unavailable";
          fail(new Error(msg.daemon_state || "Browser bridge authorization handshake failed"));
          try { port.disconnect(); } catch {}
          return;
        }
        settled = true;
        clearTimeout(handshakeTimer);
        port.onMessage.removeListener(handshakeListener);
        isConnected = true;
        connectionStatus = "ready";
        reconnectAttempts = 0;
        resolve();
      };
      port.onMessage.addListener(handshakeListener);
      port.postMessage({
        type: "handshake",
        protocol: PROTOCOL_VERSION,
        profileId: typeof profileId === "string" ? profileId : "",
        timestamp: Date.now()
      });
    } catch (error) {
      fail(error);
    }
  });

  try {
    await connectPromise;
  } finally {
    connectPromise = null;
  }
}

/**
 * Schedule reconnection with capped exponential backoff. The bridge never
 * gives up permanently: Chrome may start long before the Comptrol sidecar.
 */
function scheduleReconnect() {
  if (!profileSelected || isConnected || reconnectTimer || connectionStatus.endsWith("_required")) return;

  const exponent = Math.min(reconnectAttempts, RECONNECT_MAX_EXPONENT);
  const delay = Math.min(
    RECONNECT_BASE_DELAY_MS * Math.pow(2, exponent),
    RECONNECT_MAX_DELAY_MS
  );
  reconnectAttempts = Math.min(reconnectAttempts + 1, RECONNECT_MAX_EXPONENT);
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connectNative().catch(error => {
      const requiresAction = classifyConnectionFailure(error);
      if (requiresAction) {
        const reason = String(error?.message || error);
        console.error("Browser bridge needs one setup action:", reason);
        return;
      }
      console.error("Reconnect failed:", error);
      scheduleReconnect();
    });
  }, delay);
}

/**
 * Handle messages from native host
 */
function handleNativeMessage(message) {
  // A wake request means the daemon believes the SW was suspended (a command
  // timed out while the heartbeat looked fresh). Reconnect + replay targets.
  if (message?.type === "wake") {
    // K6: a wake request arriving over the port proves the session is alive —
    // bouncing the port (the old behavior) churned healthy sessions during
    // live recovery and fixed nothing. Only reconnect when the port is
    // actually gone; otherwise remediate wedged attachments and replay
    // targets so the daemon can re-lease its pending commands.
    if (isConnected && nativePort) {
      void (async () => {
        for (const [targetId, attachment] of [...attachedTargets]) {
          if ((attachment.orphanedOps || 0) > 0) await remediateAttachment(targetId);
        }
        await sendTargetsToNative();
      })();
      return;
    }
    void wakeAndReconnect(message.reason);
    return;
  }
  void dispatchNativeMessage(message).catch(error => {
    console.error("Native command dispatch failed:", error);
    if (message?.requestId) {
      void sendCommandResult({
        type: `${message.type || "command"}_result`,
        requestId: message.requestId,
        ok: false,
        error: error.message
      });
    }
  });
}

function commandEntryTimestamp(entry) {
  return Number(entry?.completedAt || entry?.startedAt || 0);
}

async function readCommandLedger() {
  const stored = await chrome.storage.local.get(COMMAND_CACHE_KEY);
  const entries = stored[COMMAND_CACHE_KEY] || {};
  const now = Date.now();
  let changed = false;
  for (const [id, entry] of Object.entries(entries)) {
    if (now - commandEntryTimestamp(entry) > COMMAND_CACHE_TTL_MS) {
      delete entries[id];
      changed = true;
      continue;
    }
    // T9: an inflight entry stamped by a previous service-worker instance can
    // never complete (its handler died mid-flight). Answer its replay once
    // with an honest reconciliation refusal and let the TTL retire it —
    // instead of pinning the request_id forever.
    if (entry.status === "inflight" && entry.instanceId !== swInstanceId &&
        now - commandEntryTimestamp(entry) > 30_000) {
      entries[id] = {
        status: "completed",
        completedAt: now,
        response: {
          type: `${entry.commandType || "command"}_result`,
          requestId: id,
          ok: false,
          error: {
            code: "requires_reconciliation",
            message: "This command was dispatched by a previous extension instance that restarted mid-execution. Comptrol will not repeat a potentially completed browser mutation without reconciliation."
          }
        }
      };
      changed = true;
    }
  }
  if (changed) {
    await chrome.storage.local.set({ [COMMAND_CACHE_KEY]: entries });
  }
  return entries;
}

async function writeBoundedCommandLedger(entries) {
  const ordered = Object.entries(entries).sort(
    (a, b) => commandEntryTimestamp(b[1]) - commandEntryTimestamp(a[1])
  );
  const bounded = Object.fromEntries(ordered.slice(0, COMMAND_CACHE_MAX_ENTRIES));
  await chrome.storage.local.set({ [COMMAND_CACHE_KEY]: bounded });
}

async function beginDedupedCommand(message) {
  return withCommandLedgerLock(async () => {
    const requestId = message?.requestId;
    if (!requestId || !DEDUPED_COMMAND_TYPES.has(message.type)) {
      return { execute: true };
    }
    if (inflightCommandIds.has(requestId)) {
      return { execute: false };
    }

    const entries = await readCommandLedger();
    const entry = entries[requestId];
    if (entry?.status === "completed") {
      if (entry.response) {
        sendToNative(entry.response);
      } else {
        sendToNative({
          type: `${message.type}_result`,
          requestId,
          ok: false,
          error: {
            code: "requires_reconciliation",
            message: "The prior command completed but its response exceeded the durable cache limit. Inspect current browser state before issuing a new mutation."
          }
        });
      }
      return { execute: false };
    }
    if (entry?.status === "inflight") {
      sendToNative({
        type: `${message.type}_result`,
        requestId,
        ok: false,
        error: {
          code: "requires_reconciliation",
          message: "This command was already dispatched before the extension restarted. Comptrol will not repeat a potentially completed browser mutation without reconciliation."
        }
      });
      return { execute: false };
    }

    entries[requestId] = {
      status: "inflight",
      startedAt: Date.now(),
      commandType: message.type,
      instanceId: swInstanceId
    };
    await writeBoundedCommandLedger(entries);
    inflightCommandIds.add(requestId);
    return { execute: true };
  });
}
async function cacheCommandResult(response) {
  return withCommandLedgerLock(async () => {
    const requestId = response?.requestId;
    if (!requestId) return;
    const encoded = JSON.stringify(response);
    const entries = await readCommandLedger();
    entries[requestId] = {
      status: "completed",
      completedAt: Date.now(),
      response: encoded.length <= COMMAND_CACHE_MAX_RESULT_BYTES ? response : null,
      responseTooLarge: encoded.length > COMMAND_CACHE_MAX_RESULT_BYTES
    };
    await writeBoundedCommandLedger(entries);
  });
}
async function sendCommandResult(response) {
  if (response?.requestId) {
    await cacheCommandResult(response);
    inflightCommandIds.delete(response.requestId);
  }
  sendToNative(response);
}

async function dispatchNativeMessage(message) {
  const requestId = message?.requestId;
  const dedupe = await beginDedupedCommand(message);
  if (!dedupe.execute) return;

  switch (message.type) {
    case "handshake_ack":
      break;
    case "cdp_command":
      await handleCdpCommand(message);
      break;
    case "cdp_frame_command":
      await handleCdpFrameCommand(message);
      break;
    case "bridge_ping":
      await handleBridgePing(message);
      break;
    case "open_tab":
      await handleOpenTab(message);
      break;
    case "close_tab":
      await handleCloseTab(message);
      break;
    case "history":
      await handleHistory(message);
      break;
    case "download_file":
      await handleDownloadFile(message);
      break;
    case "dom_click":
      await handleDomClick(message);
      break;
    case "activate_tab":
      await handleActivateTab(message);
      break;
    case "get_targets":
      await sendTargetsToNative();
      break;
    case "attach_debugger": {
      const result = await attachDebugger(message.targetId);
      await sendCommandResult({ type: "attach_debugger_result", requestId, ...result });
      break;
    }
    case "detach_debugger": {
      const result = await detachDebugger(message.targetId);
      await sendCommandResult({ type: "detach_debugger_result", requestId, ...result });
      break;
    }
    case "restore_group": {
      const result = await restoreClosedGroup(message.groupId);
      await sendCommandResult({ type: "restore_group_result", requestId, ...result });
      break;
    }
    default:
      if (requestId) inflightCommandIds.delete(requestId);
      console.warn("Unknown native message type:", message.type);
  }
}

/**
 * Send message to native host
 */
function sendToNative(message) {
  if (nativePort && isConnected) {
    try {
      nativePort.postMessage(message);
    } catch (error) {
      console.error("Failed to send to native:", error);
    }
  }
}

/**
 * Send current targets to native host
 */
async function sendTargetsToNative() {
  if (!profileSelected || !profileId) return;
  try {
    const targets = await chrome.tabs.query({});
    const targetList = targets.map(tab => {
      const targetId = `${profileId}:${tab.id}`;
      const attachment = attachedTargets.get(targetId);
      return {
        id: targetId,
        type: "page",
        browserContextId: profileId,
        url: tab.url || "",
        title: tab.title || "",
        windowId: tab.windowId,
        index: tab.index,
        pinned: tab.pinned,
        groupId: tab.groupId,
        active: tab.active === true,
        status: tab.status,
        revision: `bridge:${targetId}:${tabGenerations.get(targetId) || attachment?.generation || 0}:${tab.url || ""}`
      };
    });
    sendToNative({ type: "targets_list", profileId, targets: targetList });
  } catch (error) {
    console.error("Failed to get targets:", error);
  }
}

/**
 * Attach debugger to a target tab (K2: serialized per target, deadline-bound).
 */
function attachDebugger(targetId) {
  if (attachedTargets.has(targetId)) {
    const attachment = attachedTargets.get(targetId);
    attachment.lastUsedAt = Date.now();
    return Promise.resolve({ ok: true, alreadyAttached: true });
  }
  if (attachInflight.has(targetId)) return attachInflight.get(targetId);
  const pending = doAttachDebugger(targetId).finally(() => attachInflight.delete(targetId));
  attachInflight.set(targetId, pending);
  return pending;
}

async function doAttachDebugger(targetId) {
  let didAttach = false;
  let tabId;
  try {
    tabId = parseLocalTargetId(targetId);

    // Attach debugger
    await withDeadline(chrome.debugger.attach({ tabId }, "1.3"), CHILD_INIT_TIMEOUT_MS * 2, "debugger.attach");
    didAttach = true;

    // Enable the domains page ops need. Network.enable is deliberately NOT
    // enabled at attach time: its event storm on busy pages amplified command
    // latency during live SPA navigation; network inspection enables it
    // lazily when a network-scoped command actually runs.
    await withDeadline(chrome.debugger.sendCommand({ tabId }, "Page.enable"), CHILD_INIT_TIMEOUT_MS, "Page.enable");
    await withDeadline(chrome.debugger.sendCommand({ tabId }, "Runtime.enable"), CHILD_INIT_TIMEOUT_MS, "Runtime.enable");
    await withDeadline(chrome.debugger.sendCommand({ tabId }, "DOM.enable"), CHILD_INIT_TIMEOUT_MS, "DOM.enable");
    const attachment = {
      tabId,
      attachedAt: Date.now(),
      lastUsedAt: Date.now(),
      generation: tabGenerations.get(targetId) || 0,
      initGeneration: 0,
      orphanedOps: 0,
      sessions: new Map(),
      frames: new Map(),
      executionContexts: new Map()
    };
    attachedTargets.set(targetId, attachment);
    const tree = await withDeadline(chrome.debugger.sendCommand({ tabId }, "Page.getFrameTree"), CHILD_INIT_TIMEOUT_MS, "Page.getFrameTree");
    collectFrameTree(attachment, tree.frameTree);
    await withDeadline(enableRecursiveFrameAttach({ tabId }, attachment), CHILD_INIT_TIMEOUT_MS, "setAutoAttach");

    return { ok: true };
  } catch (error) {
    attachedTargets.delete(String(targetId));
    if (didAttach) {
      try { await chrome.debugger.detach({ tabId }); } catch {}
    }
    return { ok: false, error: error.message };
  }
}

function collectFrameTree(attachment, node) {
  if (!node?.frame?.id) return;
  attachment.frames.set(node.frame.id, { parentId: node.frame.parentId || null, url: node.frame.url || "", loaderId: node.frame.loaderId || null });
  for (const child of node.childFrames || []) collectFrameTree(attachment, child);
}

async function enableRecursiveFrameAttach(session, _attachment) {
  await chrome.debugger.sendCommand(session, "Target.setAutoAttach", {
    autoAttach: true,
    waitForDebuggerOnStart: false,
    flatten: true,
    filter: [{ type: "iframe", exclude: false }]
  });
}

/**
 * Detach debugger from a target
 */
async function detachDebugger(targetId) {
  try {
    const tabId = parseLocalTargetId(targetId);
    await chrome.debugger.detach({ tabId });
    return { ok: true };
  } catch (error) {
    return { ok: false, error: error.message };
  } finally {
    attachedTargets.delete(String(targetId));
  }
}

/**
 * Debugger hygiene: attachments idle for DEBUGGER_IDLE_DETACH_MS are dropped
 * so the per-tab "started debugging" infobars do not accumulate forever. The
 * next command re-attaches transparently.
 */
async function sweepIdleDebuggers() {
  const now = Date.now();
  for (const [targetId, attachment] of attachedTargets) {
    const idleMs = now - (attachment.lastUsedAt || attachment.attachedAt);
    // K1: an attachment that produced an orphaned op is remediated on a
    // shorter leash; it is only kept while commands actually use it.
    const orphaned = (attachment.orphanedOps || 0) > 0;
    const limit = orphaned ? DEBUGGER_ORPHAN_DETACH_MS : DEBUGGER_IDLE_DETACH_MS;
    if (idleMs < limit) continue;
    try { await chrome.debugger.detach({ tabId: attachment.tabId }); } catch {}
    attachedTargets.delete(targetId);
  }
}

/**
 * Handle debugger events
 */
function handleDebuggerEvent(source, method, params) {
  const targetId = `${profileId}:${source.tabId}`;
  const attachment = attachedTargets.get(targetId);

  // K3: events are NOT attachment use. Only commands touch lastUsedAt, so a
  // busy page's event stream can no longer starve the idle sweep while its
  // commands are wedged.
  if (!attachment) return;

  if (method === "Target.attachedToTarget" && params?.sessionId) {
    attachment.sessions.set(params.sessionId, { targetInfo: params.targetInfo || {}, parentSessionId: source.sessionId || null });
    // K2: bounded, serialized, deadline-bound init queue (replaces an
    // unbounded fire-and-forget init storm).
    enqueueChildInit(source.tabId, params.sessionId, attachment.initGeneration);
  } else if (method === "Target.detachedFromTarget" && params?.sessionId) {
    attachment.sessions.delete(params.sessionId);
    for (const [frameId, frame] of attachment.frames) {
      if (frame.sessionId === params.sessionId) attachment.frames.delete(frameId);
    }
    for (const [contextId, context] of attachment.executionContexts) {
      if (context.sessionId === params.sessionId) attachment.executionContexts.delete(contextId);
    }
  } else if (method === "Runtime.executionContextCreated" && params?.context?.id !== undefined) {
    attachment.executionContexts.set(params.context.id, { frameId: params.context.auxData?.frameId || null, sessionId: source.sessionId || null });
  } else if (method === "Runtime.executionContextDestroyed") {
    attachment.executionContexts.delete(params?.executionContextId);
  } else if (method === "Runtime.executionContextsCleared") {
    for (const [contextId, context] of attachment.executionContexts) {
      if (context.sessionId === (source.sessionId || null)) attachment.executionContexts.delete(contextId);
    }
  } else if (method === "Page.frameNavigated" && params?.frame?.id) {
    const frame = params.frame;
    attachment.frames.set(frame.id, { parentId: frame.parentId || null, url: frame.url || "", loaderId: frame.loaderId || null, sessionId: source.sessionId || null });
    if (!frame.parentId) {
      // K2: top-frame navigation invalidates queued child-session inits for
      // the pre-navigation document.
      attachment.initGeneration = (attachment.initGeneration || 0) + 1;
      void bumpTabGeneration(source.tabId).then(generation => { attachment.generation = generation; }).catch(() => {});
    }
  } else if (method === "Page.frameDetached" && params?.frameId) {
    attachment.frames.delete(params.frameId);
  }

  // K2: forward only state-change events. Runtime/Network stream noise on
  // busy pages flooded the native pipe during live SPA navigation and
  // amplified command latency; consumers track frames, sessions, and dialogs.
  const FORWARDED_DEBUGGER_EVENTS = new Set([
    "Target.attachedToTarget",
    "Target.detachedFromTarget",
    "Page.frameNavigated",
    "Page.frameDetached",
    "Page.navigatedWithinDocument",
    "Page.loadEventFired",
    "Page.javascriptDialogOpening"
  ]);
  if (method === "Page.frameNavigated" && params?.frame?.parentId) {
    // iframe navigations are tracked in the local frame map but not forwarded.
    return;
  }
  if (FORWARDED_DEBUGGER_EVENTS.has(method)) {
    sendToNative({
      type: "debugger_event",
      targetId,
      generation: attachment.generation,
      method,
      params
    });
  }
}

/**
 * Handle debugger detach
 */
function handleDebuggerDetach(source, reason) {
  const targetId = `${profileId}:${source.tabId}`;
  attachedTargets.delete(targetId);

  sendToNative({
    type: "debugger_detached",
    targetId,
    reason
  });
}

/**
 * Handle CDP commands from native host
 */
async function handleCdpCommand(message) {
  const { requestId, targetId, method, params } = message;
  // K1: exactly one response per command, even when the deadline fires while
  // the CDP op is still in flight inside Chrome.
  let responded = false;
  const respond = async response => {
    if (responded) return;
    responded = true;
    await sendCommandResult(response);
  };
  try {
    const tabId = parseLocalTargetId(targetId);
    if (!attachedTargets.has(targetId)) {
      const attached = await attachDebugger(targetId);
      if (!attached.ok) {
        await respond({ type: "cdp_command_result", requestId, ok: false, error: attached.error || "debugger attach failed" });
        return;
      }
    } else {
      attachedTargets.get(targetId).lastUsedAt = Date.now();
    }
    const result = await withDeadline(
      chrome.debugger.sendCommand({ tabId }, method, params || {}),
      DEBUGGER_COMMAND_TIMEOUT_MS,
      `cdp:${method}`
    );
    pageOps.lastOkAt = Date.now();
    await respond({ type: "cdp_command_result", requestId, ok: true, result });
  } catch (error) {
    pageOps.lastFailAt = Date.now();
    if (error instanceof SwDeadlineError) {
      const attachment = attachedTargets.get(String(targetId));
      if (attachment) attachment.orphanedOps = (attachment.orphanedOps || 0) + 1;
      pageOps.orphaned += 1;
      void remediateAttachment(targetId);
      await respond({
        type: "cdp_command_result",
        requestId,
        ok: false,
        error: {
          code: "sw_deadline",
          message: `Debugger command ${method} exceeded ${DEBUGGER_COMMAND_TIMEOUT_MS} ms; the attachment was remediated`,
          retryable: true
        }
      });
      return;
    }
    await respond({ type: "cdp_command_result", requestId, ok: false, error: error.message });
  }
}

/**
 * Frame-scoped CDP: resolve the frameId against the attachment frame maps
 * (populated via Target.setAutoAttach + Page.getFrameTree), then send the
 * command on the owning OOPIF/iframe session. This reaches cross-origin
 * iframes (Canva embeds, Google Docs editors) that plain tab-level
 * Runtime.evaluate cannot.
 */
async function handleCdpFrameCommand(message) {
  const { requestId, frameId, method, params } = message;
  try {
    if (!frameId || typeof frameId !== "string") throw new Error("cdp_frame_command needs a frameId");
    let found = null;
    for (const [ownerTargetId, attachment] of attachedTargets) {
      const frame = attachment.frames.get(frameId);
      if (frame) {
        found = { ownerTargetId, attachment, frame };
        break;
      }
    }
    if (!found) {
      // The frame may exist but the owning tab was never attached. Attaching
      // every tab to hunt one frame would spam infobars; report honestly.
      await sendCommandResult({
        type: "cdp_frame_command_result",
        requestId,
        ok: false,
        error: {
          code: "frame_not_attached",
          message: "Frame is not in any attached target's frame tree; attach its tab (browser.cdp.attach) or navigate first.",
          known_frames: [...attachedTargets.values()].flatMap(a => [...a.frames.keys()]).slice(0, 64)
        }
      });
      return;
    }
    const { ownerTargetId, attachment, frame } = found;
    attachment.lastUsedAt = Date.now();
    const session = frame.sessionId ? { tabId: attachment.tabId, sessionId: frame.sessionId } : { tabId: attachment.tabId };
    // K1: same deadline + remediation discipline as tab-level commands.
    let result;
    try {
      result = await withDeadline(
        chrome.debugger.sendCommand(session, method, params || {}),
        DEBUGGER_COMMAND_TIMEOUT_MS,
        `frame:${method}`
      );
    } catch (error) {
      pageOps.lastFailAt = Date.now();
      if (error instanceof SwDeadlineError) {
        attachment.orphanedOps = (attachment.orphanedOps || 0) + 1;
        pageOps.orphaned += 1;
        void remediateAttachment(ownerTargetId);
        await sendCommandResult({
          type: "cdp_frame_command_result",
          requestId,
          ok: false,
          error: { code: "sw_deadline", message: `Frame command ${method} exceeded ${DEBUGGER_COMMAND_TIMEOUT_MS} ms; the attachment was remediated`, retryable: true }
        });
        return;
      }
      throw error;
    }
    pageOps.lastOkAt = Date.now();
    await sendCommandResult({ type: "cdp_frame_command_result", requestId, ok: true, result: { frame_id: frameId, session_scoped: Boolean(frame.sessionId), ...result } });
  } catch (error) {
    await sendCommandResult({ type: "cdp_frame_command_result", requestId, ok: false, error: error.message });
  }
}

function waitForTabUpdate(tabId, predicate, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    const tick = async () => {
      try {
        const tab = await chrome.tabs.get(tabId);
        if (predicate(tab)) {
          resolve(tab);
          return;
        }
      } catch (error) {
        reject(error);
        return;
      }
      if (Date.now() >= deadline) {
        reject(new Error("Timed out waiting for tab state"));
        return;
      }
      setTimeout(tick, 50);
    };
    tick();
  });
}

async function handleBridgePing(message) {
  await sendCommandResult({
    type: "bridge_ping_result",
    requestId: message.requestId,
    ok: true,
    // K5: page-op stats ride along so healthz/doctor can separate "channel
    // alive" from "page ops alive".
    result: { pong: true, timestamp: Date.now(), page_ops: pageOpsSnapshot() }
  });
}

async function handleOpenTab(message) {
  try {
    if (!profileSelected || (message.browserContextId && message.browserContextId !== "default" && message.browserContextId !== profileId)) {
      throw new Error("Requested browser profile is not the selected Comptrol profile");
    }
    const tab = await chrome.tabs.create({
      url: message.url,
      active: !Boolean(message.background)
    });
    const targetId = `${profileId}:${tab.id}`;
    await sendCommandResult({
      type: "open_tab_result",
      requestId: message.requestId,
      ok: true,
      result: {
        target: {
          id: targetId,
          type: "page",
          browser_context_id: profileId,
          url: tab.url || message.url,
          title: tab.title || "",
          revision: `bridge:${targetId}:${tabGenerations.get(targetId) || 0}:${tab.url || message.url}`
        },
        visibility: message.background ? "background" : "foreground",
        profile: "attached_existing_browser",
        account_state: "same_browser_profile",
        mouse: "untouched",
        clipboard: "untouched",
        verified: true
      }
    });
    await sendTargetsToNative();
  } catch (error) {
    await sendCommandResult({ type: "open_tab_result", requestId: message.requestId, ok: false, error: error.message });
  }
}

/**
 * Activate (or deactivate) a tab. `browser.cdp.activate_tab` lets the runtime
 * get a rendering-dependent operation done with a brief, disclosed focus hop
 * and hand the foreground back, instead of failing outright in a hidden tab.
 */
async function handleActivateTab(message) {
  try {
    const tabId = parseLocalTargetId(String(message.targetId));
    if (message.deactivate) {
      const tab = await chrome.tabs.get(tabId);
      const all = await chrome.tabs.query({ windowId: tab.windowId });
      const neighbor = all.find(candidate => candidate.id !== tabId && !candidate.pinned)
        || all.find(candidate => candidate.id !== tabId);
      if (neighbor) await chrome.tabs.update(neighbor.id, { active: true });
      await sendCommandResult({
        type: "activate_tab_result",
        requestId: message.requestId,
        ok: true,
        result: { deactivated: true, target_id: message.targetId, verified: true }
      });
      return;
    }
    await chrome.tabs.update(tabId, { active: true });
    const updated = await chrome.tabs.get(tabId);
    await sendCommandResult({
      type: "activate_tab_result",
      requestId: message.requestId,
      ok: true,
      result: {
        activated: true,
        target_id: message.targetId,
        active: updated.active === true,
        verified: updated.active === true
      }
    });
    await sendTargetsToNative();
  } catch (error) {
    await sendCommandResult({ type: "activate_tab_result", requestId: message.requestId, ok: false, error: error.message });
  }
}

async function handleCloseTab(message) {
  try {
    const tabId = parseLocalTargetId(message.targetId);
    await chrome.tabs.remove(tabId);
    attachedTargets.delete(String(message.targetId));
    await sendCommandResult({
      type: "close_tab_result",
      requestId: message.requestId,
      ok: true,
      result: {
        closed: true,
        target_id: String(message.targetId),
        mouse: "untouched",
        clipboard: "untouched",
        verified: true
      }
    });
    await sendTargetsToNative();
  } catch (error) {
    await sendCommandResult({ type: "close_tab_result", requestId: message.requestId, ok: false, error: error.message });
  }
}

async function handleHistory(message) {
  try {
    const targetId = String(message.targetId);
    const tabId = parseLocalTargetId(targetId);
    if (!attachedTargets.has(targetId)) {
      const attached = await attachDebugger(targetId);
      if (!attached.ok) throw new Error(attached.error || "debugger attach failed");
    }
    const before = await chrome.debugger.sendCommand(
      { tabId },
      "Page.getNavigationHistory",
      {}
    );
    const currentIndex = Number(before.currentIndex);
    const destinationIndex = message.forward ? currentIndex + 1 : currentIndex - 1;
    const destination = before.entries?.[destinationIndex];
    if (!destination) throw new Error(message.forward ? "No forward history" : "No back history");
    await chrome.debugger.sendCommand(
      { tabId },
      "Page.navigateToHistoryEntry",
      { entryId: destination.id }
    );
    const deadline = Date.now() + 5000;
    let observed;
    while (Date.now() < deadline) {
      observed = await chrome.debugger.sendCommand(
        { tabId },
        "Page.getNavigationHistory",
        {}
      );
      if (Number(observed.currentIndex) === destinationIndex) break;
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    if (Number(observed?.currentIndex) !== destinationIndex) {
      throw new Error("Browser did not confirm history navigation");
    }
    await sendCommandResult({
      type: "history_result",
      requestId: message.requestId,
      ok: true,
      result: {
        direction: message.forward ? "forward" : "back",
        entry: destination,
        current_index: destinationIndex,
        verified: true,
        mouse: "untouched",
        clipboard: "untouched"
      }
    });
    await sendTargetsToNative();
  } catch (error) {
    await sendCommandResult({ type: "history_result", requestId: message.requestId, ok: false, error: error.message });
  }
}

function waitForDownload(downloadId, timeoutMs = 30000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      chrome.downloads.onChanged.removeListener(listener);
      reject(new Error("Timed out waiting for browser download"));
    }, timeoutMs);
    const listener = delta => {
      if (delta.id !== downloadId || delta.state?.current !== "complete") return;
      clearTimeout(timer);
      chrome.downloads.onChanged.removeListener(listener);
      chrome.downloads.search({ id: downloadId }).then(items => {
        if (!items.length || !items[0].filename) reject(new Error("Completed download has no file path"));
        else resolve(items[0]);
      }, reject);
    };
    chrome.downloads.onChanged.addListener(listener);
  });
}

async function handleDownloadFile(message) {
  try {
    const targetId = String(message.targetId);
    const tabId = parseLocalTargetId(targetId);
    if (!attachedTargets.has(targetId)) {
      const attached = await attachDebugger(targetId);
      if (!attached.ok) throw new Error(attached.error || "debugger attach failed");
    }
    const selector = JSON.stringify(message.selector || "#download");
    const hrefResult = await chrome.debugger.sendCommand(
      { tabId },
      "Runtime.evaluate",
      {
        expression: `(() => { const node = document.querySelector(${selector}); return node ? node.href || node.getAttribute('href') : null; })()`,
        returnByValue: true,
        awaitPromise: true
      }
    );
    const href = hrefResult?.result?.value;
    if (!href) throw new Error("Download target did not expose a URL");
    const downloadId = await chrome.downloads.download({
      url: href,
      filename: message.fileName || undefined,
      saveAs: false,
      conflictAction: "overwrite"
    });
    const item = await waitForDownload(downloadId);
    await sendCommandResult({
      type: "download_file_result",
      requestId: message.requestId,
      ok: true,
      result: {
        downloadId,
        path: item.filename,
        fileName: message.fileName || "",
        verified: true
      }
    });
  } catch (error) {
    await sendCommandResult({ type: "download_file_result", requestId: message.requestId, ok: false, error: error.message });
  }
}

async function handleDomClick(message) {
  try {
    if (!profileSelected) throw new Error("Select this Chrome profile in the Browser Bridge popup first");
    const tabId = parseLocalTargetId(String(message.targetId));
    const tab = await chrome.tabs.get(tabId);
    const url = new URL(tab.url || "");
    if (url.protocol !== "http:" && url.protocol !== "https:") throw new Error("DOM actions are limited to ordinary HTTP and HTTPS pages");
    const selector = String(message.selector || "");
    const postcondition = message.postcondition;
    if (!selector || selector.length > 512 || !postcondition || typeof postcondition.selector !== "string" || !postcondition.selector || postcondition.selector.length > 512 || typeof postcondition.present !== "boolean") {
      throw new Error("dom_click requires a bounded selector and explicit selector-presence postcondition");
    }
    const origin = `${url.origin}/*`;
    const allowed = await chrome.permissions.contains({ permissions: ["scripting"], origins: [origin] });
    if (!allowed) throw new Error("Grant scripting access for this site from the Browser Bridge popup before using DOM actions");
    const [execution] = await chrome.scripting.executeScript({
      target: { tabId },
      world: "ISOLATED",
      func: async (clickSelector, expectedSelector, expectedPresent) => {
        const element = document.querySelector(clickSelector);
        if (!element) return { clicked: false, reason: "selector_not_found" };
        if (Boolean(document.querySelector(expectedSelector)) === expectedPresent) {
          return { clicked: false, reason: "postcondition_already_satisfied_before_action" };
        }
        element.click();
        const deadline = Date.now() + 2000;
        while (Date.now() < deadline) {
          if (Boolean(document.querySelector(expectedSelector)) === expectedPresent) {
            return { clicked: true, postcondition_met: true };
          }
          await new Promise(resolve => setTimeout(resolve, 50));
        }
        return { clicked: true, postcondition_met: false };
      },
      args: [selector, postcondition.selector, postcondition.present]
    });
    const result = execution?.result;
    if (!result?.clicked || !result.postcondition_met) throw new Error(result?.reason || "DOM action postcondition was not observed");
    await sendCommandResult({ type: "dom_click_result", requestId: message.requestId, ok: true, result: { clicked: true, verified: true, targetId: message.targetId } });
  } catch (error) {
    await sendCommandResult({ type: "dom_click_result", requestId: message.requestId, ok: false, error: error.message });
  }
}

/**
 * Restore a closed tab group
 */
async function restoreClosedGroup(groupId) {
  try {
    // Get recently closed sessions
    const sessions = await chrome.sessions.getRecentlyClosed({ maxResults: 50 });

    // Find the group
    const groupSession = sessions.find(s => s.window?.tabs?.some(t => t.groupId === parseInt(groupId, 10)));

    if (!groupSession) {
      return { ok: false, error: "Group not found in recently closed" };
    }

    // Restore the session
    const restored = await chrome.sessions.restore(groupSession.window?.sessionId || groupSession.tab?.sessionId);

    return { ok: true, restored };
  } catch (error) {
    return { ok: false, error: error.message };
  }
}

/**
 * Observe open groups and store snapshots
 */
async function observeOpenGroups() {
  try {
    const groups = await chrome.tabGroups.query({});
    const tabs = await chrome.tabs.query({ windowType: "normal" });

    const snapshots = groups.map(group => {
      const groupTabs = tabs
        .filter(tab => tab.groupId === group.id)
        .sort((a, b) => a.index - b.index)
        .map(tab => ({
          url: tab.url || "",
          title: tab.title || "",
          index: tab.index,
          pinned: Boolean(tab.pinned)
        }));

      return {
        groupId: group.id,
        title: group.title || "",
        color: group.color,
        windowId: group.windowId,
        tabs: groupTabs,
        timestamp: Date.now()
      };
    }).filter(s => s.tabs.length > 0);

    // Store snapshots
    await chrome.storage.local.set({ "comptrol_group_snapshots": snapshots });

    // Send to native host
    sendToNative({ type: "group_snapshots", snapshots });
  } catch (error) {
    console.error("Failed to observe groups:", error);
  }
}

/**
 * Initialize extension
 */
async function initialize() {
  await loadProfileState();
  if (!profileSelected) {
    connectionStatus = "profile_not_selected";
    return;
  }
  await loadTabGenerations();
  // K9: a previous service-worker life may have left chrome.debugger
  // attachments behind (they survive suspension). Half-initialized orphan
  // sessions are exactly what wedged commands after restarts; detach them
  // and let the next command attach a clean session.
  try {
    const orphanedAttachments = await chrome.debugger.getTargets();
    for (const targetInfo of orphanedAttachments) {
      if (targetInfo.attached && Number.isInteger(targetInfo.tabId)) {
        try { await chrome.debugger.detach({ tabId: targetInfo.tabId }); } catch {}
      }
    }
  } catch {}
  try {
    await connectNative();
    console.log("Connected to Comptrol daemon");
  } catch (error) {
    console.error("Failed to connect to daemon:", error);
    if (!classifyConnectionFailure(error)) scheduleReconnect();
  }

  chrome.alarms.create(HEALTH_CHECK_ALARM, { periodInMinutes: 0.5 });
  chrome.alarms.create(GROUP_OBSERVE_ALARM, { periodInMinutes: 1 });
  chrome.alarms.create(DEBUGGER_SWEEP_ALARM, { periodInMinutes: 1 });

  if (!listenersRegistered) {
    listenersRegistered = true;
    chrome.debugger.onEvent.addListener(handleDebuggerEvent);
    chrome.debugger.onDetach.addListener(handleDebuggerDetach);

    chrome.alarms.onAlarm.addListener(async alarm => {
      if (alarm.name === HEALTH_CHECK_ALARM) {
        // Resurrection backstop: alarms fire even when every other event is
        // quiet. Reconnect when the native port is down; refresh targets when
        // it is up; rebuild the offscreen keeper if Chrome evicted it.
        if (!isConnected) scheduleReconnect();
        else await sendTargetsToNative();
        void ensureOffscreenKeepalive();
      } else if (alarm.name === GROUP_OBSERVE_ALARM) {
        await observeOpenGroups();
      } else if (alarm.name === DEBUGGER_SWEEP_ALARM) {
        await sweepIdleDebuggers();
      }
    });

    chrome.tabs.onCreated.addListener(() => sendTargetsToNative());
    chrome.tabs.onRemoved.addListener(() => sendTargetsToNative());
    chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
      if (changeInfo.status === "complete" || changeInfo.discarded === true || changeInfo.discarded === false) {
        void bumpTabGeneration(tabId).finally(() => sendTargetsToNative());
      } else {
        void sendTargetsToNative();
      }
    });
    chrome.tabs.onReplaced.addListener((addedTabId, removedTabId) => {
      tabGenerations.delete(targetKey(removedTabId));
      void bumpTabGeneration(addedTabId).finally(() => sendTargetsToNative());
    });
    chrome.tabGroups.onCreated.addListener(() => observeOpenGroups());
    chrome.tabGroups.onUpdated.addListener(() => observeOpenGroups());
    chrome.tabGroups.onRemoved.addListener(() => observeOpenGroups());
  }

  // Keep-alive keeper: works whether or not the SW was freshly started.
  void ensureOffscreenKeepalive();

  await observeOpenGroups();
  await sendTargetsToNative();
}

// Message handler for popup/extension pages
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  switch (message?.type) {
    case "get_status":
      sendResponse({ ok: true, connected: isConnected, status: connectionStatus, profileId, profileSelected, targets: Array.from(attachedTargets.keys()) });
      return true;

    case "select_profile":
      (async () => {
        await loadProfileState();
        profileSelected = true;
        connectionStatus = "connecting";
        await chrome.storage.local.set({ comptrol_profile_selected: true });
        await initialize();
        await sendTargetsToNative();
        sendResponse({ ok: true, profileId, profileSelected, connected: isConnected, status: connectionStatus });
      })().catch(error => sendResponse({ ok: false, error: error.message }));
      return true;

    case "deselect_profile":
      (async () => {
        profileSelected = false;
        connectionStatus = "profile_not_selected";
        await chrome.storage.local.set({ comptrol_profile_selected: false });
        if (reconnectTimer) clearTimeout(reconnectTimer);
        reconnectTimer = null;
        if (nativePort) nativePort.disconnect();
        nativePort = null;
        isConnected = false;
        sendResponse({ ok: true, profileId, profileSelected: false, connected: false, status: connectionStatus });
      })();
      return true;

    case "attach_debugger":
      attachDebugger(message.targetId).then(result => sendResponse({ ok: true, ...result }));
      return true;

    case "detach_debugger":
      detachDebugger(message.targetId).then(result => sendResponse({ ok: true, ...result }));
      return true;

    case "get_targets":
      chrome.tabs.query({}).then(tabs => {
        sendResponse({ ok: true, targets: profileSelected ? tabs.map(t => ({ id: `${profileId}:${t.id}`, browserContextId: profileId, url: t.url, title: t.title })) : [] });
      });
      return true;

    default:
      sendResponse({ ok: false, error: "Unknown message type" });
  }
  return true;
});

// Initialize on startup
initialize().catch(console.error);

chrome.runtime.onStartup.addListener(() => initialize().catch(console.error));
chrome.runtime.onInstalled.addListener(() => initialize().catch(console.error));
