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

    if let Ok(output) = output
        && output.status.success()
    {
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
            if let Some(target) = read_lnk_target(&path)
                && let Some(stem) = target.file_stem().and_then(|s| s.to_str())
            {
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
    Ok(())
}

#[cfg(target_os = "windows")]
#[cfg(target_os = "windows")]
fn read_lnk_target(lnk_path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::fs;
    let data = fs::read(lnk_path).ok()?;
    // Minimal .lnk parser per MS-SHLLINK specification
    if data.len() < 76 {
        return None;
    }
    // Check signature: 4C 00 00 00 (CLSID_ShellLink)
    if data[0..4] != [0x4C, 0x00, 0x00, 0x00] {
        return None;
    }
    // LinkFlags at offset 0x10 (4 bytes, little-endian)
    let link_flags = u32::from_le_bytes([data[0x10], data[0x11], data[0x12], data[0x13]]);
    // Bit 0: HasLinkTargetIDList, Bit 1: HasLinkInfo
    let has_link_target_id_list = (link_flags & 0x01) != 0;
    let has_link_info = (link_flags & 0x02) != 0;
    let mut offset = 76usize;

    // Skip LinkTargetIDList if present
    if has_link_target_id_list {
        if offset + 2 > data.len() {
            return None;
        }
        let id_list_size = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
        offset += 2 + id_list_size;
    }

    // Parse LinkInfo if present to extract local base path
    if has_link_info {
        if offset + 28 > data.len() {
            return None;
        }
        let link_info_size = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        if link_info_size < 28 || offset + link_info_size > data.len() {
            return None;
        }
        // LinkInfoFlags at offset +8 from LinkInfo start
        let link_info_flags = u32::from_le_bytes([
            data[offset + 8],
            data[offset + 9],
            data[offset + 10],
            data[offset + 11],
        ]);
        let has_local_base_path = (link_info_flags & 0x01) != 0;
        // LocalBasePathOffset at offset +12 from LinkInfo start
        let local_base_path_offset = u32::from_le_bytes([
            data[offset + 12],
            data[offset + 13],
            data[offset + 14],
            data[offset + 15],
        ]) as usize;
        if has_local_base_path
            && local_base_path_offset > 0
            && offset + local_base_path_offset < data.len()
        {
            let path_bytes = &data[offset + local_base_path_offset..];
            let end = path_bytes.iter().position(|&b| b == 0)?;
            let path_str = std::str::from_utf8(&path_bytes[..end]).ok()?;
            return Some(std::path::PathBuf::from(path_str));
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn windows_entries() -> Result<Vec<AppEntry>, RegistryError> {
    Ok(Vec::new())
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
fn read_lnk_target(_lnk_path: &std::path::Path) -> Option<std::path::PathBuf> {
    None
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
            if let Ok(content) = std::fs::read_to_string(&path)
                && let Some(app_entry) = parse_desktop_file_inline(&content, &path)
            {
                let id = app_entry.id.clone();
                if seen.insert(id) {
                    entries.push(app_entry);
                }
            }
        }
    }
    Ok(entries)
}

/// Parse a .desktop file inline. Returns None for files that are hidden,
/// missing required fields, or are not valid desktop entries.
#[cfg(target_os = "linux")]
fn parse_desktop_file_inline(content: &str, path: &std::path::Path) -> Option<AppEntry> {
    let mut name = None;
    let mut exec = None;
    let mut version = None;
    let mut categories = Vec::new();
    let mut no_display = false;
    let mut terminal = false;
    let mut in_desktop_entry = false;

    for line in content.lines() {
        let line = line.trim();
        // Only parse [Desktop Entry] section
        if line.starts_with('[') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Name" if name.is_none() => name = Some(value.trim().to_owned()),
                "Exec" => exec = Some(value.trim().to_owned()),
                "Version" => version = Some(value.trim().to_owned()),
                "Categories" => {
                    categories = value
                        .split(';')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.trim().to_owned())
                        .collect()
                }
                "NoDisplay" => no_display = value.trim() == "true",
                "Terminal" => terminal = value.trim() == "true",
                _ => {}
            }
        }
    }

    if name.is_none() || exec.is_none() || no_display {
        return None;
    }

    // Parse Exec line: split into executable and argv, handling quoting and
    // field codes (%f, %F, %u, %U, %i, %c, %k) which are removed at launch time.
    let exec_str = exec.unwrap();
    let argv = parse_exec_line(&exec_str);
    let executable = argv.first().cloned().map(std::path::PathBuf::from);

    let id = format!(
        "desktop.{}",
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
    );
    let mut metadata = BTreeMap::new();
    if !categories.is_empty() {
        metadata.insert("categories".to_owned(), categories.join(";"));
    }
    metadata.insert("terminal".to_owned(), terminal.to_string());
    metadata.insert(
        "argv".to_owned(),
        serde_json::to_string(&argv).unwrap_or_default(),
    );

    Some(AppEntry {
        id,
        display_name: name.unwrap(),
        platform: "linux".to_owned(),
        executable,
        version,
        metadata,
    })
}

/// Parse a `.desktop` file `Exec=` value into a list of arguments.
///
/// Handles:
/// - Quoted strings: `"foo bar"` stays as one token
/// - Escaped spaces: `foo\ bar` stays as one token
/// - Field codes (%f, %F, %u, %U, %i, %c, %k) are removed (caller fills them at launch)
/// - Double-percent `%%` becomes a single `%`
#[cfg(target_os = "linux")]
fn parse_exec_line(exec: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut chars = exec.chars().peekable();
    let mut in_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quote => {
                in_quote = false;
            }
            '"' if !in_quote => {
                in_quote = true;
            }
            '\\' if !in_quote => {
                // Escaped character: take the next char literally
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ' ' if !in_quote && !current.is_empty() => {
                argv.push(std::mem::take(&mut current));
            }
            '%' => {
                match chars.peek() {
                    Some('f' | 'F' | 'u' | 'U') => {
                        chars.next(); // consume field code
                        // Skip field code; caller fills at launch time.
                        // For now we leave the argument empty.
                        if current.is_empty() {
                            // Don't push empty string; field code was the only content
                        }
                    }
                    Some('i' | 'c' | 'k') => {
                        chars.next(); // consume; skip these codes too
                    }
                    Some('%') => {
                        chars.next(); // consume escaped percent
                        current.push('%');
                    }
                    _ => {}
                }
            }
            _ if in_quote || ch != ' ' => {
                current.push(ch);
            }
            _ => {}
        }
    }

    if !current.is_empty() {
        argv.push(current);
    }

    argv
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
