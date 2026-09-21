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

// Debugger attachment state
const attachedTargets = new Map(); // targetId -> { tabId, debuggerPort, generation }

// Alarm for periodic health checks
const HEALTH_CHECK_ALARM = "comptrol-health-check";
const GROUP_OBSERVE_ALARM = "comptrol-observe-groups";

/**
 * Establish native messaging connection to Comptrol daemon
 */
async function connectNative() {
  return new Promise((resolve, reject) => {
    try {
      const port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
      nativePort = port;
      
      port.onMessage.addListener(handleNativeMessage);
      
      port.onDisconnect.addListener(() => {
        console.log("Native messaging disconnected");
        nativePort = null;
        isConnected = false;
        scheduleReconnect();
      });
      
      // Send handshake
      port.postMessage({
        type: "handshake",
        protocol: PROTOCOL_VERSION,
        timestamp: Date.now()
      });
      
      // Wait for handshake response
      const timeout = setTimeout(() => {
        reject(new Error("Handshake timeout"));
      }, 5000);
      
      port.onMessage.addListener(function listener(msg) {
        if (msg.type === "handshake_ack") {
          clearTimeout(timeout);
          port.onMessage.removeListener(listener);
          isConnected = true;
          reconnectAttempts = 0;
          resolve();
        }
      });
      
    } catch (error) {
      reject(error);
    }
  });
}

/**
 * Schedule reconnection with exponential backoff
 */
function scheduleReconnect() {
  if (reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
    console.error("Max reconnect attempts reached");
    return;
  }
  
  const delay = RECONNECT_BASE_DELAY_MS * Math.pow(2, reconnectAttempts);
  reconnectAttempts++;
  
  setTimeout(() => {
    connectNative().catch(err => {
      console.error("Reconnect failed:", err);
      scheduleReconnect();
    });
  }, delay);
}

/**
 * Handle messages from native host
 */
function handleNativeMessage(message) {
  switch (message.type) {
    case "handshake_ack":
      // Handled in connectNative
      break;
    case "cdp_command":
      handleCdpCommand(message);
      break;
    case "get_targets":
      sendTargetsToNative();
      break;
    case "attach_debugger":
      attachDebugger(message.targetId).then(result => {
        sendToNative({ type: "attach_debugger_result", requestId: message.requestId, ...result });
      });
      break;
    case "detach_debugger":
      detachDebugger(message.targetId).then(result => {
        sendToNative({ type: "detach_debugger_result", requestId: message.requestId, ...result });
      });
      break;
    case "restore_group":
      restoreClosedGroup(message.groupId).then(result => {
        sendToNative({ type: "restore_group_result", requestId: message.requestId, ...result });
      });
      break;
    default:
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
    const targetList = targets.map(tab => ({
      id: tab.id.toString(),
      url: tab.url,
      title: tab.title,
      windowId: tab.windowId,
      index: tab.index,
      pinned: tab.pinned,
      groupId: tab.groupId,
      status: tab.status
    }));
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
    
    // Listen for debugger events
    chrome.debugger.onEvent.addListener(handleDebuggerEvent);
    chrome.debugger.onDetach.addListener(handleDebuggerDetach);
    
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
    if (isNaN(tabId)) {
      sendToNative({ type: "cdp_command_result", requestId, ok: false, error: "Invalid target ID" });
      return;
    }
    
    const result = await chrome.debugger.sendCommand({ tabId }, method, params || {});
    sendToNative({ type: "cdp_command_result", requestId, ok: true, result });
  } catch (error) {
    sendToNative({ type: "cdp_command_result", requestId, ok: false, error: error.message });
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
  // Connect to native host
  try {
    await connectNative();
    console.log("Connected to Comptrol daemon");
  } catch (error) {
    console.error("Failed to connect to daemon:", error);
    scheduleReconnect();
  }
  
  // Set up alarms
  chrome.alarms.create(HEALTH_CHECK_ALARM, { periodInMinutes: 1 });
  chrome.alarms.create(GROUP_OBSERVE_ALARM, { periodInMinutes: 1 });
  
  // Set up alarm listeners
  chrome.alarms.onAlarm.addListener(async (alarm) => {
    if (alarm.name === HEALTH_CHECK_ALARM) {
      // Health check - verify native connection
      if (!isConnected) {
        scheduleReconnect();
      }
      // Send targets periodically
      await sendTargetsToNative();
    } else if (alarm.name === GROUP_OBSERVE_ALARM) {
      await observeOpenGroups();
    }
  });
  
  // Set up tab/group listeners for real-time updates
  chrome.tabs.onCreated.addListener(() => sendTargetsToNative());
  chrome.tabs.onRemoved.addListener(() => sendTargetsToNative());
  chrome.tabs.onUpdated.addListener(() => sendTargetsToNative());
  chrome.tabGroups.onCreated.addListener(() => observeOpenGroups());
  chrome.tabGroups.onUpdated.addListener(() => observeOpenGroups());
  chrome.tabGroups.onRemoved.addListener(() => observeOpenGroups());
  
  // Initial observation
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