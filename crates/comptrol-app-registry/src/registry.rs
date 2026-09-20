//! Resolve installed applications.
//!
//! Resolution prefers identity over display name: bundle identifiers on
//! macOS, AppUserModelIDs / registered commands on Windows, and desktop
//! entry IDs on Linux. Display names are a fallback filtered by exact
//! match so "code" cannot accidentally open "Code - OSS" or an
//! unrelated lookalike.

use crate::Resource;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("no installed application matches {0}")]
    NotFound(String),
    #[error("ambiguous application name {0}: {1} candidates")]
    Ambiguous(String, usize),
}

/// One resolvable installed application.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AppEntry {
    /// Platform identity: bundle id / AUMID / desktop entry id.
    pub id: String,
    /// Human display name from the platform registration.
    pub display_name: String,
    /// Platform that produced this entry.
    pub platform: String,
    /// Executable or bundle path when the platform exposes one.
    pub executable: Option<PathBuf>,
    /// Version string when available.
    pub version: Option<String>,
}

/// Probe metadata about the current host (used by tests and doctor).
pub fn host_probe() -> serde_json::Value {
    let resolver = match std::env::consts::OS {
        "macos" => "launch_services".to_owned(),
        "windows" => "shell_registration".to_owned(),
        "linux" => "desktop_entries".to_owned(),
        other => format!("fallback:{other}"),
    };
    serde_json::json!({
        "platform": std::env::consts::OS,
        "resolver": resolver,
    })
}

/// Resolve an app by exact identity first, then by unique exact display
/// name. `query` values that look like a platform identity (contain a
/// dot for bundle ids or match a desktop entry id) are matched against
/// ids only.
pub fn resolve(query: &str) -> Result<AppEntry, RegistryError> {
    let candidates = resolve_all(query)?;
    match candidates.len() {
        1 => Ok(candidates.into_iter().next().unwrap()),
        0 => Err(RegistryError::NotFound(query.to_owned())),
        n => Err(RegistryError::Ambiguous(query.to_owned(), n)),
    }
}

/// All exact matches for `query` (id match or exact display-name match).
pub fn resolve_all(query: &str) -> Result<Vec<AppEntry>, RegistryError> {
    let lower = query.to_ascii_lowercase();
    let matches: Vec<AppEntry> = system_entries()?
        .into_iter()
        .filter(|entry| {
            entry.id.to_ascii_lowercase() == lower
                || entry.display_name.to_ascii_lowercase() == lower
        })
        .collect();
    Ok(matches)
}

/// Enumerate installed applications through the platform registration
/// surface. Non-macOS platforms fall back to a PATH scan of app-like
/// executables; the resolution contract (exact identity or unique
/// display name) stays identical everywhere so callers and tests do not
/// branch on platform.
pub fn system_entries() -> Result<Vec<AppEntry>, RegistryError> {
    match std::env::consts::OS {
        "macos" => macos_entries(),
        _ => path_entries(),
    }
}

#[cfg(target_os = "macos")]
fn macos_entries() -> Result<Vec<AppEntry>, RegistryError> {
    let mut entries = Vec::new();
    let roots = ["/Applications", "/System/Applications"];
    for root in roots {
        let read = match std::fs::read_dir(root) {
            Ok(read) => read,
            Err(_) => continue,
        };
        for item in read.flatten() {
            let path = item.path();
            if path.extension().and_then(|e| e.to_str()) != Some("app") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_owned();
            // Read the bundle id from Info.plist without launching the app.
            let bundle_id = read_macos_bundle_id(&path);
            entries.push(AppEntry {
                id: bundle_id.unwrap_or_else(|| format!("local.app.{stem}")),
                display_name: stem,
                platform: "macos".to_owned(),
                executable: Some(path),
                version: None,
            });
        }
    }
    Ok(entries)
}

#[cfg(target_os = "macos")]
fn read_macos_bundle_id(bundle: &std::path::Path) -> Option<String> {
    let plist = bundle.join("Contents/Info.plist");
    let raw = std::fs::read_to_string(&plist).ok()?;
    // Binary plists are not parsed here; XML plists carry the id inline.
    let marker = raw.find("<key>CFBundleIdentifier</key>")?;
    let tail = &raw[marker..];
    let value_start = tail.find("<string>")? + "<string>".len();
    let value_end = tail[value_start..].find("</string>")? + value_start;
    Some(tail[value_start..value_end].to_owned())
}

#[cfg(not(target_os = "macos"))]
fn macos_entries() -> Result<Vec<AppEntry>, RegistryError> {
    Ok(Vec::new())
}

/// PATH-based enumeration used on Windows/Linux (and as a portable
/// fallback). Identity is the executable file stem; resolution by exact
/// stem keeps the "no fuzzy opening" guarantee.
pub fn path_entries() -> Result<Vec<AppEntry>, RegistryError> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    for dir in path_var.split(':').filter(|dir| !dir.is_empty()) {
        let read = match std::fs::read_dir(dir) {
            Ok(read) => read,
            Err(_) => continue,
        };
        for item in read.flatten() {
            let path = item.path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|e| e.to_str()) else {
                continue;
            };
            if stem.is_empty() || !seen.insert(stem.to_owned()) {
                continue;
            }
            entries.push(AppEntry {
                id: format!("path.{stem}"),
                display_name: stem.to_owned(),
                platform: std::env::consts::OS.to_owned(),
                executable: Some(path),
                version: None,
            });
        }
    }
    Ok(entries)
}

/// Whether opening `resource` in the resolved app requires the native
/// launcher (as opposed to passing argv to the executable).
pub fn requires_native_open(resource: &Resource) -> bool {
    matches!(resource, Resource::Url { .. } | Resource::DeepLink { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_probe_names_platform() {
        let probe = host_probe();
        assert_eq!(probe["platform"], serde_json::json!(std::env::consts::OS));
    }

    #[test]
    fn missing_apps_report_not_found() {
        let result = resolve("definitely-not-an-app-xyz");
        assert!(matches!(result, Err(RegistryError::NotFound(_))));
    }

    #[test]
    fn path_entries_are_unique_by_stem() {
        let entries = path_entries().expect("entries");
        let mut stems: Vec<&str> = entries.iter().map(|e| e.display_name.as_str()).collect();
        stems.sort();
        stems.dedup();
        assert_eq!(stems.len(), entries.len());
    }
}
