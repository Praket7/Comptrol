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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PairingRecord {
    pub pairing_id: String,
    pub token_hash: String,
    pub scopes: Vec<String>,
    pub created_at_ms: u128,
    pub expires_at_ms: u128,
    pub accepted: bool,
    pub revoked: bool,
}

#[derive(Debug)]
pub struct PairingStore {
    path: std::path::PathBuf,
    records: HashMap<String, PairingRecord>,
}

impl PairingStore {
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(state_dir)?;
        let path = state_dir.join("pairings.jsonl");
        let mut records = HashMap::new();
        if path.exists() {
            for line in BufReader::new(File::open(&path)?).lines() {
                if let Ok(record) = serde_json::from_str::<PairingRecord>(&line?) {
                    records.insert(record.pairing_id.clone(), record);
                }
            }
        }
        Ok(Self { path, records })
    }

    pub fn create(
        &mut self,
        scopes: Vec<String>,
        ttl_ms: Option<u128>,
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

    pub fn list(&self) -> Vec<PairingRecord> {
        self.records.values().cloned().collect()
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

    #[test]
    fn pairing_scopes_accept_expire_and_revoke() {
        let path = std::env::temp_dir().join(format!("comptrol-pairing-{}", now_ms()));
        let mut store = PairingStore::open(&path).expect("store");
        let (record, token) = store
            .create(
                vec!["observe".to_owned(), "observe".to_owned()],
                Some(60_000),
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
        let path = std::env::temp_dir().join(format!("comptrol-pairing-{}", now_ms()));
        let mut store = PairingStore::open(&path).expect("store");
        assert!(store.create(vec!["terminal_all".to_owned()], None).is_err());
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn pairing_rejects_expired_code() {
        let path = std::env::temp_dir().join(format!("comptrol-pairing-{}", now_ms()));
        let mut store = PairingStore::open(&path).expect("store");
        let (record, token) = store
            .create(vec!["observe".to_owned()], Some(60_000))
            .expect("create");
        store
            .records
            .get_mut(&record.pairing_id)
            .expect("record")
            .expires_at_ms = now_ms().saturating_sub(1);
        assert!(store.accept(&token).is_err());
        let _ = fs::remove_dir_all(path);
    }
}
