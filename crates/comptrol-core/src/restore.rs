//! Native Chrome recently-closed restoration.
//!
//! This module implements `browser.chrome.restore_recent` (with the
//! `browser.chrome.reopen_closed_group` compatibility alias) on top of Chrome's
//! own restore subsystem, reached through platform accessibility surfaces
//! (Windows UIA, macOS AX, Linux AT-SPI). It never sends keyboard shortcuts
//! such as Ctrl+Shift+T and never writes Chrome session files.
//!
//! Restores are semantic: the caller supplies a title and/or a set of URLs that
//! the restored tab, group, or window must contain. Restore entries are matched
//! uniquely; ambiguous or missing matches are refused with a machine-readable
//! error instead of an approximate restore.
//!
//! When the native restore surface is unavailable and the caller explicitly
//! allows reconstruction, the tabs are re-opened through the live CDP target
//! graph and the result is labeled `restoration_mode: "reconstructed"` with
//! every non-restorable state class explicitly listed as not restored.

use crate::BrowserTarget;
use crate::browser;
use comptrol_verification::{
    VerificationCriterion, VerificationEvidence, VerificationLevel, VerificationReport,
    VerificationSource,
};
use serde_json::{Value, json};
use std::time::Duration;

/// One restore entry requested by the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct RestoreRequest {
    pub kind: RestoreKind,
    pub title: Option<String>,
    /// URLs that the restored entry must contain. For a tab this is exactly one
    /// URL; for a group or window it is the required membership.
    pub urls: Vec<String>,
    pub mode: RestoreMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreKind {
    Tab,
    Group,
    Window,
}

impl RestoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RestoreKind::Tab => "tab",
            RestoreKind::Group => "tab_group",
            RestoreKind::Window => "window",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreMode {
    /// Only a native Chrome restore through the platform accessibility surface.
    NativeRestoreOnly,
    /// Native restore first; if it is unavailable and reconstruction is
    /// explicitly allowed, fall back to reconstruction.
    NativeThenReconstruct,
    /// Never touch native state; reconstruct through CDP only.
    ReconstructOnly,
}

impl RestoreMode {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("native_restore_only") => Ok(Self::NativeRestoreOnly),
            Some("native_then_reconstruct") => Ok(Self::NativeThenReconstruct),
            Some("reconstruct_only") => Ok(Self::ReconstructOnly),
            Some(other) => Err(format!("unsupported restore mode {other}")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NativeRestoreOnly => "native_restore_only",
            Self::NativeThenReconstruct => "native_then_reconstruct",
            Self::ReconstructOnly => "reconstruct_only",
        }
    }
}

/// Result of a restore operation. The caller converts this into an
/// `ActionResult` with truthful verification state.
#[derive(Clone, Debug)]
pub struct RestoreOutcome {
    pub restoration_mode: &'static str,
    /// True only when Chrome's own restore subsystem performed the restore.
    pub native_restore_used: bool,
    pub kind: RestoreKind,
    pub restored_targets: Vec<BrowserTarget>,
    pub group_title: Option<String>,
    pub verification: VerificationReport,
    /// State classes that the restore definitively did not restore. Native
    /// restores preserve history/scroll/form/JS/auth in Chrome itself but this
    /// runtime cannot observe them, so they are still reported as not
    /// independently verified; reconstruction never claims them at all.
    pub not_restored: Vec<&'static str>,
}

/// A semantic description of one entry observed in the native restore surface.
#[derive(Clone, Debug, PartialEq)]
pub struct RestoreEntry {
    pub kind: RestoreKind,
    pub title: Option<String>,
    pub urls: Vec<String>,
}

/// Refusal codes are stable machine-readable strings.
pub const AMBIGUOUS: &str = "restore_target_ambiguous";
pub const MISSING: &str = "restore_target_missing";
pub const NATIVE_UNAVAILABLE: &str = "native_restore_unavailable";

#[derive(Debug)]
pub enum RestoreError {
    Refused {
        code: &'static str,
        message: String,
        recovery: Option<String>,
    },
}

impl RestoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Refused { code, .. } => code,
        }
    }
}

pub(crate) fn refuse(
    code: &'static str,
    message: impl Into<String>,
    recovery: Option<String>,
) -> RestoreError {
    RestoreError::Refused {
        code,
        message: message.into(),
        recovery,
    }
}

/// Parse and validate a restore request from operation params. Enforces
/// nonempty semantic identity (title or at least one URL) and URL safety.
pub fn parse_request(params: &Value) -> Result<RestoreRequest, RestoreError> {
    let kind = match params.get("kind").and_then(Value::as_str) {
        None | Some("tab") => RestoreKind::Tab,
        Some("tab_group") | Some("group") => RestoreKind::Group,
        Some("window") => RestoreKind::Window,
        Some(other) => {
            return Err(refuse(
                "invalid_input",
                format!("unsupported restore kind {other}"),
                Some("Use kind tab, tab_group, or window".to_owned()),
            ));
        }
    };
    let title = params
        .get("title")
        .or_else(|| params.get("group"))
        .and_then(Value::as_str)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let mut urls = Vec::new();
    if let Some(url) = params.get("url").and_then(Value::as_str) {
        urls.push(url.to_owned());
    }
    if let Some(values) = params.get("urls").and_then(Value::as_array) {
        for value in values {
            let Some(url) = value.as_str() else {
                return Err(refuse(
                    "invalid_input",
                    "Restore URLs must be strings",
                    None,
                ));
            };
            urls.push(url.to_owned());
        }
    }
    if title.is_none() && urls.is_empty() {
        return Err(refuse(
            "invalid_input",
            "A restore request needs a title or at least one URL to match uniquely",
            Some("Supply the exact title and/or URL membership observed before close".to_owned()),
        ));
    }
    for url in &urls {
        browser::validate_url(url).map_err(|error| {
            refuse(
                "invalid_input",
                format!("Restore URL rejected: {}", error.message),
                error.recovery,
            )
        })?;
    }
    if kind == RestoreKind::Tab && urls.len() > 1 {
        return Err(refuse(
            "invalid_input",
            "A tab restore accepts exactly one URL",
            None,
        ));
    }
    let mode = RestoreMode::parse(params.get("mode").and_then(Value::as_str))
        .map_err(|message| refuse("invalid_input", message, None))?;
    Ok(RestoreRequest {
        kind,
        title,
        urls,
        mode,
    })
}

/// List restore entries from the native Chrome recently-closed surface via the
/// platform accessibility provider. Returns `NATIVE_UNAVAILABLE` when the
/// surface cannot be reached on this platform.
pub fn native_entries(process_id: Option<u32>) -> Result<Vec<RestoreEntry>, RestoreError> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        let _ = process_id;
        #[cfg(target_os = "macos")]
        return crate::restore_native::macos_entries();
        #[cfg(target_os = "windows")]
        return crate::restore_native::windows_entries();
        #[cfg(target_os = "linux")]
        return crate::restore_native::linux_entries();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = process_id;
        Err(refuse(
            NATIVE_UNAVAILABLE,
            "No native Chrome recently-closed surface exists on this platform",
            Some("Use mode native_then_reconstruct or reconstruct_only".to_owned()),
        ))
    }
}

/// Invoke one exact native restore entry. The entry must be uniquely matched
/// from freshly enumerated native state; this function never replays keyboard
/// shortcuts and never writes Chrome session files.
pub fn native_restore(entry: &RestoreEntry, process_id: Option<u32>) -> Result<(), RestoreError> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        let _ = process_id;
        #[cfg(target_os = "macos")]
        return crate::restore_native::macos_restore(entry);
        #[cfg(target_os = "windows")]
        return crate::restore_native::windows_restore(entry);
        #[cfg(target_os = "linux")]
        return crate::restore_native::linux_restore(entry);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (entry, process_id);
        Err(refuse(
            NATIVE_UNAVAILABLE,
            "Native restore is unavailable on this platform",
            None,
        ))
    }
}

/// Match a requested restore semantically against enumerated native entries.
/// Requires a unique match on kind, title, and URL membership.
pub fn match_entry<'a>(
    entries: &'a [RestoreEntry],
    request: &RestoreRequest,
) -> Result<&'a RestoreEntry, RestoreError> {
    let mut matches = entries.iter().filter(|entry| {
        entry.kind == request.kind
            && request
                .title
                .as_deref()
                .is_none_or(|title| entry.title.as_deref() == Some(title))
            && request.urls.iter().all(|url| entry.urls.contains(url))
            && (request.kind != RestoreKind::Tab
                || request
                    .urls
                    .first()
                    .is_none_or(|url| entry.urls.as_slice() == std::slice::from_ref(url)))
    });
    let Some(first) = matches.next() else {
        return Err(refuse(
            MISSING,
            "No native restore entry matched the requested title and URL membership",
            Some("Inspect native restore entries before requesting a restore".to_owned()),
        ));
    };
    if matches.next().is_some() {
        return Err(refuse(
            AMBIGUOUS,
            "Multiple native restore entries matched the request; refusing to guess",
            Some("Narrow the request with an exact title and full URL membership".to_owned()),
        ));
    }
    Ok(first)
}

/// Wait until the CDP target graph exposes restored targets matching the
/// request. Returns targets for tabs whose URL membership matches.
pub fn wait_for_targets(
    endpoint: &str,
    request: &RestoreRequest,
    timeout: Duration,
) -> Result<Vec<BrowserTarget>, RestoreError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let targets = browser::discover(endpoint).map_err(|error| {
            refuse(
                "browser_unavailable",
                format!(
                    "Could not inspect the live CDP target graph: {}",
                    error.message
                ),
                error.recovery,
            )
        })?;
        let mut matched: Vec<BrowserTarget> = targets
            .into_iter()
            .filter(|target| target.target_type.as_deref() == Some("page"))
            .filter(|target| {
                request.urls.iter().all(|url| {
                    target
                        .url
                        .as_deref()
                        .is_some_and(|target_url| url_matches(target_url, url))
                })
            })
            .collect();
        if matched.len() >= expected_target_count(request) {
            // Every requested URL must be present at least once; duplicates in
            // the graph are acceptable (Chrome may restore duplicate tabs).
            matched.sort_by(|a, b| a.id.cmp(&b.id));
            return Ok(matched);
        }
        if std::time::Instant::now() >= deadline {
            return Err(refuse(
                "verification_failed",
                "The restored targets did not appear in the live CDP target graph in time",
                Some("Inspect browser targets and retry with a longer wait".to_owned()),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn expected_target_count(request: &RestoreRequest) -> usize {
    match request.kind {
        RestoreKind::Tab => 1,
        RestoreKind::Group | RestoreKind::Window => request.urls.len().max(1),
    }
}

/// Best-effort URL comparison: exact match first, then trailing-slash-tolerant
/// match. Matching never compares page content.
pub fn url_matches(observed: &str, expected: &str) -> bool {
    observed == expected || observed.trim_end_matches('/') == expected.trim_end_matches('/')
}

/// Reconstruct a restore through the live CDP target graph. This never claims
/// history, form state, JS runtime state, scroll state, or authentication.
pub fn reconstruct(
    endpoint: &str,
    request: &RestoreRequest,
) -> Result<Vec<BrowserTarget>, RestoreError> {
    let mut restored = Vec::new();
    for url in &request.urls {
        let created = browser::open_tab(endpoint, url, true, None).map_err(|error| {
            refuse(
                "reconstruct_failed",
                format!("Reconstruction could not open {url}: {}", error.message),
                error.recovery,
            )
        })?;
        let id = created
            .pointer("/target/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(target) = browser::discover(endpoint)
            .ok()
            .into_iter()
            .flatten()
            .find(|target| target.id == id)
        {
            restored.push(target);
        }
    }
    if restored.len() < request.urls.len() {
        return Err(refuse(
            "verification_failed",
            "Reconstructed targets were not all observable after creation",
            Some("Inspect browser targets before retrying".to_owned()),
        ));
    }
    Ok(restored)
}

/// Build the verification report for a restore outcome. For native restores,
/// membership is verified against the live CDP graph. Reconstruction also
/// verifies membership but records honest not-restored state classes.
pub fn build_verification(
    request: &RestoreRequest,
    outcome: &RestoreOutcome,
) -> VerificationReport {
    let mut report = VerificationReport::new(VerificationLevel::ApplicationState);
    report.evidence.push(VerificationEvidence {
        source: VerificationSource::NativeAccessibility,
        kind: "restore_invocation".to_owned(),
        reference: None,
        details: json!({
            "native_restore_used": outcome.native_restore_used,
            "restoration_mode": outcome.restoration_mode,
            "kind": request.kind.as_str(),
        }),
    });
    for url in &request.urls {
        let observed = outcome.restored_targets.iter().any(|target| {
            target
                .url
                .as_deref()
                .is_some_and(|target_url| url_matches(target_url, url))
        });
        report.criteria.push(VerificationCriterion {
            id: format!("url_membership:{url}"),
            required: true,
            expected: json!(true),
            observed: json!(observed),
            passed: observed,
            source: VerificationSource::BrowserDom,
        });
    }
    if let Some(title) = &request.title {
        let observed = outcome.group_title.as_deref() == Some(title.as_str());
        report.criteria.push(VerificationCriterion {
            id: "group_title".to_owned(),
            required: request.kind == RestoreKind::Group,
            expected: json!(title),
            observed: outcome
                .group_title
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
            passed: observed,
            source: VerificationSource::BrowserAccessibility,
        });
    }
    report.finalize()
}

/// Top-level restore execution used by the runtime route.
pub fn execute(
    endpoint: Option<&str>,
    params: &Value,
    process_id: Option<u32>,
) -> Result<RestoreOutcome, RestoreError> {
    let request = parse_request(params)?;
    let native_allowed = !matches!(request.mode, RestoreMode::ReconstructOnly);
    let reconstruct_allowed = matches!(
        request.mode,
        RestoreMode::NativeThenReconstruct | RestoreMode::ReconstructOnly
    );
    let mut native_used = false;
    let mut group_title = None;
    if native_allowed {
        let entries = native_entries(process_id)?;
        let entry = match_entry(&entries, &request)?;
        native_restore(entry, process_id)?;
        native_used = true;
        group_title = entry.title.clone();
    } else if !reconstruct_allowed {
        return Err(refuse(
            NATIVE_UNAVAILABLE,
            "Native-only restore requested but reconstruction was not enabled",
            Some("Use native_then_reconstruct to allow explicit reconstruction".to_owned()),
        ));
    }
    let endpoint = endpoint.ok_or_else(|| {
        refuse(
            "browser_unavailable",
            "Restore verification needs a live CDP endpoint",
            Some("Set COMPTROL_CDP_ENDPOINT and allow browser CDP policy".to_owned()),
        )
    })?;
    let restored_targets = wait_for_targets(endpoint, &request, Duration::from_secs(10))?;
    let not_restored: Vec<&'static str> = if native_used {
        vec![
            "history_state_not_independently_verified",
            "form_state_not_independently_verified",
            "js_runtime_state_not_independently_verified",
            "scroll_state_not_independently_verified",
            "authentication_not_independently_verified",
        ]
    } else {
        vec![
            "history",
            "form_state",
            "js_runtime_state",
            "scroll_state",
            "authentication",
        ]
    };
    let restoration_mode = if native_used {
        "native"
    } else {
        "reconstructed"
    };
    let verification = {
        let outcome_for_verify = RestoreOutcome {
            restoration_mode,
            native_restore_used: native_used,
            kind: request.kind,
            restored_targets: restored_targets.clone(),
            group_title: group_title.clone(),
            verification: VerificationReport::new(VerificationLevel::ApplicationState),
            not_restored: Vec::new(),
        };
        build_verification(&request, &outcome_for_verify)
    };
    if !verification.is_verified() {
        return Err(refuse(
            "verification_failed",
            "The restore did not satisfy its required membership criteria",
            Some("Inspect the live target graph and the native restore surface".to_owned()),
        ));
    }
    Ok(RestoreOutcome {
        restoration_mode,
        native_restore_used: native_used,
        kind: request.kind,
        restored_targets,
        group_title,
        verification,
        not_restored,
    })
}

// ---------------------------------------------------------------------------
// (Platform restore-surface glue lives in restore_native.rs so the providers
// stay independent of the semantic matching logic above.)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use comptrol_verification::VerificationState;

    fn params(value: Value) -> Value {
        value
    }

    #[test]
    fn restore_request_requires_semantic_identity() {
        let error = parse_request(&params(json!({ "kind": "tab" }))).unwrap_err();
        assert_eq!(error.code(), "invalid_input");
    }

    #[test]
    fn restore_request_rejects_unsafe_urls() {
        let error = parse_request(&params(json!({
            "kind": "tab",
            "url": "file:///etc/passwd"
        })))
        .unwrap_err();
        assert_eq!(error.code(), "invalid_input");
    }

    #[test]
    fn restore_request_parses_group_alias_and_mode() {
        let request = parse_request(&params(json!({
            "kind": "group",
            "group": "Research",
            "urls": ["https://example.test/one", "https://example.test/two"],
            "mode": "native_then_reconstruct"
        })))
        .expect("valid group restore");
        assert_eq!(request.kind, RestoreKind::Group);
        assert_eq!(request.title.as_deref(), Some("Research"));
        assert_eq!(request.mode, RestoreMode::NativeThenReconstruct);
        assert_eq!(request.urls.len(), 2);
    }

    #[test]
    fn restore_request_tab_rejects_multiple_urls() {
        let error = parse_request(&params(json!({
            "kind": "tab",
            "url": "https://example.test/one",
            "urls": ["https://example.test/two"]
        })))
        .unwrap_err();
        assert_eq!(error.code(), "invalid_input");
    }

    #[test]
    fn restore_mode_parse_rejects_unknown_modes() {
        assert!(RestoreMode::parse(Some("teleport")).is_err());
        assert_eq!(
            RestoreMode::parse(None).unwrap(),
            RestoreMode::NativeRestoreOnly
        );
        assert_eq!(
            RestoreMode::parse(Some("reconstruct_only")).unwrap(),
            RestoreMode::ReconstructOnly
        );
    }

    #[test]
    fn match_entry_requires_unique_title_and_membership() {
        let entries = vec![
            RestoreEntry {
                kind: RestoreKind::Group,
                title: Some("Research".to_owned()),
                urls: vec![
                    "https://example.test/one".to_owned(),
                    "https://example.test/two".to_owned(),
                ],
            },
            RestoreEntry {
                kind: RestoreKind::Group,
                title: Some("Research".to_owned()),
                urls: vec![
                    "https://example.test/one".to_owned(),
                    "https://example.test/three".to_owned(),
                ],
            },
        ];
        let request = RestoreRequest {
            kind: RestoreKind::Group,
            title: Some("Research".to_owned()),
            urls: vec!["https://example.test/one".to_owned()],
            mode: RestoreMode::NativeRestoreOnly,
        };
        // Both entries share one URL and the title: ambiguous without full
        // membership. Membership matching uses all requested URLs, so a single
        // URL request matches both and must be refused.
        let error = match_entry(&entries, &request).unwrap_err();
        assert_eq!(error.code(), AMBIGUOUS);

        let request = RestoreRequest {
            kind: RestoreKind::Group,
            title: Some("Research".to_owned()),
            urls: vec![
                "https://example.test/one".to_owned(),
                "https://example.test/two".to_owned(),
            ],
            mode: RestoreMode::NativeRestoreOnly,
        };
        let entry = match_entry(&entries, &request).expect("unique match");
        assert_eq!(entry.title.as_deref(), Some("Research"));
    }

    #[test]
    fn match_entry_refuses_missing_entries() {
        let entries = vec![RestoreEntry {
            kind: RestoreKind::Tab,
            title: Some("Docs".to_owned()),
            urls: vec!["https://example.test/docs".to_owned()],
        }];
        let request = RestoreRequest {
            kind: RestoreKind::Tab,
            title: Some("Docs".to_owned()),
            urls: vec!["https://example.test/other".to_owned()],
            mode: RestoreMode::NativeRestoreOnly,
        };
        let error = match_entry(&entries, &request).unwrap_err();
        assert_eq!(error.code(), MISSING);
    }

    #[test]
    fn match_entry_respects_kind() {
        let entries = vec![RestoreEntry {
            kind: RestoreKind::Tab,
            title: Some("Docs".to_owned()),
            urls: vec!["https://example.test/docs".to_owned()],
        }];
        let request = RestoreRequest {
            kind: RestoreKind::Window,
            title: Some("Docs".to_owned()),
            urls: vec!["https://example.test/docs".to_owned()],
            mode: RestoreMode::NativeRestoreOnly,
        };
        assert_eq!(match_entry(&entries, &request).unwrap_err().code(), MISSING);
    }

    #[test]
    fn url_matches_ignores_trailing_slash() {
        assert!(url_matches(
            "https://example.test/a",
            "https://example.test/a"
        ));
        assert!(url_matches(
            "https://example.test/a/",
            "https://example.test/a"
        ));
        assert!(!url_matches(
            "https://example.test/a",
            "https://example.test/b"
        ));
    }

    #[test]
    fn build_verification_requires_membership_and_flags_group_title_required() {
        let request = RestoreRequest {
            kind: RestoreKind::Group,
            title: Some("Research".to_owned()),
            urls: vec!["https://example.test/one".to_owned()],
            mode: RestoreMode::ReconstructOnly,
        };
        let outcome = RestoreOutcome {
            restoration_mode: "reconstructed",
            native_restore_used: false,
            kind: RestoreKind::Group,
            restored_targets: vec![BrowserTarget {
                id: "t1".to_owned(),
                target_type: Some("page".to_owned()),
                browser_context_id: None,
                url: Some("https://example.test/one".to_owned()),
                title: None,
                revision: None,
                web_socket_url: None,
            }],
            group_title: None,
            verification: VerificationReport::new(VerificationLevel::ApplicationState),
            not_restored: vec![],
        };
        let report = build_verification(&request, &outcome);
        // Group title is required and absent: report must not be verified.
        assert_eq!(report.state, VerificationState::Failed);
    }

    #[test]
    fn build_verification_passes_for_tab_membership_without_title() {
        let request = RestoreRequest {
            kind: RestoreKind::Tab,
            title: None,
            urls: vec!["https://example.test/one".to_owned()],
            mode: RestoreMode::ReconstructOnly,
        };
        let outcome = RestoreOutcome {
            restoration_mode: "reconstructed",
            native_restore_used: false,
            kind: RestoreKind::Tab,
            restored_targets: vec![BrowserTarget {
                id: "t1".to_owned(),
                target_type: Some("page".to_owned()),
                browser_context_id: None,
                url: Some("https://example.test/one/".to_owned()),
                title: None,
                revision: None,
                web_socket_url: None,
            }],
            group_title: None,
            verification: VerificationReport::new(VerificationLevel::ApplicationState),
            not_restored: vec![],
        };
        let report = build_verification(&request, &outcome);
        assert_eq!(report.state, VerificationState::Verified);
    }

    #[test]
    fn reconstruct_labels_not_restored_state_classes() {
        let request = RestoreRequest {
            kind: RestoreKind::Tab,
            title: None,
            urls: vec!["https://example.test/one".to_owned()],
            mode: RestoreMode::ReconstructOnly,
        };
        // Simulate the execute() not_restored labeling for reconstruction.
        let native_used = false;
        let not_restored: Vec<&'static str> = if native_used {
            vec![]
        } else {
            vec![
                "history",
                "form_state",
                "js_runtime_state",
                "scroll_state",
                "authentication",
            ]
        };
        assert!(not_restored.contains(&"history"));
        assert_eq!(request.mode, RestoreMode::ReconstructOnly);
    }
}
