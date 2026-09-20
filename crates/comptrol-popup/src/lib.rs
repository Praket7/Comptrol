//! Cross-platform popup, modal, and permission-dialog engine.
//!
//! Popups are classified into typed classes before anything is dismissed.
//! Only categories the user's popup policy permits are auto-dismissed.
//! The following are NEVER auto-approved, regardless of policy:
//! system authentication, installer elevation, security warnings,
//! purchases/payments, legal agreements, destructive confirmations,
//! OAuth scopes, extension installation permissions, and ambiguous
//! message recipients.
//!
//! Dismissal prefers semantic Close/Cancel/Not Now actions. A scoped key
//! event is a last resort and only when the app exposes no semantic action.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Typed popup classes from the V5 product contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PopupClass {
    Informational,
    CookieOrSitePreference,
    UpdateAvailable,
    UnsavedChanges,
    FilePicker,
    PermissionRequest,
    BrowserPermission,
    SystemAuthentication,
    InstallerElevation,
    SecurityWarning,
    PurchaseOrPayment,
    LegalAgreement,
    DestructiveConfirmation,
    CrashOrRecovery,
    Unknown,
}

impl PopupClass {
    pub fn as_str(self) -> &'static str {
        match self {
            PopupClass::Informational => "informational",
            PopupClass::CookieOrSitePreference => "cookie_or_site_preference",
            PopupClass::UpdateAvailable => "update_available",
            PopupClass::UnsavedChanges => "unsaved_changes",
            PopupClass::FilePicker => "file_picker",
            PopupClass::PermissionRequest => "permission_request",
            PopupClass::BrowserPermission => "browser_permission",
            PopupClass::SystemAuthentication => "system_authentication",
            PopupClass::InstallerElevation => "installer_elevation",
            PopupClass::SecurityWarning => "security_warning",
            PopupClass::PurchaseOrPayment => "purchase_or_payment",
            PopupClass::LegalAgreement => "legal_agreement",
            PopupClass::DestructiveConfirmation => "destructive_confirmation",
            PopupClass::CrashOrRecovery => "crash_or_recovery",
            PopupClass::Unknown => "unknown",
        }
    }

    /// Classes that are never auto-dismissed, no matter the user policy.
    pub fn never_auto(self) -> bool {
        matches!(
            self,
            PopupClass::SystemAuthentication
                | PopupClass::InstallerElevation
                | PopupClass::SecurityWarning
                | PopupClass::PurchaseOrPayment
                | PopupClass::LegalAgreement
                | PopupClass::DestructiveConfirmation
                | PopupClass::PermissionRequest
                | PopupClass::BrowserPermission
                | PopupClass::Unknown
        )
    }

    /// Classes eligible for auto-dismiss when the user opted in.
    pub fn auto_eligible(self) -> bool {
        matches!(
            self,
            PopupClass::Informational
                | PopupClass::CookieOrSitePreference
                | PopupClass::UpdateAvailable
                | PopupClass::CrashOrRecovery
        )
    }
}

/// One observed popup/modal/dialog.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PopupInfo {
    pub id: String,
    pub class: PopupClass,
    pub title: Option<String>,
    pub target: String,
    pub close_actions: Vec<String>,
    pub observed_at_ms: u128,
}

/// User popup preferences: which eligible categories may auto-dismiss.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PopupPolicy {
    pub dismiss_informational: bool,
    pub dismiss_cookie_banners: bool,
    pub dismiss_update_prompts: bool,
    pub dismiss_tips: bool,
}

impl PopupPolicy {
    /// Conservative default: dismiss nothing automatically.
    pub fn conservative() -> Self {
        Self::default()
    }
}

/// Modal stack per target/process so dismissal can verify the underlying
/// target identity is unchanged afterwards.
#[derive(Clone, Debug, Default)]
pub struct PopupStack {
    entries: HashMap<String, Vec<PopupInfo>>,
}

impl PopupStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, target: &str, popup: PopupInfo) {
        self.entries
            .entry(target.to_owned())
            .or_default()
            .push(popup);
    }

    pub fn top(&self, target: &str) -> Option<&PopupInfo> {
        self.entries.get(target).and_then(|stack| stack.last())
    }

    pub fn pop(&mut self, target: &str) -> Option<PopupInfo> {
        self.entries.get_mut(target).and_then(|stack| stack.pop())
    }

    pub fn depth(&self, target: &str) -> usize {
        self.entries
            .get(target)
            .map(|stack| stack.len())
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PopupError {
    #[error("dismissal of {class} requires explicit user authorization")]
    AuthorizationRequired { class: String },
    #[error("no popup is present on target {target}")]
    NothingToDismiss { target: String },
    #[error("popup dismissal left the target in an unexpected state: {reason}")]
    TargetChanged { reason: String },
}

/// Heuristic classifier over accessible role/name/text signals. Conservative
/// by design: anything unrecognized is [`PopupClass::Unknown`], which is
/// never auto-dismissed.
pub fn classify(role: &str, name: &str, text: &str) -> PopupClass {
    let haystack = format!("{role} {name} {text}").to_lowercase();
    let contains_any = |words: &[&str]| words.iter().any(|w| haystack.contains(w));
    if contains_any(&[
        "password",
        "enter your",
        "authenticate",
        "touch id",
        "face id",
        "pin required",
        "uac",
        "administrator permission",
        "polkit",
        "authentication required",
    ]) {
        return PopupClass::SystemAuthentication;
    }
    if contains_any(&[
        "do you want to allow",
        "elevation",
        "installer",
        "user account control",
    ]) {
        return PopupClass::InstallerElevation;
    }
    if contains_any(&[
        "certificate",
        "not secure",
        "security warning",
        "malware",
        "phishing",
        "untrusted",
    ]) {
        return PopupClass::SecurityWarning;
    }
    if contains_any(&[
        "payment",
        "purchase",
        "checkout",
        "billing",
        "subscribe",
        "buy now",
    ]) {
        return PopupClass::PurchaseOrPayment;
    }
    if contains_any(&[
        "license agreement",
        "terms of service",
        "terms and conditions",
        "eula",
        "accept the agreement",
    ]) {
        return PopupClass::LegalAgreement;
    }
    if contains_any(&[
        "are you sure",
        "delete",
        "remove permanently",
        "erase",
        "confirm deletion",
        "destructive",
    ]) {
        return PopupClass::DestructiveConfirmation;
    }
    if contains_any(&[
        "permission",
        "allow",
        "would like to access",
        "oauth",
        "grant",
    ]) {
        return PopupClass::PermissionRequest;
    }
    if contains_any(&["cookie", "privacy notice", "consent", "tracking"]) {
        return PopupClass::CookieOrSitePreference;
    }
    if contains_any(&["update available", "new version", "restart to update"]) {
        return PopupClass::UpdateAvailable;
    }
    if contains_any(&["unsaved", "save changes", "discard"]) {
        return PopupClass::UnsavedChanges;
    }
    if contains_any(&["open file", "save as", "choose file", "file picker"]) {
        return PopupClass::FilePicker;
    }
    if contains_any(&[
        "crashed",
        "recovered",
        "restore session",
        "unexpectedly quit",
    ]) {
        return PopupClass::CrashOrRecovery;
    }
    if contains_any(&[
        "dialog",
        "alert",
        "modal",
        "toast",
        "notification",
        "tip",
        "what's new",
    ]) {
        return PopupClass::Informational;
    }
    PopupClass::Unknown
}

/// Decide whether a popup may be auto-dismissed under the user policy.
/// Never-auto classes always return an error.
pub fn authorize_dismissal(
    popup: &PopupInfo,
    policy: &PopupPolicy,
) -> Result<DismissalPlan, PopupError> {
    if popup.class.never_auto() {
        return Err(PopupError::AuthorizationRequired {
            class: popup.class.as_str().to_owned(),
        });
    }
    let allowed = match popup.class {
        PopupClass::Informational | PopupClass::CrashOrRecovery => policy.dismiss_informational,
        PopupClass::CookieOrSitePreference => policy.dismiss_cookie_banners,
        PopupClass::UpdateAvailable => policy.dismiss_update_prompts,
        _ => false,
    };
    if !allowed {
        return Err(PopupError::AuthorizationRequired {
            class: popup.class.as_str().to_owned(),
        });
    }
    Ok(DismissalPlan::for_popup(popup))
}

/// Preferred dismissal action: semantic Close/Cancel/Not Now first, scoped
/// key event only as a last resort.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "method")]
pub enum DismissalPlan {
    SemanticAction { action: String },
    ScopedKey { key: String },
}

impl DismissalPlan {
    pub fn for_popup(popup: &PopupInfo) -> Self {
        for candidate in [
            "close",
            "cancel",
            "not now",
            "dismiss",
            "no thanks",
            "later",
        ] {
            if popup
                .close_actions
                .iter()
                .any(|action| action.to_lowercase() == candidate)
            {
                return DismissalPlan::SemanticAction {
                    action: candidate.to_owned(),
                };
            }
        }
        if let Some(action) = popup.close_actions.first() {
            return DismissalPlan::SemanticAction {
                action: action.clone(),
            };
        }
        DismissalPlan::ScopedKey {
            key: "Escape".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests;
