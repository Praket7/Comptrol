#![deny(unsafe_code)]

pub mod adapters;
pub mod browser;
pub mod checkpoints;
pub mod events;
pub mod geometry;
pub mod integration;
pub mod trace;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use adapters::{AdapterDescriptor, AdapterRegistry};
pub use checkpoints::{Checkpoint, CheckpointStore};
pub use events::{Event, EventBus};
pub use geometry::{DisplayGeometry, Point, VirtualDesktop};
pub use trace::{TraceEntry, TraceMode, TraceRecorder, read_trace};

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
            ]);
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
    Dispatched,
    Complete,
    Unknown,
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
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
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
        self.records
            .values()
            .filter(|record| record.result.is_some())
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

    pub fn complete(
        &mut self,
        request: &OperationRequest,
        result: &ActionResult,
    ) -> io::Result<()> {
        let state = if matches!(&result.delivery, DeliveryState::Unknown) {
            DurableState::Unknown
        } else {
            DurableState::Complete
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
            "filesystem.restore_checkpoint" => {
                restore_checkpoint(&request, operation_id, &self.checkpoints)
            }
            "desktop.notify" => desktop_notify(&request, operation_id),
            "macos.ax.press" => macos_ax_press(&request, operation_id),
            "macos.ax.set_value" => macos_ax_set_value(&request, operation_id),
            "browser.fixture.submit" => browser_fixture_submit(&request, operation_id),
            "browser.cdp.evaluate"
            | "browser.cdp.navigate"
            | "browser.cdp.upload"
            | "browser.cdp.download"
            | "browser.cdp.fill"
            | "browser.cdp.click"
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
        if record.intent == "filesystem.write" || record.intent == "browser.cdp.download" {
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
        "desktop.notify" | "filesystem.write" | "filesystem.restore_checkpoint" => Risk::R1,
        "macos.ax.press" | "macos.ax.set_value" => Risk::R2,
        "browser.fixture.submit" => Risk::R1,
        "browser.cdp.evaluate"
        | "browser.cdp.navigate"
        | "browser.cdp.upload"
        | "browser.cdp.download"
        | "browser.cdp.fill"
        | "browser.cdp.click" => Risk::R2,
        "browser.cdp.wait_for" => Risk::R0,
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
        disturbance: json!({ "foreground_changed": false }),
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
    if request.intent == "filesystem.write" {
        if let Some(path) = request.params.get("path").and_then(Value::as_str) {
            metadata["path"] = json!(state_dir().join("sandbox").join(path));
        }
        if let Some(content) = request.params.get("content").and_then(Value::as_str) {
            metadata["content_len"] = json!(content.len());
            metadata["content_hash"] = json!(stable_hash(content.as_bytes()));
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
        "filesystem.restore_checkpoint" => "sandbox_checkpoint",
        "desktop.notify" => "platform_notification",
        "macos.ax.press" | "macos.ax.set_value" => "macos_ax",
        "browser.fixture.submit" => "browser_fixture",
        "browser.cdp.evaluate"
        | "browser.cdp.navigate"
        | "browser.cdp.upload"
        | "browser.cdp.download"
        | "browser.cdp.fill"
        | "browser.cdp.click"
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
    if matches!(
        request.intent.as_str(),
        "browser.cdp.fill" | "browser.cdp.click" | "browser.cdp.wait_for"
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
                } else {
                    VerificationState::Unverified
                },
                json!({ "result": data, "postcondition": if verified { "verified" } else { "unverified" } }),
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
            Some(value) => Some(ax_postcondition(value)?),
            None => None,
        };
        match verification {
            Some(script) => format!("perform action \"AXPress\" of targetElement\nreturn {script}"),
            None => "perform action \"AXPress\" of targetElement\nreturn \"pressed\"".to_owned(),
        }
    } else if let Some(value) = action.strip_prefix("set_value:") {
        format!(
            "set value of targetElement to {value}\nreturn ((value of targetElement as text) is {value})"
        )
    } else {
        return None;
    };
    Some(format!(
        "tell application \"System Events\"\ntell application process {app}\nset targetWindow to {window}\nset matches to (every {element} of targetWindow whose name is {control})\nif (count of matches) is not 1 then error \"target_ambiguous\"\nset targetElement to item 1 of matches\n{action_line}\nend tell\nend tell"
    ))
}

fn ax_postcondition(value: &Value) -> Option<String> {
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
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Local CDP evaluation navigation uploads and downloads require explicit policy"
                .to_owned(),
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
            available: false,
            risk: Risk::R2,
            route: "platform_accessibility".to_owned(),
            note: "Not advertised until a platform backend and verification suite exist".to_owned(),
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
                "actuation": false,
                "requires": "Windows UI Automation fixture validation"
            },
            "linux_atspi": {
                "configured": std::env::var_os("AT_SPI_BUS_ADDRESS").is_some(),
                "actuation": false,
                "requires": "AT SPI fixture validation"
            },
            "linux_x11": {
                "configured": std::env::var_os("DISPLAY").is_some(),
                "actuation": false,
                "requires": "X11 fixture validation"
            },
            "linux_wayland": {
                "configured": std::env::var_os("WAYLAND_DISPLAY").is_some(),
                "actuation": false,
                "requires": "Wayland portal and fixture validation"
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
            "browser_fixture": runtime.policy.allowed_intents.contains("browser.fixture.submit")
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
        });
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("policy_denied")
        );
        assert!(matches!(result.delivery, DeliveryState::Refused));
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
        };
        let script = ax_script(&request, "press").expect("semantic script");
        assert!(script.contains("perform action \"AXPress\""));
        assert!(script.contains("enabled of targetElement is true"));
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
        });
        assert_eq!(result.verification, VerificationState::Verified);
        assert_eq!(result.data["done"], true);
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
        let reconciled = runtime.reconcile("op-restart");
        assert_eq!(reconciled["state"], "reconciled");
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
        let descriptor = registry.list()[0].clone();
        assert!(!registry.register(descriptor.clone()));
        assert!(registry.register(AdapterDescriptor {
            name: "fixture".to_owned(),
            ..descriptor
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
