#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: &str = "0.1";
pub const SERVER_VERSION: &str = "0.1.0";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Risk {
    R0,
    R1,
    R2,
    R3,
    R4,
}

impl Risk {
    pub fn mutation(self) -> bool {
        self > Self::R0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Target {
    pub kind: String,
    pub id: Option<String>,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OperationRequest {
    pub intent: String,
    #[serde(default)]
    pub target: Option<Target>,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub postcondition: Option<Value>,
    #[serde(default)]
    pub risk: Option<Risk>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    NotDispatched,
    Delivered,
    Refused,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    NotAttempted,
    None,
    Changed,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    NotAttempted,
    Verified,
    Unverified,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    None,
    IdempotentReplay,
    RequiresReconciliation,
    Stopped,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ComptrolError {
    pub code: String,
    pub message: String,
    pub recovery: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActionResult {
    pub operation_id: String,
    pub intent: String,
    pub route: String,
    pub target: Option<Target>,
    pub preflight: String,
    pub delivery: DeliveryState,
    pub effect: EffectState,
    pub verification: VerificationState,
    pub disturbance: Value,
    pub recovery: RecoveryState,
    pub data: Value,
    pub error: Option<ComptrolError>,
}

impl ActionResult {
    fn refused(request: &OperationRequest, operation_id: String, error: ComptrolError) -> Self {
        Self {
            operation_id,
            intent: request.intent.clone(),
            route: "policy".to_owned(),
            target: request.target.clone(),
            preflight: "failed".to_owned(),
            delivery: DeliveryState::Refused,
            effect: EffectState::NotAttempted,
            verification: VerificationState::NotAttempted,
            disturbance: json!({ "foreground_changed": false }),
            recovery: RecoveryState::None,
            data: Value::Null,
            error: Some(error),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Capability {
    pub name: String,
    pub available: bool,
    pub risk: Risk,
    pub route: String,
    pub note: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Lease {
    pub id: String,
    pub scope: String,
    pub expires_at_ms: u128,
}

#[derive(Clone, Debug)]
pub struct LeaseManager {
    leases: HashMap<String, Lease>,
    sequence: u64,
}

impl LeaseManager {
    pub fn new() -> Self {
        Self {
            leases: HashMap::new(),
            sequence: 0,
        }
    }

    pub fn acquire(&mut self, scope: &str, ttl: Duration) -> Lease {
        self.sequence += 1;
        let lease = Lease {
            id: format!("lease-{}-{}", now_ms(), self.sequence),
            scope: scope.to_owned(),
            expires_at_ms: now_ms() + ttl.as_millis(),
        };
        self.leases.insert(lease.id.clone(), lease.clone());
        lease
    }

    pub fn valid(&mut self, id: &str, scope: &str) -> bool {
        self.expire();
        self.leases
            .get(id)
            .is_some_and(|lease| lease.scope == scope && lease.expires_at_ms > now_ms())
    }

    pub fn release(&mut self, id: &str) -> bool {
        self.leases.remove(id).is_some()
    }

    fn expire(&mut self) {
        let now = now_ms();
        self.leases.retain(|_, lease| lease.expires_at_ms > now);
    }
}

impl Default for LeaseManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub allow_sandbox_writes: bool,
    pub allow_desktop_notify: bool,
    pub max_risk: Risk,
    pub allowed_intents: HashSet<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            allow_sandbox_writes: false,
            allow_desktop_notify: false,
            max_risk: Risk::R0,
            allowed_intents: HashSet::from([
                "system.ping".to_owned(),
                "desktop.observe".to_owned(),
            ]),
        }
    }
}

impl Policy {
    pub fn from_environment() -> Self {
        let mut policy = Self::default();
        if std::env::var("COMPTROL_ALLOW_SANDBOX_WRITES").as_deref() == Ok("1") {
            policy.allow_sandbox_writes = true;
            policy.max_risk = Risk::R1;
            policy.allowed_intents.insert("filesystem.write".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_DESKTOP_NOTIFY").as_deref() == Ok("1") {
            policy.allow_desktop_notify = true;
            policy.max_risk = Risk::R1;
            policy.allowed_intents.insert("desktop.notify".to_owned());
        }
        policy
    }

    pub fn authorize(&self, intent: &str, risk: Risk) -> Result<(), ComptrolError> {
        if risk > self.max_risk || !self.allowed_intents.contains(intent) {
            return Err(ComptrolError {
                code: "policy_denied".to_owned(),
                message: format!("Policy denied intent {intent}"),
                recovery: Some("Change local policy outside the agent tool channel".to_owned()),
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct AuditJournal {
    path: PathBuf,
    file: File,
}

impl AuditJournal {
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(state_dir)?;
        restrict_dir(state_dir)?;
        let path = state_dir.join("audit.jsonl");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, result: &ActionResult) -> io::Result<()> {
        let row = json!({
            "event_id": format!("event-{}", now_ms()),
            "operation_id": result.operation_id,
            "intent": result.intent,
            "target_kind": result.target.as_ref().map(|t| t.kind.clone()),
            "risk_data": "redacted",
            "route": result.route,
            "delivery": result.delivery,
            "effect": result.effect,
            "verification": result.verification,
            "recovery": result.recovery,
            "error_code": result.error.as_ref().map(|e| e.code.clone()),
        });
        serde_json::to_writer(&mut self.file, &row)?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
}

#[derive(Debug)]
pub struct StopLatch {
    path: PathBuf,
}

impl StopLatch {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("stop.latch"),
        }
    }
    pub fn engaged(&self) -> bool {
        self.path.exists()
    }
    pub fn engage(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, b"stopped\n")
    }
    pub fn resume(&self) -> io::Result<()> {
        if self.path.exists() {
            fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Runtime {
    pub policy: Policy,
    pub leases: LeaseManager,
    pub journal: AuditJournal,
    pub stop: StopLatch,
    idempotent: HashMap<String, ActionResult>,
    sequence: u64,
}

impl Runtime {
    pub fn new(state_dir: PathBuf) -> io::Result<Self> {
        Ok(Self {
            policy: Policy::from_environment(),
            leases: LeaseManager::new(),
            journal: AuditJournal::open(&state_dir)?,
            stop: StopLatch::new(&state_dir),
            idempotent: HashMap::new(),
            sequence: 0,
        })
    }

    pub fn operate(&mut self, request: OperationRequest) -> ActionResult {
        let operation_id = request
            .idempotency_key
            .clone()
            .filter(|key| !key.trim().is_empty())
            .unwrap_or_else(|| self.next_operation_id());
        if let Some(previous) = request
            .idempotency_key
            .as_ref()
            .and_then(|key| self.idempotent.get(key))
        {
            let mut replay = previous.clone();
            replay.recovery = RecoveryState::IdempotentReplay;
            return replay;
        }
        let risk = request.risk.unwrap_or_else(|| classify(&request.intent));
        if risk.mutation() && self.stop.engaged() {
            let result = ActionResult::refused(
                &request,
                operation_id,
                ComptrolError {
                    code: "stopped".to_owned(),
                    message: "The local emergency stop latch is engaged".to_owned(),
                    recovery: Some("A human must resume the daemon locally".to_owned()),
                },
            );
            self.remember(&request, result.clone());
            return result;
        }
        if let Err(error) = self.policy.authorize(&request.intent, risk) {
            let result = ActionResult::refused(&request, operation_id, error);
            self.remember(&request, result.clone());
            return result;
        }
        if request.dry_run {
            let result = ActionResult {
                operation_id,
                intent: request.intent.clone(),
                route: route_for(&request.intent),
                target: request.target.clone(),
                preflight: "passed".to_owned(),
                delivery: DeliveryState::NotDispatched,
                effect: EffectState::NotAttempted,
                verification: VerificationState::NotAttempted,
                disturbance: json!({ "foreground_changed": false }),
                recovery: RecoveryState::None,
                data: json!({ "dry_run": true, "risk": risk }),
                error: None,
            };
            self.remember(&request, result.clone());
            return result;
        }
        let result = match request.intent.as_str() {
            "system.ping" => success(
                &request,
                operation_id,
                "native",
                EffectState::None,
                VerificationState::Verified,
                json!({ "ready": true, "protocol": PROTOCOL_VERSION }),
            ),
            "desktop.observe" => desktop_observe(&request, operation_id),
            "filesystem.write" => sandbox_write(&request, operation_id),
            "desktop.notify" => desktop_notify(&request, operation_id),
            _ => ActionResult::refused(
                &request,
                operation_id,
                ComptrolError {
                    code: "unsupported_capability".to_owned(),
                    message: format!("No route is implemented for {}", request.intent),
                    recovery: Some("Inspect capabilities and use a supported intent".to_owned()),
                },
            ),
        };
        self.remember(&request, result.clone());
        result
    }

    pub fn inspect(&mut self, kind: &str) -> Value {
        match kind {
            "doctor" => doctor(self),
            "capabilities" => json!(capabilities()),
            "desktop" | "system" => {
                desktop_observe(
                    &OperationRequest {
                        intent: "desktop.observe".to_owned(),
                        target: None,
                        params: Value::Null,
                        postcondition: None,
                        risk: Some(Risk::R0),
                        idempotency_key: None,
                        dry_run: false,
                    },
                    self.next_operation_id(),
                )
                .data
            }
            "status" => {
                json!({ "protocol": PROTOCOL_VERSION, "server": SERVER_VERSION, "stop_latched": self.stop.engaged(), "audit_path": self.journal.path() })
            }
            _ => {
                json!({ "error": { "code": "unsupported_capability", "message": "Unknown inspection kind" } })
            }
        }
    }

    pub fn watch(&mut self, operation_id: &str) -> Value {
        if let Some(result) = self
            .idempotent
            .values()
            .find(|result| result.operation_id == operation_id)
        {
            json!({ "state": "complete", "result": result })
        } else {
            json!({ "state": "unknown", "operation_id": operation_id, "error": "operation_unknown" })
        }
    }

    fn next_operation_id(&mut self) -> String {
        self.sequence += 1;
        format!("op-{}-{}", now_ms(), self.sequence)
    }

    fn remember(&mut self, request: &OperationRequest, result: ActionResult) {
        if let Some(key) = request.idempotency_key.as_ref() {
            self.idempotent.insert(key.clone(), result.clone());
        }
        if let Err(error) = self.journal.append(&result) {
            eprintln!("comptrol audit journal error: {error}");
        }
    }
}

fn classify(intent: &str) -> Risk {
    match intent {
        "system.ping" | "desktop.observe" => Risk::R0,
        "desktop.notify" | "filesystem.write" => Risk::R1,
        _ => Risk::R2,
    }
}

fn route_for(intent: &str) -> String {
    match intent {
        "system.ping" => "native",
        "desktop.observe" => "platform_observe",
        "filesystem.write" => "sandbox_filesystem",
        "desktop.notify" => "platform_notification",
        _ => "none",
    }
    .to_owned()
}

fn success(
    request: &OperationRequest,
    operation_id: String,
    route: &str,
    effect: EffectState,
    verification: VerificationState,
    data: Value,
) -> ActionResult {
    ActionResult {
        operation_id,
        intent: request.intent.clone(),
        route: route.to_owned(),
        target: request.target.clone(),
        preflight: "passed".to_owned(),
        delivery: DeliveryState::Delivered,
        effect,
        verification,
        disturbance: json!({ "foreground_changed": false }),
        recovery: RecoveryState::None,
        data,
        error: None,
    }
}

fn desktop_observe(request: &OperationRequest, operation_id: String) -> ActionResult {
    let mut data = json!({ "platform": std::env::consts::OS, "arch": std::env::consts::ARCH, "current_dir": std::env::current_dir().ok(), "route": "platform_observe", "permission": "not_required_for_basic_process_observation" });
    if cfg!(target_os = "macos") {
        match Command::new("osascript")
            .args([
                "-e",
                "tell application \"System Events\" to get name of every application process",
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                data["applications"] = json!(String::from_utf8_lossy(&output.stdout).trim());
                data["accessibility"] = json!("reachable");
            }
            Ok(output) => {
                data["accessibility"] = json!("permission_required");
                data["diagnostic"] = json!(String::from_utf8_lossy(&output.stderr).trim());
            }
            Err(error) => {
                data["accessibility"] = json!("unavailable");
                data["diagnostic"] = json!(error.to_string());
            }
        }
    } else if let Ok(output) = Command::new("ps").args(["-A", "-o", "comm="]).output()
        && output.status.success()
    {
        data["processes"] = json!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .take(200)
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
        );
    }
    success(
        request,
        operation_id,
        "platform_observe",
        EffectState::None,
        VerificationState::Verified,
        data,
    )
}

fn sandbox_write(request: &OperationRequest, operation_id: String) -> ActionResult {
    let relative = request
        .params
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("note.txt");
    let content = request
        .params
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if relative.starts_with('/') || relative.contains("..") {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "Sandbox writes accept relative paths without parent traversal".to_owned(),
                recovery: Some("Use a relative path inside the Comptrol state sandbox".to_owned()),
            },
        );
    }
    let root = state_dir().join("sandbox");
    let path = root.join(relative);
    if let Err(error) = fs::create_dir_all(&root).and_then(|_| fs::write(&path, content.as_bytes()))
    {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "write_failed".to_owned(),
                message: error.to_string(),
                recovery: None,
            },
        );
    }
    success(
        request,
        operation_id,
        "sandbox_filesystem",
        EffectState::Changed,
        VerificationState::Verified,
        json!({ "path": path, "bytes": content.len() }),
    )
}

fn desktop_notify(request: &OperationRequest, operation_id: String) -> ActionResult {
    let title = request
        .params
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Comptrol");
    let body = request
        .params
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        let script = format!(
            "display notification {} with title {}",
            apple_quote(body),
            apple_quote(title)
        );
        match Command::new("osascript").args(["-e", &script]).status() {
            Ok(status) if status.success() => success(
                request,
                operation_id,
                "platform_notification",
                EffectState::Changed,
                VerificationState::Unverified,
                json!({ "sent": true }),
            ),
            Ok(_) | Err(_) => ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "verification_failed".to_owned(),
                    message: "The operating system did not confirm the notification".to_owned(),
                    recovery: Some(
                        "Inspect permissions and retry once the target is healthy".to_owned(),
                    ),
                },
            ),
        }
    } else {
        ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "unsupported_surface".to_owned(),
                message: "Desktop notifications are only implemented on macOS in this release"
                    .to_owned(),
                recovery: Some("Use desktop.observe or configure a platform adapter".to_owned()),
            },
        )
    }
}

fn apple_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', " ")
    )
}

pub fn capabilities() -> Vec<Capability> {
    vec![
        Capability {
            name: "system.ping".to_owned(),
            available: true,
            risk: Risk::R0,
            route: "native".to_owned(),
            note: "Local readiness check".to_owned(),
        },
        Capability {
            name: "desktop.observe".to_owned(),
            available: true,
            risk: Risk::R0,
            route: "platform_observe".to_owned(),
            note: "Reports current platform state and best effort application observation"
                .to_owned(),
        },
        Capability {
            name: "filesystem.write".to_owned(),
            available: false,
            risk: Risk::R1,
            route: "sandbox_filesystem".to_owned(),
            note: "Available only after local sandbox policy is enabled".to_owned(),
        },
        Capability {
            name: "desktop.notify".to_owned(),
            available: false,
            risk: Risk::R1,
            route: "platform_notification".to_owned(),
            note: "Available only after local notification policy is enabled".to_owned(),
        },
        Capability {
            name: "browser.cdp".to_owned(),
            available: false,
            risk: Risk::R1,
            route: "browser_protocol".to_owned(),
            note: "Reserved for the browser adapter milestone".to_owned(),
        },
        Capability {
            name: "desktop.semantic_input".to_owned(),
            available: false,
            risk: Risk::R2,
            route: "platform_accessibility".to_owned(),
            note: "Not advertised until a platform backend and verification suite exist".to_owned(),
        },
    ]
}

fn doctor(runtime: &Runtime) -> Value {
    json!({ "server": SERVER_VERSION, "protocol": PROTOCOL_VERSION, "platform": std::env::consts::OS, "architecture": std::env::consts::ARCH, "daemon": { "state": "in_process", "available": true }, "policy": { "max_risk": runtime.policy.max_risk, "sandbox_writes": runtime.policy.allow_sandbox_writes, "desktop_notify": runtime.policy.allow_desktop_notify }, "journal": { "available": true, "path": runtime.journal.path() }, "stop_latch": { "engaged": runtime.stop.engaged() }, "desktop_observation": { "available": true, "semantic_mutation": false }, "browser": { "available": false, "status": "not_configured" }, "remote": { "available": false, "binding": "loopback_only" }, "state_dir": state_dir() })
}

fn state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("COMPTROL_STATE_DIR") {
        return PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".comptrol");
    }
    PathBuf::from(".comptrol")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn restrict_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum WorkflowOp {
    Sense { key: String, value: Value },
    Assert { key: String, equals: Value },
    Set { key: String, value: Value },
    Wait { milliseconds: u64 },
    Return { value: Value },
}

pub fn execute_workflow(ops: &[WorkflowOp]) -> Result<Value, ComptrolError> {
    let mut memory = HashMap::<String, Value>::new();
    for op in ops {
        match op {
            WorkflowOp::Sense { key, value } | WorkflowOp::Set { key, value } => {
                memory.insert(key.clone(), value.clone());
            }
            WorkflowOp::Assert { key, equals } => {
                if memory.get(key) != Some(equals) {
                    return Err(ComptrolError {
                        code: "verification_failed".to_owned(),
                        message: format!("Workflow assertion failed for {key}"),
                        recovery: Some("Reobserve and compile a repaired branch".to_owned()),
                    });
                }
            }
            WorkflowOp::Wait { milliseconds } => {
                std::thread::sleep(Duration::from_millis((*milliseconds).min(60_000)))
            }
            WorkflowOp::Return { value } => return Ok(value.clone()),
        }
    }
    Ok(Value::Null)
}

pub fn default_state_dir() -> PathBuf {
    state_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn runtime() -> Runtime {
        Runtime::new(std::env::temp_dir().join(format!("comptrol-test-{}", now_ms())))
            .expect("runtime")
    }

    #[test]
    fn policy_denies_mutation_by_default() {
        let mut runtime = runtime();
        let result = runtime.operate(OperationRequest {
            intent: "desktop.click".to_owned(),
            target: None,
            params: Value::Null,
            postcondition: None,
            risk: None,
            idempotency_key: Some("deny".to_owned()),
            dry_run: false,
        });
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("policy_denied")
        );
        assert!(matches!(result.delivery, DeliveryState::Refused));
    }

    #[test]
    fn idempotency_replays_without_second_dispatch() {
        let mut runtime = runtime();
        let request = OperationRequest {
            intent: "system.ping".to_owned(),
            target: None,
            params: Value::Null,
            postcondition: None,
            risk: None,
            idempotency_key: Some("same".to_owned()),
            dry_run: false,
        };
        let first = runtime.operate(request.clone());
        let second = runtime.operate(request);
        assert_eq!(first.operation_id, second.operation_id);
        assert!(matches!(second.recovery, RecoveryState::IdempotentReplay));
    }

    #[test]
    fn stop_latch_blocks_mutation() {
        let mut runtime = runtime();
        runtime.stop.engage().expect("stop");
        runtime
            .policy
            .allowed_intents
            .insert("filesystem.write".to_owned());
        runtime.policy.max_risk = Risk::R1;
        let result = runtime.operate(OperationRequest {
            intent: "filesystem.write".to_owned(),
            target: None,
            params: json!({"path":"x","content":"secret"}),
            postcondition: None,
            risk: None,
            idempotency_key: Some("stopped".to_owned()),
            dry_run: false,
        });
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("stopped")
        );
    }

    #[test]
    fn lease_expires() {
        let mut leases = LeaseManager::new();
        let lease = leases.acquire("desktop", Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(3));
        assert!(!leases.valid(&lease.id, "desktop"));
    }

    #[test]
    fn workflow_asserts_before_returning() {
        let result = execute_workflow(&[
            WorkflowOp::Sense {
                key: "state".to_owned(),
                value: json!("ready"),
            },
            WorkflowOp::Assert {
                key: "state".to_owned(),
                equals: json!("ready"),
            },
            WorkflowOp::Return {
                value: json!({"verified": true}),
            },
        ])
        .expect("workflow");
        assert_eq!(result["verified"], true);
    }
}
