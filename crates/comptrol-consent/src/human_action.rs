//! Human action broker.
//!
//! When only the user can approve something (UAC, polkit, browser
//! permission prompts, OAuth consent, TCC grants), the operation pauses
//! in an explicit `awaiting_human_action` state carrying a structured
//! challenge. Comptrol never enters a secret and never auto-accepts a
//! privilege prompt; it resumes only after the user resolves the
//! challenge and an independent post-condition check passes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// The kinds of approval only a human can give. Each maps to a concrete
/// platform surface the broker can foreground and later observe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ChallengeKind {
    /// Windows UAC secure-desktop elevation.
    NativeAuthentication { platform: String },
    /// macOS admin/auth dialogs or TCC permission sheets.
    PermissionGrant {
        platform: String,
        permission: String,
    },
    /// Linux polkit/PAM agent prompts.
    PrivilegeAgent { platform: String },
    /// Browser permission or OAuth consent surfaces.
    BrowserConsent { origin: String },
    /// Extension or helper installation authorization.
    ExtensionInstall { host: String },
}

/// What the user is being asked to approve and where the prompt appears.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HumanActionChallenge {
    pub kind: ChallengeKind,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_change: Option<String>,
    /// Where the prompt surfaces (secure_desktop, sheet, browser_tab...).
    pub prompt_location: String,
    /// Always true: the agent must never type or store the secret.
    pub agent_must_not_enter_secret: bool,
}

/// A pause request created by an operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HumanActionRequest {
    pub id: String,
    pub operation_id: String,
    pub intent: String,
    pub challenge: HumanActionChallenge,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanActionResolution {
    /// The user completed the native prompt.
    Approved,
    /// The user dismissed or denied the prompt.
    Declined,
    /// The wait timed out without a user decision.
    TimedOut,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingHumanAction {
    #[serde(flatten)]
    pub request: HumanActionRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<HumanActionResolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at_ms: Option<u64>,
}

/// Registry of paused operations. The broker is deliberately dumb: it
/// tracks state and hands out challenges; observation of the post-auth
/// environment belongs to the operation's own verification step.
#[derive(Debug, Default)]
pub struct HumanActionBroker {
    pending: HashMap<String, PendingHumanAction>,
    sequence: u64,
}

impl HumanActionBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pause an operation with a structured challenge.
    pub fn request(
        &mut self,
        operation_id: &str,
        intent: &str,
        challenge: HumanActionChallenge,
    ) -> HumanActionRequest {
        self.sequence += 1;
        let request = HumanActionRequest {
            id: format!("human-{operation_id}-{}", self.sequence),
            operation_id: operation_id.to_owned(),
            intent: intent.to_owned(),
            challenge,
            created_at_ms: now_ms(),
        };
        self.pending.insert(
            request.id.clone(),
            PendingHumanAction {
                request: request.clone(),
                resolution: None,
                resolved_at_ms: None,
            },
        );
        request
    }

    /// Record how the human responded.
    pub fn resolve(&mut self, request_id: &str, resolution: HumanActionResolution) -> bool {
        if let Some(pending) = self.pending.get_mut(request_id) {
            pending.resolution = Some(resolution);
            pending.resolved_at_ms = Some(now_ms());
            true
        } else {
            false
        }
    }

    /// Fetch a pending (unresolved) action for an operation.
    pub fn pending_for_operation(&self, operation_id: &str) -> Option<&PendingHumanAction> {
        self.pending.values().find(|pending| {
            pending.request.operation_id == operation_id && pending.resolution.is_none()
        })
    }

    /// True when every requested action for `operation_id` has been
    /// resolved as [`HumanActionResolution::Approved`], so the operation
    /// may resume and re-verify.
    pub fn approved(&self, operation_id: &str) -> bool {
        self.pending
            .values()
            .filter(|pending| pending.request.operation_id == operation_id)
            .all(|pending| pending.resolution == Some(HumanActionResolution::Approved))
            && self
                .pending
                .values()
                .any(|pending| pending.request.operation_id == operation_id)
    }

    pub fn all(&self) -> Vec<&PendingHumanAction> {
        self.pending.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elevation_challenge() -> HumanActionChallenge {
        HumanActionChallenge {
            kind: ChallengeKind::NativeAuthentication {
                platform: "windows".into(),
            },
            reason: "installer requests elevation".into(),
            target: Some("Microsoft Visual Studio Code".into()),
            requested_change: Some("install version 1.93.0".into()),
            prompt_location: "secure_desktop".into(),
            agent_must_not_enter_secret: true,
        }
    }

    #[test]
    fn broker_never_auto_approves() {
        let mut broker = HumanActionBroker::new();
        let request = broker.request("op-1", "software.install", elevation_challenge());
        assert!(broker.pending_for_operation("op-1").is_some());
        assert!(!broker.approved("op-1"), "no resolution yet");
        assert!(broker.resolve(&request.id, HumanActionResolution::Declined));
        assert!(!broker.approved("op-1"));
    }

    #[test]
    fn approval_requires_explicit_resolution() {
        let mut broker = HumanActionBroker::new();
        let request = broker.request("op-2", "software.install", elevation_challenge());
        assert!(broker.resolve(&request.id, HumanActionResolution::Approved));
        assert!(broker.approved("op-2"));
        assert!(request.challenge.agent_must_not_enter_secret);
    }

    #[test]
    fn unknown_requests_do_not_resolve() {
        let mut broker = HumanActionBroker::new();
        assert!(!broker.resolve("nope", HumanActionResolution::Approved));
    }
}
