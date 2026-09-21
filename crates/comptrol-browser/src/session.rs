//! Browser session broker.
//!
//! One logical browser/profile model over all permitted control surfaces.
//! The planner asks the broker which provider can serve an intent; the
//! model never chooses among mechanisms.
//!
//! Priority (V5 contract):
//!
//! 1. permissioned Chrome 144+ existing-session auto-connect (user clicks
//!    Allow in Chrome; Comptrol never bypasses that prompt),
//! 2. explicitly installed companion extension / native bridge,
//! 3. user-configured existing debugging endpoint,
//! 4. dedicated non-default automation profile,
//! 5. foreground native launcher without protocol control.
//!
//! Refused outright: restarting default-profile Chrome with debugging
//! flags (Chrome 136+ ignores them by design) and copying the user's
//! profile or cookies anywhere.

use super::TargetRecord;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Chrome user data directory locations by platform
fn chrome_user_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join("Library/Application Support/Google/Chrome"));
            dirs.push(home.join("Library/Application Support/Google/Chrome Beta"));
            dirs.push(home.join("Library/Application Support/Google/Chrome Dev"));
            dirs.push(home.join("Library/Application Support/Google/Chrome Canary"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let base = PathBuf::from(local);
            dirs.push(base.join("Google/Chrome/User Data"));
            dirs.push(base.join("Google/Chrome Beta/User Data"));
            dirs.push(base.join("Google/Chrome Dev/User Data"));
            dirs.push(base.join("Google/Chrome SxS/User Data"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".config/google-chrome"));
            dirs.push(home.join(".config/chromium"));
            dirs.push(home.join(".config/google-chrome-beta"));
            dirs.push(home.join(".config/google-chrome-unstable"));
        }
    }
    dirs
}

/// Find DevToolsActivePort file in Chrome user data directory
fn find_devtools_active_port(user_data_dir: &Path) -> Option<(u16, String)> {
    for entry in fs::read_dir(user_data_dir).ok()?.flatten() {
        let path = entry.path();
        if path.file_name() == Some(std::ffi::OsStr::new("DevToolsActivePort"))
            && let Ok(content) = fs::read_to_string(&path)
        {
            let lines: Vec<&str> = content.lines().collect();
            if lines.len() >= 2
                && let Ok(port) = lines[0].parse::<u16>()
            {
                let ws_path = lines[1].to_string();
                return Some((port, ws_path));
            }
        }
    }
    None
}

/// Attempt to connect via Chrome 144+ permissioned auto-connect using DevToolsActivePort.
///
/// This implements the actual Chrome DevTools Protocol approach:
/// 1. Find Chrome user data directory and read DevToolsActivePort
/// 2. Construct ws://127.0.0.1:<port><path> URL
/// 3. Connect to the WebSocket (user must have enabled remote debugging and clicked Allow)
pub async fn connect_permissioned_auto_connect(
    _debug_port: u16,
    timeout: Duration,
) -> Result<String, String> {
    let start = std::time::Instant::now();

    for user_data_dir in chrome_user_data_dirs() {
        if let Some((port, ws_path)) = find_devtools_active_port(&user_data_dir) {
            let _ws_url = format!("ws://127.0.0.1:{}{}", port, ws_path);

            // Verify we can connect to the WebSocket
            let client = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| format!("HTTP client error: {}", e))?;

            let http_url = format!("http://127.0.0.1:{}/json/version", port);
            let deadline = start + timeout;

            while std::time::Instant::now() < deadline {
                if let Ok(resp) = client.get(&http_url).send().await
                    && resp.status().is_success()
                {
                    return Ok(format!("ws://127.0.0.1:{}{}", port, ws_path));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            return Err(format!(
                "Found DevToolsActivePort at {} but Chrome did not respond on http://127.0.0.1:{}/json/version within timeout. Ensure remote debugging is enabled in chrome://inspect/#remote-debugging and you have clicked Allow.",
                user_data_dir.display(),
                port
            ));
        }
    }

    Err(
        "Chrome DevToolsActivePort not found. Ensure Chrome 144+ is running with remote debugging enabled (chrome://inspect/#remote-debugging) and you have a user profile with remote debugging consent."
            .to_owned(),
    )
}

/// Get the auto-connect debug port from environment or default.
pub fn auto_connect_debug_port() -> u16 {
    std::env::var("COMPTROL_CHROME_DEBUG_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9222)
}

/// Control surfaces the broker knows about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionProvider {
    /// Chrome 144+ permissioned existing-session auto-connect.
    PermissionedAutoConnect,
    /// Signed companion extension / native messaging bridge.
    CompanionExtension,
    /// Operator-configured CDP endpoint (`COMPTROL_CDP_ENDPOINT`).
    ExplicitCdp,
    /// Dedicated non-default automation profile.
    DedicatedProfile,
    /// Foreground launcher with no protocol control.
    NativeLauncher,
}

impl SessionProvider {
    pub fn id(self) -> &'static str {
        match self {
            SessionProvider::PermissionedAutoConnect => "chrome_permissioned_auto_connect",
            SessionProvider::CompanionExtension => "companion_extension",
            SessionProvider::ExplicitCdp => "explicit_cdp_endpoint",
            SessionProvider::DedicatedProfile => "dedicated_automation_profile",
            SessionProvider::NativeLauncher => "native_launcher",
        }
    }

    /// Whether this provider can observe the user's signed-in state.
    /// PermissionedAutoConnect (user clicks Allow) and CompanionExtension
    /// (extension has access to the user's browser session) both qualify.
    pub fn signed_in_capable(self) -> bool {
        matches!(
            self,
            SessionProvider::PermissionedAutoConnect | SessionProvider::CompanionExtension
        )
    }
}

/// One discovered browser control surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSession {
    pub provider: SessionProvider,
    pub available: bool,
    pub reason: String,
    pub signed_in_capable: bool,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Discover local browser surfaces without connecting to anything.
/// Check if the companion extension native host is registered on this platform.
fn native_bridge_available() -> bool {
    let host_id = "comptrol_browser_bridge";
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        let manifest = std::path::PathBuf::from(format!(
            "{home}/Library/Application Support/Google/Chrome/NativeMessagingHosts/{host_id}.json"
        ));
        manifest.exists()
    }
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        let manifest = std::path::PathBuf::from(format!(
            "{home}/.config/google-chrome/NativeMessagingHosts/{host_id}.json"
        ));
        manifest.exists()
    }
    #[cfg(target_os = "windows")]
    {
        // Check registry for native messaging host registration
        std::process::Command::new("reg")
            .args([
                "query",
                "HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\comptrol_browser_bridge",
                "/ve",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

pub fn list_sessions() -> Vec<BrowserSession> {
    let cdp_configured = std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some();
    let auto_connect_armed = std::env::var("COMPTROL_CHROME_AUTO_CONNECT").as_deref() == Ok("1");
    vec![
        BrowserSession {
            provider: SessionProvider::PermissionedAutoConnect,
            available: auto_connect_armed,
            reason: if auto_connect_armed {
                "permissioned auto-connect is armed; Chrome still shows its Allow prompt per connection".to_owned()
            } else {
                "remote debugging consent is not enabled; open chrome://inspect/#remote-debugging and enable it, then arm COMPTROL_CHROME_AUTO_CONNECT".to_owned()
            },
            signed_in_capable: true,
        },
        BrowserSession {
            provider: SessionProvider::CompanionExtension,
            available: native_bridge_available(),
            reason: if native_bridge_available() {
                "companion extension native host is registered; CDP commands route through the daemon"
                    .to_owned()
            } else {
                "companion extension native host is not registered; install the Browser Bridge extension"
                    .to_owned()
            },
            signed_in_capable: true,
        },
        BrowserSession {
            provider: SessionProvider::ExplicitCdp,
            available: cdp_configured,
            reason: if cdp_configured {
                "COMPTROL_CDP_ENDPOINT is configured".to_owned()
            } else {
                "COMPTROL_CDP_ENDPOINT is not configured".to_owned()
            },
            signed_in_capable: false,
        },
        BrowserSession {
            provider: SessionProvider::DedicatedProfile,
            available: true,
            reason: "always available as the unsigned fallback; never used for signed-in state"
                .to_owned(),
            signed_in_capable: false,
        },
        BrowserSession {
            provider: SessionProvider::NativeLauncher,
            available: true,
            reason: "foreground launcher with launcher-acceptance reporting only".to_owned(),
            signed_in_capable: false,
        },
    ]
}

/// Select the strongest available provider for an intent.
/// `needs_signed_in` forces the permissioned route or an explicit refusal:
/// the broker never silently substitutes the isolated profile for the
/// user's signed-in tabs.
pub fn select_provider(needs_signed_in: bool) -> Result<SessionProvider, String> {
    let sessions = list_sessions();
    let get = |provider: SessionProvider| {
        sessions
            .iter()
            .find(|session| session.provider == provider)
            .expect("broker lists every provider")
    };
    if needs_signed_in {
        // Try permissioned auto-connect first (user clicks Allow in Chrome)
        let permissioned = get(SessionProvider::PermissionedAutoConnect);
        if permissioned.available {
            return Ok(SessionProvider::PermissionedAutoConnect);
        }
        // Fall back to companion extension if native host is registered
        let extension = get(SessionProvider::CompanionExtension);
        if extension.available {
            return Ok(SessionProvider::CompanionExtension);
        }
        return Err(format!(
            "signed-in browser control needs either the permissioned existing-session route ({}) or the companion extension ({})",
            permissioned.reason, extension.reason
        ));
    }
    for provider in [
        SessionProvider::PermissionedAutoConnect,
        SessionProvider::CompanionExtension,
        SessionProvider::ExplicitCdp,
        SessionProvider::DedicatedProfile,
    ] {
        if get(provider).available {
            return Ok(provider);
        }
    }
    Ok(SessionProvider::NativeLauncher)
}

/// Live per-target state cache over the event-maintained [`TargetGraph`].
///
/// Entries are keyed by target id and validated against the graph's current
/// revision: a cached snapshot is returned only when its revision still
/// matches the live graph, so DOM/navigation mutations that bump the
/// revision always invalidate. There is no TTL guessing — the revision is
/// the source of truth, and explicit `invalidate` covers cases the graph
/// does not observe (e.g. extension-driven group changes).
#[derive(Clone, Debug, Default)]
pub struct TargetStateCache {
    entries: HashMap<String, CachedTargetState>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CachedTargetState {
    pub record: TargetRecord,
    pub observed_at_ms: u128,
    pub hits: u64,
}

impl TargetStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached record when its revision matches the live graph,
    /// otherwise refresh from the graph and cache the new snapshot.
    /// `None` when the target is absent from the graph (also evicts).
    pub fn get(&mut self, graph: &super::TargetGraph, target_id: &str) -> Option<TargetRecord> {
        let live = match graph.targets.get(target_id) {
            Some(record) => record.clone(),
            None => {
                self.entries.remove(target_id);
                return None;
            }
        };
        match self.entries.get_mut(target_id) {
            Some(cached) if cached.record.revision == live.revision => {
                cached.hits = cached.hits.saturating_add(1);
                Some(cached.record.clone())
            }
            _ => {
                self.entries.insert(
                    target_id.to_owned(),
                    CachedTargetState {
                        record: live.clone(),
                        observed_at_ms: now_ms(),
                        hits: 0,
                    },
                );
                Some(live)
            }
        }
    }

    /// Peek without refreshing: `Some` only on a revision match.
    pub fn peek(&self, graph: &super::TargetGraph, target_id: &str) -> Option<TargetRecord> {
        let cached = self.entries.get(target_id)?;
        let live = graph.targets.get(target_id)?;
        if cached.record.revision == live.revision {
            Some(cached.record.clone())
        } else {
            None
        }
    }

    pub fn invalidate(&mut self, target_id: &str) {
        self.entries.remove(target_id);
    }

    pub fn invalidate_all(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, revision: &str) -> TargetRecord {
        TargetRecord {
            id: id.to_owned(),
            target_type: "page".to_owned(),
            browser_context_id: None,
            session_id: None,
            url: Some("https://example.com".to_owned()),
            title: Some("Example".to_owned()),
            opener_id: None,
            attached: true,
            generation: 1,
            revision: revision.to_owned(),
        }
    }

    fn graph_with(record: TargetRecord) -> super::super::TargetGraph {
        let mut graph = super::super::TargetGraph::default();
        graph.apply_created(record);
        graph
    }

    #[test]
    fn cache_returns_live_on_first_read_then_hits() {
        let mut cache = TargetStateCache::new();
        let graph = graph_with(record("t1", "rev-1"));
        let first = cache.get(&graph, "t1").expect("present");
        assert_eq!(first.revision, "rev-1");
        let second = cache.get(&graph, "t1").expect("cached");
        assert_eq!(second.revision, "rev-1");
        assert_eq!(cache.entries["t1"].hits, 1);
    }

    #[test]
    fn revision_bump_invalidates() {
        let mut cache = TargetStateCache::new();
        let graph = graph_with(record("t1", "rev-1"));
        cache.get(&graph, "t1");
        assert!(cache.peek(&graph, "t1").is_some());
        let mut changed = graph;
        changed.apply_changed("t1", Some("https://example.com/next".to_owned()), None);
        assert!(cache.peek(&changed, "t1").is_none());
        let refreshed = cache.get(&changed, "t1").expect("refreshed");
        assert_eq!(refreshed.url.as_deref(), Some("https://example.com/next"));
    }

    #[test]
    fn missing_target_evicts() {
        let mut cache = TargetStateCache::new();
        let graph = graph_with(record("t1", "rev-1"));
        cache.get(&graph, "t1");
        assert_eq!(cache.len(), 1);
        let empty = super::super::TargetGraph::default();
        assert!(cache.get(&empty, "t1").is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn signed_in_selection_refuses_without_permissioned_route() {
        let result = select_provider(true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("permissioned"));
    }

    #[test]
    fn unsigned_selection_prefers_explicit_or_dedicated() {
        // Without env configuration this resolves to the dedicated profile.
        assert_eq!(
            select_provider(false).expect("always resolves"),
            SessionProvider::DedicatedProfile
        );
    }

    #[test]
    fn broker_lists_every_provider_with_reasons() {
        let sessions = list_sessions();
        assert_eq!(sessions.len(), 5);
        for session in &sessions {
            assert!(!session.reason.is_empty());
        }
        // Both PermissionedAutoConnect and CompanionExtension are signed-in capable
        assert!(
            sessions
                .iter()
                .filter(|session| session.signed_in_capable)
                .count()
                == 2
        );
    }
}
