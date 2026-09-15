const STORAGE_KEY = "comptrol.closedGroupSnapshots.v1";
const MAX_SNAPSHOTS = 25;

function normalizeUrl(url) {
  try {
    const parsed = new URL(url);
    parsed.hash = "";
    return parsed.toString();
  } catch {
    return url || "";
  }
}

function snapshotFingerprint(snapshot) {
  return JSON.stringify({
    title: snapshot.title || "",
    color: snapshot.color,
    windowId: snapshot.windowId,
    tabs: snapshot.tabs.map((tab) => ({
      url: normalizeUrl(tab.url),
      title: tab.title || "",
      index: tab.index,
      pinned: Boolean(tab.pinned)
    }))
  });
}

async function readSnapshots() {
  const data = await chrome.storage.local.get(STORAGE_KEY);
  return Array.isArray(data[STORAGE_KEY]) ? data[STORAGE_KEY] : [];
}

async function writeSnapshots(snapshots) {
  await chrome.storage.local.set({ [STORAGE_KEY]: snapshots.slice(0, MAX_SNAPSHOTS) });
}

async function observeOpenGroups() {
  const groups = await chrome.tabGroups.query({});
  const tabs = await chrome.tabs.query({ windowType: "normal" });
  const previous = await readSnapshots();
  const observed = groups.map((group) => ({
    groupId: group.id,
    title: group.title || "",
    color: group.color,
    windowId: group.windowId,
    updatedAt: Date.now(),
    tabs: tabs
      .filter((tab) => tab.groupId === group.id)
      .sort((left, right) => left.index - right.index)
      .map((tab) => ({
        url: tab.url || "",
        title: tab.title || "",
        index: tab.index,
        pinned: Boolean(tab.pinned)
      }))
  })).filter((snapshot) => snapshot.tabs.length > 0);
  const currentFingerprints = new Set(observed.map(snapshotFingerprint));
  const retained = previous.filter((snapshot) => !currentFingerprints.has(snapshotFingerprint(snapshot)));
  await writeSnapshots([...observed, ...retained]);
  return observed;
}

function sessionTabs(session) {
  if (session?.window?.tabs) return session.window.tabs;
  if (session?.tab) return [session.tab];
  return [];
}

function matchScore(snapshot, session) {
  const tabs = sessionTabs(session);
  if (tabs.length !== snapshot.tabs.length) return -1;
  const expected = snapshot.tabs.map((tab) => normalizeUrl(tab.url)).sort();
  const actual = tabs.map((tab) => normalizeUrl(tab.url)).sort();
  if (expected.some((url, index) => url !== actual[index])) return -1;
  const titleMatches = snapshot.tabs.filter((expectedTab) =>
    tabs.some((actualTab) => normalizeUrl(actualTab.url) === normalizeUrl(expectedTab.url) &&
      expectedTab.title && actualTab.title === expectedTab.title)
  ).length;
  return titleMatches + (snapshot.title ? 1 : 0);
}

async function restoreClosedGroup(groupTitle) {
  if (!groupTitle || /[\u0000-\u001f\u007f]/.test(groupTitle)) {
    throw new Error("group_title_invalid");
  }
  const snapshots = (await readSnapshots()).filter((snapshot) => snapshot.title === groupTitle);
  if (snapshots.length !== 1) throw new Error(snapshots.length ? "group_snapshot_ambiguous" : "group_snapshot_missing");
  const snapshot = snapshots[0];
  const sessions = await chrome.sessions.getRecentlyClosed({ maxResults: 25 });
  const candidates = sessions
    .map((session) => ({ session, score: matchScore(snapshot, session) }))
    .filter((candidate) => candidate.score >= 0)
    .sort((left, right) => right.score - left.score);
  let restoredTabsFromSingles = [];
  let restored;
  if (candidates.length === 1 && candidates[0].score >= 1) {
    const session = candidates[0].session;
    restored = await chrome.sessions.restore(session.window?.sessionId || session.tab?.sessionId);
  } else {
    const singleSessions = snapshot.tabs.map((expectedTab) => {
      const matches = sessions.filter((session) => {
        const tab = session.tab;
        return tab && normalizeUrl(tab.url) === normalizeUrl(expectedTab.url) &&
          (!expectedTab.title || !tab.title || tab.title === expectedTab.title);
      });
      return matches.length === 1 ? matches[0] : null;
    });
    if (singleSessions.some((session) => !session) || new Set(singleSessions.map((session) => session?.tab?.sessionId)).size !== snapshot.tabs.length) {
      throw new Error(candidates.length ? "closed_group_session_ambiguous" : "closed_group_session_missing");
    }
    for (const session of singleSessions) {
      const one = await chrome.sessions.restore(session.tab.sessionId);
      const tab = one.tab || one.window?.tabs?.[0];
      if (tab) restoredTabsFromSingles.push(tab);
    }
    restored = { tab: restoredTabsFromSingles[0] };
  }
  const windowId = restored.window?.id || restored.tab?.windowId;
  if (windowId === undefined) throw new Error("restored_window_missing");
  const restoredTabs = await chrome.tabs.query({ windowId });
  const targetUrls = new Set(snapshot.tabs.map((tab) => normalizeUrl(tab.url)));
  const targetTabs = restoredTabsFromSingles.length
    ? restoredTabsFromSingles
    : restoredTabs.filter((tab) => targetUrls.has(normalizeUrl(tab.url)));
  if (targetTabs.length !== snapshot.tabs.length) throw new Error("restored_group_membership_mismatch");
  const groupId = await chrome.tabs.group({ tabIds: targetTabs.map((tab) => tab.id) });
  await chrome.tabGroups.update(groupId, { title: snapshot.title, color: snapshot.color });
  const verified = await chrome.tabs.query({ windowId });
  const verifiedGroup = verified.filter((tab) => tab.groupId === groupId);
  if (verifiedGroup.length !== snapshot.tabs.length || verifiedGroup.some((tab) => !targetUrls.has(normalizeUrl(tab.url)))) {
    throw new Error("restored_group_verification_failed");
  }
  await observeOpenGroups();
  return { groupId, windowId, tabCount: verifiedGroup.length, verified: true };
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === "listSnapshots") {
    readSnapshots().then((snapshots) => sendResponse({ ok: true, snapshots }));
    return true;
  }
  if (message?.type === "restoreClosedGroup") {
    restoreClosedGroup(message.groupTitle)
      .then((data) => sendResponse({ ok: true, data }))
      .catch((error) => sendResponse({ ok: false, error: error.message }));
    return true;
  }
  return false;
});

chrome.alarms.create("comptrol-observe-groups", { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === "comptrol-observe-groups") observeOpenGroups().catch(() => {});
});
chrome.runtime.onStartup.addListener(() => observeOpenGroups().catch(() => {}));
chrome.runtime.onInstalled.addListener(() => observeOpenGroups().catch(() => {}));
chrome.tabs.onCreated.addListener(() => observeOpenGroups().catch(() => {}));
chrome.tabs.onRemoved.addListener(() => observeOpenGroups().catch(() => {}));
chrome.tabs.onUpdated.addListener(() => observeOpenGroups().catch(() => {}));
chrome.tabGroups.onCreated.addListener(() => observeOpenGroups().catch(() => {}));
chrome.tabGroups.onUpdated.addListener(() => observeOpenGroups().catch(() => {}));
chrome.tabGroups.onRemoved.addListener(() => observeOpenGroups().catch(() => {}));
