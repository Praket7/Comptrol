use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const ADAPTER_PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AdapterManifest {
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub platforms: Vec<String>,
    pub applications: Vec<String>,
    pub isolation: Isolation,
    pub capabilities: Vec<CapabilitySpec>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Isolation {
    pub mode: String,
    pub network: String,
    pub filesystem: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CapabilitySpec {
    pub intent: String,
    pub risk: String,
    pub background: String,
    pub verification: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CapabilityToken {
    pub token: String,
    pub adapter_id: String,
    pub capability: String,
    pub resource: String,
    pub operation_id: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RpcRequest {
    pub protocol_version: u32,
    pub adapter_instance_id: String,
    pub request_id: String,
    pub deadline_ms: u64,
    pub capability_token: Option<CapabilityToken>,
    pub resource_scope: String,
    pub method: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RpcResponse {
    pub protocol_version: u32,
    pub adapter_instance_id: String,
    pub request_id: String,
    pub ok: bool,
    pub health: HealthState,
    pub payload: Value,
    pub error: Option<RpcError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Available,
    Degraded,
    RequiresConsent,
    Unsupported,
    Unhealthy,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError(pub String);

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ValidationError {}

impl AdapterManifest {
    pub fn from_toml(text: &str) -> Result<Self, ValidationError> {
        let manifest: Self = toml::from_str(text)
            .map_err(|error| ValidationError(format!("invalid adapter manifest: {error}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.manifest_version != 1 {
            return Err(ValidationError("manifest_version must be 1".to_owned()));
        }
        for (label, value) in [
            ("id", &self.id),
            ("name", &self.name),
            ("version", &self.version),
            ("isolation.mode", &self.isolation.mode),
            ("isolation.network", &self.isolation.network),
            ("isolation.filesystem", &self.isolation.filesystem),
        ] {
            if value.trim().is_empty() || value.chars().any(char::is_control) {
                return Err(ValidationError(format!(
                    "{label} must be non-empty and printable"
                )));
            }
        }
        if self.id.split('.').count() < 2 || self.id.chars().any(char::is_whitespace) {
            return Err(ValidationError(
                "id must be a dotted, whitespace-free identifier".to_owned(),
            ));
        }
        if self.platforms.is_empty() || self.applications.is_empty() || self.capabilities.is_empty()
        {
            return Err(ValidationError(
                "platforms, applications, and capabilities are required".to_owned(),
            ));
        }
        if self.isolation.mode != "out_of_process"
            || self.isolation.network != "loopback_only"
            || self.isolation.filesystem != "declared_scopes"
        {
            return Err(ValidationError(
                "adapters must use out_of_process, loopback_only, declared_scopes isolation"
                    .to_owned(),
            ));
        }
        let mut intents = std::collections::HashSet::new();
        for capability in &self.capabilities {
            if capability.intent.trim().is_empty()
                || capability.risk.trim().is_empty()
                || capability.background.trim().is_empty()
                || capability.verification.trim().is_empty()
                || !intents.insert(&capability.intent)
            {
                return Err(ValidationError(
                    "capabilities must have unique complete intents".to_owned(),
                ));
            }
        }
        Ok(())
    }

    pub fn declares(&self, intent: &str) -> bool {
        self.capabilities
            .iter()
            .any(|capability| capability.intent == intent)
    }
}

impl RpcRequest {
    pub fn validate(&self, manifest: &AdapterManifest, now_ms: u64) -> Result<(), ValidationError> {
        if self.protocol_version != ADAPTER_PROTOCOL_VERSION {
            return Err(ValidationError(
                "unsupported adapter protocol version".to_owned(),
            ));
        }
        if self.adapter_instance_id.trim().is_empty()
            || self.request_id.trim().is_empty()
            || self.method.trim().is_empty()
            || self.resource_scope.trim().is_empty()
        {
            return Err(ValidationError(
                "adapter request identity and scope are required".to_owned(),
            ));
        }
        let encoded = serde_json::to_vec(self)
            .map_err(|error| ValidationError(format!("request encoding failed: {error}")))?;
        if encoded.len() > MAX_FRAME_BYTES {
            return Err(ValidationError(
                "adapter request exceeds frame limit".to_owned(),
            ));
        }
        if self.method == "execute" || self.method == "verify" {
            let token = self.capability_token.as_ref().ok_or_else(|| {
                ValidationError("mutation RPC requires a capability token".to_owned())
            })?;
            if token.adapter_id != manifest.id
                || token.capability
                    != self
                        .payload
                        .get("intent")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                || token.resource != self.resource_scope
                || token.expires_at_ms <= now_ms
            {
                return Err(ValidationError(
                    "capability token is missing, expired, or out of scope".to_owned(),
                ));
            }
            if !manifest.declares(&token.capability) {
                return Err(ValidationError(
                    "capability is not declared by the adapter manifest".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

pub fn frame(value: &impl Serialize) -> Result<Vec<u8>, ValidationError> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| ValidationError(format!("frame encoding failed: {error}")))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ValidationError("frame exceeds adapter limit".to_owned()));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| ValidationError("frame length overflow".to_owned()))?;
    let mut output = length.to_be_bytes().to_vec();
    output.extend(payload);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> AdapterManifest {
        AdapterManifest {
            manifest_version: 1,
            id: "comptrol.test".to_owned(),
            name: "Test adapter".to_owned(),
            version: "0.1.0".to_owned(),
            platforms: vec!["linux".to_owned()],
            applications: vec!["test".to_owned()],
            isolation: Isolation {
                mode: "out_of_process".to_owned(),
                network: "loopback_only".to_owned(),
                filesystem: "declared_scopes".to_owned(),
            },
            capabilities: vec![CapabilitySpec {
                intent: "test.value.set".to_owned(),
                risk: "R2".to_owned(),
                background: "supported".to_owned(),
                verification: "application_state".to_owned(),
            }],
        }
    }

    #[test]
    fn manifest_validation_rejects_in_process_adapter() {
        let mut value = manifest();
        value.isolation.mode = "in_process".to_owned();
        assert!(value.validate().is_err());
    }

    #[test]
    fn mutation_request_requires_scoped_live_token() {
        let value = RpcRequest {
            protocol_version: 1,
            adapter_instance_id: "instance".to_owned(),
            request_id: "request".to_owned(),
            deadline_ms: 100,
            capability_token: None,
            resource_scope: "document:test".to_owned(),
            method: "execute".to_owned(),
            payload: serde_json::json!({"intent":"test.value.set"}),
        };
        assert!(value.validate(&manifest(), 10).is_err());
    }

    #[test]
    fn frames_are_length_prefixed_and_bounded() {
        let encoded = frame(&serde_json::json!({"ok":true})).unwrap();
        assert_eq!(
            u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize,
            encoded.len() - 4
        );
    }
}
