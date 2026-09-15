#![deny(unsafe_code)]

pub mod adapters;
pub mod browser;
pub mod checkpoints;
pub mod events;
pub mod geometry;
pub mod integration;
pub mod pairing;
pub mod trace;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use adapters::{AdapterDescriptor, AdapterRegistry};
pub use checkpoints::{Checkpoint, CheckpointStore};
pub use events::{Event, EventBus};
pub use geometry::{DisplayGeometry, Point, VirtualDesktop};
pub use trace::{TraceEntry, TraceMode, TraceRecorder, read_trace};

pub const PROTOCOL_VERSION: &str = "0.1";
pub const SERVER_VERSION: &str = "0.1.11";
pub const MAX_PROTOCOL_BYTES: usize = 1024 * 1024;

pub fn privacy_status() -> Value {
    json!({
        "telemetry": { "enabled": false, "default": false },
        "automatic_updates": { "enabled": false, "network_on_startup": false },
        "network": {
            "core_after_install": "none",
            "explicit_features": ["browser_cdp", "remote_host", "update", "package_operation"]
        },
        "redacted_by_default": [
            "screenshots", "ocr_text", "accessibility_tree_text", "typed_content",
            "clipboard", "credentials", "window_titles", "urls", "file_paths",
            "commands", "document_contents"
        ]
    })
}

pub fn privacy_network_endpoints() -> Value {
    json!({
        "endpoints": [
            {
                "name": "browser_cdp",
                "configured": std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some(),
                "address": std::env::var("COMPTROL_CDP_ENDPOINT").ok(),
                "reason": "Only used when an explicit browser action is requested"
            },
            {
                "name": "remote_host",
                "configured": false,
                "address": Value::Null,
                "reason": "Remote transport is disabled until mutual TLS is configured"
            },
            {
                "name": "update_service",
                "configured": false,
                "address": Value::Null,
                "reason": "Automatic update checks are disabled and no update endpoint is configured"
            },
            {
                "name": "telemetry",
                "configured": false,
                "address": Value::Null,
                "reason": "Opt in telemetry is not implemented"
            }
        ],
        "local_only": ["mcp_stdio", "loopback_dashboard", "local_state", "local_audit"]
    })
}

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
    #[serde(default)]
    pub background: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    NotDispatched,
    Delivered,
    Refused,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    NotAttempted,
    None,
    Changed,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    NotAttempted,
    Verified,
    Unverified,
    Failed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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
pub struct BrowserTarget {
    pub id: String,
    pub target_type: Option<String>,
    pub browser_context_id: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub revision: Option<String>,
    pub web_socket_url: Option<String>,
}

pub fn bind_browser_target(
    targets: &[BrowserTarget],
    target_id: &str,
    browser_context_id: Option<&str>,
    revision: Option<&str>,
) -> Result<BrowserTarget, ComptrolError> {
    let Some(target) = targets.iter().find(|target| target.id == target_id) else {
        return Err(ComptrolError {
            code: "target_gone".to_owned(),
            message: "The requested browser target is not present".to_owned(),
            recovery: Some("Refresh browser targets before mutation".to_owned()),
        });
    };
    if target
        .target_type
        .as_deref()
        .is_some_and(|target_type| target_type != "page")
    {
        return Err(ComptrolError {
            code: "wrong_target_type".to_owned(),
            message: "The requested browser target is not a page".to_owned(),
            recovery: Some("Select an exact page target before mutation".to_owned()),
        });
    }
    if browser_context_id
        .is_some_and(|expected| target.browser_context_id.as_deref() != Some(expected))
        || revision.is_some_and(|expected| target.revision.as_deref() != Some(expected))
    {
        return Err(ComptrolError {
            code: "stale_reference".to_owned(),
            message: "The browser target context or revision changed".to_owned(),
            recovery: Some("Inspect browser targets and bind again".to_owned()),
        });
    }
    Ok(target.clone())
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
                "platform.broker.observe".to_owned(),
                "browser.cdp.wait_for".to_owned(),
                "browser.cdp.accessibility_snapshot".to_owned(),
                "browser.cdp.reopen_closed_group".to_owned(),
                "workflow.execute".to_owned(),
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
            policy.allowed_intents.insert("filesystem.copy".to_owned());
            policy
                .allowed_intents
                .insert("filesystem.restore_checkpoint".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_DESKTOP_NOTIFY").as_deref() == Ok("1") {
            policy.allow_desktop_notify = true;
            policy.max_risk = Risk::R1;
            policy.allowed_intents.insert("desktop.notify".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_MACOS_AX").as_deref() == Ok("1") {
            policy.max_risk = Risk::R2;
            policy.allowed_intents.insert("macos.ax.press".to_owned());
            policy
                .allowed_intents
                .insert("macos.ax.set_value".to_owned());
            policy
                .allowed_intents
                .insert("browser.chrome.reopen_closed_group".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_APP_LAUNCH").as_deref() == Ok("1") {
            policy.max_risk = Risk::R2;
            policy.allowed_intents.insert("desktop.open_app".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_COMMANDS").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R3);
            policy.allowed_intents.insert("command.run".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_WINDOWS_UIA").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy
                .allowed_intents
                .insert("windows.uia.press".to_owned());
            policy
                .allowed_intents
                .insert("windows.uia.set_value".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_LINUX_ATSPI").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy
                .allowed_intents
                .insert("linux.atspi.press".to_owned());
            policy
                .allowed_intents
                .insert("linux.atspi.set_value".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_BROWSER_FIXTURE").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R1);
            policy
                .allowed_intents
                .insert("browser.fixture.submit".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy.allowed_intents.extend([
                "browser.cdp.evaluate".to_owned(),
                "browser.cdp.navigate".to_owned(),
                "browser.cdp.upload".to_owned(),
                "browser.cdp.download".to_owned(),
                "browser.cdp.fill".to_owned(),
                "browser.cdp.click".to_owned(),
                "browser.cdp.focus".to_owned(),
                "browser.cdp.open_tab".to_owned(),
                "browser.cdp.close_tab".to_owned(),
                "browser.cdp.history_back".to_owned(),
                "browser.cdp.history_forward".to_owned(),
                "browser.cdp.semantic_click".to_owned(),
                "browser.cdp.workflow".to_owned(),
            ]);
        }
        if std::env::var("COMPTROL_ALLOW_BROWSER_LAUNCH").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy
                .allowed_intents
                .insert("browser.chrome.open_tab".to_owned());
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DurableState {
    Prepared,
    Authorized,
    Dispatched,
    Observed,
    Verified,
    Committed,
    Failed,
    Interrupted,
    Complete,
    Unknown,
    RollbackPending,
    RolledBack,
    Reconciled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DurableOperation {
    pub operation_id: String,
    pub idempotency_key: Option<String>,
    pub intent: String,
    pub risk: Risk,
    pub target: Option<Target>,
    pub state: DurableState,
    pub metadata: Value,
    pub result: Option<ActionResult>,
}

#[derive(Debug)]
pub struct OperationJournal {
    path: PathBuf,
    file: File,
    records: HashMap<String, DurableOperation>,
}

impl OperationJournal {
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(state_dir)?;
        let path = state_dir.join("operations.jsonl");
        let mut records = HashMap::new();
        if path.exists() {
            let file = File::open(&path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if let Ok(record) = serde_json::from_str::<DurableOperation>(&line) {
                    records.insert(record.operation_id.clone(), record);
                }
            }
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let pending = records
            .values()
            .filter(|record| {
                matches!(
                    record.state,
                    DurableState::Prepared | DurableState::Authorized | DurableState::Dispatched
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        for mut record in pending {
            record.state = if record.state == DurableState::Dispatched {
                DurableState::Unknown
            } else {
                DurableState::Interrupted
            };
            serde_json::to_writer(&mut file, &record)?;
            file.write_all(b"\n")?;
            records.insert(record.operation_id.clone(), record);
        }
        file.flush()?;
        Ok(Self {
            path,
            file,
            records,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, operation_id: &str) -> Option<&DurableOperation> {
        self.records.get(operation_id)
    }

    pub fn completed(&self) -> impl Iterator<Item = &DurableOperation> {
        self.records.values().filter(|record| {
            record.result.is_some()
                && matches!(
                    record.state,
                    DurableState::Committed
                        | DurableState::Observed
                        | DurableState::Complete
                        | DurableState::Failed
                        | DurableState::RolledBack
                        | DurableState::Reconciled
                )
        })
    }

    pub fn prepare(
        &mut self,
        request: &OperationRequest,
        operation_id: &str,
        risk: Risk,
    ) -> io::Result<()> {
        self.write(DurableOperation {
            operation_id: operation_id.to_owned(),
            idempotency_key: request.idempotency_key.clone(),
            intent: request.intent.clone(),
            risk,
            target: request.target.clone(),
            state: DurableState::Prepared,
            metadata: operation_metadata(request),
            result: None,
        })
    }

    pub fn dispatched(&mut self, operation_id: &str) -> io::Result<()> {
        self.update(operation_id, |record| {
            record.state = DurableState::Dispatched
        })
    }

    pub fn authorized(&mut self, operation_id: &str) -> io::Result<()> {
        self.update(operation_id, |record| {
            record.state = DurableState::Authorized
        })
    }

    pub fn complete(
        &mut self,
        request: &OperationRequest,
        result: &ActionResult,
    ) -> io::Result<()> {
        let state = match (&result.delivery, &result.verification, &result.error) {
            (DeliveryState::Unknown, _, _) => DurableState::Unknown,
            (DeliveryState::Refused, _, _) => DurableState::Failed,
            (DeliveryState::Delivered, VerificationState::Verified, None) => {
                DurableState::Committed
            }
            (DeliveryState::Delivered, VerificationState::Failed, _) => DurableState::Failed,
            (DeliveryState::Delivered, _, _) => DurableState::Observed,
            (DeliveryState::NotDispatched, _, _) => DurableState::Failed,
        };
        self.write(DurableOperation {
            operation_id: result.operation_id.clone(),
            idempotency_key: request.idempotency_key.clone(),
            intent: request.intent.clone(),
            risk: request.risk.unwrap_or_else(|| classify(&request.intent)),
            target: request.target.clone(),
            state,
            metadata: operation_metadata(request),
            result: Some(result.clone()),
        })
    }

    pub fn reconciled(
        &mut self,
        operation: DurableOperation,
        result: ActionResult,
    ) -> io::Result<()> {
        self.write(DurableOperation {
            state: DurableState::Reconciled,
            result: Some(result),
            ..operation
        })
    }

    fn update<F>(&mut self, operation_id: &str, mutate: F) -> io::Result<()>
    where
        F: FnOnce(&mut DurableOperation),
    {
        let mut record = self
            .records
            .get(operation_id)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "operation not found"))?;
        mutate(&mut record);
        self.write(record)
    }

    fn write(&mut self, record: DurableOperation) -> io::Result<()> {
        serde_json::to_writer(&mut self.file, &record)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.records.insert(record.operation_id.clone(), record);
        Ok(())
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
    pub operations: OperationJournal,
    pub checkpoints: CheckpointStore,
    pub adapters: AdapterRegistry,
    pub events: EventBus,
    pub trace: Option<TraceRecorder>,
    pub stop: StopLatch,
    idempotent: HashMap<String, ActionResult>,
    sequence: u64,
}

impl Runtime {
    pub fn new(state_dir: PathBuf) -> io::Result<Self> {
        let trace = std::env::var_os("COMPTROL_TRACE_PATH")
            .map(PathBuf::from)
            .map(|path| {
                TraceRecorder::open(
                    path,
                    match std::env::var("COMPTROL_TRACE_MODE").as_deref() {
                        Ok("developer") => TraceMode::Developer,
                        Ok("fixture_full") => TraceMode::FixtureFull,
                        _ => TraceMode::PrivacyMinimal,
                    },
                )
            })
            .transpose()?;
        Self::build(state_dir, trace)
    }

    pub fn with_trace(state_dir: PathBuf, path: PathBuf, mode: TraceMode) -> io::Result<Self> {
        Self::build(state_dir, Some(TraceRecorder::open(path, mode)?))
    }

    fn build(state_dir: PathBuf, trace: Option<TraceRecorder>) -> io::Result<Self> {
        let operations = OperationJournal::open(&state_dir)?;
        let checkpoints = CheckpointStore::new(&state_dir)?;
        let mut idempotent = HashMap::new();
        for record in operations.completed() {
            if let (Some(key), Some(result)) = (&record.idempotency_key, &record.result) {
                idempotent.insert(key.clone(), result.clone());
            }
        }
        Ok(Self {
            policy: Policy::from_environment(),
            leases: LeaseManager::new(),
            journal: AuditJournal::open(&state_dir)?,
            operations,
            checkpoints,
            adapters: AdapterRegistry::builtin(),
            events: EventBus::default(),
            trace,
            stop: StopLatch::new(&state_dir),
            idempotent,
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
        if let Some(background) = request.background.as_deref()
            && !matches!(
                background,
                "strict_background"
                    | "prefer_background"
                    | "foreground_allowed"
                    | "foreground_required"
            )
        {
            let result = ActionResult::refused(
                &request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Unknown background posture".to_owned(),
                    recovery: Some("Use strict_background, prefer_background, foreground_allowed, or foreground_required".to_owned()),
                },
            );
            self.remember(&request, result.clone());
            return result;
        }
        if let Some(key) = request.idempotency_key.as_ref()
            && let Some(record) = self
                .operations
                .records
                .values()
                .find(|record| record.idempotency_key.as_ref() == Some(key))
            && matches!(
                record.state,
                DurableState::Dispatched | DurableState::Unknown
            )
        {
            return unknown_result(&request, record.operation_id.clone());
        }
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
        if risk.mutation()
            && let Err(error) = self
                .operations
                .prepare(&request, &operation_id, risk)
                .and_then(|_| self.operations.authorized(&operation_id))
                .and_then(|_| self.operations.dispatched(&operation_id))
        {
            let result = ActionResult::refused(
                &request,
                operation_id,
                ComptrolError {
                    code: "recovery_unavailable".to_owned(),
                    message: error.to_string(),
                    recovery: Some(
                        "Repair the local operation journal before allowing mutations".to_owned(),
                    ),
                },
            );
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
            "workflow.execute" => execute_workflow_request(&request, operation_id),
            "desktop.observe" => desktop_observe(&request, operation_id),
            "platform.broker.observe" => platform_broker_observe(&request, operation_id),
            "filesystem.write" => sandbox_write(&request, operation_id, &self.checkpoints),
            "filesystem.copy" => sandbox_copy(&request, operation_id, &self.checkpoints),
            "filesystem.restore_checkpoint" => {
                restore_checkpoint(&request, operation_id, &self.checkpoints)
            }
            "desktop.notify" => desktop_notify(&request, operation_id),
            "desktop.open_app" => desktop_open_app(&request, operation_id),
            "browser.chrome.open_tab" => browser_chrome_open_tab(&request, operation_id),
            "browser.chrome.reopen_closed_group" => {
                browser_chrome_reopen_closed_group(&request, operation_id)
            }
            "command.run" => command_run(&request, operation_id),
            "windows.uia.press" | "windows.uia.set_value" => {
                windows_uia_action(&request, operation_id)
            }
            "linux.atspi.press" | "linux.atspi.set_value" => {
                linux_atspi_action(&request, operation_id)
            }
            "macos.ax.press" => macos_ax_press(&request, operation_id),
            "macos.ax.set_value" => macos_ax_set_value(&request, operation_id),
            "browser.fixture.submit" => browser_fixture_submit(&request, operation_id),
            "browser.cdp.reopen_closed_group" => {
                browser_closed_group_unsupported(&request, operation_id)
            }
            "browser.cdp.evaluate"
            | "browser.cdp.navigate"
            | "browser.cdp.upload"
            | "browser.cdp.download"
            | "browser.cdp.fill"
            | "browser.cdp.click"
            | "browser.cdp.focus"
            | "browser.cdp.open_tab"
            | "browser.cdp.close_tab"
            | "browser.cdp.history_back"
            | "browser.cdp.history_forward"
            | "browser.cdp.semantic_click"
            | "browser.cdp.workflow"
            | "browser.cdp.accessibility_snapshot"
            | "browser.cdp.wait_for" => browser_cdp_action(&request, operation_id),
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
            "platform" => platform_diagnostics(),
            "browser" => match std::env::var("COMPTROL_CDP_ENDPOINT") {
                Ok(endpoint) => match browser::discover(&endpoint) {
                    Ok(targets) => json!({ "endpoint": endpoint, "targets": targets }),
                    Err(error) => json!({ "endpoint": endpoint, "error": error }),
                },
                Err(_) => {
                    json!({ "available": false, "reason": "COMPTROL_CDP_ENDPOINT is not configured" })
                }
            },
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
                        background: None,
                    },
                    self.next_operation_id(),
                )
                .data
            }
            "status" => {
                json!({ "protocol": PROTOCOL_VERSION, "server": SERVER_VERSION, "stop_latched": self.stop.engaged(), "audit_path": self.journal.path() })
            }
            "events" => json!(self.events.since(0, None)),
            "checkpoints" => json!({ "path": self.checkpoints.path() }),
            "adapters" => json!(self.adapters.list()),
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
        } else if let Some(record) = self.operations.record(operation_id) {
            json!({ "state": format!("{:?}", record.state).to_lowercase(), "operation_id": operation_id, "reconcile_required": matches!(record.state, DurableState::Dispatched | DurableState::Unknown), "metadata": record.metadata })
        } else {
            json!({ "state": "unknown", "operation_id": operation_id, "error": "operation_unknown" })
        }
    }

    pub fn reconcile(&mut self, operation_id: &str) -> Value {
        let Some(record) = self.operations.record(operation_id).cloned() else {
            return json!({ "state": "unknown", "operation_id": operation_id, "error": "operation_unknown" });
        };
        if !matches!(
            record.state,
            DurableState::Dispatched | DurableState::Unknown
        ) {
            return self.watch(operation_id);
        }
        if record.intent == "filesystem.write"
            || record.intent == "filesystem.copy"
            || record.intent == "browser.cdp.download"
        {
            let path = record
                .metadata
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let expected_hash = record.metadata.get("content_hash").and_then(Value::as_u64);
            let present = path.as_ref().is_some_and(|candidate| candidate.is_file());
            let hash_matches = match (path.as_ref(), expected_hash.as_ref()) {
                (Some(candidate), Some(expected)) => {
                    fs::read(candidate).is_ok_and(|bytes| stable_hash(&bytes) == *expected)
                }
                _ => false,
            };
            if let (Some(path), true) = (path, present && (expected_hash.is_none() || hash_matches))
            {
                let result = ActionResult {
                    operation_id: record.operation_id.clone(),
                    intent: record.intent.clone(),
                    route: "recovery_observation".to_owned(),
                    target: record.target.clone(),
                    preflight: "reconciled".to_owned(),
                    delivery: DeliveryState::Delivered,
                    effect: EffectState::Changed,
                    verification: VerificationState::Verified,
                    disturbance: json!({ "foreground_changed": false }),
                    recovery: RecoveryState::None,
                    data: json!({ "path": path, "reconciled": true, "postcondition": "file_present" }),
                    error: None,
                };
                let idempotency_key = record.idempotency_key.clone();
                if let Err(error) = self.operations.reconciled(record, result.clone()) {
                    return json!({ "state": "unknown", "operation_id": operation_id, "error": { "code": "recovery_write_failed", "message": error.to_string() } });
                }
                if let Some(key) = idempotency_key {
                    self.idempotent.insert(key, result.clone());
                }
                return json!({ "state": "reconciled", "result": result });
            }
        }
        if record.intent == "browser.fixture.submit"
            && let (Some(endpoint), Some(key)) = (
                std::env::var_os("COMPTROL_CDP_ENDPOINT"),
                record.idempotency_key.as_deref(),
            )
        {
            match browser::fixture_state(&endpoint.to_string_lossy()) {
                Ok(state)
                    if state
                        .get("submissions")
                        .and_then(Value::as_array)
                        .is_some_and(|submissions| {
                            submissions.iter().any(|submission| {
                                submission.get("idempotency_key").and_then(Value::as_str)
                                    == Some(key)
                            })
                        }) =>
                {
                    let result = ActionResult {
                        operation_id: record.operation_id.clone(),
                        intent: record.intent.clone(),
                        route: "recovery_observation".to_owned(),
                        target: record.target.clone(),
                        preflight: "reconciled".to_owned(),
                        delivery: DeliveryState::Delivered,
                        effect: EffectState::Changed,
                        verification: VerificationState::Verified,
                        disturbance: json!({ "foreground_changed": false }),
                        recovery: RecoveryState::None,
                        data: json!({ "reconciled": true, "idempotency_key": key }),
                        error: None,
                    };
                    let idempotency_key = record.idempotency_key.clone();
                    if let Err(error) = self.operations.reconciled(record, result.clone()) {
                        return json!({ "state": "unknown", "operation_id": operation_id, "error": { "code": "recovery_write_failed", "message": error.to_string() } });
                    }
                    if let Some(key) = idempotency_key {
                        self.idempotent.insert(key, result.clone());
                    }
                    return json!({ "state": "reconciled", "result": result });
                }
                Ok(_) => {}
                Err(error) => {
                    return json!({ "state": "unknown", "operation_id": operation_id, "error": error });
                }
            }
        }
        if (record.intent == "desktop.open_app"
            || record.intent == "macos.ax.press"
            || record.intent == "macos.ax.set_value")
            && cfg!(target_os = "macos")
        {
            let confirmed = if record.intent == "desktop.open_app" {
                record
                    .metadata
                    .get("app")
                    .and_then(Value::as_str)
                    .map(|app| {
                        let script = format!(
                            "tell application \"System Events\" to exists process {}",
                            apple_quote(app)
                        );
                        run_osascript(&script).is_ok_and(|output| {
                            output.status.success()
                                && String::from_utf8_lossy(&output.stdout).trim() == "true"
                        })
                    })
                    .unwrap_or(false)
            } else {
                ax_reconcile(&record.metadata)
            };
            if confirmed {
                let result = ActionResult {
                    operation_id: record.operation_id.clone(),
                    intent: record.intent.clone(),
                    route: "recovery_observation".to_owned(),
                    target: record.target.clone(),
                    preflight: "reconciled".to_owned(),
                    delivery: DeliveryState::Delivered,
                    effect: EffectState::Changed,
                    verification: VerificationState::Verified,
                    disturbance: json!({ "foreground_changed": false }),
                    recovery: RecoveryState::None,
                    data: json!({ "reconciled": true, "postcondition": "observed" }),
                    error: None,
                };
                let idempotency_key = record.idempotency_key.clone();
                if let Err(error) = self.operations.reconciled(record, result.clone()) {
                    return json!({ "state": "unknown", "operation_id": operation_id, "error": { "code": "recovery_write_failed", "message": error.to_string() } });
                }
                if let Some(key) = idempotency_key {
                    self.idempotent.insert(key, result.clone());
                }
                return json!({ "state": "reconciled", "result": result });
            }
        }
        json!({ "state": "unknown", "operation_id": operation_id, "error": "operation_unknown", "reconcile_required": true })
    }

    fn next_operation_id(&mut self) -> String {
        self.sequence += 1;
        format!("op-{}-{}", now_ms(), self.sequence)
    }

    fn remember(&mut self, request: &OperationRequest, result: ActionResult) {
        if let Some(key) = request.idempotency_key.as_ref()
            && !matches!(&result.delivery, DeliveryState::Unknown)
        {
            self.idempotent.insert(key.clone(), result.clone());
        }
        if let Err(error) = self.operations.complete(request, &result) {
            eprintln!("comptrol operation journal error: {error}");
        }
        if let Err(error) = self.journal.append(&result) {
            eprintln!("comptrol audit journal error: {error}");
        }
        self.events.emit(
            "operation.completed",
            json!({ "operation_id": result.operation_id, "intent": result.intent, "verification": result.verification }),
        );
        if let Some(trace) = &self.trace
            && let Err(error) = trace.append(request, &result)
        {
            eprintln!("comptrol trace error: {error}");
        }
    }
}

fn classify(intent: &str) -> Risk {
    match intent {
        "system.ping" | "desktop.observe" | "platform.broker.observe" | "workflow.execute" => {
            Risk::R0
        }
        "desktop.notify"
        | "filesystem.write"
        | "filesystem.copy"
        | "filesystem.restore_checkpoint" => Risk::R1,
        "desktop.open_app"
        | "macos.ax.press"
        | "macos.ax.set_value"
        | "browser.chrome.open_tab"
        | "browser.chrome.reopen_closed_group" => Risk::R2,
        "command.run" => Risk::R3,
        "windows.uia.press" | "windows.uia.set_value" => Risk::R2,
        "linux.atspi.press" | "linux.atspi.set_value" => Risk::R2,
        "browser.fixture.submit" => Risk::R1,
        "browser.cdp.evaluate"
        | "browser.cdp.navigate"
        | "browser.cdp.upload"
        | "browser.cdp.download"
        | "browser.cdp.fill"
        | "browser.cdp.click"
        | "browser.cdp.focus"
        | "browser.cdp.open_tab"
        | "browser.cdp.close_tab"
        | "browser.cdp.history_back"
        | "browser.cdp.history_forward"
        | "browser.cdp.semantic_click" => Risk::R2,
        "browser.cdp.workflow" => Risk::R2,
        "browser.cdp.wait_for"
        | "browser.cdp.accessibility_snapshot"
        | "browser.cdp.reopen_closed_group" => Risk::R0,
        _ => Risk::R2,
    }
}

fn unknown_result(request: &OperationRequest, operation_id: String) -> ActionResult {
    ActionResult {
        operation_id,
        intent: request.intent.clone(),
        route: "recovery_observation".to_owned(),
        target: request.target.clone(),
        preflight: "unknown_after_restart".to_owned(),
        delivery: DeliveryState::Unknown,
        effect: EffectState::Unknown,
        verification: VerificationState::Unverified,
        disturbance: json!({
            "foreground_changed": false,
            "mouse": "untouched",
            "clipboard": "untouched",
            "posture": request.background.as_deref().unwrap_or("foreground_allowed")
        }),
        recovery: RecoveryState::RequiresReconciliation,
        data: Value::Null,
        error: Some(ComptrolError {
            code: "operation_unknown".to_owned(),
            message: "The operation was recorded as dispatched before the previous runtime stopped"
                .to_owned(),
            recovery: Some("Call reconcile after observing the target state".to_owned()),
        }),
    }
}

fn operation_metadata(request: &OperationRequest) -> Value {
    let mut metadata = json!({
        "target_kind": request.target.as_ref().map(|target| target.kind.clone()),
    });
    if let Some(background) = request.background.as_deref() {
        metadata["background"] = json!(background);
    }
    if request.intent == "filesystem.write" {
        if let Some(path) = request.params.get("path").and_then(Value::as_str) {
            metadata["path"] = json!(state_dir().join("sandbox").join(path));
        }
        if let Some(content) = request.params.get("content").and_then(Value::as_str) {
            metadata["content_len"] = json!(content.len());
            metadata["content_hash"] = json!(stable_hash(content.as_bytes()));
        }
    }
    if request.intent == "filesystem.copy" {
        if let Some(destination) = request.params.get("destination").and_then(Value::as_str) {
            metadata["path"] = json!(state_dir().join("sandbox").join(destination));
        }
        if let Some(source) = request.params.get("source").and_then(Value::as_str)
            && let Ok(path) = fs::canonicalize(state_dir().join("sandbox").join(source))
            && let Ok(bytes) = fs::read(path)
        {
            metadata["content_hash"] = json!(stable_hash(&bytes));
            metadata["content_len"] = json!(bytes.len());
        }
    }
    if request.intent == "browser.cdp.download" {
        let key = request.idempotency_key.as_deref().unwrap_or("unkeyed");
        let file_name = request
            .params
            .get("file_name")
            .and_then(Value::as_str)
            .unwrap_or("fixture.txt");
        if Path::new(file_name)
            .file_name()
            .and_then(|name| name.to_str())
            == Some(file_name)
        {
            metadata["path"] = json!(
                state_dir()
                    .join("sandbox")
                    .join("downloads")
                    .join(format!("{:016x}", stable_hash(key.as_bytes())))
                    .join(file_name)
            );
            metadata["file_name"] = json!(file_name);
        }
    }
    if request.intent == "browser.fixture.submit" {
        for (key, parameter) in [
            ("target_id", "target_id"),
            ("browser_context_id", "browser_context_id"),
            ("revision", "revision"),
        ] {
            if let Some(value) = request.params.get(parameter).and_then(Value::as_str) {
                metadata[key] = json!(value);
            }
        }
    }
    if request.intent == "desktop.open_app"
        && let Some(app) = request.params.get("app").and_then(Value::as_str)
    {
        metadata["app"] = json!(app);
    }
    if request.intent == "macos.ax.press" || request.intent == "macos.ax.set_value" {
        for key in ["app", "control", "role", "window"] {
            if let Some(value) = request.params.get(key).and_then(Value::as_str) {
                metadata[key] = json!(value);
            }
        }
        if metadata.get("role").is_none() {
            metadata["role"] = json!("button");
        }
        if let Some(postcondition) = request.postcondition.as_ref()
            && let Some(attribute) = postcondition.get("attribute").and_then(Value::as_str)
        {
            metadata["postcondition_attribute"] = json!(attribute);
            if let Some(expected) = postcondition.get("equals") {
                if let Some(value) = expected.as_str() {
                    metadata["postcondition_hash"] = json!(stable_hash(value.as_bytes()));
                    metadata["postcondition_len"] = json!(value.len());
                } else if let Some(value) = expected.as_bool() {
                    metadata["postcondition_bool"] = json!(value);
                }
            }
        }
        if request.intent == "macos.ax.set_value"
            && let Some(value) = request.params.get("value").and_then(Value::as_str)
        {
            metadata["value_hash"] = json!(stable_hash(value.as_bytes()));
            metadata["value_len"] = json!(value.len());
        }
        metadata["action"] = json!(request.intent.strip_prefix("macos.ax.").unwrap_or_default());
    }
    if request.intent == "browser.chrome.reopen_closed_group"
        && let Some(group) = request.params.get("group").and_then(Value::as_str)
    {
        metadata["app"] = json!("Google Chrome");
        metadata["control"] = json!(format!("{group} group Closed"));
        metadata["role"] = json!("button");
        if let Some(window) = request.params.get("window").and_then(Value::as_str) {
            metadata["window"] = json!(window);
        }
        metadata["group"] = json!(group);
        metadata["action"] = json!("reopen_closed_group");
        metadata["postcondition_attribute"] = json!("closed_group_absent");
        metadata["postcondition_bool"] = json!(true);
    }
    if request.intent == "command.run" {
        if let Some(program) = request.params.get("program").and_then(Value::as_str) {
            metadata["program"] = json!(program);
        }
        if let Some(args) = request.params.get("args").and_then(Value::as_array) {
            metadata["args_count"] = json!(args.len());
            if let Ok(bytes) = serde_json::to_vec(args) {
                metadata["args_hash"] = json!(stable_hash(&bytes));
            }
        }
        if let Some(cwd) = request.params.get("cwd").and_then(Value::as_str) {
            metadata["cwd"] = json!(cwd);
        }
    }
    metadata
}

fn stable_hash(bytes: &[u8]) -> u64 {
    // ponytail: local reconciliation fingerprint, replace with a cryptographic digest when remote integrity is added
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn route_for(intent: &str) -> String {
    match intent {
        "system.ping" => "native",
        "desktop.observe" => "platform_observe",
        "platform.broker.observe" => "platform_broker",
        "workflow.execute" => "workflow",
        "filesystem.write" => "sandbox_filesystem",
        "filesystem.copy" => "sandbox_filesystem",
        "filesystem.restore_checkpoint" => "sandbox_checkpoint",
        "desktop.notify" => "platform_notification",
        "desktop.open_app" => "platform_launch",
        "browser.chrome.open_tab" => "browser_launcher",
        "browser.chrome.reopen_closed_group" => "chrome_ax",
        "command.run" => "process_argv",
        "windows.uia.press" | "windows.uia.set_value" => "windows_uia",
        "linux.atspi.press" | "linux.atspi.set_value" => "linux_atspi",
        "macos.ax.press" | "macos.ax.set_value" => "macos_ax",
        "browser.fixture.submit" => "browser_fixture",
        "browser.cdp.evaluate"
        | "browser.cdp.navigate"
        | "browser.cdp.upload"
        | "browser.cdp.download"
        | "browser.cdp.fill"
        | "browser.cdp.click"
        | "browser.cdp.focus"
        | "browser.cdp.open_tab"
        | "browser.cdp.close_tab"
        | "browser.cdp.history_back"
        | "browser.cdp.history_forward"
        | "browser.cdp.semantic_click"
        | "browser.cdp.workflow"
        | "browser.cdp.accessibility_snapshot"
        | "browser.cdp.reopen_closed_group"
        | "browser.cdp.wait_for" => "browser_protocol",
        _ => "none",
    }
    .to_owned()
}

fn restore_checkpoint(
    request: &OperationRequest,
    operation_id: String,
    checkpoints: &CheckpointStore,
) -> ActionResult {
    let Some(id) = request.params.get("checkpoint").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Checkpoint restore needs a checkpoint id".to_owned(),
                recovery: None,
            },
        );
    };
    match checkpoints.restore_id(id) {
        Ok(checkpoint) => success(
            request,
            operation_id,
            "sandbox_checkpoint",
            EffectState::Changed,
            VerificationState::Verified,
            json!({ "checkpoint": checkpoint.id, "path": checkpoint.source }),
        ),
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "checkpoint_restore_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect available local checkpoints".to_owned()),
            },
        ),
    }
}

fn execute_workflow_request(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(ops) = request.params.get("ops") else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Workflow execution needs an ops array".to_owned(),
                recovery: None,
            },
        );
    };
    let Ok(ops) = serde_json::from_value::<Vec<WorkflowOp>>(ops.clone()) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Workflow ops did not match the closed workflow schema".to_owned(),
                recovery: None,
            },
        );
    };
    match execute_workflow(&ops) {
        Ok(value) => success(
            request,
            operation_id,
            "workflow",
            EffectState::None,
            VerificationState::Verified,
            value,
        ),
        Err(error) => ActionResult {
            operation_id,
            intent: request.intent.clone(),
            route: "workflow".to_owned(),
            target: request.target.clone(),
            preflight: "passed".to_owned(),
            delivery: DeliveryState::Delivered,
            effect: EffectState::None,
            verification: VerificationState::Failed,
            disturbance: json!({ "foreground_changed": false }),
            recovery: RecoveryState::None,
            data: Value::Null,
            error: Some(error),
        },
    }
}

fn browser_fixture_submit(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(endpoint) = std::env::var_os("COMPTROL_CDP_ENDPOINT") else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "browser_unavailable".to_owned(),
                message: "COMPTROL_CDP_ENDPOINT is not configured".to_owned(),
                recovery: Some("Configure a local browser fixture endpoint".to_owned()),
            },
        );
    };
    let Some(key) = request
        .idempotency_key
        .as_deref()
        .filter(|key| !key.is_empty())
    else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "idempotency_required".to_owned(),
                message: "Browser submission requires an idempotency key".to_owned(),
                recovery: Some("Retry with a stable idempotency key".to_owned()),
            },
        );
    };
    let target_id = request.params.get("target_id").and_then(Value::as_str);
    let browser_context_id = request
        .params
        .get("browser_context_id")
        .and_then(Value::as_str);
    let revision = request.params.get("revision").and_then(Value::as_str);
    let message = request
        .params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (Some(target_id), Some(browser_context_id), Some(revision)) =
        (target_id, browser_context_id, revision)
    else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser submission needs target id, browser context, and revision"
                    .to_owned(),
                recovery: Some("Inspect browser targets before mutation".to_owned()),
            },
        );
    };
    match browser::fixture_submit(
        &endpoint.to_string_lossy(),
        target_id,
        browser_context_id,
        revision,
        key,
        message,
    ) {
        Ok(data) => success(
            request,
            operation_id,
            "browser_fixture",
            if data.get("state").and_then(Value::as_str) == Some("replayed") {
                EffectState::None
            } else {
                EffectState::Changed
            },
            VerificationState::Verified,
            data,
        ),
        Err(error) => browser_failure(request, operation_id, error),
    }
}

fn browser_cdp_action(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(endpoint) = std::env::var_os("COMPTROL_CDP_ENDPOINT") else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "browser_unavailable".to_owned(),
                message: "COMPTROL_CDP_ENDPOINT is not configured".to_owned(),
                recovery: Some("Configure a local browser DevTools endpoint".to_owned()),
            },
        );
    };
    if request.intent == "browser.cdp.open_tab" {
        let Some(url) = request.params.get("url").and_then(Value::as_str) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Opening a browser tab needs a URL".to_owned(),
                    recovery: None,
                },
            );
        };
        let background = request
            .params
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if request.background.as_deref() == Some("strict_background") && !background {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "background_unavailable".to_owned(),
                    message: "Strict background browser opening requires background true"
                        .to_owned(),
                    recovery: Some(
                        "Request a background tab through the existing browser profile".to_owned(),
                    ),
                },
            );
        }
        let background = background || request.background.as_deref() == Some("strict_background");
        let browser_context_id = request
            .params
            .get("browser_context_id")
            .and_then(Value::as_str);
        return match browser::open_tab(
            &endpoint.to_string_lossy(),
            url,
            background,
            browser_context_id,
        ) {
            Ok(data) => success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                VerificationState::Verified,
                data,
            ),
            Err(error) => browser_failure(request, operation_id, error),
        };
    }
    if request.intent == "browser.cdp.semantic_click" {
        return browser_cdp_semantic_click(request, operation_id, &endpoint);
    }
    if request.intent == "browser.cdp.workflow" {
        return browser_cdp_workflow(request, operation_id, &endpoint);
    }
    let Some(target_id) = request.params.get("target_id").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser CDP actions need a target id".to_owned(),
                recovery: Some("Inspect browser targets before mutation".to_owned()),
            },
        );
    };
    let Some(browser_context_id) = request
        .params
        .get("browser_context_id")
        .and_then(Value::as_str)
    else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser CDP actions need a browser context id".to_owned(),
                recovery: Some("Inspect targets and include the exact browser context".to_owned()),
            },
        );
    };
    let Some(revision) = request.params.get("revision").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser CDP actions need a target revision".to_owned(),
                recovery: Some("Inspect targets and include the exact target revision".to_owned()),
            },
        );
    };
    if request.intent == "browser.cdp.close_tab" {
        return match browser::close_tab(
            &endpoint.to_string_lossy(),
            target_id,
            browser_context_id,
            revision,
        ) {
            Ok(data) => success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                VerificationState::Verified,
                data,
            ),
            Err(error) => browser_failure(request, operation_id, error),
        };
    }
    if matches!(
        request.intent.as_str(),
        "browser.cdp.history_back" | "browser.cdp.history_forward"
    ) {
        return match browser::history(
            &endpoint.to_string_lossy(),
            target_id,
            browser_context_id,
            revision,
            request.intent == "browser.cdp.history_forward",
        ) {
            Ok(data) => success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                VerificationState::Verified,
                data,
            ),
            Err(error) => browser_failure(request, operation_id, error),
        };
    }
    if request.intent == "browser.cdp.accessibility_snapshot" {
        let depth = request
            .params
            .get("depth")
            .and_then(Value::as_u64)
            .unwrap_or(8)
            .min(20);
        return match browser::cdp_call(
            &endpoint.to_string_lossy(),
            target_id,
            Some(browser_context_id),
            Some(revision),
            "Accessibility.getFullAXTree",
            json!({ "depth": depth }),
        ) {
            Ok(data) => success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::None,
                VerificationState::Verified,
                json!({ "snapshot": data, "depth": depth, "verified": true }),
            ),
            Err(error) => browser_failure(request, operation_id, error),
        };
    }
    if matches!(
        request.intent.as_str(),
        "browser.cdp.fill" | "browser.cdp.click" | "browser.cdp.focus" | "browser.cdp.wait_for"
    ) {
        return browser_cdp_dom_action(
            request,
            operation_id,
            &endpoint,
            target_id,
            browser_context_id,
            revision,
        );
    }
    if request.intent == "browser.cdp.upload" {
        return browser_cdp_upload(
            request,
            operation_id,
            &endpoint,
            target_id,
            Some(browser_context_id),
            Some(revision),
        );
    }
    if request.intent == "browser.cdp.download" {
        return browser_cdp_download(
            request,
            operation_id,
            &endpoint,
            target_id,
            Some(browser_context_id),
            Some(revision),
        );
    }
    let (method, params) = match request.intent.as_str() {
        "browser.cdp.evaluate" => {
            let Some(expression) = request.params.get("expression").and_then(Value::as_str) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser evaluation needs an expression".to_owned(),
                        recovery: None,
                    },
                );
            };
            (
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": request.params.get("await_promise").and_then(Value::as_bool).unwrap_or(true)
                }),
            )
        }
        "browser.cdp.navigate" => {
            let Some(url) = request.params.get("url").and_then(Value::as_str) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser navigation needs a URL".to_owned(),
                        recovery: None,
                    },
                );
            };
            ("Page.navigate", json!({ "url": url }))
        }
        _ => unreachable!(),
    };
    match browser::cdp_call(
        &endpoint.to_string_lossy(),
        target_id,
        Some(browser_context_id),
        Some(revision),
        method,
        params,
    ) {
        Ok(data) => success(
            request,
            operation_id,
            "browser_protocol",
            EffectState::Changed,
            if request.intent == "browser.cdp.evaluate" {
                if data.get("exceptionDetails").is_some() {
                    VerificationState::Failed
                } else {
                    VerificationState::Verified
                }
            } else {
                VerificationState::Unverified
            },
            data,
        ),
        Err(error) => browser_failure(request, operation_id, error),
    }
}

fn browser_cdp_semantic_click(
    request: &OperationRequest,
    operation_id: String,
    endpoint: &std::ffi::OsStr,
) -> ActionResult {
    let Some(target_id) = request.params.get("target_id").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Semantic browser clicks need a target id".to_owned(),
                recovery: Some("Inspect browser targets before the semantic action".to_owned()),
            },
        );
    };
    let browser_context_id = request
        .params
        .get("browser_context_id")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let Some(locator) = request.params.get("locator") else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Semantic browser clicks need a locator object".to_owned(),
                recovery: Some(
                    "Provide role/name, text, test_id, href_contains, or selector".to_owned(),
                ),
            },
        );
    };
    let revision = request.params.get("revision").and_then(Value::as_str);
    let timeout = request
        .params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(3_000)
        .min(30_000);
    match browser::semantic_click(
        &endpoint.to_string_lossy(),
        target_id,
        browser_context_id,
        revision,
        locator,
        timeout,
    ) {
        Ok(data) => success(
            request,
            operation_id,
            "browser_protocol",
            EffectState::Changed,
            VerificationState::Verified,
            data,
        ),
        Err(error) => browser_failure(request, operation_id, error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum BrowserWorkflowStep {
    Navigate {
        url: String,
        #[serde(default)]
        url_contains: Option<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Click {
        locator: Value,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    WaitUrl {
        contains: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
}

fn browser_cdp_workflow(
    request: &OperationRequest,
    operation_id: String,
    endpoint: &std::ffi::OsStr,
) -> ActionResult {
    let Some(target_id) = request.params.get("target_id").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser workflows need a target id".to_owned(),
                recovery: Some(
                    "Inspect browser targets and include the exact target identity".to_owned(),
                ),
            },
        );
    };
    let Some(browser_context_id) = request
        .params
        .get("browser_context_id")
        .and_then(Value::as_str)
    else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser workflows need a browser context id".to_owned(),
                recovery: Some("Inspect targets and include the exact browser context".to_owned()),
            },
        );
    };
    let Some(steps_value) = request.params.get("steps") else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser workflows need a steps array".to_owned(),
                recovery: Some("Use navigate, click, and wait_url steps".to_owned()),
            },
        );
    };
    let Ok(steps) = serde_json::from_value::<Vec<BrowserWorkflowStep>>(steps_value.clone()) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser workflow steps did not match the closed schema".to_owned(),
                recovery: Some("Use data-only navigate, click, and wait_url steps".to_owned()),
            },
        );
    };
    if steps.is_empty() || steps.len() > 32 {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser workflows must contain between 1 and 32 steps".to_owned(),
                recovery: None,
            },
        );
    }
    let endpoint = endpoint.to_string_lossy();
    let initial_revision = request.params.get("revision").and_then(Value::as_str);
    let targets = match browser::discover(&endpoint) {
        Ok(targets) => targets,
        Err(error) => return browser_failure(request, operation_id, error),
    };
    let target = match crate::bind_browser_target(
        &targets,
        target_id,
        Some(browser_context_id),
        initial_revision,
    ) {
        Ok(target) => target,
        Err(error) => return browser_failure(request, operation_id, error),
    };
    let mut revision = target.revision;
    let mut completed = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let result = match step {
            BrowserWorkflowStep::Navigate {
                url,
                url_contains,
                timeout_ms,
            } => {
                if let Err(error) = browser::validate_url(url) {
                    return browser_failure(request, operation_id, error);
                }
                match browser::cdp_call(
                    &endpoint,
                    target_id,
                    Some(browser_context_id),
                    revision.as_deref(),
                    "Page.navigate",
                    json!({"url": url}),
                ) {
                    Ok(data) => {
                        if let Some(expected) = url_contains
                            && let Err(error) = browser_wait_for_url(
                                &endpoint,
                                target_id,
                                browser_context_id,
                                expected,
                                timeout_ms.unwrap_or(2_000),
                            )
                        {
                            return browser_failure(request, operation_id, error);
                        }
                        Ok(json!({"action":"navigate", "url": url, "protocol": data}))
                    }
                    Err(error) => Err(error),
                }
            }
            BrowserWorkflowStep::Click {
                locator,
                timeout_ms,
            } => browser::semantic_click(
                &endpoint,
                target_id,
                browser_context_id,
                revision.as_deref(),
                locator,
                timeout_ms.unwrap_or(1_500).clamp(100, 10_000),
            ),
            BrowserWorkflowStep::WaitUrl {
                contains,
                timeout_ms,
            } => browser_wait_for_url(
                &endpoint,
                target_id,
                browser_context_id,
                contains,
                timeout_ms.unwrap_or(2_000),
            )
            .map(|_| json!({"action":"wait_url", "contains": contains})),
        };
        let data = match result {
            Ok(data) => data,
            Err(error) => return browser_failure(request, operation_id, error),
        };
        completed.push(json!({"index": index, "result": data}));
        if let Ok(current) = browser::discover(&endpoint)
            && let Ok(bound) =
                crate::bind_browser_target(&current, target_id, Some(browser_context_id), None)
        {
            revision = bound.revision;
        }
    }
    success(
        request,
        operation_id,
        "browser_protocol",
        EffectState::Changed,
        VerificationState::Verified,
        json!({"steps": completed, "step_count": completed.len(), "verified": true}),
    )
}

fn browser_wait_for_url(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    contains: &str,
    timeout_ms: u64,
) -> Result<Value, ComptrolError> {
    if contains.is_empty() || contains.chars().any(char::is_control) {
        return Err(ComptrolError {
            code: "invalid_input".to_owned(),
            message: "URL postconditions must contain non-control text".to_owned(),
            recovery: None,
        });
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.clamp(100, 30_000));
    loop {
        if let Ok(targets) = browser::discover(endpoint)
            && let Ok(target) =
                crate::bind_browser_target(&targets, target_id, Some(browser_context_id), None)
            && target
                .url
                .as_deref()
                .is_some_and(|url| url.contains(contains))
        {
            return Ok(json!({"url": target.url, "verified": true}));
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(ComptrolError {
        code: "verification_failed".to_owned(),
        message: format!("Browser URL did not contain {contains}"),
        recovery: Some("Inspect the target and retry with a bounded postcondition".to_owned()),
    })
}

fn browser_cdp_dom_action(
    request: &OperationRequest,
    operation_id: String,
    endpoint: &std::ffi::OsStr,
    target_id: &str,
    browser_context_id: &str,
    revision: &str,
) -> ActionResult {
    let timeout = request
        .params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(5_000)
        .min(30_000);
    let (expression, verified_by_default) = match request.intent.as_str() {
        "browser.cdp.focus" => {
            let Some(selector) = request.params.get("selector").and_then(Value::as_str) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser focus needs a selector".to_owned(),
                        recovery: None,
                    },
                );
            };
            let Ok(selector) = serde_json::to_string(selector) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser selector is not serializable".to_owned(),
                        recovery: None,
                    },
                );
            };
            (
                format!(
                    "(() => {{ const element = document.querySelector({selector}); if (!element) throw new Error('target missing'); element.focus(); return document.activeElement === element; }})()"
                ),
                true,
            )
        }
        "browser.cdp.fill" => {
            let Some(selector) = request.params.get("selector").and_then(Value::as_str) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser fill needs a selector".to_owned(),
                        recovery: None,
                    },
                );
            };
            let Some(value) = request.params.get("value").and_then(Value::as_str) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser fill needs a value".to_owned(),
                        recovery: None,
                    },
                );
            };
            let Ok(selector) = serde_json::to_string(selector) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser selector is not serializable".to_owned(),
                        recovery: None,
                    },
                );
            };
            let Ok(value) = serde_json::to_string(value) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser value is not serializable".to_owned(),
                        recovery: None,
                    },
                );
            };
            (
                format!(
                    "(() => {{ const element = document.querySelector({selector}); if (!element) throw new Error('target missing'); element.focus(); element.value = {value}; element.dispatchEvent(new Event('input', {{ bubbles: true }})); element.dispatchEvent(new Event('change', {{ bubbles: true }})); return element.value === {value}; }})()"
                ),
                true,
            )
        }
        "browser.cdp.click" => {
            let Some(selector) = request.params.get("selector").and_then(Value::as_str) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser click needs a selector".to_owned(),
                        recovery: None,
                    },
                );
            };
            let Ok(selector) = serde_json::to_string(selector) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser selector is not serializable".to_owned(),
                        recovery: None,
                    },
                );
            };
            (
                format!(
                    "(() => {{ const element = document.querySelector({selector}); if (!element) throw new Error('target missing'); element.click(); return true; }})()"
                ),
                false,
            )
        }
        "browser.cdp.wait_for" => {
            let Some(selector) = request.params.get("selector").and_then(Value::as_str) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser wait needs a selector".to_owned(),
                        recovery: None,
                    },
                );
            };
            let property = request
                .params
                .get("property")
                .and_then(Value::as_str)
                .unwrap_or("textContent");
            if !matches!(
                property,
                "textContent" | "value" | "title" | "href" | "checked" | "disabled"
            ) {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser wait property is not allowlisted".to_owned(),
                        recovery: None,
                    },
                );
            }
            let condition = if let Some(expected) = request.params.get("equals") {
                let Ok(expected) = serde_json::to_string(expected) else {
                    return ActionResult::refused(
                        request,
                        operation_id,
                        ComptrolError {
                            code: "invalid_input".to_owned(),
                            message: "Browser wait value is not serializable".to_owned(),
                            recovery: None,
                        },
                    );
                };
                format!("JSON.stringify(element[{property:?}]) === JSON.stringify({expected})")
            } else if let Some(expected) = request.params.get("contains").and_then(Value::as_str) {
                let Ok(expected) = serde_json::to_string(expected) else {
                    return ActionResult::refused(
                        request,
                        operation_id,
                        ComptrolError {
                            code: "invalid_input".to_owned(),
                            message: "Browser wait value is not serializable".to_owned(),
                            recovery: None,
                        },
                    );
                };
                format!("String(element[{property:?}] ?? '').includes({expected})")
            } else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser wait needs equals or contains".to_owned(),
                        recovery: None,
                    },
                );
            };
            let Ok(selector) = serde_json::to_string(selector) else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "invalid_input".to_owned(),
                        message: "Browser selector is not serializable".to_owned(),
                        recovery: None,
                    },
                );
            };
            (
                format!(
                    "(() => {{ const element = document.querySelector({selector}); return Boolean(element) && {condition}; }})()"
                ),
                true,
            )
        }
        _ => unreachable!(),
    };
    let evaluate = || {
        browser::cdp_call(
            &endpoint.to_string_lossy(),
            target_id,
            Some(browser_context_id),
            Some(revision),
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
        )
    };
    let postcondition_requested = request.intent == "browser.cdp.click"
        && request
            .params
            .get("verify_expression")
            .and_then(Value::as_str)
            .is_some();
    let deadline = Instant::now() + Duration::from_millis(timeout);
    loop {
        let data = match evaluate() {
            Ok(data) => data,
            Err(error) => return browser_failure(request, operation_id, error),
        };
        let value = data.get("result").and_then(|result| result.get("value"));
        if value == Some(&Value::Bool(true)) {
            let verified = if request.intent == "browser.cdp.click" {
                if let Some(expression) = request
                    .params
                    .get("verify_expression")
                    .and_then(Value::as_str)
                {
                    match browser::cdp_call(
                        &endpoint.to_string_lossy(),
                        target_id,
                        Some(browser_context_id),
                        Some(revision),
                        "Runtime.evaluate",
                        json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
                    ) {
                        Ok(postcondition) => {
                            postcondition
                                .get("result")
                                .and_then(|result| result.get("value"))
                                == Some(&Value::Bool(true))
                        }
                        Err(error) => return browser_failure(request, operation_id, error),
                    }
                } else {
                    false
                }
            } else {
                verified_by_default
            };
            return success(
                request,
                operation_id,
                "browser_protocol",
                if request.intent == "browser.cdp.wait_for" {
                    EffectState::None
                } else {
                    EffectState::Changed
                },
                if verified {
                    VerificationState::Verified
                } else if postcondition_requested {
                    VerificationState::Failed
                } else {
                    VerificationState::Unverified
                },
                json!({ "result": data, "postcondition": if verified { "verified" } else if postcondition_requested { "failed" } else { "unverified" } }),
            );
        }
        if request.intent != "browser.cdp.wait_for" || Instant::now() >= deadline {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "verification_failed".to_owned(),
                    message: "The browser did not confirm the requested DOM condition".to_owned(),
                    recovery: Some("Inspect the exact page state and retry once".to_owned()),
                },
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn browser_cdp_upload(
    request: &OperationRequest,
    operation_id: String,
    endpoint: &std::ffi::OsStr,
    target_id: &str,
    browser_context_id: Option<&str>,
    revision: Option<&str>,
) -> ActionResult {
    let Some(path) = request.params.get("path").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser upload needs a path".to_owned(),
                recovery: None,
            },
        );
    };
    let path = PathBuf::from(path);
    let Ok(sandbox) = fs::canonicalize(state_dir().join("sandbox")) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "The Comptrol sandbox is not available".to_owned(),
                recovery: Some("Create an authorized sandbox file first".to_owned()),
            },
        );
    };
    let Ok(path) = fs::canonicalize(path) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "The upload file does not exist".to_owned(),
                recovery: Some("Use an existing file inside the Comptrol sandbox".to_owned()),
            },
        );
    };
    if !path.starts_with(&sandbox) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "Browser uploads are restricted to the Comptrol sandbox".to_owned(),
                recovery: Some("Copy the file into the authorized sandbox first".to_owned()),
            },
        );
    }
    let selector = request
        .params
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("#upload");
    match browser::cdp_upload(
        &endpoint.to_string_lossy(),
        target_id,
        browser_context_id,
        revision,
        selector,
        &path,
    ) {
        Ok(data) => success(
            request,
            operation_id,
            "browser_protocol",
            EffectState::Changed,
            VerificationState::Verified,
            data,
        ),
        Err(error) => browser_failure(request, operation_id, error),
    }
}

fn browser_cdp_download(
    request: &OperationRequest,
    operation_id: String,
    endpoint: &std::ffi::OsStr,
    target_id: &str,
    browser_context_id: Option<&str>,
    revision: Option<&str>,
) -> ActionResult {
    let Some(key) = request.idempotency_key.as_deref() else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "idempotency_required".to_owned(),
                message: "Browser downloads require an idempotency key".to_owned(),
                recovery: Some("Retry with a stable idempotency key".to_owned()),
            },
        );
    };
    let file_name = request
        .params
        .get("file_name")
        .and_then(Value::as_str)
        .unwrap_or("fixture.txt");
    let download_dir = state_dir()
        .join("sandbox")
        .join("downloads")
        .join(format!("{:016x}", stable_hash(key.as_bytes())));
    let expected_path = download_dir.join(file_name);
    if expected_path.is_file() {
        return success(
            request,
            operation_id,
            "browser_protocol",
            EffectState::None,
            VerificationState::Verified,
            json!({ "path": expected_path, "file_name": file_name, "verified": true, "replayed": true }),
        );
    }
    let selector = request
        .params
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("#download");
    match browser::cdp_download(
        &endpoint.to_string_lossy(),
        target_id,
        browser_context_id,
        revision,
        selector,
        &download_dir,
        file_name,
    ) {
        Ok(data) => success(
            request,
            operation_id,
            "browser_protocol",
            EffectState::Changed,
            VerificationState::Verified,
            data,
        ),
        Err(error) => browser_failure(request, operation_id, error),
    }
}

fn browser_failure(
    request: &OperationRequest,
    operation_id: String,
    error: ComptrolError,
) -> ActionResult {
    if matches!(
        error.code.as_str(),
        "browser_dispatch_failed" | "browser_response_failed" | "browser_unavailable"
    ) {
        return ActionResult {
            operation_id,
            intent: request.intent.clone(),
            route: "browser_protocol".to_owned(),
            target: request.target.clone(),
            preflight: "passed".to_owned(),
            delivery: DeliveryState::Unknown,
            effect: EffectState::Unknown,
            verification: VerificationState::Unverified,
            disturbance: json!({ "foreground_changed": false }),
            recovery: RecoveryState::RequiresReconciliation,
            data: Value::Null,
            error: Some(error),
        };
    }
    ActionResult::refused(request, operation_id, error)
}

fn browser_closed_group_unsupported(
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    ActionResult::refused(
        request,
        operation_id,
        ComptrolError {
            code: "closed_group_unsupported".to_owned(),
            message: "Chrome does not expose closed tab groups as live DevTools targets".to_owned(),
            recovery: Some(
                "Reopen the group in Chrome, then inspect and bind its new live targets".to_owned(),
            ),
        },
    )
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
        disturbance: json!({
            "foreground_changed": foreground_changed(request),
            "mouse": "untouched",
            "clipboard": "untouched",
            "posture": request.background.as_deref().unwrap_or("foreground_allowed")
        }),
        recovery: RecoveryState::None,
        data,
        error: None,
    }
}

fn foreground_changed(request: &OperationRequest) -> bool {
    match request.intent.as_str() {
        "desktop.open_app" => true,
        "browser.cdp.open_tab" => !request
            .params
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "browser.chrome.open_tab" => true,
        "browser.chrome.reopen_closed_group" => true,
        _ => false,
    }
}

fn platform_broker_observe(request: &OperationRequest, operation_id: String) -> ActionResult {
    let requested = request
        .params
        .get("broker")
        .and_then(Value::as_str)
        .unwrap_or("current");
    let capabilities = platform_capabilities();
    let current_name = match std::env::consts::OS {
        "macos" => "platform.macos.ax",
        "windows" => "platform.windows.uia",
        "linux" => {
            if std::env::var_os("AT_SPI_BUS_ADDRESS").is_some() {
                "platform.linux.atspi"
            } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                "platform.linux.wayland"
            } else {
                "platform.linux.x11"
            }
        }
        _ => "platform.macos.ax",
    };
    let selected = capabilities
        .iter()
        .find(|capability| {
            if requested == "current" {
                capability.name == current_name
            } else {
                capability.name == requested || capability.route == requested
            }
        })
        .cloned();
    let Some(selected) = selected else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "unsupported_capability".to_owned(),
                message: format!("Unknown platform broker {requested}"),
                recovery: Some(
                    "Inspect platform capabilities for the available brokers".to_owned(),
                ),
            },
        );
    };
    success(
        request,
        operation_id,
        "platform_broker",
        EffectState::None,
        VerificationState::Verified,
        json!({
            "broker": selected,
            "operations": ["observe"],
            "semantic_mutation": false,
            "refusal": "Actuation is not enabled until the platform fixture matrix passes"
        }),
    )
}

fn desktop_observe(request: &OperationRequest, operation_id: String) -> ActionResult {
    let mut data = json!({ "platform": std::env::consts::OS, "arch": std::env::consts::ARCH, "current_dir": std::env::current_dir().ok(), "route": "platform_observe", "permission": "not_required_for_basic_process_observation" });
    if cfg!(target_os = "macos") {
        match run_osascript(
            "tell application \"System Events\" to get name of every application process",
        ) {
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

fn sandbox_write(
    request: &OperationRequest,
    operation_id: String,
    checkpoints: &CheckpointStore,
) -> ActionResult {
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
    if let Err(error) = fs::create_dir_all(&root)
        .and_then(|_| checkpoints.create(&operation_id, &path))
        .and_then(|_| fs::write(&path, content.as_bytes()))
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
        operation_id.clone(),
        "sandbox_filesystem",
        EffectState::Changed,
        VerificationState::Verified,
        json!({ "path": path, "bytes": content.len(), "checkpoint": operation_id }),
    )
}

fn sandbox_copy(
    request: &OperationRequest,
    operation_id: String,
    checkpoints: &CheckpointStore,
) -> ActionResult {
    sandbox_copy_at(
        request,
        operation_id,
        checkpoints,
        &state_dir().join("sandbox"),
    )
}

fn sandbox_copy_at(
    request: &OperationRequest,
    operation_id: String,
    checkpoints: &CheckpointStore,
    sandbox_root: &Path,
) -> ActionResult {
    let Some(source) = request.params.get("source").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Sandbox copy needs a source path".to_owned(),
                recovery: None,
            },
        );
    };
    let Some(destination) = request.params.get("destination").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Sandbox copy needs a destination path".to_owned(),
                recovery: None,
            },
        );
    };
    if !sandbox_relative(source) || !sandbox_relative(destination) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "Sandbox copy accepts relative paths without parent traversal".to_owned(),
                recovery: Some("Use regular files inside the Comptrol sandbox".to_owned()),
            },
        );
    }
    let Ok(source) = fs::canonicalize(sandbox_root.join(source)) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "Sandbox copy source does not exist".to_owned(),
                recovery: None,
            },
        );
    };
    let Ok(root) = fs::canonicalize(sandbox_root) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "The Comptrol sandbox is unavailable".to_owned(),
                recovery: None,
            },
        );
    };
    if !source.starts_with(&root) || !source.is_file() {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "Sandbox copy source must be a regular sandbox file".to_owned(),
                recovery: None,
            },
        );
    }
    let destination = root.join(destination);
    let Some(parent) = destination.parent() else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "Sandbox copy destination has no parent".to_owned(),
                recovery: None,
            },
        );
    };
    let parent_safe = fs::create_dir_all(parent).is_ok()
        && fs::canonicalize(parent).is_ok_and(|canonical| canonical.starts_with(&root));
    if destination.is_symlink() || !parent_safe {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "Sandbox copy destination is not a safe sandbox path".to_owned(),
                recovery: None,
            },
        );
    }
    let Ok(bytes) = fs::read(&source) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "read_failed".to_owned(),
                message: "Sandbox copy source could not be read".to_owned(),
                recovery: None,
            },
        );
    };
    if let Err(error) = checkpoints
        .create(&operation_id, &destination)
        .and_then(|_| fs::copy(&source, &destination).map(|_| ()))
    {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "copy_failed".to_owned(),
                message: error.to_string(),
                recovery: None,
            },
        );
    }
    let verified =
        fs::read(&destination).is_ok_and(|copied| stable_hash(&copied) == stable_hash(&bytes));
    if !verified {
        return ActionResult {
            operation_id,
            intent: request.intent.clone(),
            route: "sandbox_filesystem".to_owned(),
            target: request.target.clone(),
            preflight: "passed".to_owned(),
            delivery: DeliveryState::Delivered,
            effect: EffectState::Changed,
            verification: VerificationState::Failed,
            disturbance: json!({ "foreground_changed": false }),
            recovery: RecoveryState::None,
            data: json!({ "source": source, "destination": destination }),
            error: Some(ComptrolError {
                code: "verification_failed".to_owned(),
                message: "Sandbox copy did not match the source".to_owned(),
                recovery: None,
            }),
        };
    }
    success(
        request,
        operation_id,
        "sandbox_filesystem",
        EffectState::Changed,
        VerificationState::Verified,
        json!({ "source": source, "destination": destination, "bytes": bytes.len() }),
    )
}

fn sandbox_relative(path: &str) -> bool {
    !path.is_empty() && !path.starts_with('/') && !path.split('/').any(|part| part == "..")
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
        match run_osascript(&script) {
            Ok(output) if output.status.success() => success(
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

fn desktop_open_app(request: &OperationRequest, operation_id: String) -> ActionResult {
    if request.background.as_deref() == Some("strict_background") {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "background_unavailable".to_owned(),
                message: "Opening an application may activate the desktop and cannot satisfy strict background posture".to_owned(),
                recovery: Some("Use foreground_allowed or open a browser target through CDP".to_owned()),
            },
        );
    }
    let Some(app) = request.params.get("app").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Opening an app needs an exact application name".to_owned(),
                recovery: None,
            },
        );
    };
    if app.is_empty()
        || app.chars().any(char::is_control)
        || app.contains('/')
        || app.contains('\\')
    {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "App launch accepts a local application name without a path".to_owned(),
                recovery: Some("Use the exact installed application name".to_owned()),
            },
        );
    }
    let status = if cfg!(target_os = "macos") {
        Command::new("open").args(["-a", app]).status()
    } else if cfg!(target_os = "windows") {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Process -FilePath $env:COMPTROL_APP_NAME",
            ])
            .env("COMPTROL_APP_NAME", app)
            .status()
    } else if cfg!(target_os = "linux") {
        Command::new("gtk-launch").arg(app).status()
    } else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "unsupported_surface".to_owned(),
                message: "Application launch is unsupported on this operating system".to_owned(),
                recovery: Some("Inspect platform capabilities".to_owned()),
            },
        );
    };
    match status {
        Ok(status) if status.success() => {
            let verified = if cfg!(target_os = "macos") {
                let verify_script = format!(
                    "tell application \"System Events\" to exists process {}",
                    apple_quote(app)
                );
                run_osascript(&verify_script).is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).trim() == "true"
                })
            } else {
                false
            };
            success(
                request,
                operation_id,
                "platform_launch",
                EffectState::Changed,
                if verified {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                json!({ "app": app, "opened": true, "mouse": "untouched", "clipboard": "untouched", "postcondition": if verified { "process_present" } else { "launcher_accepted" } }),
            )
        }
        Ok(output) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "launch_failed".to_owned(),
                message: format!("macOS LaunchServices returned {}", output),
                recovery: Some("Check the installed application name".to_owned()),
            },
        ),
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "launch_unavailable".to_owned(),
                message: error.to_string(),
                recovery: Some("Check that macOS LaunchServices is available".to_owned()),
            },
        ),
    }
}

fn browser_chrome_open_tab(request: &OperationRequest, operation_id: String) -> ActionResult {
    if request.background.as_deref() == Some("strict_background") {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "background_unavailable".to_owned(),
                message: "The native Chrome launcher may activate the desktop".to_owned(),
                recovery: Some(
                    "Use browser.cdp.open_tab with an explicit background tab".to_owned(),
                ),
            },
        );
    }
    let Some(url) = request.params.get("url").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Opening a Chrome tab needs a URL".to_owned(),
                recovery: None,
            },
        );
    };
    match browser::launch_chrome_tab(url) {
        Ok(data) => success(
            request,
            operation_id,
            "browser_launcher",
            EffectState::Changed,
            VerificationState::Unverified,
            data,
        ),
        Err(error) => ActionResult::refused(request, operation_id, error),
    }
}

fn browser_chrome_reopen_closed_group(
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    if !cfg!(target_os = "macos") {
        return unsupported_ax(request, operation_id);
    }
    if request.background.as_deref() == Some("strict_background") {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "background_unavailable".to_owned(),
                message: "Reopening a closed Chrome group activates the visible browser".to_owned(),
                recovery: Some("Use a live CDP target for strict background control".to_owned()),
            },
        );
    }
    let Some(group) = request.params.get("group").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Reopening a closed Chrome group needs an exact group name".to_owned(),
                recovery: None,
            },
        );
    };
    if group.is_empty() || group.chars().any(char::is_control) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Chrome group names cannot be empty or contain control characters"
                    .to_owned(),
                recovery: None,
            },
        );
    }
    let script = chrome_closed_group_ax_script(request, group);
    match run_osascript(&script) {
        Ok(output)
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == "true" =>
        {
            success(
                request,
                operation_id,
                "chrome_ax",
                EffectState::Changed,
                VerificationState::Verified,
                json!({
                    "group": group,
                    "postcondition": "closed_group_button_absent",
                    "mouse": "untouched",
                    "clipboard": "untouched"
                }),
            )
        }
        Ok(output) => ax_failure(request, operation_id, &output),
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "adapter_unavailable".to_owned(),
                message: error.to_string(),
                recovery: Some("Check Chrome and macOS Accessibility permission".to_owned()),
            },
        ),
    }
}

fn chrome_closed_group_ax_script(request: &OperationRequest, group: &str) -> String {
    let control = apple_quote(&format!("{group} group Closed"));
    let window = request
        .params
        .get("window")
        .and_then(Value::as_str)
        .map(apple_quote)
        .map(|name| format!("first window whose name is {name}"))
        .unwrap_or_else(|| "window 1".to_owned());
    format!(
        "tell application \"System Events\"\ntell application process \"Google Chrome\"\nset targetWindow to {window}\nset matches to (every button of targetWindow whose name is {control})\nif (count of matches) is not 1 then error \"target_ambiguous\"\nperform action \"AXPress\" of item 1 of matches\ndelay 0.05\nset remaining to (every button of targetWindow whose name is {control})\nreturn ((count of remaining) is 0)\nend tell\nend tell"
    )
}

const COMMAND_OUTPUT_LIMIT: usize = 64 * 1024;

fn command_run(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(program) = request.params.get("program").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "command.run needs a program".to_owned(),
                recovery: None,
            },
        );
    };
    if program.is_empty() || program.chars().any(char::is_control) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "command.run accepts a nonempty program without control characters"
                    .to_owned(),
                recovery: None,
            },
        );
    }
    let Some(args) = request.params.get("args").and_then(Value::as_array) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "command.run needs an args array".to_owned(),
                recovery: Some(
                    "Pass argv as structured strings rather than a shell command".to_owned(),
                ),
            },
        );
    };
    if args.len() > 128 {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "input_too_large".to_owned(),
                message: "command.run accepts at most 128 arguments".to_owned(),
                recovery: None,
            },
        );
    }
    let mut argv = Vec::with_capacity(args.len());
    for value in args {
        let Some(value) = value.as_str() else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "command.run args must be strings".to_owned(),
                    recovery: None,
                },
            );
        };
        if value.chars().any(char::is_control) || value.len() > 64 * 1024 {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "command.run arguments contain invalid or oversized data".to_owned(),
                    recovery: None,
                },
            );
        }
        argv.push(value);
    }
    let allowed = std::env::var("COMPTROL_COMMAND_ALLOWLIST")
        .ok()
        .into_iter()
        .flat_map(|value| {
            value
                .split(',')
                .map(str::trim)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .any(|allowed| allowed == program);
    if !allowed {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "policy_denied".to_owned(),
                message: "The program is not in the explicit local command allowlist".to_owned(),
                recovery: Some(
                    "Add the exact executable to COMPTROL_COMMAND_ALLOWLIST locally".to_owned(),
                ),
            },
        );
    }
    let Some(cwd) = request.params.get("cwd").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "command.run needs an explicit cwd".to_owned(),
                recovery: Some("Pass a directory within COMPTROL_COMMAND_ROOT".to_owned()),
            },
        );
    };
    let Ok(cwd) = fs::canonicalize(cwd) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "command.run cwd does not resolve to a directory".to_owned(),
                recovery: None,
            },
        );
    };
    if !cwd.is_dir() {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "command.run cwd is not a directory".to_owned(),
                recovery: None,
            },
        );
    }
    let Ok(root) = std::env::var("COMPTROL_COMMAND_ROOT") else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "policy_denied".to_owned(),
                message: "COMPTROL_COMMAND_ROOT is required for command.run".to_owned(),
                recovery: Some(
                    "Set an explicit local command root outside the agent channel".to_owned(),
                ),
            },
        );
    };
    let Ok(root) = fs::canonicalize(root) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "COMPTROL_COMMAND_ROOT does not resolve".to_owned(),
                recovery: None,
            },
        );
    };
    if !cwd.starts_with(&root) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_denied".to_owned(),
                message: "command.run cwd is outside the command root".to_owned(),
                recovery: None,
            },
        );
    }
    let timeout_ms = request
        .params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(30_000)
        .clamp(1, 60_000);
    let mut child = match Command::new(program)
        .args(&argv)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "launch_failed".to_owned(),
                    message: error.to_string(),
                    recovery: Some(
                        "Check the allowlisted executable and local permissions".to_owned(),
                    ),
                },
            );
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let output_thread = std::thread::spawn(move || read_bounded(stdout));
    let error_thread = std::thread::spawn(move || read_bounded(stderr));
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status: Result<std::process::ExitStatus, io::Error> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = output_thread.join();
                let _ = error_thread.join();
                return ActionResult {
                    operation_id,
                    intent: request.intent.clone(),
                    route: "process_argv".to_owned(),
                    target: request.target.clone(),
                    preflight: "passed".to_owned(),
                    delivery: DeliveryState::Unknown,
                    effect: EffectState::Unknown,
                    verification: VerificationState::Unverified,
                    disturbance: json!({ "foreground_changed": false }),
                    recovery: RecoveryState::RequiresReconciliation,
                    data: Value::Null,
                    error: Some(ComptrolError {
                        code: "provider_timeout".to_owned(),
                        message: "The command exceeded its bounded timeout".to_owned(),
                        recovery: Some("Observe the command result before retrying".to_owned()),
                    }),
                };
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = output_thread.join();
                let _ = error_thread.join();
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "process_wait_failed".to_owned(),
                        message: error.to_string(),
                        recovery: Some("Inspect the process state before retrying".to_owned()),
                    },
                );
            }
        }
    };
    let stdout = output_thread.join().unwrap_or_default();
    let stderr = error_thread.join().unwrap_or_default();
    let exit_code = status.ok().and_then(|value| value.code());
    let expected = request
        .postcondition
        .as_ref()
        .filter(|value| value.get("kind").and_then(Value::as_str) == Some("exit_code"))
        .and_then(|value| value.get("value").and_then(Value::as_i64))
        .and_then(|value| i32::try_from(value).ok());
    let verified = expected.map_or(exit_code == Some(0), |value| exit_code == Some(value));
    let data = json!({
        "program": program,
        "args_count": argv.len(),
        "cwd": cwd,
        "exit_code": exit_code,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "output_truncated": stdout.len() == COMMAND_OUTPUT_LIMIT || stderr.len() == COMMAND_OUTPUT_LIMIT,
    });
    if verified {
        success(
            request,
            operation_id,
            "process_argv",
            EffectState::Changed,
            VerificationState::Verified,
            data,
        )
    } else {
        ActionResult {
            operation_id,
            intent: request.intent.clone(),
            route: "process_argv".to_owned(),
            target: request.target.clone(),
            preflight: "passed".to_owned(),
            delivery: DeliveryState::Delivered,
            effect: EffectState::Changed,
            verification: VerificationState::Failed,
            disturbance: json!({ "foreground_changed": false }),
            recovery: RecoveryState::None,
            data,
            error: Some(ComptrolError {
                code: "verification_failed".to_owned(),
                message: format!("Command exited with {:?}", exit_code),
                recovery: Some("Inspect the bounded command output and postcondition".to_owned()),
            }),
        }
    }
}

fn read_bounded(mut input: impl Read) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    while output.len() < COMMAND_OUTPUT_LIMIT {
        let remaining = COMMAND_OUTPUT_LIMIT - output.len();
        let chunk_len = remaining.min(buffer.len());
        let size = input.read(&mut buffer[..chunk_len]).unwrap_or(0);
        if size == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..size]);
    }
    output
}

fn run_bounded(mut command: Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let output_thread = std::thread::spawn(move || read_bounded(stdout));
    let error_thread = std::thread::spawn(move || read_bounded(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = output_thread.join();
                let _ = error_thread.join();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "provider timed out",
                ));
            }
        }
    };
    Ok(Output {
        status,
        stdout: output_thread.join().unwrap_or_default(),
        stderr: error_thread.join().unwrap_or_default(),
    })
}

const WINDOWS_UIA_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$all = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
$matches = @()
foreach ($candidate in $all) {
    if ($env:COMPTROL_UIA_PROCESS_ID -and $candidate.Current.ProcessId -ne [int]$env:COMPTROL_UIA_PROCESS_ID) { continue }
    if ($env:COMPTROL_UIA_NAME -and $candidate.Current.Name -cne $env:COMPTROL_UIA_NAME) { continue }
    if ($env:COMPTROL_UIA_AUTOMATION_ID -and $candidate.Current.AutomationId -cne $env:COMPTROL_UIA_AUTOMATION_ID) { continue }
    if ($env:COMPTROL_UIA_ROLE -and $candidate.Current.ControlType.ProgrammaticName -notlike ('*.' + $env:COMPTROL_UIA_ROLE)) { continue }
    $matches += $candidate
}
if ($matches.Count -ne 1) { throw 'target_ambiguous' }
$element = $matches[0]
$verified = $false
if ($env:COMPTROL_UIA_ACTION -eq 'press') {
    $pattern = $element.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $pattern.Invoke()
    if (-not $env:COMPTROL_UIA_VERIFY_ATTRIBUTE) { $verified = $false }
} elseif ($env:COMPTROL_UIA_ACTION -eq 'set_value') {
    $pattern = $element.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
    $pattern.SetValue($env:COMPTROL_UIA_VALUE)
    $verified = ($pattern.Current.Value -ceq $env:COMPTROL_UIA_VALUE)
} else { throw 'unsupported_action' }
if ($env:COMPTROL_UIA_VERIFY_ATTRIBUTE -eq 'name') { $verified = ($element.Current.Name -ceq $env:COMPTROL_UIA_VERIFY_VALUE) }
if ($env:COMPTROL_UIA_VERIFY_ATTRIBUTE -eq 'enabled') { $verified = ($element.Current.IsEnabled.ToString().ToLower() -ceq $env:COMPTROL_UIA_VERIFY_VALUE.ToLower()) }
if ($env:COMPTROL_UIA_VERIFY_ATTRIBUTE -eq 'value') {
    $valuePattern = $element.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
    $verified = ($valuePattern.Current.Value -ceq $env:COMPTROL_UIA_VERIFY_VALUE)
}
([pscustomobject]@{ verified = $verified; action = $env:COMPTROL_UIA_ACTION } | ConvertTo-Json -Compress)
"#;

const LINUX_ATSPI_SCRIPT: &str = r#"
import json
import os
import gi
gi.require_version('Atspi', '2.0')
from gi.repository import Atspi

Atspi.init()
name = os.environ.get('COMPTROL_ATSPI_NAME', '')
role = os.environ.get('COMPTROL_ATSPI_ROLE', '')
process_id = int(os.environ.get('COMPTROL_ATSPI_PROCESS_ID', '0'))
action_name = os.environ.get('COMPTROL_ATSPI_ACTION', '')
value = os.environ.get('COMPTROL_ATSPI_VALUE', '')
verify_attribute = os.environ.get('COMPTROL_ATSPI_VERIFY_ATTRIBUTE', '')
verify_value = os.environ.get('COMPTROL_ATSPI_VERIFY_VALUE', '')

def walk(node):
    yield node
    for index in range(node.get_child_count()):
        child = node.get_child_at_index(index)
        if child is not None:
            yield from walk(child)

matches = []
for index in range(Atspi.get_desktop_count()):
    desktop = Atspi.get_desktop(index)
    if desktop is not None:
        for item in walk(desktop):
            if process_id and item.get_process_id() != process_id:
                continue
            if name and item.get_name() != name:
                continue
            if role and item.get_role_name() != role:
                continue
            matches.append(item)
if len(matches) != 1:
    raise RuntimeError('target_ambiguous')
item = matches[0]
verified = False
if os.environ.get('COMPTROL_ATSPI_OPERATION') == 'press':
    action = item.get_action_iface()
    if action is None:
        raise RuntimeError('action_unavailable')
    selected = -1
    for index in range(action.get_n_actions()):
        if not action_name or action.get_action_name(index) in (action_name, 'click', 'press', 'activate'):
            selected = index
            break
    if selected < 0 or not action.do_action(selected):
        raise RuntimeError('action_failed')
elif os.environ.get('COMPTROL_ATSPI_OPERATION') == 'set_value':
    editable = item.get_editable_text_iface()
    if editable is None:
        raise RuntimeError('editable_text_unavailable')
    editable.set_text_contents(value)
else:
    raise RuntimeError('unsupported_action')
if verify_attribute == 'name':
    verified = item.get_name() == verify_value
elif verify_attribute == 'value':
    text = item.get_text_iface()
    verified = text is not None and text.get_text(0, -1) == verify_value
elif verify_attribute == 'exists':
    verified = True
else:
    verified = os.environ.get('COMPTROL_ATSPI_OPERATION') == 'set_value' and verify_value == value
print(json.dumps({'verified': verified, 'operation': os.environ.get('COMPTROL_ATSPI_OPERATION')}))
"#;

fn semantic_provider_result(
    request: &OperationRequest,
    operation_id: String,
    route: &str,
    output: io::Result<Output>,
) -> ActionResult {
    let output = match output {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            return ActionResult {
                operation_id,
                intent: request.intent.clone(),
                route: route.to_owned(),
                target: request.target.clone(),
                preflight: "passed".to_owned(),
                delivery: DeliveryState::Unknown,
                effect: EffectState::Unknown,
                verification: VerificationState::Unverified,
                disturbance: json!({ "foreground_changed": false, "mouse": "untouched", "clipboard": "untouched" }),
                recovery: RecoveryState::RequiresReconciliation,
                data: Value::Null,
                error: Some(ComptrolError {
                    code: "provider_timeout".to_owned(),
                    message: "The semantic provider exceeded its bounded call timeout".to_owned(),
                    recovery: Some("Observe the target before retrying".to_owned()),
                }),
            };
        }
        Err(error) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "adapter_unavailable".to_owned(),
                    message: error.to_string(),
                    recovery: Some("Inspect platform permissions and adapter health".to_owned()),
                },
            );
        }
    };
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        let code = if diagnostic.contains("target_ambiguous") {
            "target_ambiguous"
        } else if diagnostic.contains("unsupported") || diagnostic.contains("unavailable") {
            "unsupported_surface"
        } else {
            "verification_failed"
        };
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: code.to_owned(),
                message: "The semantic provider did not confirm the requested action".to_owned(),
                recovery: Some(
                    "Refresh the exact target and inspect platform capability state".to_owned(),
                ),
            },
        );
    }
    let data: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| json!({}));
    if data.get("verified").and_then(Value::as_bool) == Some(true) {
        success(
            request,
            operation_id,
            route,
            EffectState::Changed,
            VerificationState::Verified,
            json!({ "verified": true, "mouse": "untouched", "clipboard": "untouched" }),
        )
    } else {
        success(
            request,
            operation_id,
            route,
            EffectState::Changed,
            VerificationState::Unverified,
            json!({ "verified": false, "mouse": "untouched", "clipboard": "untouched" }),
        )
    }
}

fn windows_uia_action(request: &OperationRequest, operation_id: String) -> ActionResult {
    if !cfg!(target_os = "windows") {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "unsupported_surface".to_owned(),
                message: "Windows UI Automation is only available on Windows".to_owned(),
                recovery: Some("Inspect platform capabilities".to_owned()),
            },
        );
    }
    let Some(process_id) = request.params.get("process_id").and_then(Value::as_u64) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Windows UI Automation needs an exact process_id".to_owned(),
                recovery: Some("Observe the UIA tree and bind the process generation".to_owned()),
            },
        );
    };
    let name = request.params.get("name").and_then(Value::as_str);
    let automation_id = request.params.get("automation_id").and_then(Value::as_str);
    if name.is_none() && automation_id.is_none() {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Windows UI Automation needs a name or automation_id".to_owned(),
                recovery: None,
            },
        );
    }
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            WINDOWS_UIA_SCRIPT,
        ])
        .env("COMPTROL_UIA_PROCESS_ID", process_id.to_string())
        .env(
            "COMPTROL_UIA_ACTION",
            if request.intent.ends_with("press") {
                "press"
            } else {
                "set_value"
            },
        );
    if let Some(name) = name {
        command.env("COMPTROL_UIA_NAME", name);
    }
    if let Some(automation_id) = automation_id {
        command.env("COMPTROL_UIA_AUTOMATION_ID", automation_id);
    }
    if let Some(role) = request.params.get("role").and_then(Value::as_str) {
        command.env("COMPTROL_UIA_ROLE", role);
    }
    if let Some(value) = request.params.get("value").and_then(Value::as_str) {
        command.env("COMPTROL_UIA_VALUE", value);
    }
    if let Some(postcondition) = request.postcondition.as_ref()
        && let (Some(attribute), Some(expected)) = (
            postcondition.get("attribute").and_then(Value::as_str),
            postcondition.get("equals"),
        )
    {
        command.env("COMPTROL_UIA_VERIFY_ATTRIBUTE", attribute);
        command.env(
            "COMPTROL_UIA_VERIFY_VALUE",
            expected
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| expected.to_string()),
        );
    }
    semantic_provider_result(
        request,
        operation_id,
        "windows_uia",
        run_bounded(command, Duration::from_millis(1500)),
    )
}

fn linux_atspi_action(request: &OperationRequest, operation_id: String) -> ActionResult {
    if !cfg!(target_os = "linux") {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "unsupported_surface".to_owned(),
                message: "Linux AT SPI is only available on Linux".to_owned(),
                recovery: Some("Inspect platform capabilities".to_owned()),
            },
        );
    }
    let Some(process_id) = request.params.get("process_id").and_then(Value::as_u64) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Linux AT SPI needs an exact process_id".to_owned(),
                recovery: Some("Observe the accessibility tree and bind the process".to_owned()),
            },
        );
    };
    let Some(name) = request.params.get("name").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Linux AT SPI needs an exact accessible name".to_owned(),
                recovery: None,
            },
        );
    };
    let mut command = Command::new("python3");
    command
        .args(["-c", LINUX_ATSPI_SCRIPT])
        .env("COMPTROL_ATSPI_PROCESS_ID", process_id.to_string())
        .env("COMPTROL_ATSPI_NAME", name)
        .env(
            "COMPTROL_ATSPI_OPERATION",
            if request.intent.ends_with("press") {
                "press"
            } else {
                "set_value"
            },
        );
    if let Some(role) = request.params.get("role").and_then(Value::as_str) {
        command.env("COMPTROL_ATSPI_ROLE", role);
    }
    if let Some(action) = request.params.get("action").and_then(Value::as_str) {
        command.env("COMPTROL_ATSPI_ACTION", action);
    }
    if let Some(value) = request.params.get("value").and_then(Value::as_str) {
        command.env("COMPTROL_ATSPI_VALUE", value);
    }
    if let Some(postcondition) = request.postcondition.as_ref()
        && let (Some(attribute), Some(expected)) = (
            postcondition.get("attribute").and_then(Value::as_str),
            postcondition.get("equals"),
        )
    {
        command.env("COMPTROL_ATSPI_VERIFY_ATTRIBUTE", attribute);
        command.env(
            "COMPTROL_ATSPI_VERIFY_VALUE",
            expected
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| expected.to_string()),
        );
    }
    semantic_provider_result(
        request,
        operation_id,
        "linux_atspi",
        run_bounded(command, Duration::from_millis(1500)),
    )
}

fn macos_ax_press(request: &OperationRequest, operation_id: String) -> ActionResult {
    if !cfg!(target_os = "macos") {
        return unsupported_ax(request, operation_id);
    }
    let Some(script) = ax_script(request, "press") else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "macos.ax.press needs app, control, and optional role".to_owned(),
                recovery: None,
            },
        );
    };
    match run_osascript(&script) {
        Ok(output)
            if output.status.success()
                && request.postcondition.is_some()
                && String::from_utf8_lossy(&output.stdout).trim() == "true" =>
        {
            success(
                request,
                operation_id,
                "macos_ax",
                EffectState::Changed,
                VerificationState::Verified,
                json!({ "pressed": true, "postcondition": "verified" }),
            )
        }
        Ok(output) if output.status.success() => success(
            request,
            operation_id,
            "macos_ax",
            EffectState::Changed,
            VerificationState::Unverified,
            json!({ "pressed": true, "postcondition": "unverified" }),
        ),
        Ok(output) => ax_failure(request, operation_id, &output),
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "adapter_unavailable".to_owned(),
                message: error.to_string(),
                recovery: Some("Check macOS Accessibility permission".to_owned()),
            },
        ),
    }
}

fn macos_ax_set_value(request: &OperationRequest, operation_id: String) -> ActionResult {
    if !cfg!(target_os = "macos") {
        return unsupported_ax(request, operation_id);
    }
    let Some(value) = request.params.get("value").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "macos.ax.set_value needs a value".to_owned(),
                recovery: None,
            },
        );
    };
    let Some(script) = ax_script(request, &format!("set_value:{}", apple_quote(value))) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "macos.ax.set_value needs app, control, and optional role".to_owned(),
                recovery: None,
            },
        );
    };
    match run_osascript(&script) {
        Ok(output)
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == "true" =>
        {
            success(
                request,
                operation_id,
                "macos_ax",
                EffectState::Changed,
                VerificationState::Verified,
                json!({ "value_length": value.len(), "postcondition": "value_equal" }),
            )
        }
        Ok(output) => ax_failure(request, operation_id, &output),
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "adapter_unavailable".to_owned(),
                message: error.to_string(),
                recovery: Some("Check macOS Accessibility permission".to_owned()),
            },
        ),
    }
}

fn ax_reconcile(metadata: &Value) -> bool {
    let Some(app) = metadata.get("app").and_then(Value::as_str) else {
        return false;
    };
    let Some(control) = metadata.get("control").and_then(Value::as_str) else {
        return false;
    };
    if metadata.get("action").and_then(Value::as_str) == Some("reopen_closed_group") {
        let window = metadata
            .get("window")
            .and_then(Value::as_str)
            .map(apple_quote)
            .map(|name| format!("first window whose name is {name}"))
            .unwrap_or_else(|| "window 1".to_owned());
        let script = format!(
            "tell application \"System Events\"\ntell application process {}\nset targetWindow to {window}\nset remaining to (every button of targetWindow whose name is {})\nreturn ((count of remaining) is 0)\nend tell\nend tell",
            apple_quote(app),
            apple_quote(control)
        );
        let Ok(output) = run_osascript(&script) else {
            return false;
        };
        return output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true";
    }
    let Some(role) = metadata.get("role").and_then(Value::as_str) else {
        return false;
    };
    let element = match role {
        "button" => "button",
        "text_field" => "text field",
        "text_area" => "text area",
        "checkbox" => "checkbox",
        "static_text" => "static text",
        _ => return false,
    };
    let window = metadata
        .get("window")
        .and_then(Value::as_str)
        .map(apple_quote)
        .map(|name| format!("first window whose name is {name}"))
        .unwrap_or_else(|| "window 1".to_owned());
    let attribute = if metadata.get("action").and_then(Value::as_str) == Some("set_value") {
        "value"
    } else {
        let Some(attribute) = metadata
            .get("postcondition_attribute")
            .and_then(Value::as_str)
        else {
            return false;
        };
        attribute
    };
    let observation = match attribute {
        "value" | "name" => format!("({} of targetElement as text)", attribute),
        "enabled" | "focused" => format!("{} of targetElement", attribute),
        "exists" => "true".to_owned(),
        _ => return false,
    };
    let script = format!(
        "tell application \"System Events\"\ntell application process {}\nset targetWindow to {}\nset matches to (every {} of targetWindow whose name is {})\nif (count of matches) is not 1 then error \"target_ambiguous\"\nset targetElement to item 1 of matches\nreturn {}\nend tell\nend tell",
        apple_quote(app),
        window,
        element,
        apple_quote(control),
        observation
    );
    let Ok(output) = run_osascript(&script) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let observed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if attribute == "value" && metadata.get("action").and_then(Value::as_str) == Some("set_value") {
        return metadata
            .get("value_hash")
            .and_then(Value::as_u64)
            .is_some_and(|expected| stable_hash(observed.as_bytes()) == expected);
    }
    if let Some(expected) = metadata.get("postcondition_hash").and_then(Value::as_u64) {
        return stable_hash(observed.as_bytes()) == expected;
    }
    metadata
        .get("postcondition_bool")
        .and_then(Value::as_bool)
        .is_some_and(|expected| observed == expected.to_string())
}

fn ax_script(request: &OperationRequest, action: &str) -> Option<String> {
    let app = apple_quote(request.params.get("app")?.as_str()?);
    let control = apple_quote(request.params.get("control")?.as_str()?);
    let role = request
        .params
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("button");
    let element = match role {
        "button" => "button",
        "text_field" => "text field",
        "text_area" => "text area",
        "checkbox" => "checkbox",
        "static_text" => "static text",
        _ => return None,
    };
    let window = request
        .params
        .get("window")
        .and_then(Value::as_str)
        .map(apple_quote)
        .map(|name| format!("first window whose name is {name}"))
        .unwrap_or_else(|| "window 1".to_owned());
    let action_line = if action == "press" {
        let verification = match request.postcondition.as_ref() {
            Some(value) => Some(ax_postcondition_for_target(value)?),
            None => None,
        };
        match verification {
            Some(script) => format!("perform action \"AXPress\" of targetElement\nreturn {script}"),
            None => "perform action \"AXPress\" of targetElement\nreturn \"pressed\"".to_owned(),
        }
    } else {
        let value = action.strip_prefix("set_value:")?;
        format!(
            "set value of targetElement to {value}\nreturn ((value of targetElement as text) is {value})"
        )
    };
    Some(format!(
        "tell application \"System Events\"\ntell application process {app}\nset targetWindow to {window}\nset matches to (every {element} of targetWindow whose name is {control})\nif (count of matches) is not 1 then error \"target_ambiguous\"\nset targetElement to item 1 of matches\n{action_line}\nend tell\nend tell"
    ))
}

fn ax_postcondition_for_target(value: &Value) -> Option<String> {
    let attribute = value.get("attribute")?.as_str()?;
    let expected = value.get("equals")?;
    match attribute {
        "value" | "name" => Some(format!(
            "(({} of targetElement as text) is {})",
            attribute,
            apple_quote(expected.as_str()?)
        )),
        "enabled" | "focused" => Some(format!(
            "({} of targetElement is {})",
            attribute,
            if expected.as_bool()? { "true" } else { "false" }
        )),
        "exists" if expected.as_bool()? => Some("true".to_owned()),
        _ => None,
    }
}

fn ax_failure(
    request: &OperationRequest,
    operation_id: String,
    output: &std::process::Output,
) -> ActionResult {
    let diagnostic = String::from_utf8_lossy(&output.stderr).to_lowercase();
    let code = if diagnostic.contains("target_ambiguous") {
        "target_ambiguous"
    } else if diagnostic.contains("not authorized") || diagnostic.contains("assistive") {
        "permission_required"
    } else {
        "verification_failed"
    };
    ActionResult::refused(
        request,
        operation_id,
        ComptrolError {
            code: code.to_owned(),
            message: "macOS Accessibility did not confirm the requested semantic action".to_owned(),
            recovery: Some(
                "Refresh the target and verify macOS Accessibility permission".to_owned(),
            ),
        },
    )
}

fn unsupported_ax(request: &OperationRequest, operation_id: String) -> ActionResult {
    ActionResult::refused(
        request,
        operation_id,
        ComptrolError {
            code: "unsupported_surface".to_owned(),
            message: "macOS Accessibility actions are only available on macOS".to_owned(),
            recovery: Some("Inspect platform capabilities".to_owned()),
        },
    )
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

fn run_osascript(script: &str) -> io::Result<Output> {
    let mut child = Command::new("osascript")
        .args(["-e", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "macOS accessibility provider timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn capabilities() -> Vec<Capability> {
    let mut result = vec![
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
            name: "platform.broker.observe".to_owned(),
            available: true,
            risk: Risk::R0,
            route: "platform_broker".to_owned(),
            note: "Reports Windows UI Automation and Linux accessibility broker state without actuation"
                .to_owned(),
        },
        Capability {
            name: "filesystem.write".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SANDBOX_WRITES").as_deref() == Ok("1"),
            risk: Risk::R1,
            route: "sandbox_filesystem".to_owned(),
            note: "Available only after local sandbox policy is enabled".to_owned(),
        },
        Capability {
            name: "filesystem.copy".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SANDBOX_WRITES").as_deref() == Ok("1"),
            risk: Risk::R1,
            route: "sandbox_filesystem".to_owned(),
            note: "Copies regular files inside the sandbox after canonical path checks".to_owned(),
        },
        Capability {
            name: "desktop.notify".to_owned(),
            available: cfg!(target_os = "macos")
                && std::env::var("COMPTROL_ALLOW_DESKTOP_NOTIFY").as_deref() == Ok("1"),
            risk: Risk::R1,
            route: "platform_notification".to_owned(),
            note: "Available only after local notification policy is enabled".to_owned(),
        },
        Capability {
            name: "desktop.open_app".to_owned(),
            available: (cfg!(target_os = "macos")
                || cfg!(target_os = "windows")
                || cfg!(target_os = "linux"))
                && std::env::var("COMPTROL_ALLOW_APP_LAUNCH").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "platform_launch".to_owned(),
            note: "Opens an exact app through the native desktop launcher without mouse or clipboard input".to_owned(),
        },
        Capability {
            name: "browser.chrome.open_tab".to_owned(),
            available: (cfg!(target_os = "macos")
                || cfg!(target_os = "windows")
                || cfg!(target_os = "linux"))
                && std::env::var("COMPTROL_ALLOW_BROWSER_LAUNCH").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_launcher".to_owned(),
            note: "Opens a foreground Chrome tab in the existing default browser profile and reports launcher acceptance only".to_owned(),
        },
        Capability {
            name: "browser.chrome.reopen_closed_group".to_owned(),
            available: cfg!(target_os = "macos")
                && macos_accessibility_reachable()
                && std::env::var("COMPTROL_ALLOW_MACOS_AX").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "chrome_ax".to_owned(),
            note: "Reopens one exact closed Chrome tab group through semantic Accessibility control with a postcondition".to_owned(),
        },
        Capability {
            name: "command.run".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_COMMANDS").as_deref() == Ok("1")
                && std::env::var_os("COMPTROL_COMMAND_ROOT").is_some(),
            risk: Risk::R3,
            route: "process_argv".to_owned(),
            note: "Runs an explicitly allowlisted executable with argv inside an explicit local root and no shell".to_owned(),
        },
        Capability {
            name: "daemon.ipc".to_owned(),
            available: cfg!(unix) || cfg!(windows),
            risk: Risk::R0,
            route: if cfg!(windows) {
                "local_named_pipe".to_owned()
            } else {
                "local_unix_socket".to_owned()
            },
            note: "Provides bounded versioned local daemon IPC on the host platform".to_owned(),
        },
        Capability {
            name: "windows.uia.semantic".to_owned(),
            available: cfg!(target_os = "windows")
                && std::env::var("COMPTROL_ALLOW_WINDOWS_UIA").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "windows_uia".to_owned(),
            note: "Uses Windows UI Automation Invoke and Value patterns with exact process and element binding".to_owned(),
        },
        Capability {
            name: "linux.atspi.semantic".to_owned(),
            available: cfg!(target_os = "linux")
                && std::env::var("COMPTROL_ALLOW_LINUX_ATSPI").as_deref() == Ok("1")
                && std::env::var_os("AT_SPI_BUS_ADDRESS").is_some(),
            risk: Risk::R2,
            route: "linux_atspi".to_owned(),
            note: "Uses AT SPI action and editable text interfaces with exact process and element binding".to_owned(),
        },
        Capability {
            name: "browser.cdp".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Local CDP evaluation navigation uploads and downloads require explicit policy"
                .to_owned(),
        },
        Capability {
            name: "browser.cdp.open_tab".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Opens a visible or background tab in the existing local browser profile without mouse or clipboard input".to_owned(),
        },
        Capability {
            name: "browser.cdp.close_tab".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Closes one exact live page target after context and revision validation".to_owned(),
        },
        Capability {
            name: "browser.cdp.history".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Moves one exact live page target through bounded browser history without foreground input".to_owned(),
        },
        Capability {
            name: "browser.cdp.accessibility_snapshot".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R0,
            route: "browser_protocol".to_owned(),
            note: "Reads a bounded accessibility tree from one exact live page target".to_owned(),
        },
        Capability {
            name: "browser.cdp.focus".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R1,
            route: "browser_protocol".to_owned(),
            note: "Focuses one exact live page element without mouse or clipboard input".to_owned(),
        },
        Capability {
            name: "browser.cdp.semantic_click".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Resolves a fresh semantic locator, checks visibility and overlay coverage, then retries once after a stale target revision".to_owned(),
        },
        Capability {
            name: "browser.cdp.workflow".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Executes a bounded data-only browser navigation and semantic-click workflow in one MCP operation with URL postconditions".to_owned(),
        },
        Capability {
            name: "browser.cdp.reopen_closed_group".to_owned(),
            available: false,
            risk: Risk::R0,
            route: "browser_protocol".to_owned(),
            note: "Closed tab groups are not exposed as portable live DevTools targets".to_owned(),
        },
        Capability {
            name: "browser.cdp.discovery".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some(),
            risk: Risk::R0,
            route: "browser_protocol".to_owned(),
            note: "Discovers exact local browser targets without mutation".to_owned(),
        },
        Capability {
            name: "browser.fixture.submit".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_FIXTURE").as_deref() == Ok("1"),
            risk: Risk::R1,
            route: "browser_fixture".to_owned(),
            note: "Fixture only mutation with exact target identity and idempotency".to_owned(),
        },
        Capability {
            name: "desktop.semantic_input".to_owned(),
            available: cfg!(target_os = "macos")
                && macos_accessibility_reachable()
                && std::env::var("COMPTROL_ALLOW_MACOS_AX").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "macos_ax".to_owned(),
            note: "macOS semantic press and value routes require explicit policy and Accessibility permission".to_owned(),
        },
    ];
    result.extend(platform_capabilities());
    result
}

pub fn platform_capabilities() -> Vec<Capability> {
    vec![
        Capability {
            name: "platform.macos.ax".to_owned(),
            available: cfg!(target_os = "macos") && macos_accessibility_reachable(),
            risk: Risk::R2,
            route: "macos_ax".to_owned(),
            note: "Public System Events route with explicit local policy".to_owned(),
        },
        Capability {
            name: "platform.windows.uia".to_owned(),
            available: cfg!(target_os = "windows")
                && std::env::var("COMPTROL_WINDOWS_UIA").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "windows_uia".to_owned(),
            note: "Windows UI Automation broker detection and read only diagnostics".to_owned(),
        },
        Capability {
            name: "platform.linux.atspi".to_owned(),
            available: cfg!(target_os = "linux")
                && std::env::var_os("AT_SPI_BUS_ADDRESS").is_some(),
            risk: Risk::R2,
            route: "linux_atspi".to_owned(),
            note: "Linux AT SPI broker detection and read only diagnostics".to_owned(),
        },
        Capability {
            name: "platform.linux.x11".to_owned(),
            available: cfg!(target_os = "linux") && std::env::var_os("DISPLAY").is_some(),
            risk: Risk::R2,
            route: "linux_x11".to_owned(),
            note: "Linux X11 broker detection and read only diagnostics".to_owned(),
        },
        Capability {
            name: "platform.linux.wayland".to_owned(),
            available: cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some(),
            risk: Risk::R2,
            route: "linux_wayland".to_owned(),
            note: "Linux Wayland broker detection and read only diagnostics".to_owned(),
        },
    ]
}

pub fn platform_diagnostics() -> Value {
    let mac_accessible = cfg!(target_os = "macos") && macos_accessibility_reachable();
    let windows_uia_configured = std::env::var("COMPTROL_WINDOWS_UIA").as_deref() == Ok("1");
    let windows_uia_actuation = cfg!(target_os = "windows")
        && std::env::var("COMPTROL_ALLOW_WINDOWS_UIA").as_deref() == Ok("1");
    let at_spi_configured = std::env::var_os("AT_SPI_BUS_ADDRESS").is_some();
    let at_spi_actuation = cfg!(target_os = "linux")
        && at_spi_configured
        && std::env::var("COMPTROL_ALLOW_LINUX_ATSPI").as_deref() == Ok("1");
    let x11_configured = std::env::var_os("DISPLAY").is_some();
    let wayland_configured = std::env::var_os("WAYLAND_DISPLAY").is_some();
    json!({
        "os": std::env::consts::OS,
        "desktop": std::env::var("XDG_CURRENT_DESKTOP").ok(),
        "session_type": std::env::var("XDG_SESSION_TYPE").ok(),
        "display": std::env::var("DISPLAY").ok().is_some(),
        "wayland": std::env::var("WAYLAND_DISPLAY").ok().is_some(),
        "at_spi": std::env::var("AT_SPI_BUS_ADDRESS").ok().is_some(),
        "brokers": {
            "windows_uia": {
                "configured": std::env::var("COMPTROL_WINDOWS_UIA").as_deref() == Ok("1"),
                "actuation": windows_uia_actuation,
                "status": if !cfg!(target_os = "windows") { "unsupported" } else if windows_uia_actuation { "available" } else if windows_uia_configured { "degraded" } else { "unavailable" },
                "requires": if windows_uia_actuation { "Windows UI Automation fixture validation" } else { "COMPTROL_ALLOW_WINDOWS_UIA and UI Automation permission" }
            },
            "linux_atspi": {
                "configured": std::env::var_os("AT_SPI_BUS_ADDRESS").is_some(),
                "actuation": at_spi_actuation,
                "status": if !cfg!(target_os = "linux") { "unsupported" } else if at_spi_actuation { "available" } else if at_spi_configured { "degraded" } else { "unavailable" },
                "requires": if at_spi_actuation { "AT SPI fixture validation" } else { "AT SPI bus and COMPTROL_ALLOW_LINUX_ATSPI" }
            },
            "linux_x11": {
                "configured": std::env::var_os("DISPLAY").is_some(),
                "actuation": false,
                "status": if !cfg!(target_os = "linux") { "unsupported" } else if x11_configured { "degraded" } else { "unavailable" },
                "requires": "X11 fixture validation"
            },
            "linux_wayland": {
                "configured": std::env::var_os("WAYLAND_DISPLAY").is_some(),
                "actuation": false,
                "status": if !cfg!(target_os = "linux") { "unsupported" } else if wayland_configured { "degraded" } else { "unavailable" },
                "requires": "Wayland portal and fixture validation"
            },
            "macos_ax": {
                "configured": cfg!(target_os = "macos"),
                "actuation": mac_accessible,
                "status": if !cfg!(target_os = "macos") { "unsupported" } else if mac_accessible { "available" } else { "requires_human_consent" },
                "requires": "macOS Accessibility permission and fixture validation"
            }
        },
        "capabilities": platform_capabilities(),
    })
}

fn macos_accessibility_reachable() -> bool {
    run_osascript("tell application \"System Events\" to get name of every application process")
        .is_ok_and(|output| output.status.success())
}

fn doctor(runtime: &Runtime) -> Value {
    json!({
        "server": SERVER_VERSION,
        "protocol": PROTOCOL_VERSION,
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "daemon": { "state": "in_process", "available": true },
        "mcp_adapter": { "available": true, "transport": "stdio", "command": "comptrol mcp" },
        "policy": {
            "max_risk": runtime.policy.max_risk,
            "sandbox_writes": runtime.policy.allow_sandbox_writes,
            "desktop_notify": runtime.policy.allow_desktop_notify,
            "macos_ax": runtime.policy.allowed_intents.contains("macos.ax.press"),
            "browser_fixture": runtime.policy.allowed_intents.contains("browser.fixture.submit"),
            "commands": runtime.policy.allowed_intents.contains("command.run")
        },
        "journal": { "available": true, "path": runtime.journal.path() },
        "operations": { "available": true, "path": runtime.operations.path() },
        "checkpoints": { "available": true, "path": runtime.checkpoints.path() },
        "trace": { "enabled": runtime.trace.is_some(), "path": runtime.trace.as_ref().map(|trace| trace.path()) },
        "stop_latch": { "engaged": runtime.stop.engaged() },
        "desktop_observation": { "available": true, "semantic_mutation": platform_capabilities().iter().any(|capability| capability.name == "platform.macos.ax" && capability.available) },
        "platform": platform_diagnostics(),
        "browser": {
            "configured": std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some(),
            "fixture_mutation": runtime.policy.allowed_intents.contains("browser.fixture.submit"),
            "status": if std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some() { "configured" } else { "not_configured" }
        },
        "client_configuration": integration::list(),
        "remote": { "available": false, "binding": "loopback_only" },
        "state_dir": state_dir()
    })
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
#[serde(tag = "op", rename_all = "snake_case")]
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn runtime() -> Runtime {
        let suffix = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Runtime::new(std::env::temp_dir().join(format!("comptrol-test-{}-{}", now_ms(), suffix)))
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
            background: None,
        });
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("policy_denied")
        );
        assert!(matches!(result.delivery, DeliveryState::Refused));
    }

    #[test]
    fn invalid_background_posture_is_refused() {
        let mut runtime = runtime();
        let result = runtime.operate(OperationRequest {
            intent: "system.ping".to_owned(),
            target: None,
            params: Value::Null,
            postcondition: None,
            risk: None,
            idempotency_key: Some("bad-background".to_owned()),
            dry_run: false,
            background: Some("silent_downgrade".to_owned()),
        });
        assert_eq!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_input")
        );
    }

    #[test]
    fn macos_press_can_require_an_explicit_postcondition() {
        let request = OperationRequest {
            intent: "macos.ax.press".to_owned(),
            target: None,
            params: json!({"app":"Fixture","control":"Submit","role":"button"}),
            postcondition: Some(json!({"attribute":"enabled","equals":true})),
            risk: None,
            idempotency_key: Some("press-check".to_owned()),
            dry_run: false,
            background: None,
        };
        let script = ax_script(&request, "press").expect("semantic script");
        assert!(script.contains("perform action \"AXPress\""));
        assert!(script.contains("enabled of targetElement is true"));
    }

    #[test]
    fn chrome_closed_group_script_uses_exact_accessibility_press() {
        let request = OperationRequest {
            intent: "browser.chrome.reopen_closed_group".to_owned(),
            target: None,
            params: json!({"group":"Research", "window":"Chrome Window"}),
            postcondition: None,
            risk: None,
            idempotency_key: Some("closed-group-script".to_owned()),
            dry_run: false,
            background: None,
        };
        let script = chrome_closed_group_ax_script(&request, "Research");
        assert!(script.contains("Research group Closed"));
        assert!(script.contains("perform action \"AXPress\""));
        assert!(script.contains("count of remaining"));
        assert!(!script.contains("keystroke"));
        assert!(!script.contains("clipboard"));
    }

    #[test]
    fn chrome_closed_group_metadata_is_reconcilable_without_tab_content() {
        let request = OperationRequest {
            intent: "browser.chrome.reopen_closed_group".to_owned(),
            target: None,
            params: json!({"group":"Research"}),
            postcondition: None,
            risk: Some(Risk::R2),
            idempotency_key: Some("closed-group-metadata".to_owned()),
            dry_run: false,
            background: None,
        };
        let metadata = operation_metadata(&request);
        assert_eq!(metadata["app"], "Google Chrome");
        assert_eq!(metadata["control"], "Research group Closed");
        assert_eq!(metadata["postcondition_attribute"], "closed_group_absent");
        assert_eq!(metadata["postcondition_bool"], true);
    }

    #[test]
    fn semantic_mutation_metadata_is_recoverable_without_typed_content() {
        let request = OperationRequest {
            intent: "macos.ax.set_value".to_owned(),
            target: None,
            params: json!({"app":"Fixture","control":"Name","value":"safe","role":"text_field"}),
            postcondition: Some(json!({"attribute":"value","equals":"safe"})),
            risk: Some(Risk::R2),
            idempotency_key: Some("value-check".to_owned()),
            dry_run: false,
            background: None,
        };
        let metadata = operation_metadata(&request);
        assert_eq!(metadata["app"], "Fixture");
        assert_eq!(metadata["control"], "Name");
        assert_eq!(metadata["action"], "set_value");
        assert_eq!(metadata["postcondition_attribute"], "value");
        assert!(metadata.get("postcondition").is_none());
        assert!(metadata.get("value").is_none());
        assert!(metadata.get("value_hash").is_some());
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
            background: None,
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
            background: None,
        });
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("stopped")
        );
    }

    #[test]
    fn sandbox_copy_verifies_and_rejects_parent_traversal() {
        let directory = std::env::temp_dir().join(format!("comptrol-copy-{}", now_ms()));
        let sandbox = directory.join("sandbox");
        fs::create_dir_all(&sandbox).expect("sandbox");
        fs::write(sandbox.join("source.txt"), b"copy me").expect("source");
        let checkpoints = CheckpointStore::new(&directory).expect("checkpoints");
        let request = OperationRequest {
            intent: "filesystem.copy".to_owned(),
            target: None,
            params: json!({"source":"source.txt","destination":"nested/copy.txt"}),
            postcondition: None,
            risk: Some(Risk::R1),
            idempotency_key: Some("copy-test".to_owned()),
            dry_run: false,
            background: None,
        };
        let result = sandbox_copy_at(&request, "copy-op".to_owned(), &checkpoints, &sandbox);
        assert_eq!(result.verification, VerificationState::Verified);
        assert_eq!(
            fs::read(sandbox.join("nested/copy.txt")).expect("copy"),
            b"copy me"
        );
        let mut invalid = request;
        invalid.params["source"] = json!("../outside.txt");
        let result = sandbox_copy_at(&invalid, "copy-invalid".to_owned(), &checkpoints, &sandbox);
        assert_eq!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("path_denied")
        );
        let _ = fs::remove_dir_all(directory);
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

    #[test]
    fn workflow_executes_through_one_operation() {
        let mut runtime = runtime();
        let result = runtime.operate(OperationRequest {
            intent: "workflow.execute".to_owned(),
            target: None,
            params: json!({
                "ops": [
                    {"op":"sense","key":"ready","value":true},
                    {"op":"assert","key":"ready","equals":true},
                    {"op":"return","value":{"done":true}}
                ]
            }),
            postcondition: None,
            risk: None,
            idempotency_key: Some("workflow-operation".to_owned()),
            dry_run: false,
            background: None,
        });
        assert_eq!(result.verification, VerificationState::Verified);
        assert_eq!(result.data["done"], true);
    }

    #[test]
    fn browser_workflow_schema_is_bounded_and_data_only() {
        let steps: Vec<BrowserWorkflowStep> = serde_json::from_value(json!([
            {"action":"navigate","url":"https://example.com","url_contains":"example.com"},
            {"action":"click","locator":{"role":"link","name":"Example"},"timeout_ms":900},
            {"action":"wait_url","contains":"/done","timeout_ms":1200}
        ]))
        .expect("browser workflow schema");
        assert_eq!(steps.len(), 3);
        assert!(
            serde_json::from_value::<Vec<BrowserWorkflowStep>>(json!([
                {"action":"evaluate","expression":"alert(1)"}
            ]))
            .is_err()
        );
    }

    #[test]
    fn restart_returns_unknown_then_reconciles_written_file() {
        let dir = std::env::temp_dir().join(format!("comptrol-recovery-{}", now_ms()));
        let path = dir.join("sandbox").join("recovered.txt");
        fs::create_dir_all(path.parent().expect("parent")).expect("sandbox");
        fs::write(&path, b"recovered").expect("fixture");
        let request = OperationRequest {
            intent: "filesystem.write".to_owned(),
            target: None,
            params: json!({ "path": "recovered.txt", "content": "recovered" }),
            postcondition: None,
            risk: Some(Risk::R1),
            idempotency_key: Some("recover-key".to_owned()),
            dry_run: false,
            background: None,
        };
        let record = DurableOperation {
            operation_id: "op-restart".to_owned(),
            idempotency_key: Some("recover-key".to_owned()),
            intent: request.intent.clone(),
            risk: Risk::R1,
            target: None,
            state: DurableState::Dispatched,
            metadata: json!({ "path": path, "content_hash": stable_hash(b"recovered") }),
            result: None,
        };
        fs::create_dir_all(&dir).expect("state");
        fs::write(
            dir.join("operations.jsonl"),
            serde_json::to_string(&record).expect("record") + "\n",
        )
        .expect("journal");
        let mut runtime = Runtime::new(dir).expect("runtime");
        let unknown = runtime.operate(request);
        assert_eq!(
            unknown.error.as_ref().map(|error| error.code.as_str()),
            Some("operation_unknown")
        );
        assert_eq!(runtime.watch("op-restart")["state"], "unknown");
        let reconciled = runtime.reconcile("op-restart");
        assert_eq!(reconciled["state"], "reconciled");
    }

    #[test]
    fn restart_marks_pre_dispatch_work_interrupted() {
        let dir = std::env::temp_dir().join(format!("comptrol-interrupted-{}", now_ms()));
        let record = DurableOperation {
            operation_id: "op-interrupted".to_owned(),
            idempotency_key: Some("interrupted-key".to_owned()),
            intent: "command.run".to_owned(),
            risk: Risk::R3,
            target: None,
            state: DurableState::Authorized,
            metadata: json!({"program":"fixture"}),
            result: None,
        };
        fs::create_dir_all(&dir).expect("state");
        fs::write(
            dir.join("operations.jsonl"),
            serde_json::to_string(&record).expect("record") + "\n",
        )
        .expect("journal");
        let journal = OperationJournal::open(&dir).expect("reopen journal");
        assert_eq!(
            journal.record("op-interrupted").expect("operation").state,
            DurableState::Interrupted
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_result_is_not_replayed_after_restart() {
        let dir = std::env::temp_dir().join(format!("comptrol-unknown-{}", now_ms()));
        let request = OperationRequest {
            intent: "filesystem.write".to_owned(),
            target: None,
            params: json!({ "path": "unknown.txt", "content": "unknown" }),
            postcondition: None,
            risk: Some(Risk::R1),
            idempotency_key: Some("unknown-key".to_owned()),
            dry_run: false,
            background: None,
        };
        let result = unknown_result(&request, "op-unknown".to_owned());
        let record = DurableOperation {
            operation_id: "op-unknown".to_owned(),
            idempotency_key: request.idempotency_key.clone(),
            intent: request.intent.clone(),
            risk: Risk::R1,
            target: None,
            state: DurableState::Unknown,
            metadata: json!({}),
            result: Some(result),
        };
        fs::create_dir_all(&dir).expect("state");
        fs::write(
            dir.join("operations.jsonl"),
            serde_json::to_string(&record).expect("record") + "\n",
        )
        .expect("journal");
        let mut runtime = Runtime::new(dir.clone()).expect("runtime");
        let replay = runtime.operate(request);
        assert_eq!(replay.recovery, RecoveryState::RequiresReconciliation);
        assert_eq!(replay.delivery, DeliveryState::Unknown);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn delivered_unverified_result_is_replayed_after_restart() {
        let dir = std::env::temp_dir().join(format!("comptrol-observed-{}", now_ms()));
        let request = OperationRequest {
            intent: "desktop.notify".to_owned(),
            target: None,
            params: json!({"title":"fixture","body":"fixture"}),
            postcondition: None,
            risk: Some(Risk::R1),
            idempotency_key: Some("observed-key".to_owned()),
            dry_run: false,
            background: None,
        };
        let result = success(
            &request,
            "op-observed".to_owned(),
            "platform_notification",
            EffectState::Changed,
            VerificationState::Unverified,
            json!({"sent":true}),
        );
        let record = DurableOperation {
            operation_id: "op-observed".to_owned(),
            idempotency_key: request.idempotency_key.clone(),
            intent: request.intent.clone(),
            risk: Risk::R1,
            target: None,
            state: DurableState::Observed,
            metadata: json!({}),
            result: Some(result),
        };
        fs::create_dir_all(&dir).expect("state");
        fs::write(
            dir.join("operations.jsonl"),
            serde_json::to_string(&record).expect("record") + "\n",
        )
        .expect("journal");
        let mut runtime = Runtime::new(dir.clone()).expect("runtime");
        let replay = runtime.operate(request);
        assert_eq!(replay.recovery, RecoveryState::IdempotentReplay);
        assert_eq!(replay.operation_id, "op-observed");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn browser_binding_rejects_gone_and_stale_targets() {
        let targets = vec![BrowserTarget {
            id: "tab-1".to_owned(),
            browser_context_id: Some("context-1".to_owned()),
            target_type: Some("page".to_owned()),
            url: Some("http://127.0.0.1/".to_owned()),
            title: Some("fixture".to_owned()),
            revision: Some("revision-1".to_owned()),
            web_socket_url: Some("ws://127.0.0.1/devtools/page/tab-1".to_owned()),
        }];
        assert_eq!(
            bind_browser_target(&targets, "missing", None, None)
                .expect_err("missing target")
                .code,
            "target_gone"
        );
        assert_eq!(
            bind_browser_target(&targets, "tab-1", Some("context-2"), Some("revision-1"))
                .expect_err("stale target")
                .code,
            "stale_reference"
        );
        assert_eq!(
            bind_browser_target(&targets, "tab-1", Some("context-1"), Some("revision-1"))
                .expect("bound target")
                .id,
            "tab-1"
        );
        let mut browser_ui = targets[0].clone();
        browser_ui.id = "browser-ui".to_owned();
        browser_ui.target_type = Some("browser_ui".to_owned());
        assert_eq!(
            bind_browser_target(&[browser_ui], "browser-ui", None, None)
                .expect_err("browser UI target")
                .code,
            "wrong_target_type"
        );
    }

    #[test]
    fn event_bus_deduplicates_and_bounds_history() {
        let mut events = EventBus::new(2);
        let first = events.emit("file.changed", json!({"path":"a"}));
        let duplicate = events.emit("file.changed", json!({"path":"a"}));
        assert_eq!(first.sequence, duplicate.sequence);
        events.emit("file.changed", json!({"path":"b"}));
        events.emit("file.changed", json!({"path":"c"}));
        assert_eq!(events.since(0, None).len(), 2);
        assert_eq!(
            events
                .wait_for(2, Some("file.changed"), Duration::ZERO)
                .unwrap()
                .payload["path"],
            "c"
        );
    }

    #[test]
    fn checkpoint_restores_existing_file() {
        let dir = std::env::temp_dir().join(format!("comptrol-checkpoint-{}", now_ms()));
        let source = dir.join("note.txt");
        fs::create_dir_all(&dir).expect("checkpoint dir");
        fs::write(&source, b"before").expect("source");
        let store = CheckpointStore::new(&dir).expect("store");
        let checkpoint = store.create("operation", &source).expect("checkpoint");
        fs::write(&source, b"after").expect("mutate");
        store.restore_id(&checkpoint.id).expect("restore");
        assert_eq!(fs::read(&source).expect("read"), b"before");
    }

    #[test]
    fn privacy_trace_redacts_typed_values() {
        let dir = std::env::temp_dir().join(format!("comptrol-trace-{}", now_ms()));
        let path = dir.join("trace.jsonl");
        let recorder = TraceRecorder::open(path.clone(), TraceMode::PrivacyMinimal).expect("trace");
        let request = OperationRequest {
            intent: "filesystem.write".to_owned(),
            target: None,
            params: json!({"path":"note.txt","content":"secret"}),
            postcondition: Some(json!({"value":"secret"})),
            risk: Some(Risk::R1),
            idempotency_key: Some("trace-key".to_owned()),
            dry_run: true,
            background: None,
        };
        let result = ActionResult {
            operation_id: "trace-op".to_owned(),
            intent: request.intent.clone(),
            route: "sandbox_filesystem".to_owned(),
            target: None,
            preflight: "passed".to_owned(),
            delivery: DeliveryState::NotDispatched,
            effect: EffectState::NotAttempted,
            verification: VerificationState::NotAttempted,
            disturbance: json!({"foreground_changed":false}),
            recovery: RecoveryState::None,
            data: Value::Null,
            error: None,
        };
        recorder.append(&request, &result).expect("append");
        let entries = read_trace(&path).expect("read trace");
        assert_eq!(entries[0].request.params["content"]["redacted"], true);
        assert!(entries[0].request.postcondition.is_none());
    }

    #[test]
    fn virtual_desktop_handles_left_above_and_mixed_scale_displays() {
        let desktop = VirtualDesktop {
            revision: 4,
            displays: vec![
                DisplayGeometry {
                    id: "left".to_owned(),
                    origin_logical: Point { x: -1280.0, y: 0.0 },
                    size_logical: Point {
                        x: 1280.0,
                        y: 720.0,
                    },
                    scale: 1.0,
                },
                DisplayGeometry {
                    id: "above".to_owned(),
                    origin_logical: Point { x: 0.0, y: -900.0 },
                    size_logical: Point {
                        x: 1440.0,
                        y: 900.0,
                    },
                    scale: 2.0,
                },
            ],
        };
        assert_eq!(
            desktop.physical_to_virtual("left", Point { x: 20.0, y: 30.0 }),
            Some(Point {
                x: -1260.0,
                y: 30.0
            })
        );
        assert_eq!(
            desktop.physical_to_virtual("above", Point { x: 200.0, y: 100.0 }),
            Some(Point {
                x: 100.0,
                y: -850.0
            })
        );
        assert!(desktop.contains(
            "above",
            Point {
                x: 100.0,
                y: -850.0
            }
        ));
        assert_eq!(
            desktop.virtual_to_physical(
                "above",
                Point {
                    x: 100.0,
                    y: -850.0
                }
            ),
            Some(Point { x: 200.0, y: 100.0 })
        );
    }

    #[test]
    fn adapter_registry_rejects_duplicate_names() {
        let mut registry = AdapterRegistry::builtin();
        assert!(
            registry
                .list()
                .iter()
                .any(|adapter| adapter.name == "comptrol.browser.cdp")
        );
        let descriptor = registry.list()[0].clone();
        assert!(!registry.register(descriptor.clone()));
        assert!(registry.register(AdapterDescriptor {
            name: "fixture".to_owned(),
            ..descriptor
        }));
    }

    #[test]
    fn adapter_registry_rejects_malformed_descriptors() {
        let mut registry = AdapterRegistry::default();
        assert!(!registry.register(AdapterDescriptor {
            name: "bad adapter".to_owned(),
            version: "0.1".to_owned(),
            platforms: vec!["macos".to_owned()],
            capabilities: vec!["observe".to_owned()],
            route: "native".to_owned(),
            risk: Risk::R0,
            isolation: "trusted".to_owned(),
        }));
        assert!(!registry.register(AdapterDescriptor {
            name: "comptrol.bad".to_owned(),
            version: "0.1".to_owned(),
            platforms: Vec::new(),
            capabilities: vec!["observe".to_owned()],
            route: "native".to_owned(),
            risk: Risk::R0,
            isolation: "trusted".to_owned(),
        }));
    }

    #[test]
    fn audit_journal_redacts_typed_action_data() {
        let dir = std::env::temp_dir().join(format!("comptrol-audit-{}", now_ms()));
        let mut journal = AuditJournal::open(&dir).expect("journal");
        let result = ActionResult {
            operation_id: "audit-op".to_owned(),
            intent: "filesystem.write".to_owned(),
            route: "sandbox_filesystem".to_owned(),
            target: Some(Target {
                kind: "fixture".to_owned(),
                id: Some("secret target".to_owned()),
                name: Some("private name".to_owned()),
            }),
            preflight: "passed".to_owned(),
            delivery: DeliveryState::Delivered,
            effect: EffectState::Changed,
            verification: VerificationState::Verified,
            disturbance: json!({"foreground_changed":false}),
            recovery: RecoveryState::None,
            data: json!({"content":"secret body"}),
            error: None,
        };
        journal.append(&result).expect("append");
        let contents = fs::read_to_string(journal.path()).expect("audit");
        assert!(!contents.contains("secret body"));
        assert!(contents.contains("risk_data"));
    }
}
