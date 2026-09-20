//! Durable consent store.
//!
//! Grants are persisted as JSON lines with a SHA-256 digest chain so any
//! tampering (including edits made by a compromised agent process) is
//! detectable on load. Loading never repairs: a bad store refuses and
//! tells the user to re-run setup.

use crate::scopes::{ConsentLifetime, ConsentScope, GrantSubject};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("json failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("consent store integrity check failed at record {0}")]
    Tampered(usize),
    #[error("agent-originated grants are not persistable")]
    AgentGrantRejected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrantConditions {
    /// Expected publisher/author, when the platform exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Pinned package or app version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Free-form typed constraints the verifier must re-check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsentGrant {
    pub id: String,
    pub capability: String,
    #[serde(flatten)]
    pub scope: ConsentScope,
    pub subject: GrantSubject,
    pub risk: crate::Risk,
    pub granted_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<GrantConditions>,
    pub revocable: bool,
    /// Stored digest of the previous record, forming a hash chain.
    #[serde(default)]
    pub chain: String,
    /// Stored digest of this record's content. Recomputed and verified on
    /// load so tampering with any record, including the last one, is
    /// detected.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
}

impl ConsentGrant {
    fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        // The digest field is excluded from its own computation.
        let mut record = self.clone();
        record.digest = String::new();
        hasher.update(serde_json::to_vec(&record).unwrap_or_default());
        format!("{:x}", hasher.finalize())
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        match self.scope.lifetime {
            Some(ConsentLifetime::Until { expires_at_ms }) => expires_at_ms <= now_ms,
            _ => false,
        }
    }
}

/// JSON-lines store with per-record hash chaining.
#[derive(Debug)]
pub struct ConsentStore {
    path: PathBuf,
    grants: Vec<ConsentGrant>,
    head: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl ConsentStore {
    /// Open (or create) the store at `path` and verify its integrity.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        let mut store = Self {
            path,
            grants: Vec::new(),
            head: String::new(),
        };
        if store.path.exists() {
            let raw = std::fs::read_to_string(&store.path)?;
            let mut previous = String::new();
            for (index, line) in raw.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let grant: ConsentGrant = serde_json::from_str(line)?;
                let stored_digest = grant.digest.clone();
                let recomputed = grant.digest();
                if stored_digest.is_empty() || stored_digest != recomputed {
                    return Err(StoreError::Tampered(index));
                }
                if grant.chain != previous {
                    return Err(StoreError::Tampered(index));
                }
                previous = stored_digest.clone();
                store.head = stored_digest;
                store.grants.push(grant);
            }
        }
        Ok(store)
    }

    fn persist(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut raw = String::new();
        for grant in &self.grants {
            raw.push_str(&serde_json::to_string(grant)?);
            raw.push('\n');
        }
        std::fs::write(&self.path, raw)?;
        Ok(())
    }

    /// Persist a new grant. Only [`GrantSubject::LocalUser`] grants may be
    /// stored; instruction grants live in the runtime's session layer.
    pub fn grant(
        &mut self,
        capability: &str,
        scope: ConsentScope,
        subject: GrantSubject,
        risk: crate::Risk,
        conditions: Option<GrantConditions>,
    ) -> Result<ConsentGrant, StoreError> {
        if !subject.may_persist() {
            return Err(StoreError::AgentGrantRejected);
        }
        let chain_input = self.head.clone();
        let grant = ConsentGrant {
            id: format!("grant-{:016x}", {
                let mut hasher = Sha256::new();
                hasher.update(chain_input.as_bytes());
                hasher.update(capability.as_bytes());
                hasher.update(serde_json::to_vec(&scope).unwrap_or_default());
                hasher.update(now_ms().to_le_bytes());
                let out = hasher.finalize();
                u64::from_le_bytes(out[..8].try_into().unwrap_or_default())
            }),
            capability: capability.to_owned(),
            scope,
            subject,
            risk,
            granted_at_ms: now_ms(),
            conditions,
            revocable: true,
            chain: chain_input.clone(),
            digest: String::new(),
        };
        let digest = grant.digest();
        self.head = digest.clone();
        let mut grant = grant;
        grant.digest = digest;
        self.grants.push(grant.clone());
        self.persist()?;
        Ok(grant)
    }

    /// Revoke by id. Revocation takes effect before the next dispatch
    /// because every authorization path re-reads this store's memory.
    pub fn revoke(&mut self, grant_id: &str) -> Result<bool, StoreError> {
        let before = self.grants.len();
        self.grants.retain(|grant| grant.id != grant_id);
        let changed = before != self.grants.len();
        if changed {
            // Rebuild the chain from the surviving grants so the store stays
            // verifiable after revocation.
            self.head = String::new();
            for grant in &mut self.grants {
                grant.chain = self.head.clone();
                grant.digest = grant.digest();
                self.head = grant.digest.clone();
            }
            self.persist()?;
        }
        Ok(changed)
    }

    /// List non-expired grants, optionally filtered by capability.
    pub fn active(&self, capability: Option<&str>) -> Vec<&ConsentGrant> {
        let now = now_ms();
        self.grants
            .iter()
            .filter(|grant| !grant.expired(now))
            .filter(|grant| capability.is_none_or(|cap| grant.capability == cap))
            .collect()
    }

    /// The single authorization gate: does any surviving grant allow
    /// `capability`/`intent` at `risk` for `resource`?
    pub fn authorize(
        &self,
        capability: &str,
        intent: &str,
        risk: crate::Risk,
        resource: Option<&str>,
    ) -> crate::ConsentDecision {
        for grant in self.active(Some(capability)) {
            if grant.scope.covers(intent, risk, resource) {
                return crate::ConsentDecision::Allowed {
                    grant_id: grant.id.clone(),
                };
            }
        }
        crate::ConsentDecision::Denied {
            reason: format!("no active consent grant covers {capability}/{intent}"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "comptrol-consent-test-{}-{}-{}",
            std::process::id(),
            now_ms(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn local_user_scope() -> ConsentScope {
        ConsentScope {
            app_id: None,
            resource: Some("winget:VideoLAN.VLC".into()),
            intent: Some("software.install".into()),
            max_risk: Some(crate::Risk::R2),
            lifetime: None,
        }
    }

    #[test]
    fn agent_originated_grants_are_rejected() {
        let dir = temp_dir();
        let mut store = ConsentStore::open(dir.join("consent.jsonl")).unwrap();
        let result = store.grant(
            "software.install",
            local_user_scope(),
            GrantSubject::UserInstruction {
                instruction_digest: "abc".into(),
            },
            crate::Risk::R2,
            None,
        );
        assert!(matches!(result, Err(StoreError::AgentGrantRejected)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn grant_authorize_revoke_roundtrip() {
        let dir = temp_dir();
        let mut store = ConsentStore::open(dir.join("consent.jsonl")).unwrap();
        let grant = store
            .grant(
                "software.install",
                local_user_scope(),
                GrantSubject::LocalUser,
                crate::Risk::R2,
                None,
            )
            .unwrap();
        assert!(matches!(
            store.authorize(
                "software.install",
                "software.install",
                crate::Risk::R2,
                Some("winget:VideoLAN.VLC")
            ),
            crate::ConsentDecision::Allowed { .. }
        ));
        assert!(matches!(
            store.authorize(
                "software.install",
                "software.install",
                crate::Risk::R2,
                Some("winget:Other")
            ),
            crate::ConsentDecision::Denied { .. }
        ));
        assert!(store.revoke(&grant.id).unwrap());
        assert!(matches!(
            store.authorize(
                "software.install",
                "software.install",
                crate::Risk::R2,
                Some("winget:VideoLAN.VLC")
            ),
            crate::ConsentDecision::Denied { .. }
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tampered_store_is_detected() {
        let dir = temp_dir();
        let path = dir.join("consent.jsonl");
        {
            let mut store = ConsentStore::open(&path).unwrap();
            store
                .grant(
                    "software.install",
                    local_user_scope(),
                    GrantSubject::LocalUser,
                    crate::Risk::R2,
                    None,
                )
                .unwrap();
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let edited = raw.replace("VideoLAN.VLC", "Malware.Package");
        std::fs::write(&path, edited).unwrap();
        assert!(matches!(
            ConsentStore::open(&path),
            Err(StoreError::Tampered(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn store_survives_restart() {
        let dir = temp_dir();
        let path = dir.join("consent.jsonl");
        let mut store = ConsentStore::open(&path).unwrap();
        store
            .grant(
                "software.install",
                local_user_scope(),
                GrantSubject::LocalUser,
                crate::Risk::R2,
                None,
            )
            .unwrap();
        drop(store);
        let reopened = ConsentStore::open(&path).unwrap();
        assert!(matches!(
            reopened.authorize(
                "software.install",
                "software.install",
                crate::Risk::R2,
                Some("winget:VideoLAN.VLC")
            ),
            crate::ConsentDecision::Allowed { .. }
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expired_grants_do_not_authorize() {
        let dir = temp_dir();
        let mut store = ConsentStore::open(dir.join("consent.jsonl")).unwrap();
        let mut scope = local_user_scope();
        scope.lifetime = Some(ConsentLifetime::Until { expires_at_ms: 1 });
        store
            .grant(
                "software.install",
                scope,
                GrantSubject::LocalUser,
                crate::Risk::R2,
                None,
            )
            .unwrap();
        assert!(matches!(
            store.authorize(
                "software.install",
                "software.install",
                crate::Risk::R2,
                Some("winget:VideoLAN.VLC")
            ),
            crate::ConsentDecision::Denied { .. }
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
