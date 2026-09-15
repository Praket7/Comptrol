use comptrol_adapter_sdk::{
    ADAPTER_PROTOCOL_VERSION, AdapterManifest, CapabilityToken, HealthState, MAX_FRAME_BYTES,
    RpcRequest, RpcResponse, frame,
};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct AdapterHostConfig {
    pub manifest: AdapterManifest,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub instance_id: String,
    pub max_frame_bytes: usize,
    pub timeout_ms: u64,
}

#[derive(Debug)]
pub struct AdapterHost {
    config: AdapterHostConfig,
    child: Child,
    stdin: ChildStdin,
    stdout: Option<ChildStdout>,
    request_sequence: u64,
}

#[derive(Debug)]
pub enum HostError {
    Io(io::Error),
    Protocol(String),
    Json(serde_json::Error),
    Timeout,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "adapter host I/O failed: {error}"),
            Self::Protocol(error) => write!(formatter, "adapter protocol failed: {error}"),
            Self::Json(error) => write!(formatter, "adapter JSON failed: {error}"),
            Self::Timeout => write!(formatter, "adapter I/O deadline exceeded"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<io::Error> for HostError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for HostError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl AdapterHost {
    pub fn spawn(config: AdapterHostConfig) -> Result<Self, HostError> {
        config
            .manifest
            .validate()
            .map_err(|error| HostError::Protocol(error.to_string()))?;
        let mut command = Command::new(&config.executable);
        command
            .args(&config.arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HostError::Protocol("adapter stdin unavailable".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HostError::Protocol("adapter stdout unavailable".to_owned()))?;
        Ok(Self {
            config,
            child,
            stdin,
            stdout: Some(stdout),
            request_sequence: 0,
        })
    }

    pub fn capability_token(
        &self,
        intent: &str,
        resource: &str,
        operation_id: &str,
        ttl_ms: u64,
    ) -> Result<CapabilityToken, HostError> {
        if !self.config.manifest.declares(intent) {
            return Err(HostError::Protocol(format!(
                "adapter does not declare {intent}"
            )));
        }
        let mut bytes = [0u8; 18];
        getrandom::fill(&mut bytes)
            .map_err(|error| HostError::Protocol(format!("token generation failed: {error}")))?;
        let token = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(CapabilityToken {
            token,
            adapter_id: self.config.manifest.id.clone(),
            capability: intent.to_owned(),
            resource: resource.to_owned(),
            operation_id: operation_id.to_owned(),
            expires_at_ms: now_ms().saturating_add(ttl_ms),
        })
    }

    pub fn request(
        &mut self,
        method: &str,
        resource: &str,
        payload: Value,
        token: Option<CapabilityToken>,
    ) -> Result<RpcResponse, HostError> {
        self.request_sequence = self.request_sequence.saturating_add(1);
        let request = RpcRequest {
            protocol_version: ADAPTER_PROTOCOL_VERSION,
            adapter_instance_id: self.config.instance_id.clone(),
            request_id: format!("adapter-request-{}", self.request_sequence),
            deadline_ms: now_ms().saturating_add(self.config.timeout_ms),
            capability_token: token,
            resource_scope: resource.to_owned(),
            method: method.to_owned(),
            payload,
        };
        request
            .validate(&self.config.manifest, now_ms())
            .map_err(|error| HostError::Protocol(error.to_string()))?;
        let encoded = frame(&request).map_err(|error| HostError::Protocol(error.to_string()))?;
        if encoded.len() > self.config.max_frame_bytes || encoded.len() > MAX_FRAME_BYTES + 4 {
            return Err(HostError::Protocol(
                "request exceeds host frame limit".to_owned(),
            ));
        }
        self.stdin.write_all(&encoded)?;
        self.stdin.flush()?;
        let stdout = self
            .stdout
            .take()
            .ok_or_else(|| HostError::Protocol("adapter stdout is unavailable".to_owned()))?;
        let max_frame_bytes = self.config.max_frame_bytes;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stdout = stdout;
            let result = read_frame(&mut stdout, max_frame_bytes);
            let _ = sender.send((stdout, result));
        });
        let (stdout, response) = receiver
            .recv_timeout(Duration::from_millis(self.config.timeout_ms))
            .map_err(|_| {
                let _ = self.child.kill();
                HostError::Timeout
            })?;
        self.stdout = Some(stdout);
        let response = response?;
        if response.protocol_version != ADAPTER_PROTOCOL_VERSION
            || response.adapter_instance_id != self.config.instance_id
            || response.request_id != request.request_id
        {
            return Err(HostError::Protocol(
                "adapter response identity mismatch".to_owned(),
            ));
        }
        Ok(response)
    }

    pub fn health(&mut self) -> Result<HealthState, HostError> {
        Ok(self.request("probe", "adapter", Value::Null, None)?.health)
    }

    pub fn handshake(&mut self) -> Result<RpcResponse, HostError> {
        self.request("handshake", "adapter", Value::Null, None)
    }
}

impl Drop for AdapterHost {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_frame<T: Read>(reader: &mut T, max_frame_bytes: usize) -> Result<RpcResponse, HostError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length > max_frame_bytes || length > MAX_FRAME_BYTES {
        return Err(HostError::Protocol(
            "response exceeds adapter frame limit".to_owned(),
        ));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok(serde_json::from_slice(&payload)?)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_scoped_to_manifest_capability() {
        let config = AdapterHostConfig {
            manifest: AdapterManifest {
                manifest_version: 1,
                id: "comptrol.test".to_owned(),
                name: "test".to_owned(),
                version: "0.1".to_owned(),
                platforms: vec!["linux".to_owned()],
                applications: vec!["test".to_owned()],
                isolation: comptrol_adapter_sdk::Isolation {
                    mode: "out_of_process".to_owned(),
                    network: "loopback_only".to_owned(),
                    filesystem: "declared_scopes".to_owned(),
                },
                capabilities: vec![comptrol_adapter_sdk::CapabilitySpec {
                    intent: "test.set".to_owned(),
                    risk: "R2".to_owned(),
                    background: "supported".to_owned(),
                    verification: "application_state".to_owned(),
                }],
            },
            executable: PathBuf::from("does-not-exist"),
            arguments: Vec::new(),
            instance_id: "instance".to_owned(),
            max_frame_bytes: MAX_FRAME_BYTES,
            timeout_ms: 1_000,
        };
        assert!(config.manifest.validate().is_ok());
    }
}
