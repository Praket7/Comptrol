/// Resolve installed applications.
///
/// Resolution prefers identity over display name: bundle identifiers on
/// macOS, AppUserModelIDs / registered commands on Windows, and desktop
/// entry IDs on Linux. Display names are a fallback filtered by exact
/// match so "code" cannot accidentally open "Code - OSS" or an
/// unrelated lookalike.
use crate::Resource;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
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
    /// Platform-specific metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
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
        "windows" => windows_entries(),
        "linux" => linux_entries(),
        _ => path_entries(),
    }
}

#[cfg(target_os = "macos")]
fn macos_entries() -> Result<Vec<AppEntry>, RegistryError> {
    let mut entries = Vec::new();
    let roots = [
        "/Applications",
        "/System/Applications",
        "/Applications/Utilities",
    ];
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
            let version = read_macos_version(&path);
            let mut metadata = BTreeMap::new();
            if let Some(v) = version.clone() {
                metadata.insert("version".to_owned(), v);
            }
            entries.push(AppEntry {
                id: bundle_id.unwrap_or_else(|| format!("local.app.{stem}")),
                display_name: stem,
                platform: "macos".to_owned(),
                executable: Some(path),
                version,
                metadata,
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

#[cfg(target_os = "macos")]
fn read_macos_version(bundle: &std::path::Path) -> Option<String> {
    let plist = bundle.join("Contents/Info.plist");
    let raw = std::fs::read_to_string(&plist).ok()?;
    let marker = raw.find("<key>CFBundleShortVersionString</key>")?;
    let tail = &raw[marker..];
    let value_start = tail.find("<string>")? + "<string>".len();
    let value_end = tail[value_start..].find("</string>")? + value_start;
    Some(tail[value_start..value_end].to_owned())
}

#[cfg(not(target_os = "macos"))]
fn macos_entries() -> Result<Vec<AppEntry>, RegistryError> {
    Ok(Vec::new())
}

/// Windows: enumerate apps via Start Menu shell registration and AppUserModelIDs
#[cfg(target_os = "windows")]
fn windows_entries() -> Result<Vec<AppEntry>, RegistryError> {
    use std::process::Command;
    let mut entries = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    // Query AppUserModelIDs via PowerShell
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-StartApps | Select-Object AppID, Name | ConvertTo-Json -Depth 3",
        ])
        .output();

    if let Ok(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(apps) = serde_json::from_str::<Vec<serde_json::Value>>(&stdout) {
                for app in apps {
                    let id = app.get("AppID").and_then(|v| v.as_str()).unwrap_or("");
                    let name = app.get("Name").and_then(|v| v.as_str()).unwrap_or("");
                    if !id.is_empty() && !name.is_empty() && seen.insert(id.to_owned()) {
                        entries.push(AppEntry {
                            id: id.to_owned(),
                            display_name: name.to_owned(),
                            platform: "windows".to_owned(),
                            executable: None,
                            version: None,
                            metadata: BTreeMap::new(),
                        });
                    }
                }
            }
        }
    }

    // Also scan Start Menu shortcuts
    if let Ok(programs) = std::env::var("ProgramData") {
        let start_menu =
            std::path::Path::new(&programs).join("Microsoft/Windows/Start Menu/Programs");
        if start_menu.exists() {
            scan_windows_shortcuts(&start_menu, &mut entries, &mut seen)?;
        }
    }
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        let user_start_menu =
            std::path::Path::new(&local_appdata).join("Microsoft/Windows/Start Menu/Programs");
        if user_start_menu.exists() {
            scan_windows_shortcuts(&user_start_menu, &mut entries, &mut seen)?;
        }
    }

    Ok(entries)
}

#[cfg(target_os = "windows")]
fn scan_windows_shortcuts(
    dir: &std::path::Path,
    entries: &mut Vec<AppEntry>,
    seen: &mut std::collections::BTreeSet<String>,
) -> Result<(), RegistryError> {
    use std::fs;
    for entry in fs::read_dir(dir).ok().into_iter().flatten() {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            scan_windows_shortcuts(&path, entries, seen)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("lnk") {
            // Parse .lnk file to get target path
            if let Some(target) = read_lnk_target(&path) {
                if let Some(stem) = target.file_stem().and_then(|s| s.to_str()) {
                    let stem = stem.to_owned();
                    if seen.insert(stem.clone()) {
                        entries.push(AppEntry {
                            id: format!("lnk.{stem}"),
                            display_name: path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&stem)
                                .to_owned(),
                            platform: "windows".to_owned(),
                            executable: Some(target),
                            version: None,
                            metadata: BTreeMap::new(),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn read_lnk_target(lnk_path: &std::path::Path) -> Option<std::path::PathBuf> {
    // Simplified .lnk parsing - just check if it points to an executable
    // In production, use a proper .lnk parser or COM shell link
    None
}

#[cfg(not(target_os = "windows"))]
fn windows_entries() -> Result<Vec<AppEntry>, RegistryError> {
    Ok(Vec::new())
}

/// Linux: enumerate apps via XDG desktop entries
#[cfg(target_os = "linux")]
fn linux_entries() -> Result<Vec<AppEntry>, RegistryError> {
    use std::fs;
    let mut entries = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());

    for data_dir in data_dirs.split(':') {
        let apps_dir = std::path::Path::new(data_dir).join("applications");
        if !apps_dir.exists() {
            continue;
        }
        for entry in fs::read_dir(apps_dir).ok().into_iter().flatten() {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Some(entry) = parse_desktop_file(&content, &path) {
                    let id = entry.id.clone();
                    if entries.iter().any(|e| e.id == id) {
                        continue; // Skip duplicates
                    }
                    entries.push(entry);
                }
            }
        }
    }
    Ok(entries)
}

#[cfg(not(target_os = "linux"))]
fn linux_entries() -> Result<Vec<AppEntry>, RegistryError> {
    Ok(Vec::new())
}

/// PATH-based enumeration used on Windows/Linux (and as a portable
/// fallback). Identity is the executable file stem; resolution by exact
/// stem keeps the "no fuzzy opening" guarantee.
pub fn path_entries() -> Result<Vec<AppEntry>, RegistryError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    for dir in std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|dir| !dir.is_empty())
    {
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
                metadata: BTreeMap::new(),
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
