use getrandom::fill;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_TTL_MS: u128 = 300_000;
const MAX_TTL_MS: u128 = 86_400_000;
const TOKEN_BYTES: usize = 32;
const NONCE_TTL_MS: u128 = 120_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PairingRecord {
    pub pairing_id: String,
    pub token_hash: String,
    pub scopes: Vec<String>,
    pub created_at_ms: u128,
    pub expires_at_ms: u128,
    pub accepted: bool,
    pub revoked: bool,
    pub identity_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplayNonce {
    pub nonce_hash: String,
    pub created_at_ms: u128,
    pub pairing_id: String,
}

#[derive(Debug)]
pub struct PairingStore {
    path: std::path::PathBuf,
    records: HashMap<String, PairingRecord>,
    replay_cache: HashMap<String, ReplayNonce>,
}

impl PairingStore {
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(state_dir)?;
        let path = state_dir.join("pairings.jsonl");
        let mut records = HashMap::new();
        let mut replay_cache = HashMap::new();
        if path.exists() {
            for line in BufReader::new(File::open(&path)?).lines() {
                if let Ok(record) = serde_json::from_str::<PairingRecord>(&line?) {
                    records.insert(record.pairing_id.clone(), record);
                }
            }
        }
        let replay_path = state_dir.join("replay_nonces.jsonl");
        if replay_path.exists() {
            for line in BufReader::new(File::open(&replay_path)?).lines() {
                if let Ok(nonce) = serde_json::from_str::<ReplayNonce>(&line?)
                    && now_ms().saturating_sub(nonce.created_at_ms) < NONCE_TTL_MS
                {
                    replay_cache.insert(nonce.nonce_hash.clone(), nonce);
                }
            }
        }
        Ok(Self {
            path,
            records,
            replay_cache,
        })
    }

    pub fn bind_identity(&mut self, pairing_id: &str, fingerprint: &str) -> io::Result<()> {
        let record = self
            .records
            .get_mut(pairing_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pairing id not found"))?;
        if record.revoked {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pairing revoked",
            ));
        }
        let mut updated = record.clone();
        updated.identity_fingerprint = Some(fingerprint.to_owned());
        self.write(updated)
    }

    pub fn verify_mtls_identity(
        &self,
        pairing_id: &str,
        fingerprint: &str,
        scopes: &[String],
    ) -> io::Result<&PairingRecord> {
        let record = self
            .records
            .get(pairing_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pairing id not found"))?;
        if record.revoked {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pairing revoked",
            ));
        }
        if !record.accepted {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pairing not accepted",
            ));
        }
        if record.expires_at_ms <= now_ms() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pairing expired",
            ));
        }
        match &record.identity_fingerprint {
            Some(fp) if fp == fingerprint => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "identity fingerprint mismatch",
                ));
            }
        }
        for scope in scopes {
            if !record.scopes.contains(&scope.to_owned()) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("scope not authorized: {scope}"),
                ));
            }
        }
        Ok(record)
    }

    pub fn find_by_fingerprint(&self, fingerprint: &str) -> Option<&PairingRecord> {
        self.records.values().find(|r| {
            r.identity_fingerprint.as_deref() == Some(fingerprint)
                && r.accepted
                && !r.revoked
                && r.expires_at_ms > now_ms()
        })
    }

    pub fn record_nonce(&mut self, nonce: &str, pairing_id: &str) -> io::Result<bool> {
        let now = now_ms();
        let nonce_hash = hash(nonce);
        self.replay_cache
            .retain(|_, entry| now.saturating_sub(entry.created_at_ms) < NONCE_TTL_MS);
        if self.replay_cache.contains_key(&nonce_hash) {
            return Ok(false);
        }
        let entry = ReplayNonce {
            nonce_hash: nonce_hash.clone(),
            created_at_ms: now,
            pairing_id: pairing_id.to_owned(),
        };
        self.replay_cache.insert(nonce_hash, entry.clone());
        let replay_path = self.path.with_file_name("replay_nonces.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&replay_path)?;
        serde_json::to_writer(&mut file, &entry)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(true)
    }

    pub fn list(&self) -> Vec<PairingRecord> {
        self.records.values().cloned().collect()
    }

    pub fn identity_bound_count(&self) -> usize {
        self.records
            .values()
            .filter(|r| r.identity_fingerprint.is_some())
            .count()
    }

    pub fn create(
        &mut self,
        scopes: Vec<String>,
        ttl_ms: Option<u128>,
        identity_fingerprint: Option<String>,
    ) -> io::Result<(PairingRecord, String)> {
        let now = now_ms();
        let ttl = ttl_ms.unwrap_or(DEFAULT_TTL_MS).clamp(1_000, MAX_TTL_MS);
        let mut token = [0_u8; TOKEN_BYTES];
        fill(&mut token).map_err(io::Error::other)?;
        let token_text = hex(&token);
        let pairing_id = format!("pair-{}", hex(&token[..8]));
        let record = PairingRecord {
            pairing_id: pairing_id.clone(),
            token_hash: hash(&token_text),
            scopes: normalize_scopes(scopes)?,
            created_at_ms: now,
            expires_at_ms: now.saturating_add(ttl),
            accepted: false,
            revoked: false,
            identity_fingerprint,
        };
        self.write(record.clone())?;
        Ok((record, token_text))
    }

    pub fn accept(&mut self, token: &str) -> io::Result<PairingRecord> {
        let token_hash = hash(token);
        let Some(mut record) = self
            .records
            .values()
            .find(|record| record.token_hash == token_hash)
            .cloned()
        else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pairing code not found",
            ));
        };
        if record.revoked {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pairing code revoked",
            ));
        }
        if record.accepted {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pairing code already accepted",
            ));
        }
        if record.expires_at_ms <= now_ms() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pairing code expired",
            ));
        }
        record.accepted = true;
        self.write(record.clone())?;
        Ok(record)
    }

    pub fn revoke(&mut self, pairing_id: &str) -> io::Result<PairingRecord> {
        let Some(mut record) = self.records.get(pairing_id).cloned() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "pairing id not found",
            ));
        };
        record.revoked = true;
        self.write(record.clone())?;
        Ok(record)
    }

    fn write(&mut self, record: PairingRecord) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, &record)?;
        file.write_all(b"\n")?;
        file.flush()?;
        self.records.insert(record.pairing_id.clone(), record);
        Ok(())
    }
}

fn normalize_scopes(scopes: Vec<String>) -> io::Result<Vec<String>> {
    let allowed = [
        "observe",
        "accessibility_read",
        "semantic_input",
        "raw_input",
        "focus",
        "file_read",
        "file_write",
        "terminal",
    ];
    let mut normalized = Vec::new();
    for scope in scopes {
        if !allowed.contains(&scope.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown pairing scope",
            ));
        }
        if !normalized.contains(&scope) {
            normalized.push(scope);
        }
    }
    if normalized.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pairing needs at least one scope",
        ));
    }
    Ok(normalized)
}

fn hash(value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(value.as_bytes());
    hex(&digest.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_path() -> std::path::PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("comptrol-pairing-{}-{sequence}", now_ms()))
    }

    #[test]
    fn pairing_scopes_accept_expire_and_revoke() {
        let path = test_path();
        let mut store = PairingStore::open(&path).expect("store");
        let (record, token) = store
            .create(
                vec!["observe".to_owned(), "observe".to_owned()],
                Some(60_000),
                None,
            )
            .expect("create");
        assert_eq!(record.scopes, vec!["observe"]);
        let accepted = store.accept(&token).expect("accept");
        assert!(accepted.accepted);
        store.revoke(&record.pairing_id).expect("revoke");
        assert!(store.accept(&token).is_err());
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn pairing_rejects_unknown_scope() {
        let path = test_path();
        let mut store = PairingStore::open(&path).expect("store");
        assert!(
            store
                .create(vec!["terminal_all".to_owned()], None, None)
                .is_err()
        );
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn pairing_rejects_expired_code() {
        let path = test_path();
        let mut store = PairingStore::open(&path).expect("store");
        let (record, token) = store
            .create(vec!["observe".to_owned()], Some(60_000), None)
            .expect("create");
        store
            .records
            .get_mut(&record.pairing_id)
            .expect("record")
            .expires_at_ms = now_ms().saturating_sub(1);
        assert!(store.accept(&token).is_err());
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn identity_binding_and_verification() {
        let path = test_path();
        let mut store = PairingStore::open(&path).expect("store");
        let fp = "sha256:abc123def456";
        let (record, token) = store
            .create(
                vec!["observe".to_owned()],
                Some(60_000),
                Some(fp.to_owned()),
            )
            .expect("create");
        let accepted = store.accept(&token).expect("accept");
        assert!(accepted.identity_fingerprint.as_deref() == Some(fp));
        let verified = store
            .verify_mtls_identity(&record.pairing_id, fp, &["observe".to_owned()])
            .expect("verify");
        assert_eq!(verified.identity_fingerprint.as_deref(), Some(fp));
        assert!(
            store
                .verify_mtls_identity(&record.pairing_id, "sha256:wrong", &["observe".to_owned()])
                .is_err()
        );
        assert!(
            store
                .verify_mtls_identity(&record.pairing_id, fp, &["terminal".to_owned()])
                .is_err()
        );
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn replay_nonce_prevents_reuse() {
        let path = test_path();
        let mut store = PairingStore::open(&path).expect("store");
        let nonce = "nonce-abc-123";
        let pid = "test-pairing";
        assert!(store.record_nonce(nonce, pid).expect("record"));
        assert!(
            !store
                .record_nonce(nonce, pid)
                .expect("should detect replay")
        );
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn identity_bound_count() {
        let path = test_path();
        let mut store = PairingStore::open(&path).expect("store");
        assert_eq!(store.identity_bound_count(), 0);
        store
            .create(vec!["observe".to_owned()], None, Some("fp1".to_owned()))
            .expect("create");
        store
            .create(vec!["observe".to_owned()], None, None)
            .expect("create");
        assert_eq!(store.identity_bound_count(), 1);
        let _ = fs::remove_dir_all(path);
    }
}
