//! Platform restore-surface glue for native Chrome recently-closed restore.
//!
//! Each platform must enumerate Chrome's own restore entries through its
//! accessibility surface and invoke one exact entry. No platform writes Chrome
//! session files and no platform sends keyboard shortcuts.
//!
//! Platforms without an implemented enumeration provider report
//! `native_restore_unavailable` instead of approximating a restore.

use crate::restore::{
    NATIVE_UNAVAILABLE, RestoreEntry, RestoreError, RestoreKind, native_unavailable, refuse,
};

/// Enumerate recently-closed restore entries through the native surface.
#[cfg(target_os = "macos")]
pub fn macos_entries() -> Result<Vec<RestoreEntry>, RestoreError> {
    let script = "tell application \"System Events\"\ntell application process \"Google Chrome\"\nset output to \"\"\nrepeat with w in windows\nrepeat with b in (every button of w whose description contains \"Closed\")\nset output to output & (name of b) & linefeed\nend repeat\nend repeat\nreturn output\nend tell\nend tell";
    let output = crate::run_osascript(script).map_err(|error| {
        native_unavailable(
            format!("macOS Accessibility provider unavailable: {error}"),
            Some("Grant Accessibility permission to the Comptrol host".to_owned()),
        )
    })?;
    if !output.status.success() {
        return Err(native_unavailable(
            "Chrome restore entries are not reachable through macOS Accessibility",
            Some("Open the History menu once or grant Accessibility permission".to_owned()),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(parse_button_entries(&text))
}

/// Invoke one exact restore entry on macOS by pressing its accessibility
/// button. The caller has already matched the entry uniquely.
#[cfg(target_os = "macos")]
pub fn macos_restore(entry: &RestoreEntry) -> Result<(), RestoreError> {
    let Some(title) = &entry.title else {
        return Err(refuse(
            "invalid_input",
            "macOS restore requires the exact entry title",
            None,
        ));
    };
    let control = crate::apple_quote(title);
    let script = format!(
        "tell application \"System Events\"\ntell application process \"Google Chrome\"\nset matches to {{}}\nrepeat with w in windows\nset matches to matches & (every button of w whose name is {control})\nend repeat\nif (count of matches) is not 1 then error \"target_ambiguous\"\nperform action \"AXPress\" of item 1 of matches\nreturn \"pressed\"\nend tell\nend tell"
    );
    let output = crate::run_osascript(&script).map_err(|error| {
        native_unavailable(format!("macOS restore press failed: {error}"), None)
    })?;
    if !output.status.success() {
        return Err(refuse(
            "restore_invoke_failed",
            format!(
                "The exact restore control could not be pressed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            None,
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn macos_entries() -> Result<Vec<RestoreEntry>, RestoreError> {
    Err(macos_unavailable())
}

#[cfg(not(target_os = "macos"))]
pub fn macos_restore(_entry: &RestoreEntry) -> Result<(), RestoreError> {
    Err(macos_unavailable())
}

// Only the non-macOS fallback stubs below need this constructor; on macOS
// the real provider exists and this would otherwise be dead code.
#[cfg(not(target_os = "macos"))]
fn macos_unavailable() -> RestoreError {
    native_unavailable(
        "macOS Accessibility is not available on this platform",
        None,
    )
}

/// Windows: UIA enumeration of Chrome restore entries is not implemented yet.
/// The runtime reports this honestly instead of sending keyboard shortcuts.
pub fn windows_entries() -> Result<Vec<RestoreEntry>, RestoreError> {
    Err(native_unavailable(
        "Windows UIA enumeration of Chrome recently-closed entries is not implemented in this build",
        Some("Use mode native_then_reconstruct or reconstruct_only".to_owned()),
    ))
}

pub fn windows_restore(_entry: &RestoreEntry) -> Result<(), RestoreError> {
    Err(native_unavailable(
        "Windows UIA restore invocation is not implemented in this build",
        None,
    ))
}

/// Linux: AT-SPI enumeration of Chrome restore entries is not implemented yet.
pub fn linux_entries() -> Result<Vec<RestoreEntry>, RestoreError> {
    Err(refuse(
        NATIVE_UNAVAILABLE,
        "Linux AT-SPI enumeration of Chrome recently-closed entries is not implemented in this build",
        Some("Use mode native_then_reconstruct or reconstruct_only".to_owned()),
    ))
}

pub fn linux_restore(_entry: &RestoreEntry) -> Result<(), RestoreError> {
    Err(native_unavailable(
        "Linux AT-SPI restore invocation is not implemented in this build",
        None,
    ))
}

/// Parse macOS button names into restore entries. Button names look like
/// "Research group Closed" or "Example Title Closed". This is deliberately
/// conservative: only entries that parse cleanly are returned.
pub fn parse_button_entries(text: &str) -> Vec<RestoreEntry> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.strip_suffix(" Closed"))
        .map(|title| RestoreEntry {
            kind: RestoreKind::Group,
            title: Some(title.to_owned()),
            urls: Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_group_closed_button_names() {
        let entries = parse_button_entries("Research group Closed\nDocs Closed\n\nother");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title.as_deref(), Some("Research group"));
        assert_eq!(entries[0].kind, RestoreKind::Group);
        assert_eq!(entries[1].title.as_deref(), Some("Docs"));
    }

    #[test]
    fn non_macos_providers_report_unavailable_without_dispatch() {
        if cfg!(target_os = "macos") {
            // On macOS the provider is real; skip this negative assertion.
            return;
        }
        let entry = RestoreEntry {
            kind: RestoreKind::Group,
            title: Some("Research".to_owned()),
            urls: vec![],
        };
        assert_eq!(macos_entries().unwrap_err().code(), NATIVE_UNAVAILABLE);
        assert_eq!(
            macos_restore(&entry).unwrap_err().code(),
            NATIVE_UNAVAILABLE
        );
        assert_eq!(windows_entries().unwrap_err().code(), NATIVE_UNAVAILABLE);
        assert_eq!(
            windows_restore(&entry).unwrap_err().code(),
            NATIVE_UNAVAILABLE
        );
        assert_eq!(linux_entries().unwrap_err().code(), NATIVE_UNAVAILABLE);
        assert_eq!(
            linux_restore(&entry).unwrap_err().code(),
            NATIVE_UNAVAILABLE
        );
    }
}
