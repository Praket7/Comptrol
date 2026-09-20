//! Grant subject and scope model.
//!
//! A grant names *who* granted it ([`GrantSubject`]) and *how far it
//! reaches* ([`ConsentScope`]). Scopes compose: a grant is within scope
//! only when every dimension matches or the dimension is wildcard.

use serde::{Deserialize, Serialize};

/// Who created the grant. Agent-originated subjects are structurally
/// impossible to persist: [`store::ConsentStore::grant`] rejects them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "subject")]
pub enum GrantSubject {
    /// The local user through setup or the control surface.
    LocalUser,
    /// A one-operation authorization produced by a direct user
    /// instruction. Always single-use.
    UserInstruction { instruction_digest: String },
}

impl GrantSubject {
    /// True when this subject may outlive the current operation.
    pub fn may_persist(&self) -> bool {
        matches!(self, GrantSubject::LocalUser)
    }
}

/// How long a grant survives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "lifetime")]
pub enum ConsentLifetime {
    /// Dies with the process.
    Session,
    /// Valid until `expires_at_ms` (unix epoch milliseconds).
    Until { expires_at_ms: u64 },
}

/// The reach of a grant.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentScope {
    /// Application identifier the grant covers (None = any allowed app).
    pub app_id: Option<String>,
    /// Exact resource identity (package id, document id, URL origin...).
    pub resource: Option<String>,
    /// Exact intent name the grant covers.
    pub intent: Option<String>,
    /// Restrict to R<=N intents even if the store would allow more.
    pub max_risk: Option<crate::Risk>,
    /// Process lifetime of the grant.
    pub lifetime: Option<ConsentLifetime>,
}

impl ConsentScope {
    /// Whether `candidate` is covered by this scope. `None` dimensions are
    /// wildcards only for persistent grants created by [`GrantSubject::LocalUser`];
    /// single-use instruction grants must pin the intent.
    pub fn covers(&self, intent: &str, risk: crate::Risk, resource: Option<&str>) -> bool {
        if let Some(pinned) = &self.intent
            && pinned != intent
        {
            return false;
        }
        if let Some(max) = self.max_risk
            && risk > max
        {
            return false;
        }
        if let Some(expected) = &self.resource
            && resource != Some(expected.as_str())
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk(n: u8) -> crate::Risk {
        match n {
            0 => crate::Risk::R0,
            1 => crate::Risk::R1,
            2 => crate::Risk::R2,
            _ => crate::Risk::R3,
        }
    }

    #[test]
    fn pinned_intent_blocks_others() {
        let scope = ConsentScope {
            intent: Some("software.install".into()),
            ..Default::default()
        };
        assert!(scope.covers("software.install", risk(2), None));
        assert!(!scope.covers("software.uninstall", risk(2), None));
    }

    #[test]
    fn risk_ceiling_is_enforced() {
        let scope = ConsentScope {
            intent: Some("settings.write".into()),
            max_risk: Some(risk(1)),
            ..Default::default()
        };
        assert!(scope.covers("settings.write", risk(1), None));
        assert!(!scope.covers("settings.write", risk(3), None));
    }

    #[test]
    fn resource_must_match_exactly() {
        let scope = ConsentScope {
            resource: Some("winget:VideoLAN.VLC".into()),
            ..Default::default()
        };
        assert!(scope.covers("software.install", risk(2), Some("winget:VideoLAN.VLC")));
        assert!(!scope.covers("software.install", risk(2), Some("winget:Other.App")));
        assert!(!scope.covers("software.install", risk(2), None));
    }

    #[test]
    fn only_local_user_may_persist() {
        assert!(GrantSubject::LocalUser.may_persist());
        assert!(
            !GrantSubject::UserInstruction {
                instruction_digest: "abc".into()
            }
            .may_persist()
        );
    }
}
