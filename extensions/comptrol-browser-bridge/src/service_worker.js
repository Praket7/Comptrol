/**
 * Comptrol Browser Bridge - Production Service Worker
 * 
 * This extension bridges the Comptrol runtime with existing signed-in Chrome sessions.
 * It uses chrome.debugger to attach to existing tabs, chrome.tabGroups for group management,
 * chrome.sessions for closed tab/group recovery, and native messaging for daemon communication.
 */

// Protocol version for native messaging handshake
const PROTOCOL_VERSION = "comptrol.browser.bridge/0.1.0";
const NATIVE_HOST_NAME = "comptrol_browser_bridge";
const MAX_RECONNECT_ATTEMPTS = 5;
const RECONNECT_BASE_DELAY_MS = 1000;

// Native messaging port
let nativePort = null;
let reconnectAttempts = 0;
let isConnected = false;
let reconnectTimer = null;
let connectPromise = null;
let listenersRegistered = false;

// Debugger attachment state
const attachedTargets = new Map(); // targetId -> { tabId, debuggerPort, generation }
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
  "restore_group"
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

// Alarm for periodic health checks
const HEALTH_CHECK_ALARM = "comptrol-health-check";
const GROUP_OBSERVE_ALARM = "comptrol-observe-groups";

/**
 * Establish native messaging connection to Comptrol daemon
 */
async function connectNative() {
  if (nativePort && isConnected) return;
  if (connectPromise) return connectPromise;

  connectPromise = new Promise((resolve, reject) => {
    try {
      const port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
      nativePort = port;

      port.onMessage.addListener(handleNativeMessage);
      port.onDisconnect.addListener(() => {
        if (nativePort === port) nativePort = null;
        isConnected = false;
        console.log("Native messaging disconnected");
        scheduleReconnect();
      });

      const timeout = setTimeout(() => {
        if (!isConnected) {
          try { port.disconnect(); } catch {}
          reject(new Error("Handshake timeout"));
        }
      }, 5000);

      const listener = msg => {
        if (msg.type !== "handshake_ack") return;
        clearTimeout(timeout);
        port.onMessage.removeListener(listener);
        isConnected = true;
        reconnectAttempts = 0;
        resolve();
      };
      port.onMessage.addListener(listener);
      port.postMessage({
        type: "handshake",
        protocol: PROTOCOL_VERSION,
        timestamp: Date.now()
      });
    } catch (error) {
      reject(error);
    }
  });

  try {
    await connectPromise;
  } finally {
    connectPromise = null;
  }
}

/**
 * Schedule reconnection with exponential backoff
 */
function scheduleReconnect() {
  if (isConnected || reconnectTimer || reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
    if (reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
      console.error("Max reconnect attempts reached");
    }
    return;
  }

  const delay = RECONNECT_BASE_DELAY_MS * Math.pow(2, reconnectAttempts);
  reconnectAttempts++;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connectNative().catch(error => {
      console.error("Reconnect failed:", error);
      scheduleReconnect();
    });
  }, delay);
}

/**
 * Handle messages from native host
 */
function handleNativeMessage(message) {
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
      commandType: message.type
    };
    await writeBoundedCommandLedger(entries);
    inflightCommandIds.add(requestId);
    return { execute: true };
  }
  
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
  }
  
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
  try {
    const targets = await chrome.tabs.query({});
    const targetList = targets.map(tab => {
      const targetId = tab.id.toString();
      const attachment = attachedTargets.get(targetId);
      return {
        id: targetId,
        type: "page",
        browserContextId: "default",
        url: tab.url || "",
        title: tab.title || "",
        windowId: tab.windowId,
        index: tab.index,
        pinned: tab.pinned,
        groupId: tab.groupId,
        status: tab.status,
        revision: `bridge:${targetId}:${attachment?.generation || 0}:${tab.url || ""}`
      };
    });
    sendToNative({ type: "targets_list", targets: targetList });
  } catch (error) {
    console.error("Failed to get targets:", error);
  }
}

/**
 * Attach debugger to a target tab
 */
async function attachDebugger(targetId) {
  try {
    const tabId = parseInt(targetId, 10);
    if (isNaN(tabId)) {
      return { ok: false, error: "Invalid target ID" };
    }
    
    // Check if already attached
    if (attachedTargets.has(targetId)) {
      return { ok: true, alreadyAttached: true };
    }
    
    // Attach debugger
    await chrome.debugger.attach({ tabId }, "1.3");
    
    // Enable required domains
    await chrome.debugger.sendCommand({ tabId }, "Page.enable");
    await chrome.debugger.sendCommand({ tabId }, "Runtime.enable");
    await chrome.debugger.sendCommand({ tabId }, "DOM.enable");
    await chrome.debugger.sendCommand({ tabId }, "Network.enable");
    await chrome.debugger.sendCommand({ tabId }, "Accessibility.enable");
    
    // Track attachment
    attachedTargets.set(targetId, {
      tabId,
      attachedAt: Date.now(),
      generation: 0
    });
    
    return { ok: true };
  } catch (error) {
    return { ok: false, error: error.message };
  }
}

/**
 * Detach debugger from a target
 */
async function detachDebugger(targetId) {
  try {
    const tabId = parseInt(targetId, 10);
    if (isNaN(tabId)) {
      return { ok: false, error: "Invalid target ID" };
    }
    
    await chrome.debugger.detach({ tabId });
    attachedTargets.delete(targetId);
    
    return { ok: true };
  } catch (error) {
    return { ok: false, error: error.message };
  }
}

/**
 * Handle debugger events
 */
function handleDebuggerEvent(source, method, params) {
  const targetId = source.tabId.toString();
  const attachment = attachedTargets.get(targetId);
  
  if (!attachment) return;
  
  // Increment generation on navigation
  if (method === "Page.frameNavigated" || method === "Page.lifecycleEvent") {
    attachment.generation++;
  }
  
  // Forward event to native host
  sendToNative({
    type: "debugger_event",
    targetId,
    generation: attachment.generation,
    method,
    params
  });
}

/**
 * Handle debugger detach
 */
function handleDebuggerDetach(source, reason) {
  const targetId = source.tabId.toString();
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
  try {
    const tabId = parseInt(targetId, 10);
    if (Number.isNaN(tabId)) {
      await sendCommandResult({ type: "cdp_command_result", requestId, ok: false, error: "Invalid target ID" });
      return;
    }
    if (!attachedTargets.has(targetId)) {
      const attached = await attachDebugger(targetId);
      if (!attached.ok) {
        await sendCommandResult({ type: "cdp_command_result", requestId, ok: false, error: attached.error || "debugger attach failed" });
        return;
      }
    }
    const result = await chrome.debugger.sendCommand({ tabId }, method, params || {});
    await sendCommandResult({ type: "cdp_command_result", requestId, ok: true, result });
  } catch (error) {
    await sendCommandResult({ type: "cdp_command_result", requestId, ok: false, error: error.message });
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
    result: { pong: true, timestamp: Date.now() }
  });
}

async function handleOpenTab(message) {
  try {
    const tab = await chrome.tabs.create({
      url: message.url,
      active: !Boolean(message.background)
    });
    const targetId = String(tab.id);
    await sendCommandResult({
      type: "open_tab_result",
      requestId: message.requestId,
      ok: true,
      result: {
        target: {
          id: targetId,
          type: "page",
          browser_context_id: "default",
          url: tab.url || message.url,
          title: tab.title || "",
          revision: `bridge:${targetId}:0:${tab.url || message.url}`
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

async function handleCloseTab(message) {
  try {
    const tabId = Number(message.targetId);
    if (!Number.isInteger(tabId)) throw new Error("Invalid target ID");
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
    const tabId = Number(targetId);
    if (!Number.isInteger(tabId)) throw new Error("Invalid target ID");
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
    const tabId = Number(targetId);
    if (!Number.isInteger(tabId)) throw new Error("Invalid target ID");
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
  try {
    await connectNative();
    console.log("Connected to Comptrol daemon");
  } catch (error) {
    console.error("Failed to connect to daemon:", error);
    scheduleReconnect();
  }

  chrome.alarms.create(HEALTH_CHECK_ALARM, { periodInMinutes: 1 });
  chrome.alarms.create(GROUP_OBSERVE_ALARM, { periodInMinutes: 1 });

  if (!listenersRegistered) {
    listenersRegistered = true;
    chrome.debugger.onEvent.addListener(handleDebuggerEvent);
    chrome.debugger.onDetach.addListener(handleDebuggerDetach);

    chrome.alarms.onAlarm.addListener(async alarm => {
      if (alarm.name === HEALTH_CHECK_ALARM) {
        if (!isConnected) scheduleReconnect();
        await sendTargetsToNative();
      } else if (alarm.name === GROUP_OBSERVE_ALARM) {
        await observeOpenGroups();
      }
    });

    chrome.tabs.onCreated.addListener(() => sendTargetsToNative());
    chrome.tabs.onRemoved.addListener(() => sendTargetsToNative());
    chrome.tabs.onUpdated.addListener(() => sendTargetsToNative());
    chrome.tabGroups.onCreated.addListener(() => observeOpenGroups());
    chrome.tabGroups.onUpdated.addListener(() => observeOpenGroups());
    chrome.tabGroups.onRemoved.addListener(() => observeOpenGroups());
  }

  await observeOpenGroups();
  await sendTargetsToNative();
}

// Message handler for popup/extension pages
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  switch (message?.type) {
    case "get_status":
      sendResponse({ ok: true, connected: isConnected, targets: Array.from(attachedTargets.keys()) });
      return true;
      
    case "attach_debugger":
      attachDebugger(message.targetId).then(result => sendResponse({ ok: true, ...result }));
      return true;
      
    case "detach_debugger":
      detachDebugger(message.targetId).then(result => sendResponse({ ok: true, ...result }));
      return true;
      
    case "get_targets":
      chrome.tabs.query({}).then(tabs => {
        sendResponse({ ok: true, targets: tabs.map(t => ({ id: t.id.toString(), url: t.url, title: t.title })) });
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