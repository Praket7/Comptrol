/**
 * Comptrol Browser Bridge - Popup Script
 */

const $ = (id) => document.getElementById(id);

let state = {
  connected: false,
  targets: [],
  attached: new Set()
};

async function sendMessage(message) {
  return new Promise((resolve) => {
    chrome.runtime.sendMessage(message, (response) => {
      resolve(response);
    });
  }
}

function setStatus(connected, text) {
  const dot = $("statusDot");
  const text = $("statusText");
  state.connected = connected;
  dot.className = "status-dot " + (connected ? "connected" : "disconnected");
  text.textContent = text || (connected ? "Connected to daemon" : "Disconnected");
}

function renderTargets(targets) {
  const list = $("targetList");
  state.targets = targets;
  
  if (!targets.length) {
    list.innerHTML = '<div class="empty-state">No targets found</div>';
    return;
  }
  
  list.innerHTML = targets.map(t => {
    const isAttached = state.attached.has(t.id);
    const badges = [];
    if (isAttached) badges.push('<span class="target-badge attached">Attached</span>');
    if (t.groupId) badges.push('<span class="target-badge group">Group</span>');
    if (t.pinned) badges.push('<span class="target-badge pinned">Pinned</span>');
    
    return `
      <div class="target-item" data-target-id="${t.id}">
        <div class="target-favicon">${t.url ? new URL(t.url).hostname.charAt(0).toUpperCase() : '?'}</div>
        <div class="target-info">
          <div class="target-title" title="${escapeHtml(t.title || 'Untitled')}">${escapeHtml(t.title || 'Untitled')}</div>
          <div class="target-url">${escapeHtml(t.url || '')}</div>
        </div>
        <div class="target-badges">
          ${badges.join('')}
        </div>
      </div>
    `;
  }).join('');
  
  // Add click handlers
  list.querySelectorAll('.target-item').forEach(item => {
    item.addEventListener('click', () => {
      const targetId = item.dataset.targetId;
      handleTargetClick(targetId);
    });
  });
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

async function loadStatus() {
  const response = await sendMessage({ type: "get_status" });
  if (response.ok) {
    setStatus(response.connected, response.connected ? "Connected to daemon" : "Disconnected");
    state.attached = new Set(response.targets || []);
    renderTargets(state.targets);
  }
}

async function loadTargets() {
  const response = await sendMessage({ type: "get_targets" });
  if (response.ok) {
    state.targets = response.targets;
    renderTargets(state.targets);
  }
}

async function handleTargetClick(targetId) {
  const isAttached = state.attached.has(targetId);
  const response = await sendMessage({
    type: isAttached ? "detach_debugger" : "attach_debugger",
    targetId
  });
  if (response.ok) {
    if (isAttached) {
      state.attached.delete(targetId);
    } else {
      state.attached.add(targetId);
    }
    await loadTargets();
  } else {
    alert("Failed: " + (response.error || "Unknown error"));
  }
}

async function handleReconnect() {
  const btn = $("reconnectBtn");
  const loading = $("reconnectLoading");
  btn.disabled = true;
  loading.style.display = "inline-block";
  
  // Reconnect by reloading the extension
  chrome.runtime.reload();
}

async function handleRefresh() {
  const btn = $("refreshBtn");
  const loading = $("refreshLoading");
  btn.disabled = true;
  loading.style.display = "inline-block";
  
  await loadTargets();
  await loadStatus();
  
  btn.disabled = false;
  loading.style.display = "none";
}

async function handleObserveGroups() {
  await sendMessage({ type: "observe_groups" });
  alert("Group observation triggered");
}

async function handleListSnapshots() {
  const response = await sendMessage({ type: "list_snapshots" });
  if (response.ok) {
    alert("Snapshots: " + JSON.stringify(response.snapshots, null, 2));
  } else {
    alert("Failed: " + (response.error || "Unknown error"));
  }
}

async function handleDisconnect() {
  if (confirm("Disconnect from daemon?")) {
    chrome.runtime.reload();
  }
}

function init() {
  // Initial load
  loadStatus();
  loadTargets();
  
  // Periodic refresh
  setInterval(() => {
    if (state.connected) {
      loadTargets();
    } else {
      loadStatus();
    }
  }, 5000);
  
  // Event listeners
  $("reconnectBtn").addEventListener("click", handleReconnect);
  $("refreshBtn").addEventListener("click", handleRefresh);
  $("observeGroupsBtn").addEventListener("click", handleObserveGroups);
  $("listSnapshotsBtn").addEventListener("click", handleListSnapshots);
  $("disconnectBtn").addEventListener("click", handleDisconnect);
}

// Initialize when DOM is ready
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", init);
} else {
  init();
}