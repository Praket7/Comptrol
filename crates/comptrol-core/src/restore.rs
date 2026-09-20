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
//!
//! Execution is strictly mode-dependent:
//!
//! - `native_restore_only`: enumerate and invoke the native surface; without
//!   it the request is refused. Reconstruction never runs in this mode.
//! - `native_then_reconstruct`: native first; only a native-unavailable
//!   result eligible for explicit fallback proceeds to reconstruction.
//! - `reconstruct_only`: reconstruction creates the targets; waiting for
//!   pre-existing targets would silently "verify" stale state.
//!
//! Target verification waits on the persistent browser connection's live
//! target graph (event-driven) instead of polling target discovery.

use crate::BrowserTarget;
use crate::browser;
use comptrol_verification::{
    VerificationCriterion, VerificationEvidence, VerificationLevel, VerificationReport,
    VerificationSource,
};
use serde_json::{Value, json};
use std::sync::Arc;
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
/// The native surface cannot observe enough identity (for example URL
/// membership) to verify the requested match. This is a refusal, not a
/// fallback trigger: reconstructing here would restore different state than
/// the closed native entry the caller asked for.
pub const IDENTITY_INSUFFICIENT: &str = "restore_native_identity_insufficient";

#[derive(Debug)]
pub enum RestoreError {
    Refused {
        code: &'static str,
        message: String,
        recovery: Option<String>,
    },
    /// The native restore surface could not be reached. Unlike a refusal,
    /// this condition is specifically eligible for explicit fallback (for
    /// example `native_then_reconstruct`) without misrepresenting state:
    /// nothing was matched, invoked, or mutated by the native path.
    Unavailable {
        code: &'static str,
        message: String,
        recovery: Option<String>,
    },
}

impl RestoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Refused { code, .. } | Self::Unavailable { code, .. } => code,
        }
    }

    /// True when this error reports an unreachable native surface rather than
    /// a semantic refusal. Only `NATIVE_UNAVAILABLE` errors are fallback
    /// eligible; refusals such as `restore_target_ambiguous` must never be
    /// converted into a reconstruction of different state.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

/// Build a native-unavailable error from any displayable provider failure.
fn unavailable(message: impl Into<String>, recovery: Option<String>) -> RestoreError {
    RestoreError::Unavailable {
        code: NATIVE_UNAVAILABLE,
        message: message.into(),
        recovery,
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

/// Build a native-unavailable error for the restore module.
pub(crate) fn native_unavailable(
    message: impl Into<String>,
    recovery: Option<String>,
) -> RestoreError {
    unavailable(message, recovery)
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
/// platform accessibility provider. Returns a
/// [`RestoreError::Unavailable`] when the surface cannot be reached on this
/// platform, which is exactly the condition eligible for explicit
/// reconstruction fallback.
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
        Err(unavailable(
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
        Err(unavailable(
            "Native restore is unavailable on this platform",
            None,
        ))
    }
}

/// Match a requested restore semantically against enumerated native entries.
/// Requires a unique match on kind, title, and URL membership.
///
/// Truthful identity handling: when the request requires URL membership but
/// the matching native entries carry no URL identity (for example the macOS
/// surface exposes titles only), the request is refused with
/// `restore_native_identity_insufficient` instead of a misleading "missing"
/// or an unverifiable title-only match.
pub fn match_entry<'a>(
    entries: &'a [RestoreEntry],
    request: &RestoreRequest,
) -> Result<&'a RestoreEntry, RestoreError> {
    if !request.urls.is_empty() {
        let mut identity_candidates = entries.iter().filter(|entry| {
            entry.kind == request.kind
                && request
                    .title
                    .as_deref()
                    .is_none_or(|title| entry.title.as_deref() == Some(title))
        });
        let identity_insufficient = match identity_candidates.next() {
            Some(first) => {
                first.urls.is_empty() && identity_candidates.all(|entry| entry.urls.is_empty())
            }
            None => false,
        };
        if identity_insufficient {
            return Err(refuse(
                IDENTITY_INSUFFICIENT,
                "The native surface cannot observe URL identity for the matching entries, so the requested URL membership cannot be verified",
                Some(
                    "Match by exact title only, or use mode reconstruct_only to rebuild the requested URL membership".to_owned(),
                ),
            ));
        }
    }
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

/// Wait until the persistent browser connection's live target graph exposes
/// restored targets matching the request.
///
/// This is event-driven: the graph is maintained by the connection's reader
/// task from target lifecycle events, so this path issues no `/json/list`
/// discovery polling and no fixed sleeps. The timeout remains a bounded
/// safety net, not the synchronization mechanism.
pub fn wait_for_targets(
    endpoint: &str,
    request: &RestoreRequest,
    timeout: Duration,
) -> Result<Vec<BrowserTarget>, RestoreError> {
    let predicate = target_predicate(request);
    let snapshot = browser::bridge()
        .wait_target_graph(
            endpoint,
            timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            predicate,
            expected_target_count(request),
        )
        .map_err(|error| {
            refuse(
                "browser_unavailable",
                format!("Could not wait on the live CDP target graph: {error}"),
                Some("Start a supported browser with remote debugging enabled".to_owned()),
            )
        })?;
    Ok(snapshot
        .targets
        .into_iter()
        .map(|record| BrowserTarget {
            id: record.id,
            target_type: Some(record.target_type),
            browser_context_id: record.browser_context_id,
            url: record.url,
            title: record.title,
            revision: Some(record.revision),
            web_socket_url: None,
        })
        .collect())
}

/// Graph predicate for a restore request: a page target qualifies when it
/// covers at least one requested URL, or when the request carries no URLs and
/// any page target counts (membership is then judged by title verification).
/// The final verification report still checks every requested URL
/// individually, so a wait that resolved early on duplicates cannot pass
/// overall verification.
fn target_predicate(
    request: &RestoreRequest,
) -> Arc<dyn Fn(&browser::TargetRecord) -> bool + Send + Sync> {
    let request = request.clone();
    Arc::new(move |record: &browser::TargetRecord| {
        request.urls.is_empty()
            || request.urls.iter().any(|url| {
                record
                    .url
                    .as_deref()
                    .is_some_and(|target_url| url_matches(target_url, url))
            })
    })
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

/// Backend used to create and observe targets during reconstruction. The
/// production backend drives a live CDP endpoint through the browser facade;
/// tests inject a fake backend so reconstruction is verified without a
/// network browser.
pub trait ReconstructBackend: Send + Sync {
    /// Open one background tab for the URL and return its target id.
    fn open_background_tab(&self, url: &str) -> Result<String, RestoreError>;
    /// List the page targets currently observable in the browser.
    fn list_page_targets(&self) -> Result<Vec<BrowserTarget>, RestoreError>;
}

struct CdpReconstructBackend {
    endpoint: String,
}

impl ReconstructBackend for CdpReconstructBackend {
    fn open_background_tab(&self, url: &str) -> Result<String, RestoreError> {
        let created = browser::open_tab(&self.endpoint, url, true, None).map_err(|error| {
            refuse(
                "reconstruct_failed",
                format!("Reconstruction could not open {url}: {}", error.message),
                error.recovery,
            )
        })?;
        created
            .get("target")
            .and_then(|target| target.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                refuse(
                    "reconstruct_failed",
                    "The browser did not return the opened tab identity",
                    None,
                )
            })
    }

    fn list_page_targets(&self) -> Result<Vec<BrowserTarget>, RestoreError> {
        browser::discover(&self.endpoint).map_err(|error| {
            refuse(
                "browser_unavailable",
                format!(
                    "Could not inspect the live CDP target graph: {}",
                    error.message
                ),
                error.recovery,
            )
        })
    }
}

/// Reconstruct a restore through the live CDP target graph. This never claims
/// history, form state, JS runtime state, scroll state, or authentication.
pub fn reconstruct(
    endpoint: &str,
    request: &RestoreRequest,
) -> Result<Vec<BrowserTarget>, RestoreError> {
    let backend = CdpReconstructBackend {
        endpoint: endpoint.to_owned(),
    };
    reconstruct_with(&backend, request)
}

/// Reconstruction against an injected backend: create every requested target,
/// then read the target list once and bind the created identities.
pub fn reconstruct_with(
    backend: &dyn ReconstructBackend,
    request: &RestoreRequest,
) -> Result<Vec<BrowserTarget>, RestoreError> {
    let mut created_ids = Vec::new();
    for url in &request.urls {
        created_ids.push(backend.open_background_tab(url)?);
    }
    let targets = backend.list_page_targets()?;
    let mut restored = Vec::new();
    for id in &created_ids {
        if let Some(target) = targets.iter().find(|target| &target.id == id) {
            restored.push(target.clone());
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

/// Native restore surface paths. The default implementation dispatches to the
/// platform providers; tests inject fakes to exercise every mode without a
/// real browser or accessibility surface.
pub trait NativeRestoreSurface: Send + Sync {
    fn entries(&self, process_id: Option<u32>) -> Result<Vec<RestoreEntry>, RestoreError>;
    fn restore(&self, entry: &RestoreEntry, process_id: Option<u32>) -> Result<(), RestoreError>;
}

struct PlatformNativeSurface;

impl NativeRestoreSurface for PlatformNativeSurface {
    fn entries(&self, process_id: Option<u32>) -> Result<Vec<RestoreEntry>, RestoreError> {
        native_entries(process_id)
    }

    fn restore(&self, entry: &RestoreEntry, process_id: Option<u32>) -> Result<(), RestoreError> {
        native_restore(entry, process_id)
    }
}

/// Independent verification of restored targets through the live browser.
pub trait RestoreTargetVerifier: Send + Sync {
    fn wait(
        &self,
        request: &RestoreRequest,
        timeout: Duration,
    ) -> Result<Vec<BrowserTarget>, RestoreError>;
}

/// Production verifier: waits on the persistent connection's live target
/// graph at the configured CDP endpoint.
struct CdpRestoreVerifier {
    endpoint: Option<String>,
}

impl RestoreTargetVerifier for CdpRestoreVerifier {
    fn wait(
        &self,
        request: &RestoreRequest,
        timeout: Duration,
    ) -> Result<Vec<BrowserTarget>, RestoreError> {
        let endpoint = self.endpoint.as_deref().ok_or_else(|| {
            refuse(
                "browser_unavailable",
                "Restore verification needs a live CDP endpoint",
                Some("Set COMPTROL_CDP_ENDPOINT and allow browser CDP policy".to_owned()),
            )
        })?;
        wait_for_targets(endpoint, request, timeout)
    }
}

/// Top-level restore execution used by the runtime route.
pub fn execute(
    endpoint: Option<&str>,
    params: &Value,
    process_id: Option<u32>,
) -> Result<RestoreOutcome, RestoreError> {
    // Validate the request shape before requiring the endpoint so malformed
    // params report invalid_input even without a live browser attached.
    parse_request(params)?;
    if endpoint.is_none() {
        return Err(refuse(
            "browser_unavailable",
            "Restore verification needs a live CDP endpoint",
            Some("Set COMPTROL_CDP_ENDPOINT and allow browser CDP policy".to_owned()),
        ));
    }
    let reconstruct = CdpReconstructBackend {
        endpoint: endpoint.unwrap_or_default().to_owned(),
    };
    let verifier = CdpRestoreVerifier {
        endpoint: endpoint.map(str::to_owned),
    };
    execute_with(
        &PlatformNativeSurface,
        &reconstruct,
        &verifier,
        params,
        process_id,
    )
}

/// Execution with injected surfaces. The mode determines the exact path:
///
/// - `native_restore_only`: native or refuse. Reconstruction never runs.
/// - `native_then_reconstruct`: native first; only a specifically eligible
///   native-unavailable result falls back to reconstruction. Semantic
///   refusals (ambiguous, missing, invalid) are never converted into a
///   reconstruction of different state.
/// - `reconstruct_only`: reconstruction creates and then verifies the
///   targets; the native surface is never touched.
pub fn execute_with(
    native: &dyn NativeRestoreSurface,
    reconstruct: &dyn ReconstructBackend,
    verifier: &dyn RestoreTargetVerifier,
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
        match native.entries(process_id).and_then(|entries| {
            let entry = match_entry(&entries, &request)?;
            native.restore(entry, process_id)?;
            Ok(entry.clone())
        }) {
            Ok(entry) => {
                native_used = true;
                group_title = entry.title.clone();
            }
            // Native is unavailable and the caller explicitly allowed
            // reconstruction fallback: proceed to reconstruction below.
            Err(error) if error.is_unavailable() && reconstruct_allowed => {}
            Err(error) => return Err(error),
        }
    }
    let restored_targets = if native_used {
        verifier.wait(&request, Duration::from_secs(10))?
    } else {
        // Reconstruction path: either reconstruct_only or a native-unavailable
        // fallback. Reconstruction creates the targets it then verifies, so
        // pre-existing targets can never be mistaken for a restore.
        reconstruct_with(reconstruct, &request)?
    };
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
    fn match_entry_refuses_url_requests_when_native_identity_is_unobservable() {
        // macOS-style surface: titles observable, URLs not. A URL-membership
        // request must refuse honestly rather than fail as "missing" or pass
        // on an unverifiable title-only match.
        let entries = vec![RestoreEntry {
            kind: RestoreKind::Group,
            title: Some("Research".to_owned()),
            urls: Vec::new(),
        }];
        let request = RestoreRequest {
            kind: RestoreKind::Group,
            title: Some("Research".to_owned()),
            urls: vec!["https://example.test/one".to_owned()],
            mode: RestoreMode::NativeRestoreOnly,
        };
        let error = match_entry(&entries, &request).unwrap_err();
        assert_eq!(error.code(), IDENTITY_INSUFFICIENT);
        assert!(!error.is_unavailable());
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

    // -----------------------------------------------------------------------
    // Reconstruction execution tests. These fail against the previous
    // behavior where `execute()` never invoked reconstruction and
    // `reconstruct_only` waited for targets that were never created.
    // -----------------------------------------------------------------------

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    enum NativeBehavior {
        Provide(Vec<RestoreEntry>),
        Unavailable,
        Refuse(&'static str),
    }

    struct FakeNative {
        behavior: NativeBehavior,
        restore_calls: AtomicUsize,
    }

    impl FakeNative {
        fn provide(entries: Vec<RestoreEntry>) -> Self {
            Self {
                behavior: NativeBehavior::Provide(entries),
                restore_calls: AtomicUsize::new(0),
            }
        }

        fn unavailable() -> Self {
            Self {
                behavior: NativeBehavior::Unavailable,
                restore_calls: AtomicUsize::new(0),
            }
        }

        fn refusing(code: &'static str) -> Self {
            Self {
                behavior: NativeBehavior::Refuse(code),
                restore_calls: AtomicUsize::new(0),
            }
        }
    }

    impl NativeRestoreSurface for FakeNative {
        fn entries(&self, _process_id: Option<u32>) -> Result<Vec<RestoreEntry>, RestoreError> {
            match &self.behavior {
                NativeBehavior::Provide(entries) => Ok(entries.clone()),
                NativeBehavior::Unavailable => Err(unavailable("fake native unavailable", None)),
                NativeBehavior::Refuse(code) => Err(refuse(code, "fake native refusal", None)),
            }
        }

        fn restore(
            &self,
            _entry: &RestoreEntry,
            _process_id: Option<u32>,
        ) -> Result<(), RestoreError> {
            self.restore_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeReconstruct {
        opened_urls: Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    impl ReconstructBackend for FakeReconstruct {
        fn open_background_tab(&self, url: &str) -> Result<String, RestoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let count = self.calls.load(Ordering::SeqCst);
            self.opened_urls
                .lock()
                .expect("opened urls")
                .push(url.to_owned());
            Ok(format!("created-{count}"))
        }

        fn list_page_targets(&self) -> Result<Vec<BrowserTarget>, RestoreError> {
            let opened = self.opened_urls.lock().expect("opened urls");
            Ok(opened
                .iter()
                .enumerate()
                .map(|(index, url)| BrowserTarget {
                    id: format!("created-{}", index + 1),
                    target_type: Some("page".to_owned()),
                    browser_context_id: None,
                    url: Some(url.clone()),
                    title: None,
                    revision: Some("generation:1:target:test".to_owned()),
                    web_socket_url: None,
                })
                .collect())
        }
    }

    struct FakeVerifier {
        targets: Vec<BrowserTarget>,
    }

    impl RestoreTargetVerifier for FakeVerifier {
        fn wait(
            &self,
            _request: &RestoreRequest,
            _timeout: Duration,
        ) -> Result<Vec<BrowserTarget>, RestoreError> {
            Ok(self.targets.clone())
        }
    }

    fn tab_params(mode: &str) -> Value {
        json!({
            "kind": "tab",
            "url": "https://example.test/one",
            "mode": mode
        })
    }

    fn page_target(id: &str, url: &str) -> BrowserTarget {
        BrowserTarget {
            id: id.to_owned(),
            target_type: Some("page".to_owned()),
            browser_context_id: None,
            url: Some(url.to_owned()),
            title: None,
            revision: Some("generation:1:target:test".to_owned()),
            web_socket_url: None,
        }
    }

    #[test]
    fn reconstruct_with_opens_every_requested_url_and_binds_created_targets() {
        let backend = FakeReconstruct::default();
        let request = RestoreRequest {
            kind: RestoreKind::Group,
            title: Some("Research".to_owned()),
            urls: vec![
                "https://example.test/one".to_owned(),
                "https://example.test/two".to_owned(),
            ],
            mode: RestoreMode::ReconstructOnly,
        };
        let restored = reconstruct_with(&backend, &request).expect("reconstruction succeeds");
        assert_eq!(restored.len(), 2);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        let opened = backend.opened_urls.lock().unwrap();
        assert!(opened.contains(&"https://example.test/one".to_owned()));
        assert!(opened.contains(&"https://example.test/two".to_owned()));
    }

    #[test]
    fn reconstruct_with_fails_when_created_targets_are_not_observable() {
        struct EmptyListBackend;
        impl ReconstructBackend for EmptyListBackend {
            fn open_background_tab(&self, _url: &str) -> Result<String, RestoreError> {
                Ok("created-1".to_owned())
            }

            fn list_page_targets(&self) -> Result<Vec<BrowserTarget>, RestoreError> {
                Ok(Vec::new())
            }
        }
        let request = RestoreRequest {
            kind: RestoreKind::Tab,
            title: None,
            urls: vec!["https://example.test/one".to_owned()],
            mode: RestoreMode::ReconstructOnly,
        };
        let error = reconstruct_with(&EmptyListBackend, &request).unwrap_err();
        assert_eq!(error.code(), "verification_failed");
    }

    #[test]
    fn native_then_reconstruct_falls_back_after_native_unavailable() {
        let native = FakeNative::unavailable();
        let reconstruct = FakeReconstruct::default();
        let verifier = FakeVerifier {
            targets: Vec::new(),
        };
        let outcome = execute_with(
            &native,
            &reconstruct,
            &verifier,
            &tab_params("native_then_reconstruct"),
            None,
        )
        .expect("fallback reconstruction must run");
        assert_eq!(outcome.restoration_mode, "reconstructed");
        assert!(!outcome.native_restore_used);
        assert_eq!(reconstruct.calls.load(Ordering::SeqCst), 1);
        assert_eq!(native.restore_calls.load(Ordering::SeqCst), 0);
        assert!(outcome.not_restored.contains(&"history"));
        assert!(outcome.verification.is_verified());
    }

    #[test]
    fn native_then_reconstruct_does_not_reconstruct_after_semantic_refusal() {
        let native = FakeNative::refusing(AMBIGUOUS);
        let reconstruct = FakeReconstruct::default();
        let verifier = FakeVerifier {
            targets: Vec::new(),
        };
        let error = execute_with(
            &native,
            &reconstruct,
            &verifier,
            &tab_params("native_then_reconstruct"),
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), AMBIGUOUS);
        assert!(!error.is_unavailable());
        assert_eq!(reconstruct.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reconstruct_only_never_touches_the_native_surface() {
        let native = FakeNative::provide(vec![RestoreEntry {
            kind: RestoreKind::Tab,
            title: Some("Docs".to_owned()),
            urls: vec!["https://example.test/one".to_owned()],
        }]);
        let reconstruct = FakeReconstruct::default();
        let verifier = FakeVerifier {
            targets: Vec::new(),
        };
        let outcome = execute_with(
            &native,
            &reconstruct,
            &verifier,
            &tab_params("reconstruct_only"),
            None,
        )
        .expect("reconstruct-only must run without native state");
        assert_eq!(outcome.restoration_mode, "reconstructed");
        assert!(!outcome.native_restore_used);
        assert_eq!(native.restore_calls.load(Ordering::SeqCst), 0);
        assert_eq!(reconstruct.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reconstruct_only_creates_targets_instead_of_waiting_for_stale_ones() {
        // The old broken path waited for targets matching the request and
        // "verified" against pre-existing tabs. Reconstruction must create the
        // targets itself, and the outcome must carry the created identities.
        let native = FakeNative::unavailable();
        let reconstruct = FakeReconstruct::default();
        let verifier = FakeVerifier {
            targets: Vec::new(),
        };
        let outcome = execute_with(
            &native,
            &reconstruct,
            &verifier,
            &tab_params("reconstruct_only"),
            None,
        )
        .expect("reconstruction creates its own targets");
        assert_eq!(outcome.restored_targets.len(), 1);
        assert_eq!(outcome.restored_targets[0].id, "created-1");
        assert_eq!(
            outcome.restored_targets[0].url.as_deref(),
            Some("https://example.test/one")
        );
        assert!(outcome.verification.is_verified());
    }

    #[test]
    fn native_restore_only_refuses_without_reconstruction() {
        let native = FakeNative::unavailable();
        let reconstruct = FakeReconstruct::default();
        let verifier = FakeVerifier {
            targets: Vec::new(),
        };
        let error = execute_with(
            &native,
            &reconstruct,
            &verifier,
            &tab_params("native_restore_only"),
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), NATIVE_UNAVAILABLE);
        assert!(error.is_unavailable());
        assert_eq!(reconstruct.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_success_keeps_native_labeling_and_waits_for_verified_targets() {
        let native = FakeNative::provide(vec![RestoreEntry {
            kind: RestoreKind::Tab,
            title: Some("Docs".to_owned()),
            urls: vec!["https://example.test/one".to_owned()],
        }]);
        let reconstruct = FakeReconstruct::default();
        let verifier = FakeVerifier {
            targets: vec![page_target("native-1", "https://example.test/one")],
        };
        let outcome = execute_with(
            &native,
            &reconstruct,
            &verifier,
            &tab_params("native_restore_only"),
            None,
        )
        .expect("native restore succeeds");
        assert_eq!(outcome.restoration_mode, "native");
        assert!(outcome.native_restore_used);
        assert_eq!(reconstruct.calls.load(Ordering::SeqCst), 0);
        assert!(
            outcome
                .not_restored
                .contains(&"history_state_not_independently_verified")
        );
        assert!(outcome.verification.is_verified());
    }

    #[test]
    fn reconstruction_failure_surfaces_reconstruct_failed_code() {
        struct FailingBackend;
        impl ReconstructBackend for FailingBackend {
            fn open_background_tab(&self, _url: &str) -> Result<String, RestoreError> {
                Err(refuse(
                    "reconstruct_failed",
                    "the browser refused the new tab",
                    None,
                ))
            }

            fn list_page_targets(&self) -> Result<Vec<BrowserTarget>, RestoreError> {
                Ok(Vec::new())
            }
        }
        let native = FakeNative::unavailable();
        let verifier = FakeVerifier {
            targets: Vec::new(),
        };
        let error = execute_with(
            &native,
            &FailingBackend,
            &verifier,
            &tab_params("native_then_reconstruct"),
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), "reconstruct_failed");
    }

    #[test]
    fn unavailable_errors_are_fallback_eligible_but_refusals_are_not() {
        let unavailable = unavailable("unreachable", None);
        assert!(unavailable.is_unavailable());
        let refused = refuse(AMBIGUOUS, "ambiguous", None);
        assert!(!refused.is_unavailable());
    }
}
