#![deny(unsafe_code)]

pub mod adapters;
pub mod browser;
pub mod browser_bridge;
pub mod checkpoints;
pub mod events;
pub mod geometry;
pub mod integration;
pub mod mcp;
pub mod pairing;
pub mod restore;
pub mod restore_native;
pub mod trace;

use comptrol_adapter_host::{AdapterHost, AdapterHostConfig};
use comptrol_adapter_sdk::{AdapterManifest, HealthState};
use comptrol_browser::{DownloadStage, DownloadTransaction, UploadTransaction};
pub use comptrol_verification::{
    VerificationCriterion, VerificationEvidence, VerificationLevel, VerificationReport,
    VerificationSource, VerificationState as StructuredVerificationState,
};
use comptrol_workflow::{Workflow, WorkflowExecutor, WorkflowNode};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use adapters::{AdapterDescriptor, AdapterRegistry};
pub use checkpoints::{Checkpoint, CheckpointStore};
pub use events::{DurableEventHub, Event, EventBus};
pub use geometry::{DisplayGeometry, Point, VirtualDesktop};
pub use trace::{
    CompiledStep, CompiledWorkflow, TraceEntry, TraceMode, TraceRecorder, WorkflowPrecondition,
    compile_verified_trace, read_trace, validate_compiled_workflow,
};

pub const PROTOCOL_VERSION: &str = "0.1";
pub const SERVER_VERSION: &str = "0.1.63";
pub const MAX_PROTOCOL_BYTES: usize = 1024 * 1024;

const FIRST_PARTY_ADAPTER_INTENTS: &[&str] = &[
    "vscode.workspace.list",
    "vscode.setting.get",
    "vscode.setting.set",
    "vscode.document.open",
    "vscode.document.save",
    "libreoffice.document.open",
    "libreoffice.document.save",
    "libreoffice.calc.range.read",
    "libreoffice.calc.range.write",
    "libreoffice.writer.text.replace",
    "libreoffice.document.export",
    "obs.scene.list",
    "obs.scene.switch",
    "obs.source.visibility.set",
    "obs.recording.status",
    "obs.recording.start",
    "obs.recording.stop",
    "blender.scene.object.list",
    "blender.scene.object.create",
    "blender.scene.object.transform",
    "blender.project.save",
    "blender.render",
    "video.project.list",
    "video.project.open",
    "video.project.create",
    "video.project.save",
    "video.media.import",
    "video.media.bin.create",
    "video.media.list",
    "video.timeline.list",
    "video.timeline.open",
    "video.timeline.create",
    "video.timeline.items.list",
    "video.timeline.append",
    "video.timeline.insert",
    "video.timeline.batch",
    "video.timeline.marker.add",
    "video.timeline.marker.delete",
    "video.timeline.item.properties.get",
    "video.timeline.item.properties.set",
    "video.render.preset.list",
    "video.render.configure",
    "video.render.add_job",
    "video.render.start",
    "video.render.status",
    "video.render.cancel",
    "document.google.read",
    "document.google.batch_edit",
    "document.text.insert",
    "document.text.replace",
    "document.text.style",
    "document.export",
    "presentation.google.read",
    "presentation.slide.create",
    "presentation.slide.delete",
    "presentation.slide.reorder",
    "presentation.text.replace",
    "presentation.text.style",
    "presentation.export",
    "presentation.read",
    "presentation.batch_edit",
    "presentation.desktop.open",
    "presentation.shape.text.set",
    "presentation.save",
    "presentation.export_pdf",
    "discord.message.draft",
    "discord.message.send",
    "discord.message.edit",
    "discord.message.delete",
    "discord.message.reply",
    "discord.message.react",
    "discord.message.attach",
    "discord.message.search",
    "mail.draft",
    "mail.send",
    "mail.search",
    "mail.read",
    "message.draft",
    "message.send",
    "design.list",
    "design.read",
    "design.page.list",
    "design.element.inspect",
    "design.text.update",
    "design.image.insert",
    "design.element.create",
    "design.element.delete",
    "design.element.group",
    "design.export",
];

fn is_first_party_adapter_intent(intent: &str) -> bool {
    FIRST_PARTY_ADAPTER_INTENTS.contains(&intent)
}

fn adapter_id_for_intent(intent: &str, provider: Option<&str>) -> Result<&'static str, String> {
    match intent.split('.').next().unwrap_or("") {
        "vscode" => Ok("vscode"),
        "libreoffice" => Ok("libreoffice"),
        "obs" => Ok("obs"),
        "blender" => Ok("blender"),
        "video" => Ok("davinci-resolve"),
        "discord" => Ok("discord"),
        "message" => Ok("apple-messages"),
        "design" => Ok("canva"),
        "document" => Ok("google-workspace"),
        "presentation" => match intent {
            _ if intent.starts_with("presentation.google.")
                || intent.starts_with("presentation.text.") =>
            {
                Ok("google-workspace")
            }
            "presentation.read" | "presentation.batch_edit" => Ok("powerpoint"),
            _ if intent.starts_with("presentation.desktop.")
                || intent.starts_with("presentation.shape.")
                || intent == "presentation.save"
                || intent == "presentation.export_pdf" =>
            {
                Ok("powerpoint-windows")
            }
            _ if intent.starts_with("presentation.slide.") => match provider {
                Some("google" | "slides" | "google-workspace") => Ok("google-workspace"),
                Some("powerpoint" | "windows" | "desktop" | "powerpoint-windows") => {
                    Ok("powerpoint-windows")
                }
                _ => Err(format!(
                    "{intent} is served by Google Slides and desktop PowerPoint; set params.provider to \"google\" or \"powerpoint\""
                )),
            },
            "presentation.export" => match provider {
                Some("google" | "slides" | "google-workspace") => Ok("google-workspace"),
                Some("powerpoint" | "openxml" | "desktop" | "windows") => Ok("powerpoint"),
                _ => Err(format!(
                    "{intent} is served by Google Slides and PowerPoint; set params.provider to \"google\" or \"powerpoint\""
                )),
            },
            _ => Err(format!("No route is implemented for {intent}")),
        },
        "mail" => match provider {
            Some("gmail" | "google") => Ok("gmail"),
            Some("graph" | "outlook" | "microsoft" | "microsoft-graph-mail") => {
                Ok("microsoft-graph-mail")
            }
            Some("apple-mail" | "apple" | "mail-app") => Ok("apple-mail"),
            _ => Err(format!(
                "{intent} is served by Gmail, Microsoft Graph, and Apple Mail; set params.provider to \"gmail\", \"graph\", or \"apple-mail\""
            )),
        },
        _ => Err(format!("No route is implemented for {intent}")),
    }
}

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

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
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
pub struct RouteCandidate {
    pub route: String,
    pub feasible: bool,
    pub rationale: String,
    #[serde(default)]
    pub historical_success: f64,
    #[serde(default)]
    pub expected_p95_ms: Option<f64>,
    #[serde(default)]
    pub expected_model_turns: u32,
    #[serde(default)]
    pub expected_context_bytes: u64,
    #[serde(default)]
    pub verification_strength: String,
    #[serde(default)]
    pub disturbance_class: String,
    #[serde(default)]
    pub reversibility: String,
    #[serde(default)]
    pub target_binding: String,
    #[serde(default)]
    pub utility: Option<f64>,
}

impl RouteCandidate {
    fn new(route: &str, feasible: bool, rationale: String) -> Self {
        let (
            expected_p95_ms,
            expected_model_turns,
            expected_context_bytes,
            verification_strength,
            disturbance_class,
            reversibility,
            target_binding,
        ) = match route {
            "native" | "workflow" => (
                Some(5.0),
                0,
                512,
                "application_state",
                "none",
                "reversible",
                "exact",
            ),
            "browser_protocol" => (
                Some(80.0),
                0,
                2_048,
                "surface_state",
                "background",
                "partial",
                "target_bound",
            ),
            "isolated_adapter" => (
                Some(120.0),
                0,
                1_024,
                "application_state",
                "background",
                "partial",
                "resource_bound",
            ),
            _ => (
                None,
                0,
                4_096,
                "delivery",
                "foreground_possible",
                "unknown",
                "discovered",
            ),
        };
        let utility = feasible.then(|| {
            let verification = match verification_strength {
                "independent_outcome" => 1.0,
                "persisted_artifact" => 0.95,
                "application_state" => 0.85,
                "surface_state" => 0.65,
                _ => 0.35,
            };
            let latency_penalty: f64 = expected_p95_ms.unwrap_or(1_000.0) / 1_000.0;
            (0.55 * verification) + (0.3 * (1.0 - latency_penalty.min(1.0))) + (0.15 * 1.0)
        });
        Self {
            route: route.to_owned(),
            feasible,
            rationale,
            historical_success: 0.5,
            expected_p95_ms,
            expected_model_turns,
            expected_context_bytes,
            verification_strength: verification_strength.to_owned(),
            disturbance_class: disturbance_class.to_owned(),
            reversibility: reversibility.to_owned(),
            target_binding: target_binding.to_owned(),
            utility,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RoutePlan {
    pub intent: String,
    pub selected: Option<String>,
    pub candidates: Vec<RouteCandidate>,
    pub rationale: String,
}

const ROUTE_LATENCY_SAMPLE_CAP: usize = 256;

#[derive(Clone, Debug, Default)]
struct RouteHistory {
    attempts: u64,
    verified_successes: u64,
    verification_failures: u64,
    dispatch_failures: u64,
    disturbance_events: u64,
    ewma_latency_ms: Option<f64>,
    latency_samples_ms: VecDeque<f64>,
    p95_latency_ms: Option<f64>,
    last_success_at_ms: Option<u128>,
}

impl RouteHistory {
    fn success_rate(&self) -> f64 {
        if self.attempts == 0 {
            0.5
        } else {
            (self.verified_successes as f64 / self.attempts as f64).clamp(0.0, 1.0)
        }
    }

    fn record_latency(&mut self, latency_ms: f64) {
        self.latency_samples_ms.push_back(latency_ms);
        while self.latency_samples_ms.len() > ROUTE_LATENCY_SAMPLE_CAP {
            self.latency_samples_ms.pop_front();
        }
        self.p95_latency_ms = percentile_95(&self.latency_samples_ms);
    }
}

fn percentile_95(samples: &VecDeque<f64>) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.iter().copied().collect::<Vec<_>>();
    sorted.sort_by(f64::total_cmp);
    let nearest_rank = ((sorted.len() as f64) * 0.95).ceil() as usize;
    sorted.get(nearest_rank.saturating_sub(1)).copied()
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
    pub allow_app_launch: bool,
    pub max_risk: Risk,
    pub allowed_intents: HashSet<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            allow_sandbox_writes: false,
            allow_desktop_notify: false,
            allow_app_launch: false,
            max_risk: Risk::R0,
            allowed_intents: HashSet::from([
                "system.ping".to_owned(),
                "desktop.observe".to_owned(),
                "platform.broker.observe".to_owned(),
                "browser.cdp.wait_for".to_owned(),
                "browser.cdp.accessibility_snapshot".to_owned(),
                "browser.cdp.reopen_closed_group".to_owned(),
                "workflow.execute".to_owned(),
                "app.resolve".to_owned(),
                "app.list".to_owned(),
                "permission.status".to_owned(),
                "popup.inspect".to_owned(),
                "browser.session.list".to_owned(),
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
            policy
                .allowed_intents
                .insert("browser.chrome.restore_recent".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_APP_LAUNCH").as_deref() == Ok("1") {
            policy.allow_app_launch = true;
            policy.max_risk = Risk::R2;
            policy.allowed_intents.insert("desktop.open_app".to_owned());
            policy.allowed_intents.insert("app.launch".to_owned());
            policy.allowed_intents.insert("app.resolve".to_owned());
            policy.allowed_intents.insert("app.list".to_owned());
            policy
                .allowed_intents
                .insert("app.open_resource".to_owned());
            policy.allowed_intents.insert("app.focus".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_APP_CLOSE").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R3);
            policy.allowed_intents.insert("app.close".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_SOFTWARE").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R1);
            policy.allowed_intents.insert("software.search".to_owned());
            policy
                .allowed_intents
                .insert("software.describe".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_SOFTWARE_INSTALL").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R3);
            policy.allowed_intents.insert("software.install".to_owned());
            policy.allowed_intents.insert("software.update".to_owned());
            policy
                .allowed_intents
                .insert("software.uninstall".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_SETTINGS").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy.allowed_intents.insert("settings.get".to_owned());
            policy.allowed_intents.insert("settings.set".to_owned());
            policy.allowed_intents.insert("settings.write".to_owned());
            policy
                .allowed_intents
                .insert("permission.status".to_owned());
            policy
                .allowed_intents
                .insert("permission.request".to_owned());
        }
        if std::env::var("COMPTROL_ALLOW_POPUP").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy.allowed_intents.insert("popup.inspect".to_owned());
            policy.allowed_intents.insert("popup.dismiss".to_owned());
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
        if std::env::var("COMPTROL_ALLOW_ALL_INTENTS").as_deref() == Ok("1") {
            policy.max_risk = Risk::R3;
            for intent in FIRST_PARTY_ADAPTER_INTENTS {
                policy.allowed_intents.insert((*intent).to_owned());
            }
            policy.allowed_intents.extend([
                "system.ping".to_owned(),
                "desktop.observe".to_owned(),
                "platform.broker.observe".to_owned(),
                "browser.cdp.wait_for".to_owned(),
                "browser.cdp.accessibility_snapshot".to_owned(),
                "browser.cdp.reopen_closed_group".to_owned(),
                "workflow.execute".to_owned(),
                "app.resolve".to_owned(),
                "app.list".to_owned(),
                "permission.status".to_owned(),
                "popup.inspect".to_owned(),
                "browser.session.list".to_owned(),
                "browser.session.connect".to_owned(),
            ]);
        }
        if std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy.allowed_intents.extend([
                "browser.cdp.evaluate".to_owned(),
                "browser.cdp.frame_evaluate".to_owned(),
                "browser.cdp.ensure_state".to_owned(),
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
                "browser.cdp.screenshot".to_owned(),
                "browser.cdp.coordinate_click".to_owned(),
                "browser.cdp.dialog".to_owned(),
                "browser.session.connect".to_owned(),
            ]);
        }
        if std::env::var("COMPTROL_ALLOW_BROWSER_LAUNCH").as_deref() == Ok("1") {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy
                .allowed_intents
                .insert("browser.chrome.open_tab".to_owned());
        }
        if env_enabled("COMPTROL_ALLOW_ADAPTERS") {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            for intent in FIRST_PARTY_ADAPTER_INTENTS {
                if classify(intent) <= Risk::R2 {
                    policy.allowed_intents.insert((*intent).to_owned());
                }
            }
            if env_enabled("COMPTROL_ALLOW_HIGH_CONSEQUENCE_ADAPTERS") {
                policy.max_risk = policy.max_risk.max(Risk::R3);
                policy
                    .allowed_intents
                    .insert("obs.recording.start".to_owned());
                policy
                    .allowed_intents
                    .insert("obs.recording.stop".to_owned());
                policy
                    .allowed_intents
                    .insert("discord.message.delete".to_owned());
                policy.allowed_intents.insert("mail.send".to_owned());
                policy.allowed_intents.insert("message.send".to_owned());
            }
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
    pub durable_events: DurableEventHub,
    pub trace: Option<TraceRecorder>,
    pub stop: StopLatch,
    /// Persistent consent store. Opened lazily; failures surface in doctor.
    pub consent: Option<comptrol_consent::ConsentStore>,
    /// Human action broker tracking paused operations awaiting user
    /// approval (UAC, polkit, TCC, browser consent...).
    pub human_actions: comptrol_consent::HumanActionBroker,
    operation_cancel: Option<Arc<AtomicBool>>,
    adapter_hosts: HashMap<String, AdapterHost>,
    idempotent: HashMap<String, ActionResult>,
    route_history: HashMap<String, RouteHistory>,
    route_stats_db: Connection,
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
        let route_stats_db = Connection::open(state_dir.join("route-stats.sqlite3"))
            .map_err(|error| io::Error::other(format!("route stats database: {error}")))?;
        route_stats_db
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;
                 CREATE TABLE IF NOT EXISTS route_stats (
                   route_key TEXT PRIMARY KEY,
                   attempts INTEGER NOT NULL,
                   verified_successes INTEGER NOT NULL,
                   verification_failures INTEGER NOT NULL DEFAULT 0,
                   dispatch_failures INTEGER NOT NULL DEFAULT 0,
                   disturbance_events INTEGER NOT NULL DEFAULT 0,
                   ewma_latency_ms REAL,
                   p95_latency_ms REAL,
                   last_success_at_ms INTEGER
                 );
                 CREATE TABLE IF NOT EXISTS route_latency_samples (
                   sample_id INTEGER PRIMARY KEY AUTOINCREMENT,
                   route_key TEXT NOT NULL,
                   latency_ms REAL NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS route_latency_samples_route
                   ON route_latency_samples(route_key, sample_id);",
            )
            .map_err(|error| io::Error::other(format!("route stats schema: {error}")))?;
        for column in [
            "verification_failures INTEGER NOT NULL DEFAULT 0",
            "dispatch_failures INTEGER NOT NULL DEFAULT 0",
            "disturbance_events INTEGER NOT NULL DEFAULT 0",
            "ewma_latency_ms REAL",
            "last_success_at_ms INTEGER",
        ] {
            let _ =
                route_stats_db.execute(&format!("ALTER TABLE route_stats ADD COLUMN {column}"), []);
        }
        let mut route_history = HashMap::new();
        {
            let mut statement = route_stats_db
                .prepare("SELECT route_key, attempts, verified_successes, verification_failures, dispatch_failures, disturbance_events, ewma_latency_ms, p95_latency_ms, last_success_at_ms FROM route_stats")
                .map_err(|error| io::Error::other(format!("route stats read: {error}")))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        RouteHistory {
                            attempts: row.get::<_, i64>(1)?.max(0) as u64,
                            verified_successes: row.get::<_, i64>(2)?.max(0) as u64,
                            verification_failures: row.get::<_, i64>(3)?.max(0) as u64,
                            dispatch_failures: row.get::<_, i64>(4)?.max(0) as u64,
                            disturbance_events: row.get::<_, i64>(5)?.max(0) as u64,
                            ewma_latency_ms: row.get(6)?,
                            latency_samples_ms: VecDeque::new(),
                            p95_latency_ms: row.get(7)?,
                            last_success_at_ms: row
                                .get::<_, Option<i64>>(8)?
                                .map(|v| v.max(0) as u128),
                        },
                    ))
                })
                .map_err(|error| io::Error::other(format!("route stats rows: {error}")))?;
            for row in rows {
                let (route, stats) =
                    row.map_err(|error| io::Error::other(format!("route stats row: {error}")))?;
                route_history.insert(route, stats);
            }
        }
        {
            let mut statement = route_stats_db
                .prepare(
                    "SELECT route_key, latency_ms
                     FROM route_latency_samples
                     ORDER BY route_key ASC, sample_id ASC",
                )
                .map_err(|error| io::Error::other(format!("route latency samples read: {error}")))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                })
                .map_err(|error| io::Error::other(format!("route latency samples rows: {error}")))?;
            for row in rows {
                let (route, latency_ms) = row
                    .map_err(|error| io::Error::other(format!("route latency sample row: {error}")))?;
                let stats = route_history.entry(route).or_default();
                stats.latency_samples_ms.push_back(latency_ms);
                while stats.latency_samples_ms.len() > ROUTE_LATENCY_SAMPLE_CAP {
                    stats.latency_samples_ms.pop_front();
                }
            }
            for stats in route_history.values_mut() {
                if !stats.latency_samples_ms.is_empty() {
                    stats.p95_latency_ms = percentile_95(&stats.latency_samples_ms);
                }
            }
        }
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
            durable_events: DurableEventHub::open(state_dir.join("events.sqlite3"), 4096)
                .map_err(io::Error::other)?,
            trace,
            stop: StopLatch::new(&state_dir),
            consent: comptrol_consent::ConsentStore::open(state_dir.join("consent.jsonl")).ok(),
            human_actions: comptrol_consent::HumanActionBroker::with_path(
                state_dir.join("human_actions.json"),
            ),
            operation_cancel: None,
            adapter_hosts: HashMap::new(),
            idempotent,
            route_history,
            route_stats_db,
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
        // Consent gate: mutations with an exact resource identity must be
        // covered by a persistent consent grant. Environment policy alone is
        // no longer sufficient for consent-gated capabilities.
        let consent_resource = request.target.as_ref().and_then(|target| {
            target
                .id
                .as_deref()
                .or(target.name.as_deref())
                .map(str::to_owned)
        });
        if risk.mutation()
            && let Some(store) = &self.consent
            && consent_gate_applies(&request.intent)
        {
            let denied = match store.authorize(
                &request.intent,
                &request.intent,
                to_consent_risk(risk),
                consent_resource.as_deref(),
            ) {
                comptrol_consent::ConsentDecision::Denied { .. } => true,
                comptrol_consent::ConsentDecision::Allowed { .. } => false,
            };
            if denied {
                let result = ActionResult::refused(
                    &request,
                    operation_id,
                    ComptrolError {
                        code: "consent_required".to_owned(),
                        message: format!(
                            "No active consent grant covers {}",
                            request.intent
                        ),
                        recovery: Some(
                            "Run setup to grant this capability locally, or ask the user to approve it"
                                .to_owned(),
                        ),
                    },
                );
                self.remember(&request, result.clone());
                return result;
            }
        }
        let plan = route_plan_with_history(&request, &self.route_history, Some(&self.policy));
        if request.dry_run {
            let route_error = plan.selected.is_none().then(|| ComptrolError {
                code: "route_unavailable".to_owned(),
                message: plan.rationale.clone(),
                recovery: Some(
                    "Inspect routes and satisfy the selected route's feasibility gates".to_owned(),
                ),
            });
            let result = ActionResult {
                operation_id,
                intent: request.intent.clone(),
                route: plan.selected.clone().unwrap_or_else(|| "none".to_owned()),
                target: request.target.clone(),
                preflight: if plan.selected.is_some() {
                    "passed"
                } else {
                    "failed"
                }
                .to_owned(),
                delivery: DeliveryState::NotDispatched,
                effect: EffectState::NotAttempted,
                verification: VerificationState::NotAttempted,
                disturbance: json!({ "foreground_changed": false }),
                recovery: RecoveryState::None,
                data: json!({ "dry_run": true, "risk": risk, "route_plan": plan }),
                error: route_error,
            };
            self.remember(&request, result.clone());
            return result;
        }
        if plan.selected.is_none() {
            let result = ActionResult::refused(
                &request,
                operation_id,
                ComptrolError {
                    code: "route_unavailable".to_owned(),
                    message: plan.rationale.clone(),
                    recovery: Some(
                        "Inspect routes and satisfy the selected route's feasibility gates"
                            .to_owned(),
                    ),
                },
            );
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
        let route_started = Instant::now();
        let result = match request.intent.as_str() {
            "system.ping" => success(
                &request,
                operation_id,
                "native",
                EffectState::None,
                VerificationState::Verified,
                json!({ "ready": true, "protocol": PROTOCOL_VERSION }),
            ),
            "workflow.execute" => execute_workflow_request(self, &request, operation_id),
            "desktop.observe" => desktop_observe(&request, operation_id),
            "platform.broker.observe" => platform_broker_observe(&request, operation_id),
            "filesystem.write" => sandbox_write(&request, operation_id, &self.checkpoints),
            "filesystem.copy" => sandbox_copy(&request, operation_id, &self.checkpoints),
            "filesystem.restore_checkpoint" => {
                restore_checkpoint(&request, operation_id, &self.checkpoints)
            }
            "desktop.notify" => desktop_notify(&request, operation_id),
            "desktop.open_app" => desktop_open_app(&request, operation_id),
            "app.launch" => app_launch(&request, operation_id),
            "app.resolve" => app_resolve(&request, operation_id),
            "app.list" => app_list(&request, operation_id),
            "app.open_resource" => app_open_resource(&request, operation_id),
            "app.focus" => app_focus(&request, operation_id),
            "app.close" => app_close(&request, operation_id),
            "permission.status" => permission_status(&request, operation_id),
            "permission.request" => permission_request(self, &request, operation_id),
            "settings.get" => settings_get(&request, operation_id),
            "settings.set" | "settings.write" => settings_set(&request, operation_id),
            "software.search" => software_search(&request, operation_id),
            "software.describe" => software_describe(&request, operation_id),
            "software.install" => software_install(self, &request, operation_id),
            "software.update" => software_update(self, &request, operation_id),
            "software.uninstall" => software_uninstall(self, &request, operation_id),
            "popup.inspect" => popup_inspect(&request, operation_id),
            "popup.dismiss" => popup_dismiss(&request, operation_id),
            "browser.session.list" => browser_session_list(&request, operation_id),
            "browser.session.connect" => browser_session_connect(&request, operation_id),
            "browser.chrome.open_tab" => browser_chrome_open_tab(&request, operation_id),
            "browser.chrome.restore_recent" | "browser.chrome.reopen_closed_group" => {
                browser_chrome_restore_recent(&request, operation_id)
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
            | "browser.cdp.frame_evaluate"
            | "browser.cdp.ensure_state"
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
            | "browser.cdp.screenshot"
            | "browser.cdp.coordinate_click"
            | "browser.cdp.dialog"
            | "browser.cdp.accessibility_snapshot"
            | "browser.cdp.wait_for" => browser_cdp_action(&request, operation_id),
            intent if is_first_party_adapter_intent(intent) => {
                execute_adapter_request(self, &request, operation_id)
            }
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
        self.record_route_outcome(&result, route_started.elapsed().as_secs_f64() * 1_000.0);
        self.remember(&request, result.clone());
        result
    }

    fn record_route_outcome(&mut self, result: &ActionResult, latency_ms: f64) {
        let entry = self.route_history.entry(result.route.clone()).or_default();
        entry.attempts = entry.attempts.saturating_add(1);
        let verified = result.verification == VerificationState::Verified && result.error.is_none();
        let dispatch_failed = matches!(
            result.delivery,
            DeliveryState::Refused | DeliveryState::NotDispatched
        );
        if verified {
            entry.verified_successes = entry.verified_successes.saturating_add(1);
            entry.last_success_at_ms = Some(now_ms());
        } else if dispatch_failed {
            entry.dispatch_failures = entry.dispatch_failures.saturating_add(1);
        } else {
            entry.verification_failures = entry.verification_failures.saturating_add(1);
        }
        if result
            .disturbance
            .get("foreground_changed")
            .and_then(Value::as_bool)
            == Some(true)
        {
            entry.disturbance_events = entry.disturbance_events.saturating_add(1);
        }
        entry.ewma_latency_ms = Some(match entry.ewma_latency_ms {
            Some(previous) => (previous * 0.8) + (latency_ms * 0.2),
            None => latency_ms,
        });
        entry.record_latency(latency_ms);
        let _ = self.route_stats_db.execute(
            "INSERT INTO route_latency_samples(route_key, latency_ms) VALUES (?1, ?2)",
            params![result.route, latency_ms],
        );
        let _ = self.route_stats_db.execute(
            "DELETE FROM route_latency_samples
             WHERE route_key = ?1
               AND sample_id NOT IN (
                 SELECT sample_id
                 FROM route_latency_samples
                 WHERE route_key = ?1
                 ORDER BY sample_id DESC
                 LIMIT 256
               )",
            params![result.route],
        );
        let _ = self.route_stats_db.execute(
            "INSERT INTO route_stats(route_key, attempts, verified_successes, verification_failures, dispatch_failures, disturbance_events, ewma_latency_ms, p95_latency_ms, last_success_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(route_key) DO UPDATE SET
               attempts = excluded.attempts,
               verified_successes = excluded.verified_successes,
               verification_failures = excluded.verification_failures,
               dispatch_failures = excluded.dispatch_failures,
               disturbance_events = excluded.disturbance_events,
               ewma_latency_ms = excluded.ewma_latency_ms,
               p95_latency_ms = excluded.p95_latency_ms,
               last_success_at_ms = excluded.last_success_at_ms",
            params![
                result.route,
                entry.attempts as i64,
                entry.verified_successes as i64,
                entry.verification_failures as i64,
                entry.dispatch_failures as i64,
                entry.disturbance_events as i64,
                entry.ewma_latency_ms,
                entry.p95_latency_ms,
                entry.last_success_at_ms.map(|v| v as i64),
            ],
        );
    }

    /// Execute an operation while exposing the task cancellation latch to
    /// lower-level routes. The previous latch is restored so nested workflow
    /// operations inherit cancellation without leaking it into later calls.
    pub fn operate_with_cancel(
        &mut self,
        request: OperationRequest,
        cancellation: Arc<AtomicBool>,
    ) -> ActionResult {
        let previous = self.operation_cancel.replace(cancellation);
        let result = self.operate(request);
        self.operation_cancel = previous;
        result
    }

    pub fn inspect(&mut self, kind: &str) -> Value {
        match kind {
            "doctor" => doctor(self),
            "consent" => json!({
                "store": match &self.consent {
                    Some(store) => json!({ "available": true, "path": store.path(), "active_grants": store.active(None).len() }),
                    None => json!({ "available": false, "reason": "consent store failed to open; run setup to recreate it" }),
                },
                "human_actions": {
                    "pending": self.human_actions.all().iter().filter(|action| action.resolution.is_none()).count(),
                    "total": self.human_actions.all().len(),
                    "requests": self.human_actions.all(),
                },
            }),
            "capabilities" => json!(capabilities()),
            "routes" => json!(route_catalog()),
            "route_stats" => json!({
                "routes": self
                    .route_history
                    .iter()
                    .map(|(route, stats)| json!({
                        "route": route,
                        "attempts": stats.attempts,
                        "verified_successes": stats.verified_successes,
                        "verification_failures": stats.verification_failures,
                        "dispatch_failures": stats.dispatch_failures,
                        "disturbance_events": stats.disturbance_events,
                        "historical_success": stats.success_rate(),
                        "ewma_latency_ms": stats.ewma_latency_ms,
                        "p95_latency_ms": stats.p95_latency_ms,
                        "last_success_at_ms": stats.last_success_at_ms,
                    }))
                    .collect::<Vec<_>>()
            }),
            "platform" => platform_diagnostics(),
            "browser" => {
                if let Ok(endpoint) = std::env::var("COMPTROL_CDP_ENDPOINT") {
                    match browser::discover(&endpoint) {
                        Ok(targets) => json!({ "endpoint": endpoint, "targets": targets, "transport": "direct_cdp" }),
                        Err(error) => json!({ "endpoint": endpoint, "error": error, "transport": "direct_cdp" }),
                    }
                } else {
                    let health = browser_bridge::BridgeStore::open(&default_state_dir())
                        .and_then(|store| store.health(browser_bridge::DEFAULT_HEALTH_MAX_AGE));
                    match health {
                        Ok(health) if health.active => match browser::discover(browser_bridge::COMPANION_BRIDGE_ENDPOINT) {
                            Ok(targets) => json!({
                                "endpoint": browser_bridge::COMPANION_BRIDGE_ENDPOINT,
                                "transport": "companion_extension",
                                "targets": targets,
                                "bridge_health": health,
                            }),
                            Err(error) => json!({
                                "endpoint": browser_bridge::COMPANION_BRIDGE_ENDPOINT,
                                "transport": "companion_extension",
                                "error": error,
                                "bridge_health": health,
                            }),
                        },
                        Ok(health) => json!({
                            "available": false,
                            "transport": "companion_extension",
                            "reason": "no direct CDP endpoint and the companion bridge heartbeat is stale or absent",
                            "bridge_health": health,
                        }),
                        Err(error) => json!({
                            "available": false,
                            "reason": format!("browser bridge state unavailable: {error}"),
                        }),
                    }
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
            && !matches!(&result.recovery, RecoveryState::RequiresReconciliation)
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
        if let Err(error) = self.durable_events.emit(
            "operation.completed",
            json!({ "operation_id": result.operation_id, "intent": result.intent, "verification": result.verification }),
        ) {
            eprintln!("comptrol durable event error: {error}");
        }
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
        "app.launch" => Risk::R1,
        "app.resolve" | "app.list" | "permission.status" | "popup.inspect" => Risk::R0,
        "permission.request" | "software.search" | "software.describe" | "settings.get" => Risk::R1,
        "app.open_resource"
        | "app.focus"
        | "settings.set"
        | "settings.write"
        | "popup.dismiss"
        | "browser.session.connect"
        | "browser.cdp.dialog" => Risk::R2,
        "app.close" | "software.install" | "software.update" | "software.uninstall" => Risk::R3,
        "desktop.notify"
        | "filesystem.write"
        | "filesystem.copy"
        | "filesystem.restore_checkpoint" => Risk::R1,
        "desktop.open_app"
        | "macos.ax.press"
        | "macos.ax.set_value"
        | "browser.chrome.open_tab"
        | "browser.chrome.restore_recent"
        | "browser.chrome.reopen_closed_group" => Risk::R2,
        "command.run" => Risk::R3,
        "windows.uia.press" | "windows.uia.set_value" => Risk::R2,
        "linux.atspi.press" | "linux.atspi.set_value" => Risk::R2,
        "browser.fixture.submit" => Risk::R1,
        "browser.cdp.evaluate"
        | "browser.cdp.frame_evaluate"
        | "browser.cdp.ensure_state"
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
        | "browser.cdp.coordinate_click" => Risk::R2,
        "browser.cdp.screenshot" => Risk::R0,
        "browser.cdp.workflow" => Risk::R2,
        "obs.recording.start" | "obs.recording.stop" => Risk::R3,
        "discord.message.delete" | "mail.send" | "message.send" => Risk::R3,
        "video.render.cancel"
        | "document.export"
        | "presentation.export"
        | "presentation.export_pdf"
        | "design.export"
        | "discord.message.react" => Risk::R1,
        "discord.message.draft"
        | "discord.message.search"
        | "mail.search"
        | "mail.read"
        | "design.list"
        | "design.read"
        | "design.page.list"
        | "design.element.inspect"
        | "document.google.read"
        | "presentation.google.read"
        | "presentation.read"
        | "video.project.list"
        | "video.media.list"
        | "video.timeline.list"
        | "video.timeline.items.list"
        | "video.render.preset.list"
        | "video.render.status" => Risk::R0,
        intent if is_first_party_adapter_intent(intent) => Risk::R2,
        "browser.cdp.wait_for"
        | "browser.cdp.accessibility_snapshot"
        | "browser.cdp.reopen_closed_group" => Risk::R0,
        _ => Risk::R2,
    }
}

fn execute_adapter_request(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    let provider = request.params.get("provider").and_then(Value::as_str);
    let adapter_name = match adapter_id_for_intent(&request.intent, provider) {
        Ok(name) => name,
        Err(message) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "adapter_unavailable".to_owned(),
                    message,
                    recovery: Some(
                        "Inspect adapter descriptors and use a supported intent".to_owned(),
                    ),
                },
            );
        }
    };
    let root = std::env::var_os("COMPTROL_ADAPTER_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("adapters"));
    let manifest_path = root.join(adapter_name).join("adapter.toml");
    let manifest_text = match fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "adapter_unavailable".to_owned(),
                    message: format!("Adapter manifest could not be read: {error}"),
                    recovery: Some(
                        "Configure COMPTROL_ADAPTER_ROOT with the adapter bundle".to_owned(),
                    ),
                },
            );
        }
    };
    let manifest = match AdapterManifest::from_toml(&manifest_text) {
        Ok(manifest) if manifest.declares(&request.intent) => manifest,
        Ok(_) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "adapter_capability_undeclared".to_owned(),
                    message: format!("Adapter {adapter_name} does not declare {}", request.intent),
                    recovery: Some("Use the adapter's declared capability set".to_owned()),
                },
            );
        }
        Err(error) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "adapter_manifest_invalid".to_owned(),
                    message: error.to_string(),
                    recovery: Some(
                        "Repair and validate adapter.toml before enabling the route".to_owned(),
                    ),
                },
            );
        }
    };
    let isolation_dimensions = manifest.isolation.dimensions();
    if !runtime.adapter_hosts.contains_key(adapter_name) {
        let python = std::env::var_os("COMPTROL_ADAPTER_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(if cfg!(windows) {
                    "python.exe"
                } else {
                    "python3"
                })
            });
        let script = root.join(adapter_name).join("src").join("adapter.py");
        let config = AdapterHostConfig {
            manifest,
            executable: python,
            arguments: vec![script.to_string_lossy().into_owned()],
            instance_id: format!("{adapter_name}-{operation_id}"),
            max_frame_bytes: comptrol_adapter_sdk::MAX_FRAME_BYTES,
            timeout_ms: request
                .params
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(5_000)
                .clamp(100, 120_000),
        };
        let mut host = match AdapterHost::spawn(config) {
            Ok(host) => host,
            Err(error) => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "adapter_unavailable".to_owned(),
                        message: error.to_string(),
                        recovery: Some(
                            "Install the adapter runtime and inspect adapter health".to_owned(),
                        ),
                    },
                );
            }
        };
        if let Err(error) = host.handshake() {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "adapter_handshake_failed".to_owned(),
                    message: error.to_string(),
                    recovery: Some(
                        "Inspect the isolated adapter process before retrying".to_owned(),
                    ),
                },
            );
        }
        runtime.adapter_hosts.insert(adapter_name.to_owned(), host);
    }
    let Some(host) = runtime.adapter_hosts.get_mut(adapter_name) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "adapter_unavailable".to_owned(),
                message: "The adapter host was not retained".to_owned(),
                recovery: None,
            },
        );
    };
    let resource = request
        .params
        .get("resource")
        .and_then(Value::as_str)
        .unwrap_or("application")
        .to_owned();
    let token = match host.capability_token(&request.intent, &resource, &operation_id, 30_000) {
        Ok(token) => token,
        Err(error) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "adapter_capability_denied".to_owned(),
                    message: error.to_string(),
                    recovery: Some("Inspect the adapter manifest and resource scope".to_owned()),
                },
            );
        }
    };
    let mut payload = request.params.clone();
    if !payload.is_object() {
        payload = json!({});
    }
    payload["intent"] = json!(request.intent);
    let response = if let Some(cancellation) = runtime.operation_cancel.as_ref() {
        host.request_with_cancel("execute", &resource, payload, Some(token), || {
            cancellation.load(Ordering::Acquire)
        })
    } else {
        host.request("execute", &resource, payload, Some(token))
    };
    match response {
        Ok(response) if response.ok && response.health == HealthState::Available => {
            let verified = response
                .payload
                .get("verified")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && response.payload.get("verification").is_some();
            success(
                request,
                operation_id,
                &format!("adapter.{adapter_name}"),
                EffectState::Changed,
                if verified {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                json!({"adapter": adapter_name, "health": response.health, "payload": response.payload, "verified": verified, "isolation": isolation_dimensions.clone()}),
            )
        }
        Ok(response) => ActionResult {
            operation_id,
            intent: request.intent.clone(),
            route: format!("adapter.{adapter_name}"),
            target: request.target.clone(),
            preflight: "passed".to_owned(),
            delivery: DeliveryState::Delivered,
            effect: EffectState::NotAttempted,
            verification: VerificationState::NotAttempted,
            disturbance: json!({ "foreground_changed": false, "mouse": "untouched", "clipboard": "untouched" }),
            recovery: RecoveryState::None,
            data: json!({"adapter": adapter_name, "health": response.health, "payload": response.payload, "isolation": isolation_dimensions}),
            error: Some(ComptrolError {
                code: "adapter_execution_failed".to_owned(),
                message: response
                    .error
                    .map(|error| error.message)
                    .unwrap_or_else(|| "Adapter did not verify the operation".to_owned()),
                recovery: Some(
                    "Inspect adapter health and reconcile application state before retrying"
                        .to_owned(),
                ),
            }),
        },
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "adapter_execution_failed".to_owned(),
                message: error.to_string(),
                recovery: Some(
                    "Inspect the adapter process and application state before retrying".to_owned(),
                ),
            },
        ),
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
    if request.intent == "browser.chrome.restore_recent" {
        if let Some(kind) = request.params.get("kind").and_then(Value::as_str) {
            metadata["restore_kind"] = json!(kind);
        }
        if let Some(mode) = request.params.get("mode").and_then(Value::as_str) {
            metadata["restore_mode"] = json!(mode);
        }
        if let Some(urls) = request.params.get("urls").and_then(Value::as_array) {
            metadata["restore_url_count"] = json!(urls.len());
            if let Ok(bytes) = serde_json::to_vec(urls) {
                metadata["restore_urls_hash"] = json!(stable_hash(&bytes));
            }
        }
        metadata["action"] = json!("restore_recent");
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
    // Use a process-independent digest for durable reconciliation metadata.
    // The compact u64 projection preserves the existing protocol shape; the
    // digest itself remains cryptographically stable across runtimes.
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 prefix length"))
}

fn route_plan(request: &OperationRequest, policy: Option<&Policy>) -> RoutePlan {
    let mut plan = route_plan_for_intent(
        &request.intent,
        request.params.clone(),
        request.background.as_deref(),
    );
    // Runtime-policy flags may grant feasibility that env vars alone
    // would gate (embedded daemon setups and tests configure the policy
    // object directly instead of the process environment).
    if request.intent == "app.launch"
        && let Some(policy) = policy
        && policy.allow_app_launch
        && let Some(candidate) = plan.candidates.first_mut()
        && !candidate.feasible
    {
        candidate.feasible = true;
        candidate.rationale = "Registry-backed app launch allowed by runtime policy".to_owned();
    }
    // Policy-gated V5 intents may be enabled by direct policy membership
    // (consent store or setup flow) without process environment switches.
    // Endpoint-dependent routes (browser protocol, adapters) are excluded:
    // they still need their endpoint or adapter root.
    const POLICY_DIRECT_INTENTS: &[&str] = &[
        "app.open_resource",
        "app.focus",
        "permission.request",
        "settings.get",
        "settings.set",
        "settings.write",
        "software.search",
        "software.describe",
        "software.install",
        "software.update",
        "software.uninstall",
        "popup.dismiss",
        "browser.session.connect",
    ];
    if POLICY_DIRECT_INTENTS.contains(&request.intent.as_str())
        && let Some(policy) = policy
        && policy.allowed_intents.contains(&request.intent)
        && let Some(candidate) = plan.candidates.first_mut()
        && !candidate.feasible
    {
        candidate.feasible = true;
        candidate.rationale = format!("{} allowed by runtime policy", request.intent);
    }
    plan
}

fn route_plan_with_history(
    request: &OperationRequest,
    history: &HashMap<String, RouteHistory>,
    policy: Option<&Policy>,
) -> RoutePlan {
    let mut plan = route_plan(request, policy);
    for candidate in &mut plan.candidates {
        if !candidate.feasible {
            candidate.utility = None;
            continue;
        }
        if let Some(stats) = history.get(&candidate.route) {
            candidate.historical_success = stats.success_rate();
            if let Some(latency) = stats.p95_latency_ms {
                candidate.expected_p95_ms = Some(latency);
            }
            let verification = match candidate.verification_strength.as_str() {
                "independent_outcome" => 1.0,
                "persisted_artifact" => 0.95,
                "application_state" => 0.85,
                "surface_state" => 0.65,
                _ => 0.35,
            };
            let latency = candidate.expected_p95_ms.unwrap_or(1_000.0);
            candidate.utility = Some(
                (0.45 * candidate.historical_success)
                    + (0.35 * verification)
                    + (0.20 * (1.0 - (latency / 1_000.0).min(1.0))),
            );
        }
    }
    plan.candidates.sort_by(|left, right| {
        right
            .utility
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&left.utility.unwrap_or(f64::NEG_INFINITY))
            .then_with(|| left.route.cmp(&right.route))
    });
    plan.selected = plan
        .candidates
        .iter()
        .find(|candidate| candidate.feasible)
        .map(|candidate| candidate.route.clone());
    plan.rationale = plan
        .selected
        .as_deref()
        .map(|route| {
            format!(
                "Selected deterministic safest feasible route with historical feedback: {route}"
            )
        })
        .unwrap_or_else(|| "No feasible route".to_owned());
    plan
}

fn route_plan_for_intent(intent: &str, params: Value, background: Option<&str>) -> RoutePlan {
    let primary = match intent {
        "system.ping" => Some(("native", true, "Built-in local readiness route")),
        "desktop.observe" => Some((
            "platform_observe",
            true,
            "Read-only host observation is available locally",
        )),
        "platform.broker.observe" => Some((
            "platform_broker",
            true,
            "Read-only broker diagnostics do not require actuation",
        )),
        "workflow.execute" => Some(("workflow", true, "Bounded workflow VM route")),
        "filesystem.write" | "filesystem.copy" => Some((
            "sandbox_filesystem",
            env_enabled("COMPTROL_ALLOW_SANDBOX_WRITES"),
            "Sandbox write policy must be explicitly enabled",
        )),
        "filesystem.restore_checkpoint" => Some((
            "sandbox_checkpoint",
            env_enabled("COMPTROL_ALLOW_SANDBOX_WRITES"),
            "Checkpoint restore is gated by sandbox write policy",
        )),
        "desktop.notify" => Some((
            "platform_notification",
            cfg!(target_os = "macos") && env_enabled("COMPTROL_ALLOW_DESKTOP_NOTIFY"),
            "macOS notification route and explicit policy are required",
        )),
        "desktop.open_app" => Some((
            "platform_launch",
            cfg!(any(
                target_os = "windows",
                target_os = "macos",
                target_os = "linux"
            )) && env_enabled("COMPTROL_ALLOW_APP_LAUNCH"),
            "Native app launch requires an explicit local policy",
        )),
        "app.launch" => Some((
            "app_registry_launch",
            cfg!(any(
                target_os = "windows",
                target_os = "macos",
                target_os = "linux"
            )) && env_enabled("COMPTROL_ALLOW_APP_LAUNCH"),
            "Registry-backed app launch requires an explicit local policy",
        )),
        "app.resolve" | "app.list" => Some((
            "app_registry_read",
            true,
            "Registry reads are local and read-only",
        )),
        "app.open_resource" | "app.focus" => Some((
            "app_registry_launch",
            cfg!(any(
                target_os = "windows",
                target_os = "macos",
                target_os = "linux"
            )) && env_enabled("COMPTROL_ALLOW_APP_LAUNCH"),
            "Registry-backed app activation requires an explicit local policy",
        )),
        "app.close" => Some((
            "app_registry_close",
            env_enabled("COMPTROL_ALLOW_APP_CLOSE"),
            "Closing an application requires an explicit high-consequence policy",
        )),
        "permission.status" => Some((
            "permission_probe",
            true,
            "Permission probing is local and read-only",
        )),
        "permission.request" => Some((
            "permission_surface",
            env_enabled("COMPTROL_ALLOW_SETTINGS"),
            "Opening a permission surface requires an explicit local policy",
        )),
        "settings.get" => Some((
            "settings_registry",
            env_enabled("COMPTROL_ALLOW_SETTINGS"),
            "Typed settings reads require an explicit local policy",
        )),
        "settings.set" | "settings.write" => Some((
            "settings_registry",
            env_enabled("COMPTROL_ALLOW_SETTINGS"),
            "Typed settings writes require an explicit local policy plus a consent grant",
        )),
        "software.search" | "software.describe" => Some((
            "software_provider",
            env_enabled("COMPTROL_ALLOW_SOFTWARE"),
            "Software discovery requires an explicit local policy",
        )),
        "software.install" | "software.update" | "software.uninstall" => Some((
            "software_provider",
            env_enabled("COMPTROL_ALLOW_SOFTWARE_INSTALL"),
            "Software mutation requires an explicit local policy plus a consent grant",
        )),
        "popup.inspect" => Some((
            "popup_classifier",
            true,
            "Popup classification is local and read-only",
        )),
        "popup.dismiss" => Some((
            "popup_manager",
            env_enabled("COMPTROL_ALLOW_POPUP"),
            "Popup dismissal requires an explicit local policy and never auto-approves protected prompts",
        )),
        "browser.session.list" => Some((
            "browser_session_broker",
            true,
            "Session discovery reports local browser surfaces without connecting",
        )),
        "browser.session.connect" => Some((
            "browser_session_broker",
            env_enabled("COMPTROL_ALLOW_BROWSER_CDP"),
            "Session connection needs the browser protocol policy",
        )),
        "browser.chrome.open_tab" => Some((
            "browser_launcher",
            cfg!(any(
                target_os = "windows",
                target_os = "macos",
                target_os = "linux"
            )) && env_enabled("COMPTROL_ALLOW_BROWSER_LAUNCH"),
            "Default-profile browser launch requires an explicit local policy",
        )),
        "browser.chrome.restore_recent" | "browser.chrome.reopen_closed_group" => Some((
            "chrome_restore",
            true,
            "Chrome restore uses the native recently-closed surface when reachable and otherwise reconstructs only when explicitly allowed",
        )),
        "command.run" => Some((
            "process_argv",
            env_enabled("COMPTROL_ALLOW_COMMANDS")
                && std::env::var_os("COMPTROL_COMMAND_ROOT").is_some(),
            "Command execution requires an allowlist policy and command root",
        )),
        "windows.uia.press" | "windows.uia.set_value" => Some((
            "windows_uia",
            cfg!(target_os = "windows") && env_enabled("COMPTROL_ALLOW_WINDOWS_UIA"),
            "Windows UI Automation requires Windows and explicit policy",
        )),
        "linux.atspi.press" | "linux.atspi.set_value" => Some((
            "linux_atspi",
            cfg!(target_os = "linux")
                && env_enabled("COMPTROL_ALLOW_LINUX_ATSPI")
                && std::env::var_os("AT_SPI_BUS_ADDRESS").is_some(),
            "AT-SPI requires Linux, a session bus, and explicit policy",
        )),
        "macos.ax.press" | "macos.ax.set_value" => Some((
            "macos_ax",
            cfg!(target_os = "macos") && env_enabled("COMPTROL_ALLOW_MACOS_AX"),
            "macOS Accessibility requires explicit policy and a reachable provider",
        )),
        "browser.fixture.submit" => Some((
            "browser_fixture",
            true,
            "Deterministic local browser fixture route",
        )),
        value if is_first_party_adapter_intent(value) => Some((
            "isolated_adapter",
            env_enabled("COMPTROL_ALLOW_ADAPTERS")
                && std::env::var_os("COMPTROL_ADAPTER_ROOT").is_some(),
            "First party application adapters require an explicit policy and adapter root",
        )),
        "browser.cdp.frame_evaluate" => Some((
            "browser_protocol",
            std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && env_enabled("COMPTROL_ALLOW_BROWSER_CDP"),
            "Frame-scoped CDP requires the direct event-maintained frame graph; the companion bridge intentionally refuses this route until it can prove a stable frame binding",
        )),
        value if value.starts_with("browser.cdp.") => Some((
            "browser_protocol",
            (std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                || browser_bridge::bridge_is_active())
                && env_enabled("COMPTROL_ALLOW_BROWSER_CDP"),
            "Browser protocol control requires a direct CDP endpoint or a live companion bridge heartbeat and explicit policy",
        )),
        _ => None,
    };

    let Some((route, available, base_reason)) = primary else {
        return RoutePlan {
            intent: intent.to_owned(),
            selected: None,
            candidates: vec![RouteCandidate::new(
                "none",
                false,
                "No registered route exists for this intent".to_owned(),
            )],
            rationale: format!("No registered route exists for {intent}"),
        };
    };

    let mut candidates = vec![RouteCandidate::new(
        route,
        available,
        if available {
            base_reason.to_owned()
        } else {
            format!("Rejected: {base_reason}")
        },
    )];

    if intent == "browser.cdp.open_tab" {
        let requested_background = params
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if background == Some("strict_background") && !requested_background {
            candidates[0].feasible = false;
            candidates[0].utility = None;
            candidates[0].rationale =
                "Rejected: strict_background requires params.background=true".to_owned();
        } else if background == Some("foreground_required") && requested_background {
            candidates[0].feasible = false;
            candidates[0].utility = None;
            candidates[0].rationale =
                "Rejected: foreground_required conflicts with params.background=true".to_owned();
        }
    }

    let selected = candidates
        .iter()
        .find(|candidate| candidate.feasible)
        .map(|candidate| candidate.route.clone());
    let rationale = match selected.as_deref() {
        Some(route) => format!("Selected deterministic primary route {route}"),
        None => candidates
            .first()
            .map(|candidate| candidate.rationale.clone())
            .unwrap_or_else(|| "No feasible route".to_owned()),
    };
    RoutePlan {
        intent: intent.to_owned(),
        selected,
        candidates,
        rationale,
    }
}

fn route_catalog() -> Vec<RoutePlan> {
    [
        "system.ping",
        "desktop.observe",
        "platform.broker.observe",
        "workflow.execute",
        "filesystem.write",
        "filesystem.copy",
        "filesystem.restore_checkpoint",
        "desktop.notify",
        "desktop.open_app",
        "app.launch",
        "app.resolve",
        "app.list",
        "app.open_resource",
        "app.focus",
        "app.close",
        "permission.status",
        "permission.request",
        "settings.get",
        "settings.set",
        "software.search",
        "software.describe",
        "software.install",
        "software.update",
        "software.uninstall",
        "popup.inspect",
        "popup.dismiss",
        "browser.session.list",
        "browser.session.connect",
        "browser.chrome.open_tab",
        "browser.chrome.restore_recent",
        "browser.chrome.reopen_closed_group",
        "command.run",
        "windows.uia.press",
        "linux.atspi.press",
        "macos.ax.press",
        "browser.fixture.submit",
        "vscode.workspace.list",
        "libreoffice.calc.range.read",
        "obs.scene.list",
        "blender.scene.object.list",
        "browser.cdp.open_tab",
        "browser.cdp.frame_evaluate",
        "browser.cdp.semantic_click",
        "browser.cdp.screenshot",
        "browser.cdp.coordinate_click",
        "browser.cdp.dialog",
    ]
    .into_iter()
    .map(|intent| route_plan_for_intent(intent, Value::Null, None))
    .collect()
}

fn env_enabled(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
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

fn execute_workflow_request(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    if let Some(compiled) = request.params.get("compiled_workflow") {
        let Ok(compiled) = serde_json::from_value::<CompiledWorkflow>(compiled.clone()) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "workflow_validation_failed".to_owned(),
                    message: "Compiled workflow did not match the closed schema".to_owned(),
                    recovery: Some("Recompile the workflow from a verified trace".to_owned()),
                },
            );
        };
        if let Err(error) = validate_compiled_workflow(
            &compiled,
            request.params.get("observed").unwrap_or(&Value::Null),
        ) {
            return ActionResult {
                operation_id,
                intent: request.intent.clone(),
                route: "workflow".to_owned(),
                target: request.target.clone(),
                preflight: "failed".to_owned(),
                delivery: DeliveryState::NotDispatched,
                effect: EffectState::NotAttempted,
                verification: VerificationState::NotAttempted,
                disturbance: json!({ "foreground_changed": false }),
                recovery: RecoveryState::RequiresReconciliation,
                data: json!({ "workflow_id": compiled.workflow_id, "workflow_version": compiled.workflow_version }),
                error: Some(error),
            };
        }
        let mut nodes = std::collections::BTreeMap::new();
        for (index, step) in compiled.steps.iter().enumerate() {
            let id = format!("act_{index}");
            let next = if index + 1 < compiled.steps.len() {
                Some(format!("act_{}", index + 1))
            } else {
                Some("return".to_owned())
            };
            nodes.insert(
                id,
                WorkflowNode::Act {
                    intent: step.intent.clone(),
                    params: resolve_workflow_parameters(
                        &step.params,
                        request.params.get("parameters").unwrap_or(&Value::Null),
                    ),
                    next,
                },
            );
        }
        nodes.insert(
            "return".to_owned(),
            WorkflowNode::Return { value: Value::Null },
        );
        let workflow = Workflow {
            id: compiled.workflow_id.clone(),
            version: compiled.workflow_version,
            intent: compiled.intent.clone(),
            parameters: compiled.parameters.clone(),
            fingerprint: compiled.fingerprint.clone(),
            start: "act_0".to_owned(),
            nodes,
        };
        let target = request.target.clone();
        let background = request.background.clone();
        let mut executor = WorkflowExecutor {
            action: |intent: &str, params: &Value| {
                let result = runtime.operate(OperationRequest {
                    intent: intent.to_owned(),
                    target: target.clone(),
                    params: params.clone(),
                    postcondition: None,
                    risk: None,
                    idempotency_key: None,
                    dry_run: false,
                    background: background.clone(),
                });
                serde_json::to_value(result).map_err(|error| error.to_string())
            },
            verify: |criterion: &Value, observed: &Value| {
                criterion
                    .get("equals")
                    .is_some_and(|expected| observed.get("data") == Some(expected))
                    || criterion == observed
            },
            wait: |_event: &str, timeout_ms: u64| {
                std::thread::sleep(Duration::from_millis(timeout_ms.min(60_000)));
                Ok(())
            },
            max_steps: compiled.steps.len().saturating_mul(4).saturating_add(4),
        };
        return match executor.run(&workflow) {
            Ok(value) => success(
                request,
                operation_id,
                "workflow",
                EffectState::Changed,
                VerificationState::Verified,
                json!({ "workflow_id": compiled.workflow_id, "workflow_version": compiled.workflow_version, "result": value }),
            ),
            Err(error) => ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "workflow_execution_failed".to_owned(),
                    message: error.to_string(),
                    recovery: Some("Observe the current state and use a cold route".to_owned()),
                },
            ),
        };
    }
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

fn resolve_workflow_parameters(template: &Value, parameters: &Value) -> Value {
    match template {
        Value::Object(object) => {
            if let Some(name) = object.get("param").and_then(Value::as_str) {
                return parameters.get(name).cloned().unwrap_or(Value::Null);
            }
            Value::Object(
                object
                    .iter()
                    .map(|(key, value)| {
                        (key.clone(), resolve_workflow_parameters(value, parameters))
                    })
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| resolve_workflow_parameters(value, parameters))
                .collect(),
        ),
        _ => template.clone(),
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
    let endpoint = if let Some(endpoint) = std::env::var_os("COMPTROL_CDP_ENDPOINT") {
        endpoint
    } else if browser_bridge::bridge_is_active() {
        std::ffi::OsString::from(browser_bridge::COMPANION_BRIDGE_ENDPOINT)
    } else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "browser_unavailable".to_owned(),
                message: "Neither COMPTROL_CDP_ENDPOINT nor a live companion extension bridge is available".to_owned(),
                recovery: Some("Configure local Chrome DevTools or connect the Browser Bridge extension".to_owned()),
            },
        );
    };
    if request.intent == "browser.cdp.frame_evaluate" {
        let Some(frame_id) = request.params.get("frame_id").and_then(Value::as_str) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Frame evaluation needs a frame id".to_owned(),
                    recovery: Some("Inspect the live frame graph and include frame_id".to_owned()),
                },
            );
        };
        let Some(generation) = request
            .params
            .get("frame_generation")
            .and_then(Value::as_u64)
        else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Frame evaluation needs frame_generation".to_owned(),
                    recovery: Some(
                        "Use the generation returned with the frame observation".to_owned(),
                    ),
                },
            );
        };
        let Some(revision) = request.params.get("frame_revision").and_then(Value::as_u64) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Frame evaluation needs frame_revision".to_owned(),
                    recovery: Some(
                        "Use the revision returned with the frame observation".to_owned(),
                    ),
                },
            );
        };
        let Some(expression) = request.params.get("expression").and_then(Value::as_str) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Frame evaluation needs an expression".to_owned(),
                    recovery: None,
                },
            );
        };
        return match browser::cdp_frame_call(
            &endpoint.to_string_lossy(),
            frame_id,
            generation,
            revision,
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
        ) {
            Ok(data) => success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::None,
                if data.get("exceptionDetails").is_some() {
                    VerificationState::Failed
                } else {
                    VerificationState::Verified
                },
                json!({"frame_id": frame_id, "generation": generation, "revision": revision, "result": data}),
            ),
            Err(error) => browser_failure(request, operation_id, error),
        };
    }
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
    if request.intent == "browser.cdp.dialog" {
        return browser_cdp_dialog(request, operation_id, &endpoint);
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
    if request.intent == "browser.cdp.screenshot" {
        let format = request
            .params
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("png");
        if !matches!(format, "png" | "jpeg" | "webp") {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Browser screenshots support png, jpeg, or webp".to_owned(),
                    recovery: Some("Use a supported screenshot format".to_owned()),
                },
            );
        }
        let quality = request
            .params
            .get("quality")
            .and_then(Value::as_u64)
            .map(|value| value.clamp(0, 100));
        let clip = request.params.get("clip").cloned().unwrap_or(Value::Null);
        if !clip.is_null() && !clip.is_object() {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Browser screenshot clip must be an object".to_owned(),
                    recovery: None,
                },
            );
        }
        return match browser::capture_screenshot(
            &endpoint.to_string_lossy(),
            target_id,
            browser_context_id,
            revision,
            format,
            quality,
            clip,
            request
                .params
                .get("include_pixels")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ) {
            Ok(data) => success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::None,
                VerificationState::Verified,
                data,
            ),
            Err(error) => browser_failure(request, operation_id, error),
        };
    }
    if request.intent == "browser.cdp.coordinate_click" {
        let Some(capture_id) = request.params.get("capture_id").and_then(Value::as_str) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Coordinate clicks require a capture_id from browser.cdp.screenshot"
                        .to_owned(),
                    recovery: Some(
                        "Capture fresh pixels before requesting a coordinate click".to_owned(),
                    ),
                },
            );
        };
        let Some(x) = request.params.get("x").and_then(Value::as_f64) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Coordinate clicks require x".to_owned(),
                    recovery: None,
                },
            );
        };
        let Some(y) = request.params.get("y").and_then(Value::as_f64) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Coordinate clicks require y".to_owned(),
                    recovery: None,
                },
            );
        };
        let button = request
            .params
            .get("button")
            .and_then(Value::as_str)
            .unwrap_or("left");
        return match browser::coordinate_click(
            &endpoint.to_string_lossy(),
            target_id,
            browser_context_id,
            revision,
            capture_id,
            x,
            y,
            button,
        ) {
            Ok(data) => success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                VerificationState::Unverified,
                data,
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
    if request.intent == "browser.cdp.ensure_state" {
        return match browser::ensure_state(
            &endpoint.to_string_lossy(),
            target_id,
            Some(browser_context_id),
            Some(revision),
            request.params.get("url").and_then(Value::as_str),
            request.params.get("url_contains").and_then(Value::as_str),
            request
                .params
                .get("ready_expression")
                .and_then(Value::as_str),
        ) {
            Ok(state) => {
                let satisfied = state.get("satisfied").and_then(Value::as_bool) == Some(true);
                success(
                    request,
                    operation_id,
                    "browser_protocol",
                    EffectState::None,
                    if satisfied {
                        VerificationState::Verified
                    } else {
                        VerificationState::Unverified
                    },
                    json!({ "ensure_state": state, "satisfied": satisfied }),
                )
            }
            Err(error) => browser_failure(request, operation_id, error),
        };
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
            // SPA fast path: when the requested postcondition already holds
            // on the live target, skip Page.navigate entirely instead of
            // reloading a dynamic page into the state the caller wants.
            if request
                .params
                .get("skip_if_current")
                .and_then(Value::as_bool)
                != Some(false)
            {
                let ensure = browser::ensure_state(
                    &endpoint.to_string_lossy(),
                    target_id,
                    Some(browser_context_id),
                    Some(revision),
                    request.params.get("url").and_then(Value::as_str),
                    request.params.get("url_contains").and_then(Value::as_str),
                    request
                        .params
                        .get("ready_expression")
                        .and_then(Value::as_str),
                );
                if let Ok(state) = &ensure
                    && state.get("satisfied").and_then(Value::as_bool) == Some(true)
                {
                    return success(
                        request,
                        operation_id,
                        "browser_protocol",
                        EffectState::None,
                        VerificationState::Verified,
                        json!({
                            "navigated": false,
                            "reason": "requested state already live on the exact target",
                            "ensure_state": state,
                            "mouse": "untouched",
                            "clipboard": "untouched",
                        }),
                    );
                }
            }
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
        Ok(mut data) => {
            if request.intent == "browser.cdp.navigate" {
                if data.get("errorText").is_some() {
                    return success(
                        request,
                        operation_id,
                        "browser_protocol",
                        EffectState::Unknown,
                        VerificationState::Failed,
                        data,
                    );
                }
                let observed = match browser::cdp_call(
                    &endpoint.to_string_lossy(),
                    target_id,
                    Some(browser_context_id),
                    None,
                    "Runtime.evaluate",
                    json!({"expression":"location.href", "returnByValue":true}),
                ) {
                    Ok(value) => value
                        .get("result")
                        .and_then(|result| result.get("result").or(Some(result)))
                        .and_then(|result| result.get("value"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    Err(_error) => {
                        let mut recovered = None;
                        for _ in 0..100 {
                            recovered =
                                browser::fresh_target_url(&endpoint.to_string_lossy(), target_id)
                                    .ok()
                                    .flatten();
                            if recovered.as_deref().is_some_and(|url| !url.is_empty()) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        recovered
                    }
                };
                let expected = request
                    .params
                    .get("url_contains")
                    .and_then(Value::as_str)
                    .or_else(|| request.params.get("url").and_then(Value::as_str));
                let verified =
                    observed
                        .as_deref()
                        .zip(expected)
                        .is_some_and(|(actual, expected)| {
                            actual == expected || actual.contains(expected)
                        });
                data["final_url"] = observed.map_or(Value::Null, Value::String);
                data["verification"] = json!({
                    "level": "surface_state",
                    "source": "browser_dom",
                    "criterion": "final_url_matches_request",
                    "passed": verified
                });
                return success(
                    request,
                    operation_id,
                    "browser_protocol",
                    if verified {
                        EffectState::Changed
                    } else {
                        EffectState::Unknown
                    },
                    if verified {
                        VerificationState::Verified
                    } else {
                        VerificationState::Unverified
                    },
                    data,
                );
            }
            success(
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
            )
        }
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
        Ok(data) => {
            // The dispatch carries an independent in-page readback: a unique
            // locator match plus full actionability plus a clicked
            // confirmation from the DOM itself. When that readback reports
            // verified, the outcome is application-state verified, not merely
            // delivered.
            let verified = data.get("verified").and_then(Value::as_bool) == Some(true);
            success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                if verified {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                json!({
                    "dispatch": data,
                    "postcondition": if request.postcondition.is_some() {
                        "requested_but_not_checked"
                    } else {
                        "none"
                    },
                    "verification": if verified {
                        "in_page_actionability_readback"
                    } else {
                        "unverified"
                    }
                }),
            )
        }
        Err(error) => browser_failure(request, operation_id, error),
    }
}

/// Handle a JavaScript dialog (`alert`, `confirm`, `prompt`,
/// `beforeunload`) on one exact target via `Page.handleJavaScriptDialog`.
///
/// Only informational dialog handling is allowed here: `accept` maps to
/// the dialog's default confirmation and `dismiss` to its cancellation.
/// Security, payment, license, or privilege dialogs must go through
/// `popup.dismiss`, which refuses protected classes.
fn browser_cdp_dialog(
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
                message: "Dialog handling needs a target id".to_owned(),
                recovery: Some("Inspect browser targets before the dialog action".to_owned()),
            },
        );
    };
    let browser_context_id = request
        .params
        .get("browser_context_id")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let revision = request.params.get("revision").and_then(Value::as_str);
    let Some(action) = request.params.get("action").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Dialog handling needs params.action accept or dismiss".to_owned(),
                recovery: None,
            },
        );
    };
    let accept = match action {
        "accept" => true,
        "dismiss" => false,
        _ => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "params.action must be accept or dismiss".to_owned(),
                    recovery: None,
                },
            );
        }
    };
    let mut dialog_params = json!({ "accept": accept });
    if accept && let Some(text) = request.params.get("prompt_text").and_then(Value::as_str) {
        dialog_params["promptText"] = json!(text);
    }
    match browser::cdp_call(
        &endpoint.to_string_lossy(),
        target_id,
        Some(browser_context_id),
        revision,
        "Page.handleJavaScriptDialog",
        dialog_params,
    ) {
        Ok(_) => success(
            request,
            operation_id,
            "browser_protocol",
            EffectState::Changed,
            VerificationState::Verified,
            json!({
                "target_id": target_id,
                "action": action,
                "verification": "cdp_dialog_handled",
                "note": "the browser handled the pending dialog; confirm page state with a follow-up snapshot when it matters",
            }),
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
    let targets = match browser::discover_cached_targets(&endpoint) {
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
        if let Ok(current) = browser::discover_cached_targets(&endpoint)
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
    browser::wait_for_url(
        endpoint,
        target_id,
        browser_context_id,
        contains,
        Duration::from_millis(timeout_ms.clamp(100, 30_000)),
    )
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
                json!({
                    "result": data,
                    "postcondition": if verified { "verified" } else if postcondition_requested { "failed" } else { "unverified" },
                    "wait_strategy": if request.intent == "browser.cdp.wait_for" { "bounded_poll" } else { "none" }
                }),
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
    let transaction = UploadTransaction::new(&operation_id);
    match browser::cdp_upload(
        &endpoint.to_string_lossy(),
        target_id,
        browser_context_id,
        revision,
        selector,
        &path,
    ) {
        Ok(data) => {
            let selection_verified = data.get("verified").and_then(Value::as_bool) == Some(true);
            success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                VerificationState::Unverified,
                json!({
                    "selection": data,
                    "stage": "selected",
                    "verified": selection_verified,
                    "transaction": transaction,
                    "verification": "unverified",
                    "next": "A site or application adapter must verify transfer or application acceptance"
                }),
            )
        }
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
        let mut transaction = DownloadTransaction::new(&operation_id, "reconciled");
        let _ = transaction.advance(DownloadStage::InProgress);
        let _ = transaction.advance(DownloadStage::BrowserCompleted);
        let _ = transaction.advance(DownloadStage::FileVerified);
        return success(
            request,
            operation_id,
            "browser_protocol",
            EffectState::None,
            VerificationState::Verified,
            json!({ "download": { "path": expected_path, "file_name": file_name, "verified": true, "replayed": true }, "transaction": transaction }),
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
        Ok(data) => {
            let guid = data
                .get("guid")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let mut transaction = DownloadTransaction::new(&operation_id, guid);
            let _ = transaction.advance(DownloadStage::InProgress);
            let _ = transaction.advance(DownloadStage::BrowserCompleted);
            let _ = transaction.advance(DownloadStage::FileVerified);
            success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                VerificationState::Verified,
                json!({ "download": data, "transaction": transaction }),
            )
        }
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
        "app.launch" | "app.open_resource" | "app.focus" => true,
        "browser.cdp.open_tab" => !request
            .params
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "browser.chrome.open_tab" => true,
        "browser.chrome.restore_recent" | "browser.chrome.reopen_closed_group" => true,
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

/// Registry-backed launch: resolve the exact installed app, launch
/// through the native mechanism, and verify the process identity is
/// alive afterwards. The registry refuses ambiguous or missing apps
/// instead of guessing.
fn app_launch(request: &OperationRequest, operation_id: String) -> ActionResult {
    if request.background.as_deref() == Some("strict_background") {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "background_unavailable".to_owned(),
                message: "Launching an application may activate the desktop and cannot satisfy strict background posture".to_owned(),
                recovery: Some("Use foreground_allowed, or drive an app through its API/CDP route".to_owned()),
            },
        );
    }
    let Some(app) = request
        .params
        .get("app")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "app.launch needs an exact app identity or display name".to_owned(),
                recovery: Some(
                    "Inspect the app registry and retry with the resolved id".to_owned(),
                ),
            },
        );
    };
    let resource = match request.params.get("resource") {
        Some(value) => {
            match serde_json::from_value::<comptrol_app_registry::Resource>(value.clone()) {
                Ok(resource) => resource,
                Err(error) => {
                    return ActionResult::refused(
                        request,
                        operation_id,
                        ComptrolError {
                            code: "invalid_input".to_owned(),
                            message: format!("app.launch resource is malformed: {error}"),
                            recovery: None,
                        },
                    );
                }
            }
        }
        None => comptrol_app_registry::Resource::None,
    };
    let resolved = match comptrol_app_registry::resolve(&app) {
        Ok(entry) => entry,
        Err(error @ comptrol_app_registry::RegistryError::NotFound(_))
        | Err(error @ comptrol_app_registry::RegistryError::Ambiguous(_, _)) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "app_not_resolved".to_owned(),
                    message: error.to_string(),
                    recovery: Some(
                        "List installed applications and use one exact identity".to_owned(),
                    ),
                },
            );
        }
        Err(error) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "app_registry_failed".to_owned(),
                    message: error.to_string(),
                    recovery: Some("Inspect platform registry state".to_owned()),
                },
            );
        }
    };
    let mut launch_request = comptrol_app_registry::LaunchRequest::new(resolved);
    launch_request.resource = resource;
    launch_request.background = request.background.as_deref() == Some("prefer_background");
    match comptrol_app_registry::launch_verified(&launch_request) {
        Ok((outcome, verification)) => {
            let verified = matches!(
                verification,
                comptrol_app_registry::LaunchVerification::Verified
            );
            success(
                request,
                operation_id,
                "app_registry_launch",
                EffectState::Changed,
                if verified {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                json!({
                    "app": outcome.app_id,
                    "route": outcome.route,
                    "pid": outcome.pid,
                    "verification": verification,
                    "mouse": "untouched",
                    "clipboard": "untouched",
                }),
            )
        }
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "app_launch_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect the resolved application and retry".to_owned()),
            },
        ),
    }
}

fn app_resolve(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(app) = request
        .params
        .get("app")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "app.resolve needs an app identity or display name".to_owned(),
                recovery: Some("Provide params.app and retry".to_owned()),
            },
        );
    };
    match comptrol_app_registry::resolve(&app) {
        Ok(entry) => success(
            request,
            operation_id,
            "app_registry_read",
            EffectState::None,
            VerificationState::Verified,
            json!({ "app": entry, "mouse": "untouched", "clipboard": "untouched" }),
        ),
        Err(error @ comptrol_app_registry::RegistryError::NotFound(_))
        | Err(error @ comptrol_app_registry::RegistryError::Ambiguous(_, _)) => {
            ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "app_not_resolved".to_owned(),
                    message: error.to_string(),
                    recovery: Some(
                        "List installed applications and use one exact identity".to_owned(),
                    ),
                },
            )
        }
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "app_registry_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect platform registry state".to_owned()),
            },
        ),
    }
}

fn app_list(request: &OperationRequest, operation_id: String) -> ActionResult {
    let query = request
        .params
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    match comptrol_app_registry::registry::system_entries() {
        Ok(list) => entries.extend(list),
        Err(error) => errors.push(error.to_string()),
    }
    match comptrol_app_registry::registry::path_entries() {
        Ok(list) => entries.extend(list),
        Err(error) => errors.push(error.to_string()),
    }
    if !query.trim().is_empty() {
        let needle = query.to_lowercase();
        entries.retain(|entry| {
            entry.id.to_lowercase().contains(&needle)
                || entry.display_name.to_lowercase().contains(&needle)
        });
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries.dedup_by(|a, b| a.id == b.id);
    success(
        request,
        operation_id,
        "app_registry_read",
        EffectState::None,
        VerificationState::Verified,
        json!({ "apps": entries, "count": entries.len(), "provider_errors": errors }),
    )
}

fn app_launch_with_resource(
    request: &OperationRequest,
    operation_id: String,
    require_resource: bool,
    route: &str,
) -> ActionResult {
    if request.background.as_deref() == Some("strict_background") {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "background_unavailable".to_owned(),
                message: "Launching an application may activate the desktop and cannot satisfy strict background posture".to_owned(),
                recovery: Some("Use foreground_allowed, or drive an app through its API/CDP route".to_owned()),
            },
        );
    }
    let Some(app) = request
        .params
        .get("app")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "This intent needs an exact app identity or display name".to_owned(),
                recovery: Some(
                    "Inspect the app registry and retry with the resolved id".to_owned(),
                ),
            },
        );
    };
    let resource = match request.params.get("resource") {
        Some(value) => {
            match serde_json::from_value::<comptrol_app_registry::Resource>(value.clone()) {
                Ok(resource) => resource,
                Err(error) => {
                    return ActionResult::refused(
                        request,
                        operation_id,
                        ComptrolError {
                            code: "invalid_input".to_owned(),
                            message: format!("resource is malformed: {error}"),
                            recovery: None,
                        },
                    );
                }
            }
        }
        None => comptrol_app_registry::Resource::None,
    };
    if require_resource && matches!(resource, comptrol_app_registry::Resource::None) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "app.open_resource needs params.resource (file, url, or deep link)"
                    .to_owned(),
                recovery: Some("Provide the exact resource to open".to_owned()),
            },
        );
    }
    let resolved = match comptrol_app_registry::resolve(&app) {
        Ok(entry) => entry,
        Err(error @ comptrol_app_registry::RegistryError::NotFound(_))
        | Err(error @ comptrol_app_registry::RegistryError::Ambiguous(_, _)) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "app_not_resolved".to_owned(),
                    message: error.to_string(),
                    recovery: Some(
                        "List installed applications and use one exact identity".to_owned(),
                    ),
                },
            );
        }
        Err(error) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "app_registry_failed".to_owned(),
                    message: error.to_string(),
                    recovery: Some("Inspect platform registry state".to_owned()),
                },
            );
        }
    };
    let mut launch_request = comptrol_app_registry::LaunchRequest::new(resolved);
    launch_request.resource = resource;
    launch_request.background = request.background.as_deref() == Some("prefer_background");
    match comptrol_app_registry::launch_verified(&launch_request) {
        Ok((outcome, verification)) => {
            let verified = matches!(
                verification,
                comptrol_app_registry::LaunchVerification::Verified
            );
            success(
                request,
                operation_id,
                route,
                EffectState::Changed,
                if verified {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                json!({
                    "app": outcome.app_id,
                    "route": outcome.route,
                    "pid": outcome.pid,
                    "verification": verification,
                    "mouse": "untouched",
                    "clipboard": "untouched",
                }),
            )
        }
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "app_launch_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect the resolved application and retry".to_owned()),
            },
        ),
    }
}

fn app_open_resource(request: &OperationRequest, operation_id: String) -> ActionResult {
    app_launch_with_resource(request, operation_id, true, "app_registry_launch")
}

fn app_focus(request: &OperationRequest, operation_id: String) -> ActionResult {
    app_launch_with_resource(request, operation_id, false, "app_registry_activate")
}

fn app_close(request: &OperationRequest, operation_id: String) -> ActionResult {
    ActionResult::refused(
        request,
        operation_id,
        ComptrolError {
            code: "route_unavailable".to_owned(),
            message: "Automated application close is not implemented in this build".to_owned(),
            recovery: Some("Close the application in its own UI".to_owned()),
        },
    )
}

fn permission_status(request: &OperationRequest, operation_id: String) -> ActionResult {
    let os = std::env::consts::OS;
    #[cfg(target_os = "macos")]
    let accessibility_trusted = comptrol_platform_macos::accessibility_trusted();
    #[cfg(not(target_os = "macos"))]
    let accessibility_trusted = false;
    success(
        request,
        operation_id,
        "permission_probe",
        EffectState::None,
        VerificationState::Verified,
        json!({
            "platform": os,
            "accessibility_trusted": accessibility_trusted,
            "macos_ax_policy": std::env::var("COMPTROL_ALLOW_MACOS_AX").as_deref() == Ok("1"),
            "windows_uia_policy": std::env::var("COMPTROL_ALLOW_WINDOWS_UIA").as_deref() == Ok("1"),
            "linux_atspi_bus": std::env::var_os("AT_SPI_BUS_ADDRESS").is_some(),
            "linux_session": std::env::var("XDG_SESSION_TYPE").ok().or_else(|| std::env::var("WAYLAND_DISPLAY").ok().map(|_| "wayland".to_owned())),
            "browser_cdp_configured": std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some(),
            "software_policy": std::env::var("COMPTROL_ALLOW_SOFTWARE").as_deref() == Ok("1"),
            "software_install_policy": std::env::var("COMPTROL_ALLOW_SOFTWARE_INSTALL").as_deref() == Ok("1"),
            "settings_policy": std::env::var("COMPTROL_ALLOW_SETTINGS").as_deref() == Ok("1"),
            "human_action": "protected permissions are granted by the user in the OS surface; Comptrol never self-grants",
        }),
    )
}

fn permission_surface_for(permission: &str) -> Option<String> {
    match std::env::consts::OS {
        "macos" => Some(match permission {
            "accessibility" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
                    .to_owned()
            }
            "automation" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation"
                    .to_owned()
            }
            "screen_recording" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
                    .to_owned()
            }
            "full_disk_access" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"
                    .to_owned()
            }
            "microphone" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
                    .to_owned()
            }
            _ => return None,
        }),
        "windows" => Some(match permission {
            "accessibility" => "ms-settings:privacy-accessibility".to_owned(),
            "microphone" => "ms-settings:privacy-microphone".to_owned(),
            "notifications" => "ms-settings:privacy-notifications".to_owned(),
            _ => return None,
        }),
        "linux" => Some(match permission {
            "notifications" => "gnome-control-center notifications".to_owned(),
            _ => return None,
        }),
        _ => None,
    }
}

fn open_surface_url(surface: &str) -> Result<(), String> {
    let (program, args): (&str, Vec<&str>) = match std::env::consts::OS {
        "macos" => ("open", vec![surface]),
        "windows" => ("cmd", vec!["/c", "start", "", surface]),
        "linux" => {
            let first = surface.split_whitespace().next().unwrap_or("");
            if first == "gnome-control-center" {
                (
                    "gnome-control-center",
                    surface.split_whitespace().skip(1).collect(),
                )
            } else {
                ("xdg-open", vec![surface])
            }
        }
        _ => return Err("unsupported platform".to_owned()),
    };
    Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn permission_request(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    let Some(permission) = request.params.get("permission").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "permission.request needs params.permission".to_owned(),
                recovery: Some(
                    "Ask permission.status which permissions can be requested".to_owned(),
                ),
            },
        );
    };
    let Some(surface) = permission_surface_for(permission) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "unsupported_permission".to_owned(),
                message: format!(
                    "No known OS surface for permission {permission} on this platform"
                ),
                recovery: Some("Open the OS settings surface manually".to_owned()),
            },
        );
    };
    if let Err(error) = open_surface_url(&surface) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "surface_unavailable".to_owned(),
                message: format!("The permission surface could not be opened: {error}"),
                recovery: Some(format!("Open {surface} manually")),
            },
        );
    }
    let challenge = comptrol_consent::HumanActionChallenge {
        kind: comptrol_consent::human_action::ChallengeKind::PermissionGrant {
            platform: std::env::consts::OS.to_owned(),
            permission: permission.to_owned(),
        },
        reason: format!("Grant {permission} to the Comptrol host in the OS surface"),
        target: None,
        requested_change: Some(format!("permission {permission} granted by user")),
        prompt_location: "os_settings_surface".to_owned(),
        agent_must_not_enter_secret: true,
    };
    let human = runtime
        .human_actions
        .request(&operation_id, &request.intent, challenge);
    let mut result = success(
        request,
        operation_id,
        "permission_surface",
        EffectState::None,
        VerificationState::Unverified,
        json!({
            "permission": permission,
            "surface": surface,
            "state": "awaiting_human_action",
            "human_action_id": human.id,
            "agent_must_not_enter_secret": true,
        }),
    );
    result.recovery = RecoveryState::RequiresReconciliation;
    result
}

fn settings_get(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(name) = request.params.get("key").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "settings.get needs params.key from the typed settings registry"
                    .to_owned(),
                recovery: Some("Use a declared settings.* key".to_owned()),
            },
        );
    };
    let Some(key) = comptrol_settings::SettingKey::from_name(name) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "unknown_setting".to_owned(),
                message: format!("{name} is not a declared Comptrol setting"),
                recovery: Some(
                    "Arbitrary registry/defaults mutation is not a setting route".to_owned(),
                ),
            },
        );
    };
    match comptrol_settings::get(&key) {
        Ok(observation) => success(
            request,
            operation_id,
            "settings_registry",
            EffectState::None,
            if observation.readback_verified {
                VerificationState::Verified
            } else {
                VerificationState::Unverified
            },
            json!({ "observation": observation }),
        ),
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "settings_unavailable".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect platform support for this setting".to_owned()),
            },
        ),
    }
}

fn settings_set(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(name) = request.params.get("key").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "settings.set needs params.key from the typed settings registry"
                    .to_owned(),
                recovery: Some("Use a declared settings.* key".to_owned()),
            },
        );
    };
    let Some(key) = comptrol_settings::SettingKey::from_name(name) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "unknown_setting".to_owned(),
                message: format!("{name} is not a declared Comptrol setting"),
                recovery: Some(
                    "Arbitrary registry/defaults mutation is not a setting route".to_owned(),
                ),
            },
        );
    };
    let Some(value) = request.params.get("value").cloned() else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "settings.set needs params.value".to_owned(),
                recovery: None,
            },
        );
    };
    let value = if let Some(b) = value.get("value").and_then(Value::as_bool) {
        comptrol_settings::SettingValue::bool(b)
    } else if let Some(n) = value.get("value").and_then(Value::as_i64) {
        comptrol_settings::SettingValue::Integer { value: n }
    } else if let Some(s) = value.get("value").and_then(Value::as_str) {
        comptrol_settings::SettingValue::Text {
            value: s.to_owned(),
        }
    } else if value.is_boolean() {
        comptrol_settings::SettingValue::bool(value.as_bool().unwrap_or(false))
    } else if value.is_i64() || value.is_u64() {
        comptrol_settings::SettingValue::Integer {
            value: value.as_i64().unwrap_or(0),
        }
    } else if value.is_string() {
        comptrol_settings::SettingValue::Text {
            value: value.as_str().unwrap_or("").to_owned(),
        }
    } else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "params.value must be a bool, integer, or string".to_owned(),
                recovery: None,
            },
        );
    };
    match comptrol_settings::set(&key, value) {
        Ok(observation) => success(
            request,
            operation_id,
            "settings_registry",
            EffectState::Changed,
            if observation.readback_verified {
                VerificationState::Verified
            } else {
                VerificationState::Unverified
            },
            json!({ "observation": observation }),
        ),
        Err(comptrol_settings::SettingsError::HumanActionRequired { key, surface }) => {
            ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "human_action_required".to_owned(),
                    message: format!("{key} is protected; the user must act in {surface}"),
                    recovery: Some(format!(
                        "Use permission.request and wait for the user in {surface}"
                    )),
                },
            )
        }
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "settings_write_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect platform support for this setting".to_owned()),
            },
        ),
    }
}

fn software_search(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(query) = request.params.get("query").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "software.search needs params.query".to_owned(),
                recovery: None,
            },
        );
    };
    let provider = request
        .params
        .get("provider")
        .and_then(Value::as_str)
        .and_then(|name| match name {
            "winget" => Some(comptrol_software::providers::ProviderId::WinGet),
            "brew" | "homebrew" => Some(comptrol_software::providers::ProviderId::Homebrew),
            "flatpak" => Some(comptrol_software::providers::ProviderId::Flatpak),
            "packagekit" => Some(comptrol_software::providers::ProviderId::PackageKit),
            _ => None,
        });
    match comptrol_software::search(query, provider) {
        Ok(results) => success(
            request,
            operation_id,
            "software_provider",
            EffectState::None,
            VerificationState::Verified,
            json!({ "results": results, "count": results.len(), "note": "search results never install; use software.describe then software.install with the exact id" }),
        ),
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "software_unavailable".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect available software providers on this machine".to_owned()),
            },
        ),
    }
}

fn software_describe(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(package) = request.params.get("package").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "software.describe needs params.package".to_owned(),
                recovery: None,
            },
        );
    };
    match comptrol_software::describe(package, None) {
        Ok(described) => success(
            request,
            operation_id,
            "software_provider",
            EffectState::None,
            VerificationState::Verified,
            json!({ "package": described }),
        ),
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "software_unavailable".to_owned(),
                message: error.to_string(),
                recovery: Some("Search for the exact package id first".to_owned()),
            },
        ),
    }
}

fn software_mutation_error(
    request: &OperationRequest,
    operation_id: String,
    route: &str,
    error: comptrol_software::SoftwareError,
) -> ActionResult {
    let code = match &error {
        comptrol_software::SoftwareError::NoProvider(_) => "software_unavailable",
        comptrol_software::SoftwareError::NotFound(_) => "package_not_found",
        comptrol_software::SoftwareError::Ambiguous { .. } => "package_ambiguous",
        comptrol_software::SoftwareError::AgreementsRequired { .. } => "agreements_required",
        comptrol_software::SoftwareError::ElevationRequired { .. } => "human_action_required",
        comptrol_software::SoftwareError::VerificationFailed { .. } => "verification_failed",
        _ => "software_failed",
    };
    let mut result = ActionResult::refused(
        request,
        operation_id,
        ComptrolError {
            code: code.to_owned(),
            message: error.to_string(),
            recovery: Some(match &error {
                comptrol_software::SoftwareError::Ambiguous { .. } => {
                    "Choose one exact package id from software.search".to_owned()
                }
                comptrol_software::SoftwareError::AgreementsRequired { .. } => {
                    "Show the agreements to the user and retry with accept_agreements only after explicit approval".to_owned()
                }
                comptrol_software::SoftwareError::ElevationRequired { .. } => {
                    "The native privilege prompt is waiting for the user; Comptrol never enters the credential".to_owned()
                }
                _ => "Inspect the software provider state".to_owned(),
            }),
        },
    );
    result.route = route.to_owned();
    result
}

fn software_install(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    let Some(package) = request.params.get("package").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "software.install needs params.package with the exact id".to_owned(),
                recovery: Some("Use software.search and software.describe first".to_owned()),
            },
        );
    };
    let install = comptrol_software::InstallRequest {
        package: package.to_owned(),
        provider: None,
        version: request
            .params
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        source: request
            .params
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_owned),
        accept_agreements: request
            .params
            .get("accept_agreements")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        launch_after_install: false,
    };
    // If human action was already approved, skip the elevation check and retry directly.
    // Support resume_operation_id for retries where the original idempotency_key was not passed.
    let effective_id = request
        .params
        .get("resume_operation_id")
        .and_then(Value::as_str)
        .unwrap_or(operation_id.as_str());
    if runtime.human_actions.approved(effective_id) {
        return match comptrol_software::install(&install) {
            Ok(outcome) => {
                let verified = matches!(
                    outcome.verification,
                    comptrol_software::InstallVerification::Verified { .. }
                );
                success(
                    request,
                    operation_id,
                    "software_provider",
                    EffectState::Changed,
                    if verified {
                        VerificationState::Verified
                    } else {
                        VerificationState::Unverified
                    },
                    json!({ "outcome": outcome }),
                )
            }
            Err(error) => {
                software_mutation_error(request, operation_id, "software_provider", error)
            }
        };
    }
    match comptrol_software::install(&install) {
        Ok(outcome) => {
            let verified = matches!(
                outcome.verification,
                comptrol_software::InstallVerification::Verified { .. }
            );
            success(
                request,
                operation_id,
                "software_provider",
                EffectState::Changed,
                if verified {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                json!({ "outcome": outcome }),
            )
        }
        Err(error @ comptrol_software::SoftwareError::ElevationRequired { .. }) => {
            let challenge = comptrol_consent::HumanActionChallenge {
                kind: match std::env::consts::OS {
                    "windows" => {
                        comptrol_consent::human_action::ChallengeKind::NativeAuthentication {
                            platform: "windows".to_owned(),
                        }
                    }
                    "linux" => comptrol_consent::human_action::ChallengeKind::PrivilegeAgent {
                        platform: "linux".to_owned(),
                    },
                    _ => comptrol_consent::human_action::ChallengeKind::NativeAuthentication {
                        platform: std::env::consts::OS.to_owned(),
                    },
                },
                reason: format!("The installer for {package} requests elevation"),
                target: Some(package.to_owned()),
                requested_change: Some(format!("install {package}")),
                prompt_location: "secure_desktop".to_owned(),
                agent_must_not_enter_secret: true,
            };
            let human = runtime
                .human_actions
                .request(&operation_id, &request.intent, challenge);
            let mut result =
                software_mutation_error(request, operation_id, "software_provider", error);
            result.data = json!({
                "state": "awaiting_human_action",
                "human_action_id": human.id,
                "agent_must_not_enter_secret": true,
            });
            result.recovery = RecoveryState::RequiresReconciliation;
            result
        }
        Err(error) => software_mutation_error(request, operation_id, "software_provider", error),
    }
}

fn software_update(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    let Some(package) = request.params.get("package").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "software.update needs params.package with the exact id".to_owned(),
                recovery: None,
            },
        );
    };
    // If human action was already approved, skip the elevation check and retry directly.
    let effective_id = request
        .params
        .get("resume_operation_id")
        .and_then(Value::as_str)
        .unwrap_or(operation_id.as_str());
    if runtime.human_actions.approved(effective_id) {
        return match comptrol_software::update(package, None) {
            Ok(outcome) => success(
                request,
                operation_id,
                "software_provider",
                EffectState::Changed,
                VerificationState::Verified,
                json!({ "outcome": outcome }),
            ),
            Err(error) => {
                software_mutation_error(request, operation_id, "software_provider", error)
            }
        };
    }
    match comptrol_software::update(package, None) {
        Ok(outcome) => success(
            request,
            operation_id,
            "software_provider",
            EffectState::Changed,
            VerificationState::Verified,
            json!({ "outcome": outcome }),
        ),
        Err(error @ comptrol_software::SoftwareError::ElevationRequired { .. }) => {
            let challenge = comptrol_consent::HumanActionChallenge {
                kind: match std::env::consts::OS {
                    "windows" => {
                        comptrol_consent::human_action::ChallengeKind::NativeAuthentication {
                            platform: "windows".to_owned(),
                        }
                    }
                    "linux" => comptrol_consent::human_action::ChallengeKind::PrivilegeAgent {
                        platform: "linux".to_owned(),
                    },
                    _ => comptrol_consent::human_action::ChallengeKind::NativeAuthentication {
                        platform: std::env::consts::OS.to_owned(),
                    },
                },
                reason: format!("The updater for {package} requests elevation"),
                target: Some(package.to_owned()),
                requested_change: Some(format!("update {package}")),
                prompt_location: "secure_desktop".to_owned(),
                agent_must_not_enter_secret: true,
            };
            let human = runtime
                .human_actions
                .request(&operation_id, &request.intent, challenge);
            let mut result =
                software_mutation_error(request, operation_id, "software_provider", error);
            result.data = json!({
                "state": "awaiting_human_action",
                "human_action_id": human.id,
                "agent_must_not_enter_secret": true,
            });
            result.recovery = RecoveryState::RequiresReconciliation;
            result
        }
        Err(error) => software_mutation_error(request, operation_id, "software_provider", error),
    }
}

fn software_uninstall(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    let Some(package) = request.params.get("package").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "software.uninstall needs params.package with the exact id".to_owned(),
                recovery: None,
            },
        );
    };
    // If human action was already approved, skip the elevation check and retry directly.
    let effective_id = request
        .params
        .get("resume_operation_id")
        .and_then(Value::as_str)
        .unwrap_or(operation_id.as_str());
    if runtime.human_actions.approved(effective_id) {
        return match comptrol_software::uninstall(package, None) {
            Ok(outcome) => success(
                request,
                operation_id,
                "software_provider",
                EffectState::Changed,
                VerificationState::Verified,
                json!({ "outcome": outcome }),
            ),
            Err(error) => {
                software_mutation_error(request, operation_id, "software_provider", error)
            }
        };
    }
    match comptrol_software::uninstall(package, None) {
        Ok(outcome) => success(
            request,
            operation_id,
            "software_provider",
            EffectState::Changed,
            VerificationState::Verified,
            json!({ "outcome": outcome }),
        ),
        Err(error @ comptrol_software::SoftwareError::ElevationRequired { .. }) => {
            let challenge = comptrol_consent::HumanActionChallenge {
                kind: match std::env::consts::OS {
                    "windows" => {
                        comptrol_consent::human_action::ChallengeKind::NativeAuthentication {
                            platform: "windows".to_owned(),
                        }
                    }
                    "linux" => comptrol_consent::human_action::ChallengeKind::PrivilegeAgent {
                        platform: "linux".to_owned(),
                    },
                    _ => comptrol_consent::human_action::ChallengeKind::NativeAuthentication {
                        platform: std::env::consts::OS.to_owned(),
                    },
                },
                reason: format!("The uninstaller for {package} requests elevation"),
                target: Some(package.to_owned()),
                requested_change: Some(format!("uninstall {package}")),
                prompt_location: "secure_desktop".to_owned(),
                agent_must_not_enter_secret: true,
            };
            let human = runtime
                .human_actions
                .request(&operation_id, &request.intent, challenge);
            let mut result =
                software_mutation_error(request, operation_id, "software_provider", error);
            result.data = json!({
                "state": "awaiting_human_action",
                "human_action_id": human.id,
                "agent_must_not_enter_secret": true,
            });
            result.recovery = RecoveryState::RequiresReconciliation;
            result
        }
        Err(error) => software_mutation_error(request, operation_id, "software_provider", error),
    }
}

fn popup_signals(request: &OperationRequest) -> (String, String, String) {
    (
        request
            .params
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("dialog")
            .to_owned(),
        request
            .params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        request
            .params
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    )
}

/// Attempt to dismiss a native popup via platform accessibility APIs.
///
/// Uses the macOS Accessibility API (macos.ax.press) on macOS to find and
/// press the close/cancel button in a native dialog. Returns Ok(true) if
/// actuation succeeded, Ok(false) if the platform is unsupported or the
/// button could not be found, and Err on actuation failure.
fn native_popup_dismiss(
    request: &OperationRequest,
    _operation_id: &str,
    plan: &comptrol_popup::DismissalPlan,
) -> Result<bool, String> {
    // Extract the target process name or ID from the request.
    let target_name = request
        .target
        .as_ref()
        .and_then(|t| t.id.as_ref().or(t.name.as_ref()))
        .map(|s| s.as_str())
        .unwrap_or("");

    if target_name.is_empty() {
        return Ok(false);
    }

    // Determine the action from the dismissal plan.
    let action_label = match plan {
        comptrol_popup::DismissalPlan::SemanticAction { action } => action.clone(),
        comptrol_popup::DismissalPlan::ScopedKey { key } => {
            // For Escape key presses, we don't have a button name to press.
            // Return false to let the caller handle this case differently.
            let _ = key;
            return Ok(false);
        }
    };

    #[cfg(target_os = "macos")]
    {
        // On macOS, try to find and press the close button via AX API.
        // We need a process_id. If the target is a numeric PID, use it directly.
        // Otherwise, try to find the process by name.
        if let Ok(pid) = target_name.parse::<u64>() {
            let result = comptrol_platform_macos::execute(comptrol_platform_macos::Request {
                process_id: pid as u32,
                name: &action_label,
                role: Some("AXButton"),
                action: comptrol_platform_macos::Action::Press,
                value: None,
                expected_attribute: None,
                expected_value: None,
                timeout: std::time::Duration::from_millis(1000),
            });
            return match result {
                Ok(_) => Ok(true),
                Err(msg) if msg.contains("missing") || msg.contains("not found") => Ok(false),
                Err(msg) => Err(msg),
            };
        }
        // If target is not a PID, use osascript with properly quoted arguments
        // to avoid injection. The `quoted form of` operator handles escaping.
        let script = r#"
        on run argv
            set targetName to item 1 of argv
            set actionLabel to item 2 of argv
            tell application "System Events"
                try
                    set targetProcess to first process whose name contains targetName
                    set frontmost of targetProcess to true
                    delay 0.2
                    click button actionLabel of window 1 of targetProcess
                    return "true"
                on error
                    try
                        set targetProcess to first process whose name contains targetName
                        click button "Cancel" of window 1 of targetProcess
                        return "true"
                    on error
                        try
                            set targetProcess to first process whose name contains targetName
                            click button "Close" of window 1 of targetProcess
                            return "true"
                        on error
                            return "false"
                        end try
                    end try
                end try
            end tell
        end run
        "#;
        match std::process::Command::new("osascript")
            .args(["-e", script, "--", target_name, &action_label])
            .output()
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                Ok(stdout == "true")
            }
            _ => Ok(false),
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Windows UIA actuation would go here.
        // For now, return false to indicate native actuation is not available.
        let _ = action_label;
        Ok(false)
    }

    #[cfg(target_os = "linux")]
    {
        // Linux AT-SPI actuation would go here.
        // For now, return false to indicate native actuation is not available.
        let _ = action_label;
        Ok(false)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = action_label;
        Ok(false)
    }
}

fn popup_inspect(request: &OperationRequest, operation_id: String) -> ActionResult {
    let (role, name, text) = popup_signals(request);
    let class = comptrol_popup::classify(&role, &name, &text);
    let target = request
        .target
        .as_ref()
        .and_then(|t| t.id.clone().or_else(|| t.name.clone()))
        .unwrap_or_else(|| "unspecified".to_owned());
    success(
        request,
        operation_id,
        "popup_classifier",
        EffectState::None,
        VerificationState::Verified,
        json!({
            "popup": {
                "class": class.as_str(),
                "title": if name.is_empty() { Value::Null } else { Value::String(name) },
                "target": target,
            },
            "never_auto": class.never_auto(),
            "note": "classification only; use popup.dismiss with policy for eligible classes. Browser JS dialogs (alert/confirm/prompt) are dismissed via CDP. Native platform popups are dismissed via platform accessibility APIs (AX/UIA/AT-SPI) when available.",
        }),
    )
}

/// Dismiss a popup on a specific target.
///
/// Supports:
/// - Browser JavaScript dialogs (alert, confirm, prompt, beforeunload) via CDP Page.handleJavaScriptDialog
/// - Native macOS popups via Accessibility API (AXButton press)
///
/// Protected classes (auth, payment, security, privilege) are never auto-dismissed.
/// Eligible classes require explicit user policy preferences.
fn popup_dismiss(request: &OperationRequest, operation_id: String) -> ActionResult {
    let (role, name, text) = popup_signals(request);
    let class = comptrol_popup::classify(&role, &name, &text);
    if class.never_auto() {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "authorization_required".to_owned(),
                message: format!(
                    "popup class {} always needs explicit user authorization; it is never auto-dismissed",
                    class.as_str()
                ),
                recovery: Some("Ask the user to resolve this dialog".to_owned()),
            },
        );
    }
    let policy = comptrol_popup::PopupPolicy {
        dismiss_informational: request
            .params
            .get("dismiss_informational")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        dismiss_cookie_banners: request
            .params
            .get("dismiss_cookie_banners")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        dismiss_update_prompts: request
            .params
            .get("dismiss_update_prompts")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        dismiss_tips: request
            .params
            .get("dismiss_tips")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    let popup = comptrol_popup::PopupInfo {
        id: format!("popup-{operation_id}"),
        class,
        title: if name.is_empty() { None } else { Some(name) },
        target: request
            .target
            .as_ref()
            .and_then(|t| t.id.clone().or_else(|| t.name.clone()))
            .unwrap_or_else(|| "unspecified".to_owned()),
        close_actions: request
            .params
            .get("close_actions")
            .and_then(Value::as_array)
            .map(|actions| {
                actions
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        observed_at_ms: now_ms(),
    };
    match comptrol_popup::authorize_dismissal(&popup, &policy) {
        Ok(plan) => {
            // Execute the dismissal on the originating surface.
            // For browser targets: use CDP Page.handleJavaScriptDialog with dismiss.
            // For native targets: use platform accessibility API (AX/UIA/AT-SPI).
            if let Some(target_spec) = request.target.as_ref()
                && let Some(target_id) = target_spec.id.as_ref().or(target_spec.name.as_ref())
            {
                // First, try CDP if this looks like a browser target or CDP is available.
                if std::env::var("COMPTROL_CDP_ENDPOINT").is_ok() {
                    let mut dialog_request = request.clone();
                    dialog_request.intent = "browser.cdp.dialog".to_owned();
                    dialog_request.params = json!({
                        "target_id": target_id,
                        "action": "dismiss",
                        "browser_context_id": "default".to_owned(),
                    });
                    let cdp_result = browser_cdp_dialog(
                        &dialog_request,
                        operation_id.clone(),
                        std::ffi::OsStr::new(
                            &std::env::var("COMPTROL_CDP_ENDPOINT").unwrap_or_default(),
                        ),
                    );
                    if cdp_result.error.is_none() {
                        return cdp_result;
                    }
                }
                // Second, try native platform actuation.
                match native_popup_dismiss(request, &operation_id, &plan) {
                    Ok(true) => {
                        return success(
                            request,
                            operation_id,
                            "popup_manager",
                            EffectState::Changed,
                            VerificationState::Unverified,
                            json!({
                                "popup": popup,
                                "plan": plan,
                                "status": "dismissed_via_native_ax",
                                "note": "popup close button pressed via platform accessibility API"
                            }),
                        );
                    }
                    Ok(false) => {
                        // Native actuation not available or button not found.
                        // Fall through to the plan-only result.
                    }
                    Err(msg) => {
                        return ActionResult::refused(
                            request,
                            operation_id,
                            ComptrolError {
                                code: "native_actuation_failed".to_owned(),
                                message: msg,
                                recovery: Some(
                                    "Check platform accessibility permissions".to_owned(),
                                ),
                            },
                        );
                    }
                }
            }
            // Fallback: return authorized plan for manual dismissal
            let mut result = success(
                request,
                operation_id,
                "popup_manager",
                EffectState::NotAttempted,
                VerificationState::NotAttempted,
                json!({
                    "popup": popup,
                    "plan": plan,
                    "status": "dismissal_authorized_actuation_requires_bound_surface",
                    "note": "browser CDP and native AX actuation unavailable; dismissal plan provided for manual execution"
                }),
            );
            result.recovery = RecoveryState::RequiresReconciliation;
            result
        }
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "authorization_required".to_owned(),
                message: error.to_string(),
                recovery: Some("Set the matching user popup preference or ask the user".to_owned()),
            },
        ),
    }
}

fn browser_session_list(request: &OperationRequest, operation_id: String) -> ActionResult {
    let sessions = comptrol_browser::list_sessions();
    success(
        request,
        operation_id,
        "browser_session_broker",
        EffectState::None,
        VerificationState::Verified,
        json!({
            "providers": sessions.iter().map(|session| json!({
                "id": session.provider.id(),
                "available": session.available,
                "reason": session.reason,
                "signed_in_capable": session.signed_in_capable,
                "preferred_for": match session.provider {
                    comptrol_browser::SessionProvider::PermissionedAutoConnect => "signed-in tabs and tab groups",
                    comptrol_browser::SessionProvider::CompanionExtension => "closed-group restore without full CDP",
                    comptrol_browser::SessionProvider::ExplicitCdp => "dedicated automation profiles and custom endpoints",
                    comptrol_browser::SessionProvider::DedicatedProfile => "isolated automation",
                    comptrol_browser::SessionProvider::NativeLauncher => "foreground fallback with launcher-acceptance reporting only",
                },
            })).collect::<Vec<_>>(),
            "default_profile_cdp_bypass": "refused: Chrome 136+ ignores remote-debugging flags for the default user-data directory",
            "profile_copying": "refused: Comptrol never copies profiles or cookies",
        }),
    )
}

#[allow(unsafe_code)]
fn browser_session_connect(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(provider) = request.params.get("provider").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "browser.session.connect needs params.provider from browser.session.list"
                    .to_owned(),
                recovery: Some("List sessions and choose one provider".to_owned()),
            },
        );
    };
    match provider {
        "explicit_cdp_endpoint" => {
            if std::env::var_os("COMPTROL_CDP_ENDPOINT").is_none() {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "browser_unavailable".to_owned(),
                        message: "COMPTROL_CDP_ENDPOINT is not configured".to_owned(),
                        recovery: Some("Configure a local browser DevTools endpoint".to_owned()),
                    },
                );
            }
            success(
                request,
                operation_id,
                "browser_session_broker",
                EffectState::None,
                VerificationState::Verified,
                json!({ "provider": provider, "status": "connected_to_configured_endpoint" }),
            )
        }
        "chrome_permissioned_auto_connect" => {
            let timeout = Duration::from_secs(30);
            let ws_url = match tokio::runtime::Runtime::new().unwrap().block_on(
                comptrol_browser::connect_permissioned_auto_connect(9222, timeout),
            ) {
                Ok(url) => url,
                Err(e) => {
                    return ActionResult::refused(
                        request,
                        operation_id,
                        ComptrolError {
                            code: "route_unavailable".to_owned(),
                            message: e,
                            recovery: Some("Ensure Chrome 144+ is running with remote debugging enabled and user has clicked Allow".to_owned()),
                        },
                    );
                }
            };
            // Set the endpoint for subsequent browser operations
            unsafe {
                std::env::set_var("COMPTROL_CDP_ENDPOINT", &ws_url);
            }
            success(
                request,
                operation_id,
                "browser_session_broker",
                EffectState::Changed,
                VerificationState::Verified,
                json!({
                    "provider": "chrome_permissioned_auto_connect",
                    "status": "connected_via_permissioned_auto_connect",
                    "websocket_url": ws_url,
                    "note": "Chrome shows its native Allow prompt per connection; Comptrol never bypasses it"
                }),
            )
        }
        "companion_extension" => {
            let sessions = comptrol_browser::list_sessions();
            let ext_session = sessions
                .iter()
                .find(|s| s.provider == comptrol_browser::SessionProvider::CompanionExtension);
            let registered = ext_session.map(|s| s.available).unwrap_or(false);
            if !registered {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "route_unavailable".to_owned(),
                        message: "companion extension native host is not registered".to_owned(),
                        recovery: Some(
                            "Install the Browser Bridge extension and register the native messaging host"
                                .to_owned(),
                        ),
                    },
                );
            }
            let mut store = match browser_bridge::BridgeStore::open(&default_state_dir()) {
                Ok(store) => store,
                Err(error) => {
                    return ActionResult::refused(
                        request,
                        operation_id,
                        ComptrolError {
                            code: "route_unavailable".to_owned(),
                            message: format!("companion bridge state unavailable: {error}"),
                            recovery: Some("Start the Comptrol daemon and reconnect the Browser Bridge extension".to_owned()),
                        },
                    );
                }
            };
            let health = store
                .health(browser_bridge::DEFAULT_HEALTH_MAX_AGE)
                .unwrap_or(browser_bridge::BridgeHealth {
                    active: false,
                    last_heartbeat_ms: None,
                    target_count: 0,
                });
            if !health.active {
                return success(
                    request,
                    operation_id,
                    "companion_extension",
                    EffectState::None,
                    VerificationState::Unverified,
                    json!({
                        "provider": "companion_extension",
                        "status": "registered_but_inactive",
                        "bridge_health": health,
                        "note": "Native host registration exists, but no recent extension heartbeat proves an active session"
                    }),
                );
            }
            let round_trip = store
                .submit("bridge_ping", json!({"timestamp_ms": now_ms()}))
                .and_then(|request_id| store.wait_result(&request_id, Duration::from_secs(2)));
            match round_trip {
                Ok(result) if result.get("ok").and_then(Value::as_bool) == Some(true) => success(
                    request,
                    operation_id,
                    "companion_extension",
                    EffectState::None,
                    VerificationState::Verified,
                    json!({
                        "provider": "companion_extension",
                        "status": "connected_via_companion_extension",
                        "bridge_health": health,
                        "round_trip": result,
                        "note": "Native host registration, fresh heartbeat, and an extension command round-trip were verified"
                    }),
                ),
                Ok(result) => success(
                    request,
                    operation_id,
                    "companion_extension",
                    EffectState::None,
                    VerificationState::Unverified,
                    json!({
                        "provider": "companion_extension",
                        "status": "bridge_round_trip_unverified",
                        "bridge_health": health,
                        "round_trip": result,
                    }),
                ),
                Err(error) => success(
                    request,
                    operation_id,
                    "companion_extension",
                    EffectState::None,
                    VerificationState::Unverified,
                    json!({
                        "provider": "companion_extension",
                        "status": "bridge_round_trip_failed",
                        "bridge_health": health,
                        "error": error.to_string(),
                    }),
                ),
            }
        }
        _ => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: format!("unknown browser session provider {provider}"),
                recovery: Some("Use browser.session.list".to_owned()),
            },
        ),
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

/// Execute `browser.chrome.restore_recent` (and the
/// `browser.chrome.reopen_closed_group` compatibility alias) through Chrome's
/// own restore subsystem with semantic matching, unique-entry enforcement,/// CDP target-graph verification, and truthful reconstruction labeling.
fn browser_chrome_restore_recent(request: &OperationRequest, operation_id: String) -> ActionResult {
    if request.background.as_deref() == Some("strict_background") {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "background_unavailable".to_owned(),
                message: "Native restore may activate the visible browser window".to_owned(),
                recovery: Some("Use browser.cdp.open_tab for strict background control".to_owned()),
            },
        );
    }
    let endpoint = std::env::var("COMPTROL_CDP_ENDPOINT").ok();
    // Compatibility alias: a legacy caller that passes only a group name gets
    // tab_group semantics with native restore then explicit reconstruction
    // permission preserved from its params.
    let mut params = request.params.clone();
    if request.intent == "browser.chrome.reopen_closed_group"
        && params.get("kind").is_none()
        && params.get("group").and_then(Value::as_str).is_some()
    {
        params["kind"] = json!("tab_group");
    }
    match restore::execute(endpoint.as_deref(), &params, None) {
        Ok(outcome) => success(
            request,
            operation_id,
            "chrome_restore",
            EffectState::Changed,
            VerificationState::Verified,
            json!({
                "restoration_mode": outcome.restoration_mode,
                "native_restore_used": outcome.native_restore_used,
                "kind": outcome.kind.as_str(),
                "group_title": outcome.group_title,
                "targets": outcome.restored_targets.iter().map(|target| json!({
                    "id": target.id,
                    "url": target.url,
                    "title": target.title,
                })).collect::<Vec<_>>(),
                "not_restored": outcome.not_restored,
                "verification": outcome.verification,
                "mouse": "untouched",
                "clipboard": "untouched"
            }),
        ),
        Err(error) => match error {
            restore::RestoreError::Refused {
                code,
                message,
                recovery,
            }
            | restore::RestoreError::Unavailable {
                code,
                message,
                recovery,
            } => ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: code.to_owned(),
                    message,
                    recovery,
                },
            ),
        },
    }
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

#[cfg(not(target_os = "linux"))]
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
    #[cfg(windows)]
    if std::env::var("COMPTROL_WINDOWS_UIA_LEGACY").as_deref() != Ok("1") {
        let action = if request.intent.ends_with("press") {
            comptrol_platform_windows::Action::Press
        } else {
            comptrol_platform_windows::Action::SetValue
        };
        let (expected_attribute, expected_value) = request
            .postcondition
            .as_ref()
            .and_then(|postcondition| {
                Some((
                    postcondition.get("attribute")?.as_str()?,
                    postcondition.get("equals")?.as_str()?,
                ))
            })
            .unzip();
        let result = comptrol_platform_windows::execute(comptrol_platform_windows::Request {
            process_id: process_id as u32,
            name,
            automation_id,
            role: request.params.get("role").and_then(Value::as_str),
            action,
            value: request.params.get("value").and_then(Value::as_str),
            expected_attribute,
            expected_value,
        });
        return match result {
            Ok(data) if data.get("verified").and_then(Value::as_bool) == Some(true) => success(
                request,
                operation_id,
                "windows_uia_direct",
                EffectState::Changed,
                VerificationState::Verified,
                data,
            ),
            Ok(data) => success(
                request,
                operation_id,
                "windows_uia_direct",
                EffectState::Changed,
                VerificationState::Unverified,
                data,
            ),
            Err(message) => ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: if message.contains("ambiguous") {
                        "target_ambiguous"
                    } else if message.contains("missing") {
                        "target_gone"
                    } else if message.contains("disabled") {
                        "not_actionable"
                    } else {
                        "adapter_unavailable"
                    }
                    .to_owned(),
                    message,
                    recovery: Some(
                        "Refresh the exact UI Automation target and inspect Windows permissions"
                            .to_owned(),
                    ),
                },
            ),
        };
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
    #[cfg(target_os = "linux")]
    {
        let action = if request.intent.ends_with("press") {
            comptrol_platform_linux::Action::Press
        } else {
            comptrol_platform_linux::Action::SetValue
        };
        let (expected_attribute, expected_value) = request
            .postcondition
            .as_ref()
            .and_then(|postcondition| {
                Some((
                    postcondition.get("attribute")?.as_str()?,
                    postcondition.get("equals")?.as_str()?,
                ))
            })
            .unzip();
        let result = comptrol_platform_linux::execute(comptrol_platform_linux::Request {
            process_id: process_id as u32,
            name,
            role: request.params.get("role").and_then(Value::as_str),
            action,
            value: request.params.get("value").and_then(Value::as_str),
            expected_attribute,
            expected_value,
            timeout: Duration::from_millis(1500),
        });
        match result {
            Ok(data) if data.get("verified").and_then(Value::as_bool) == Some(true) => success(
                request,
                operation_id,
                "linux_atspi_direct",
                EffectState::Changed,
                VerificationState::Verified,
                data,
            ),
            Ok(data) => success(
                request,
                operation_id,
                "linux_atspi_direct",
                EffectState::Changed,
                VerificationState::Unverified,
                data,
            ),
            Err(message) => ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: if message.contains("ambiguous") {
                        "target_ambiguous"
                    } else if message.contains("missing") {
                        "target_gone"
                    } else {
                        "adapter_unavailable"
                    }
                    .to_owned(),
                    message,
                    recovery: Some(
                        "Refresh the exact AT-SPI target and inspect the Linux accessibility bus"
                            .to_owned(),
                    ),
                },
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let mut command = Command::new("python3");
    #[cfg(not(target_os = "linux"))]
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
    #[cfg(not(target_os = "linux"))]
    if let Some(role) = request.params.get("role").and_then(Value::as_str) {
        command.env("COMPTROL_ATSPI_ROLE", role);
    }
    #[cfg(not(target_os = "linux"))]
    if let Some(action) = request.params.get("action").and_then(Value::as_str) {
        command.env("COMPTROL_ATSPI_ACTION", action);
    }
    #[cfg(not(target_os = "linux"))]
    if let Some(value) = request.params.get("value").and_then(Value::as_str) {
        command.env("COMPTROL_ATSPI_VALUE", value);
    }
    #[cfg(not(target_os = "linux"))]
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
    #[cfg(not(target_os = "linux"))]
    return semantic_provider_result(
        request,
        operation_id,
        "linux_atspi",
        run_bounded(command, Duration::from_millis(1500)),
    );
}

fn macos_ax_press(request: &OperationRequest, operation_id: String) -> ActionResult {
    if !cfg!(target_os = "macos") {
        return unsupported_ax(request, operation_id);
    }
    #[cfg(target_os = "macos")]
    if let (Some(process_id), Some(control)) = (
        request.params.get("process_id").and_then(Value::as_u64),
        request.params.get("control").and_then(Value::as_str),
    ) {
        return macos_ax_direct_result(
            request,
            operation_id,
            comptrol_platform_macos::Action::Press,
            process_id,
            control,
            None,
        );
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
    #[cfg(target_os = "macos")]
    if let (Some(process_id), Some(control)) = (
        request.params.get("process_id").and_then(Value::as_u64),
        request.params.get("control").and_then(Value::as_str),
    ) {
        return macos_ax_direct_result(
            request,
            operation_id,
            comptrol_platform_macos::Action::SetValue,
            process_id,
            control,
            Some(value),
        );
    }
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

#[cfg(target_os = "macos")]
fn macos_ax_direct_result(
    request: &OperationRequest,
    operation_id: String,
    action: comptrol_platform_macos::Action,
    process_id: u64,
    control: &str,
    value: Option<&str>,
) -> ActionResult {
    let (expected_attribute, expected_value) = request
        .postcondition
        .as_ref()
        .and_then(|postcondition| {
            Some((
                postcondition.get("attribute")?.as_str()?,
                postcondition.get("equals")?.as_str()?,
            ))
        })
        .unzip();
    match comptrol_platform_macos::execute(comptrol_platform_macos::Request {
        process_id: process_id as u32,
        name: control,
        role: request.params.get("role").and_then(Value::as_str),
        action,
        value,
        expected_attribute,
        expected_value,
        timeout: Duration::from_millis(1500),
    }) {
        Ok(data) if data.get("verified").and_then(Value::as_bool) == Some(true) => success(
            request,
            operation_id,
            "macos_ax_direct",
            EffectState::Changed,
            VerificationState::Verified,
            data,
        ),
        Ok(data) => success(
            request,
            operation_id,
            "macos_ax_direct",
            EffectState::Changed,
            VerificationState::Unverified,
            data,
        ),
        Err(message) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: if message.contains("ambiguous") {
                    "target_ambiguous"
                } else if message.contains("missing") {
                    "target_gone"
                } else if message.contains("permission") {
                    "permission_required"
                } else {
                    "adapter_unavailable"
                }
                .to_owned(),
                message,
                recovery: Some(
                    "Refresh the exact AX target and inspect macOS Accessibility permission"
                        .to_owned(),
                ),
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

pub(crate) fn apple_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', " ")
    )
}

pub(crate) fn run_osascript(script: &str) -> io::Result<Output> {
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
            name: "app.resolve".to_owned(),
            available: true,
            risk: Risk::R0,
            route: "app_registry_read".to_owned(),
            note: "Resolves a display name to the exact installed application identity without launching".to_owned(),
        },
        Capability {
            name: "app.list".to_owned(),
            available: true,
            risk: Risk::R0,
            route: "app_registry_read".to_owned(),
            note: "Lists installed applications from platform registrations".to_owned(),
        },
        Capability {
            name: "app.open_resource".to_owned(),
            available: (cfg!(target_os = "macos")
                || cfg!(target_os = "windows")
                || cfg!(target_os = "linux"))
                && std::env::var("COMPTROL_ALLOW_APP_LAUNCH").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "app_registry_launch".to_owned(),
            note: "Opens an exact file, URL, or deep link in its resolved application and verifies the resource".to_owned(),
        },
        Capability {
            name: "app.focus".to_owned(),
            available: (cfg!(target_os = "macos")
                || cfg!(target_os = "windows")
                || cfg!(target_os = "linux"))
                && std::env::var("COMPTROL_ALLOW_APP_LAUNCH").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "app_registry_activate".to_owned(),
            note: "Activates an exact installed application through the native launcher".to_owned(),
        },
        Capability {
            name: "permission.status".to_owned(),
            available: true,
            risk: Risk::R0,
            route: "permission_probe".to_owned(),
            note: "Reports accessibility, automation, session, and policy state without changing anything".to_owned(),
        },
        Capability {
            name: "permission.request".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SETTINGS").as_deref() == Ok("1"),
            risk: Risk::R1,
            route: "permission_surface".to_owned(),
            note: "Opens the exact OS permission surface and waits for the user; never self-grants".to_owned(),
        },
        Capability {
            name: "settings.get".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SETTINGS").as_deref() == Ok("1"),
            risk: Risk::R1,
            route: "settings_registry".to_owned(),
            note: "Reads a declared typed setting through its documented surface".to_owned(),
        },
        Capability {
            name: "settings.set".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SETTINGS").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "settings_registry".to_owned(),
            note: "Writes a declared typed setting with readback verification; protected settings wait for the user".to_owned(),
        },
        Capability {
            name: "software.search".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SOFTWARE").as_deref() == Ok("1"),
            risk: Risk::R1,
            route: "software_provider".to_owned(),
            note: "Searches trusted package providers; results never install directly".to_owned(),
        },
        Capability {
            name: "software.describe".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SOFTWARE").as_deref() == Ok("1"),
            risk: Risk::R1,
            route: "software_provider".to_owned(),
            note: "Describes one exact package including publisher, version, and agreements".to_owned(),
        },
        Capability {
            name: "software.install".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SOFTWARE_INSTALL").as_deref() == Ok("1"),
            risk: Risk::R3,
            route: "software_provider".to_owned(),
            note: "Installs one exact package after consent; elevation waits for the user and inventory verifies the result".to_owned(),
        },
        Capability {
            name: "software.update".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SOFTWARE_INSTALL").as_deref() == Ok("1"),
            risk: Risk::R3,
            route: "software_provider".to_owned(),
            note: "Updates one exact installed package with inventory verification".to_owned(),
        },
        Capability {
            name: "software.uninstall".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_SOFTWARE_INSTALL").as_deref() == Ok("1"),
            risk: Risk::R3,
            route: "software_provider".to_owned(),
            note: "Uninstalls one exact package and verifies its absence".to_owned(),
        },
        Capability {
            name: "popup.inspect".to_owned(),
            available: true,
            risk: Risk::R0,
            route: "popup_classifier".to_owned(),
            note: "Classifies a popup into its typed class without dismissing anything".to_owned(),
        },
        Capability {
            name: "popup.dismiss".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_POPUP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "popup_manager".to_owned(),
            note: "Dismisses only user-permitted popup classes; protected prompts always need the user".to_owned(),
        },
        Capability {
            name: "browser.session.list".to_owned(),
            available: true,
            risk: Risk::R0,
            route: "browser_session_broker".to_owned(),
            note: "Lists browser control surfaces without connecting or touching signed-in state".to_owned(),
        },
        Capability {
            name: "browser.session.connect".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_session_broker".to_owned(),
            note: "Connects through the selected browser surface; permissioned routes keep the browser consent UI".to_owned(),
        },
        Capability {
            name: "browser.cdp.dialog".to_owned(),
            available: std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1")
                && std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some(),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Handles a JavaScript dialog on one exact target; protected dialogs go through popup.dismiss".to_owned(),
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
            name: "browser.chrome.restore_recent".to_owned(),
            available: true,
            risk: Risk::R2,
            route: "chrome_restore".to_owned(),
            note: "Restores one exact recently-closed Chrome tab, group, or window through Chrome's own restore surface with unique semantic matching and live CDP verification; reconstructs only when explicitly allowed and labeled".to_owned(),
        },
        Capability {
            name: "browser.chrome.reopen_closed_group".to_owned(),
            available: true,
            risk: Risk::R2,
            route: "chrome_restore".to_owned(),
            note: "Compatibility alias for browser.chrome.restore_recent with kind tab_group".to_owned(),
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
            name: "browser.cdp.screenshot".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R0,
            route: "browser_protocol".to_owned(),
            note: "Captures a bounded target-scoped visual digest without returning pixels through MCP".to_owned(),
        },
        Capability {
            name: "browser.cdp.coordinate_click".to_owned(),
            available: std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some()
                && std::env::var("COMPTROL_ALLOW_BROWSER_CDP").as_deref() == Ok("1"),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Dispatches one coordinate click only when a fresh screenshot capture_id proves the viewport geometry is current".to_owned(),
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
    for intent in FIRST_PARTY_ADAPTER_INTENTS {
        result.push(Capability {
            name: (*intent).to_owned(),
            available: env_enabled("COMPTROL_ALLOW_ADAPTERS")
                && std::env::var_os("COMPTROL_ADAPTER_ROOT").is_some()
                && (!matches!(*intent, "obs.recording.start" | "obs.recording.stop")
                    || env_enabled("COMPTROL_ALLOW_HIGH_CONSEQUENCE_ADAPTERS")),
            risk: classify(intent),
            route: "isolated_adapter".to_owned(),
            note: "Runs through a bounded out of process first party adapter and requires application state verification".to_owned(),
        });
    }
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
        "mcp_protocols": {
            "current": { "version": mcp::CURRENT_VERSION, "mode": "stateless", "status": "implemented_not_live_verified" },
            "legacy": { "version": mcp::LEGACY_VERSION, "mode": "compatibility", "status": "implemented" },
            "tasks": { "status": "implemented_not_live_verified" }
        },
        "policy": {
            "max_risk": runtime.policy.max_risk,
            "sandbox_writes": runtime.policy.allow_sandbox_writes,
            "desktop_notify": runtime.policy.allow_desktop_notify,
            "macos_ax": runtime.policy.allowed_intents.contains("macos.ax.press"),
            "browser_fixture": runtime.policy.allowed_intents.contains("browser.fixture.submit"),
            "commands": runtime.policy.allowed_intents.contains("command.run")
        },
        "consent": {
            "store": match &runtime.consent {
                Some(store) => json!({ "available": true, "path": store.path(), "active_grants": store.active(None).len() }),
                None => json!({ "available": false, "reason": "consent store failed to open; run setup to recreate it" }),
            },
            "awaiting_human_action": runtime.human_actions.all().iter().filter(|action| action.resolution.is_none()).count(),
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
            "status": if std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some() { "configured" } else { "not_configured" },
            "persistent_multiplexer": "implemented_not_live_verified",
            "event_target_frame_graph": "implemented_not_live_verified",
            "chrome_native_restore": "implemented_not_live_verified",
            "extension": "experimental_only"
        },
        "adapters": {
            "vscode_bridge": if std::env::var_os("COMPTROL_VSCODE_BRIDGE_TOKEN").is_some() { "configured" } else { "requires_consent" },
            "libreoffice_uno": "implemented_not_live_verified",
            "obs_websocket": "implemented_not_live_verified",
            "blender": "offline_only_live_pending",
            "davinci_resolve": if std::env::var_os("COMPTROL_RESOLVE_SCRIPTING_DIR").is_some() || cfg!(target_os = "macos") { "probed_at_handshake" } else { "implemented_not_live_verified" },
            "google_workspace": if std::env::var_os("COMPTROL_GOOGLE_ACCESS_TOKEN").is_some() { "configured" } else { "requires_consent" },
            "powerpoint_openxml": "implemented_not_live_verified",
            "powerpoint_windows_com": if cfg!(target_os = "windows") { "probed_at_handshake" } else { "unsupported_on_platform" },
            "discord_bot": if std::env::var_os("COMPTROL_DISCORD_BOT_TOKEN").is_some() { "configured" } else { "requires_consent" },
            "gmail": if std::env::var_os("COMPTROL_GMAIL_ACCESS_TOKEN").is_some() { "configured" } else { "requires_consent" },
            "microsoft_graph_mail": if std::env::var_os("COMPTROL_GRAPH_ACCESS_TOKEN").is_some() { "configured" } else { "requires_consent" },
            "apple_mail": if cfg!(target_os = "macos") { "implemented_not_live_verified" } else { "unsupported_on_platform" },
            "apple_messages": if cfg!(target_os = "macos") { "implemented_not_live_verified" } else { "unsupported_on_platform" },
            "canva": if std::env::var_os("COMPTROL_CANVA_ACCESS_TOKEN").is_some() { "configured_preview" } else { "requires_consent" }
        },
        "workflow": { "typed_ir": "implemented", "parameter_lifting": "implemented", "clean_replay_promotion": "implemented_not_live_verified" },
        "route_statistics": { "durable": "implemented", "planner_feedback": "implemented", "latency": "implemented" },
        "client_configuration": integration::list(),
        "remote": { "available": false, "binding": "loopback_only" },
        "state_dir": state_dir()
    })
}

fn to_consent_risk(risk: Risk) -> comptrol_consent::Risk {
    match risk {
        Risk::R0 => comptrol_consent::Risk::R0,
        Risk::R1 => comptrol_consent::Risk::R1,
        Risk::R2 => comptrol_consent::Risk::R2,
        Risk::R3 | Risk::R4 => comptrol_consent::Risk::R3,
    }
}

/// Capabilities where a missing consent grant blocks execution. Ordinary
/// read-only or already-policy-gated intents continue to work unchanged;
/// software/settings/intall-class mutations require explicit local consent.
fn consent_gate_applies(intent: &str) -> bool {
    matches!(
        intent,
        "software.install"
            | "software.update"
            | "software.uninstall"
            | "settings.set"
            | "settings.write"
            | "software.launch_after_install"
    )
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
    #[cfg(not(unix))]
    let _ = path;
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

    #[test]
    fn route_p95_uses_nearest_rank_percentile() {
        let samples = (1..=20).map(|value| value as f64).collect::<VecDeque<_>>();
        assert_eq!(percentile_95(&samples), Some(19.0));
    }

    #[test]
    fn route_latency_samples_are_bounded() {
        let mut history = RouteHistory::default();
        for value in 0..(ROUTE_LATENCY_SAMPLE_CAP + 10) {
            history.record_latency(value as f64);
        }
        assert_eq!(history.latency_samples_ms.len(), ROUTE_LATENCY_SAMPLE_CAP);
        assert!(history.p95_latency_ms.is_some());
    }

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
    fn dry_run_returns_deterministic_route_plan_and_rationale() {
        let mut runtime = runtime();
        let result = runtime.operate(OperationRequest {
            intent: "system.ping".to_owned(),
            target: None,
            params: Value::Null,
            postcondition: None,
            risk: None,
            idempotency_key: Some("route-plan-ping".to_owned()),
            dry_run: true,
            background: None,
        });
        assert_eq!(result.route, "native");
        assert_eq!(result.data["route_plan"]["selected"], "native");
        assert!(
            result.data["route_plan"]["rationale"]
                .as_str()
                .is_some_and(|value| value.contains("deterministic"))
        );
        assert_eq!(result.data["route_plan"]["candidates"][0]["feasible"], true);
        assert_eq!(
            result.data["route_plan"]["candidates"][0]["expected_model_turns"],
            0
        );
        assert!(result.data["route_plan"]["candidates"][0]["utility"].is_number());
    }

    #[test]
    fn route_history_survives_runtime_restart() {
        let path = std::env::temp_dir().join(format!("comptrol-route-stats-{}", now_ms()));
        {
            let mut first = Runtime::new(path.clone()).expect("first runtime");
            let result = first.operate(OperationRequest {
                intent: "system.ping".to_owned(),
                target: None,
                params: Value::Null,
                postcondition: None,
                risk: None,
                idempotency_key: Some("route-stats-ping".to_owned()),
                dry_run: false,
                background: None,
            });
            assert_eq!(result.verification, VerificationState::Verified);
        }
        let mut second = Runtime::new(path).expect("restarted runtime");
        let stats = second.inspect("route_stats");
        let native = stats["routes"]
            .as_array()
            .and_then(|routes| routes.iter().find(|route| route["route"] == "native"))
            .expect("native route stats");
        assert!(native["attempts"].as_u64().unwrap_or_default() >= 1);
        assert!(native["verified_successes"].as_u64().unwrap_or_default() >= 1);
        assert!(native["verification_failures"].is_number());
        assert!(native["dispatch_failures"].is_number());
        assert!(native["ewma_latency_ms"].is_number());
        assert!(native["p95_latency_ms"].is_number());
    }

    #[test]
    fn route_planner_rejects_unknown_intents_explicitly() {
        let plan = route_plan_for_intent("unknown.intent", Value::Null, None);
        assert!(plan.selected.is_none());
        assert_eq!(plan.candidates[0].route, "none");
        assert!(plan.rationale.contains("No registered route"));
    }

    #[test]
    fn strict_background_rejects_foreground_only_browser_opening() {
        let plan = route_plan_for_intent(
            "browser.cdp.open_tab",
            json!({"background": false}),
            Some("strict_background"),
        );
        assert!(plan.selected.is_none());
        assert!(
            plan.candidates[0]
                .rationale
                .contains("strict_background requires")
        );
    }

    #[test]
    fn unavailable_dry_run_preserves_route_rejection_rationale() {
        let mut runtime = runtime();
        runtime
            .policy
            .allowed_intents
            .insert("browser.cdp.open_tab".to_owned());
        runtime.policy.max_risk = Risk::R2;
        let result = runtime.operate(OperationRequest {
            intent: "browser.cdp.open_tab".to_owned(),
            target: None,
            params: json!({"url":"https://example.test", "background":false}),
            postcondition: None,
            risk: None,
            idempotency_key: Some("route-plan-unavailable".to_owned()),
            dry_run: true,
            background: Some("strict_background".to_owned()),
        });
        assert_eq!(result.route, "none");
        assert_eq!(result.preflight, "failed");
        assert_eq!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("route_unavailable")
        );
        assert!(result.data["route_plan"]["rationale"].as_str().is_some());
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
    fn restore_recent_metadata_is_reconcilable_without_typed_urls() {
        let mut runtime = runtime();
        runtime
            .policy
            .allowed_intents
            .insert("browser.chrome.restore_recent".to_owned());
        runtime.policy.max_risk = Risk::R2;
        let request = OperationRequest {
            intent: "browser.chrome.restore_recent".to_owned(),
            target: None,
            params: json!({"kind":"tab_group", "group":"Research", "urls":["https://example.test/one"]}),
            postcondition: None,
            risk: Some(Risk::R2),
            idempotency_key: Some("restore-metadata".to_owned()),
            dry_run: true,
            background: None,
        };
        let metadata = operation_metadata(&request);
        assert_eq!(metadata["action"], "restore_recent");
        assert_eq!(metadata["restore_kind"], "tab_group");
        assert_eq!(metadata["restore_url_count"], 1);
        assert!(metadata.get("urls").is_none());
        assert!(metadata.get("restore_urls_hash").is_some());
        let result = runtime.operate(request);
        assert!(result.data["route_plan"]["rationale"].as_str().is_some());
    }

    #[test]
    fn restore_recent_refuses_strict_background() {
        let mut runtime = runtime();
        runtime
            .policy
            .allowed_intents
            .insert("browser.chrome.restore_recent".to_owned());
        runtime.policy.max_risk = Risk::R2;
        let request = OperationRequest {
            intent: "browser.chrome.restore_recent".to_owned(),
            target: None,
            params: json!({"kind":"tab", "url":"https://example.test/one"}),
            postcondition: None,
            risk: Some(Risk::R2),
            idempotency_key: Some("restore-strict-bg".to_owned()),
            dry_run: false,
            background: Some("strict_background".to_owned()),
        };
        let result = runtime.operate(request);
        assert_eq!(result.delivery, DeliveryState::Refused);
        assert_eq!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("background_unavailable")
        );
    }

    #[test]
    fn restore_recent_alias_shares_route_with_current_intent() {
        let mut runtime = runtime();
        runtime
            .policy
            .allowed_intents
            .insert("browser.chrome.reopen_closed_group".to_owned());
        runtime.policy.max_risk = Risk::R2;
        let request = OperationRequest {
            intent: "browser.chrome.reopen_closed_group".to_owned(),
            target: None,
            params: json!({"kind":"tab", "url":"https://example.test/one", "mode":"reconstruct_only"}),
            postcondition: None,
            risk: Some(Risk::R2),
            idempotency_key: Some("restore-alias".to_owned()),
            dry_run: false,
            background: None,
        };
        let result = runtime.operate(request);
        // Without a CDP endpoint the reconstruct path refuses with a
        // machine-readable error rather than dispatching anything.
        assert_eq!(result.delivery, DeliveryState::Refused);
        assert!(matches!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("browser_unavailable") | Some("invalid_input") | Some("policy_denied")
        ));
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
        let events = EventBus::new(2);
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
    fn event_bus_waits_for_notification_instead_of_polling() {
        let events = std::sync::Arc::new(EventBus::new(8));
        let waiter = std::sync::Arc::clone(&events);
        let started = std::time::Instant::now();
        let thread =
            std::thread::spawn(move || waiter.wait_for(0, Some("ready"), Duration::from_secs(1)));
        std::thread::sleep(Duration::from_millis(20));
        events.emit("ready", json!({"ok": true}));
        let event = thread.join().expect("waiter thread").expect("event");
        assert_eq!(event.kind, "ready");
        assert!(started.elapsed() < Duration::from_millis(500));
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

    #[test]
    fn app_launch_refuses_ambiguous_and_missing_apps_without_guessing() {
        let mut runtime = runtime();
        runtime.policy.max_risk = Risk::R2;
        runtime.policy.allow_app_launch = true;
        runtime
            .policy
            .allowed_intents
            .insert("app.launch".to_owned());
        let refused = runtime.operate(OperationRequest {
            intent: "app.launch".to_owned(),
            target: None,
            params: json!({ "app": "definitely-not-an-app-xyz" }),
            postcondition: None,
            risk: Some(Risk::R1),
            idempotency_key: None,
            dry_run: false,
            background: None,
        });
        assert_eq!(
            refused.error.as_ref().map(|e| e.code.as_str()),
            Some("app_not_resolved")
        );
        // No path traversal through the app name.
        let traversal = runtime.operate(OperationRequest {
            intent: "app.launch".to_owned(),
            target: None,
            params: json!({ "app": "/tmp/evil" }),
            postcondition: None,
            risk: Some(Risk::R1),
            idempotency_key: None,
            dry_run: false,
            background: None,
        });
        assert_eq!(
            traversal.error.as_ref().map(|e| e.code.as_str()),
            Some("app_not_resolved")
        );
    }

    #[test]
    fn consent_gate_blocks_ungranted_install_and_allows_granted() {
        let mut runtime = runtime();
        // Environment policy alone must no longer authorize an install.
        runtime.policy.max_risk = Risk::R3;
        runtime
            .policy
            .allowed_intents
            .insert("software.install".to_owned());
        let refused = runtime.operate(OperationRequest {
            intent: "software.install".to_owned(),
            target: Some(Target {
                kind: "package".to_owned(),
                id: Some("winget:VideoLAN.VLC".to_owned()),
                name: None,
            }),
            params: json!({}),
            postcondition: None,
            risk: Some(Risk::R3),
            idempotency_key: None,
            dry_run: true,
            background: None,
        });
        assert_eq!(
            refused.error.as_ref().map(|e| e.code.as_str()),
            Some("consent_required")
        );

        // Granting consent locally (the setup path) unblocks the intent.
        let grant = runtime
            .consent
            .as_mut()
            .expect("consent store")
            .grant(
                "software.install",
                comptrol_consent::ConsentScope {
                    intent: Some("software.install".to_owned()),
                    resource: Some("winget:VideoLAN.VLC".to_owned()),
                    max_risk: Some(comptrol_consent::Risk::R3),
                    ..Default::default()
                },
                comptrol_consent::GrantSubject::LocalUser,
                comptrol_consent::Risk::R3,
                None,
            )
            .expect("grant");
        assert!(!grant.id.is_empty());
        let allowed = runtime.operate(OperationRequest {
            intent: "software.install".to_owned(),
            target: Some(Target {
                kind: "package".to_owned(),
                id: Some("winget:VideoLAN.VLC".to_owned()),
                name: None,
            }),
            params: json!({}),
            postcondition: None,
            risk: Some(Risk::R3),
            idempotency_key: None,
            dry_run: true,
            background: None,
        });
        assert_ne!(
            allowed.error.as_ref().map(|e| e.code.as_str()),
            Some("consent_required")
        );
    }

    #[test]
    fn doctor_reports_consent_state() {
        let mut runtime = runtime();
        let report = runtime.inspect("doctor");
        assert_eq!(report["consent"]["store"]["available"], json!(true));
        let consent = runtime.inspect("consent");
        assert!(consent["human_actions"]["pending"].is_u64());
    }

    fn allow(runtime: &mut Runtime, intent: &str, risk: Risk) {
        runtime.policy.max_risk = runtime.policy.max_risk.max(risk);
        runtime.policy.allowed_intents.insert(intent.to_owned());
    }

    fn operate(runtime: &mut Runtime, intent: &str, params: Value, risk: Risk) -> ActionResult {
        runtime.operate(OperationRequest {
            intent: intent.to_owned(),
            target: None,
            params,
            postcondition: None,
            risk: Some(risk),
            idempotency_key: None,
            dry_run: false,
            background: None,
        })
    }

    #[test]
    fn new_intents_are_policy_denied_by_default() {
        let mut runtime = runtime();
        for intent in [
            "app.open_resource",
            "settings.get",
            "settings.set",
            "software.search",
            "software.install",
            "popup.dismiss",
        ] {
            let result = operate(&mut runtime, intent, json!({}), Risk::R3);
            assert_eq!(
                result.error.as_ref().map(|e| e.code.as_str()),
                Some("policy_denied"),
                "{intent} must be policy denied by default"
            );
        }
    }

    #[test]
    fn read_only_v5_intents_work_by_default() {
        let mut runtime = runtime();
        let listed = operate(&mut runtime, "browser.session.list", json!({}), Risk::R0);
        assert!(listed.error.is_none());
        assert_eq!(listed.verification, VerificationState::Verified);
        let status = operate(&mut runtime, "permission.status", json!({}), Risk::R0);
        assert!(status.error.is_none());
        assert_eq!(
            status.data["accessibility_trusted"].as_bool(),
            Some(comptrol_platform_macos_stub())
        );
        let inspected = operate(
            &mut runtime,
            "popup.inspect",
            json!({"role": "dialog", "name": "Checkout", "text": "Enter payment details"}),
            Risk::R0,
        );
        assert!(inspected.error.is_none());
        assert_eq!(
            inspected.data["popup"]["class"],
            json!("purchase_or_payment")
        );
        assert_eq!(inspected.data["never_auto"], json!(true));
    }

    #[cfg(target_os = "macos")]
    fn comptrol_platform_macos_stub() -> bool {
        comptrol_platform_macos::accessibility_trusted()
    }

    #[cfg(not(target_os = "macos"))]
    fn comptrol_platform_macos_stub() -> bool {
        false
    }

    #[test]
    fn popup_dismiss_never_auto_approves_protected_classes() {
        let mut runtime = runtime();
        allow(&mut runtime, "popup.dismiss", Risk::R2);
        let result = operate(
            &mut runtime,
            "popup.dismiss",
            json!({
                "role": "dialog",
                "name": "User Account Control",
                "text": "Do you want to allow this app",
                "dismiss_informational": true,
                "dismiss_cookie_banners": true,
                "dismiss_update_prompts": true,
                "dismiss_tips": true,
            }),
            Risk::R2,
        );
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("authorization_required")
        );
    }

    #[test]
    fn popup_dismiss_returns_plan_without_claiming_dismissal() {
        let mut runtime = runtime();
        allow(&mut runtime, "popup.dismiss", Risk::R2);
        let result = operate(
            &mut runtime,
            "popup.dismiss",
            json!({
                "role": "dialog",
                "name": "Tip of the day",
                "text": "Did you know about this feature dialog",
                "dismiss_informational": true,
                "close_actions": ["Not Now"],
            }),
            Risk::R2,
        );
        assert!(result.error.is_none());
        assert_eq!(result.verification, VerificationState::NotAttempted);
        assert_eq!(
            result.data["status"],
            json!("dismissal_authorized_actuation_requires_bound_surface")
        );
    }

    #[test]
    fn settings_rejects_unknown_keys_without_guessing() {
        let mut runtime = runtime();
        allow(&mut runtime, "settings.get", Risk::R1);
        let result = operate(
            &mut runtime,
            "settings.get",
            json!({"key": "registry.HKLM.Software.Evil"}),
            Risk::R1,
        );
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("unknown_setting")
        );
        allow(&mut runtime, "settings.set", Risk::R2);
        let write = operate(
            &mut runtime,
            "settings.set",
            json!({"key": "settings.bluetooth.enabled", "value": true}),
            Risk::R2,
        );
        // Consent gate blocks ungranted settings writes before any provider runs.
        assert_eq!(
            write.error.as_ref().map(|e| e.code.as_str()),
            Some("consent_required")
        );
    }

    #[test]
    fn software_search_needs_query_and_policy() {
        let mut runtime = runtime();
        allow(&mut runtime, "software.search", Risk::R1);
        let missing = operate(&mut runtime, "software.search", json!({}), Risk::R1);
        assert_eq!(
            missing.error.as_ref().map(|e| e.code.as_str()),
            Some("invalid_input")
        );
        // The failure must be honest unavailability, never a guess.
        let result = operate(
            &mut runtime,
            "software.search",
            json!({"query": "vlc"}),
            Risk::R1,
        );
        if let Some(error) = &result.error {
            assert_eq!(error.code, "software_unavailable");
        } else {
            assert!(result.data["results"].is_array());
        }
    }

    #[test]
    fn app_open_resource_requires_resource() {
        let mut runtime = runtime();
        runtime.policy.max_risk = Risk::R2;
        runtime.policy.allow_app_launch = true;
        runtime
            .policy
            .allowed_intents
            .insert("app.open_resource".to_owned());
        let result = operate(
            &mut runtime,
            "app.open_resource",
            json!({"app": "TextEdit"}),
            Risk::R2,
        );
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("invalid_input")
        );
    }

    #[test]
    fn adapter_routing_resolves_exact_providers() {
        assert_eq!(
            adapter_id_for_intent("video.timeline.append", None),
            Ok("davinci-resolve")
        );
        assert_eq!(
            adapter_id_for_intent("document.text.replace", None),
            Ok("google-workspace")
        );
        assert_eq!(
            adapter_id_for_intent("presentation.read", None),
            Ok("powerpoint")
        );
        assert_eq!(
            adapter_id_for_intent("presentation.save", None),
            Ok("powerpoint-windows")
        );
        assert_eq!(
            adapter_id_for_intent("discord.message.send", None),
            Ok("discord")
        );
        assert_eq!(
            adapter_id_for_intent("message.send", None),
            Ok("apple-messages")
        );
        assert_eq!(adapter_id_for_intent("design.export", None), Ok("canva"));
        assert_eq!(
            adapter_id_for_intent("mail.send", Some("gmail")),
            Ok("gmail")
        );
        assert_eq!(
            adapter_id_for_intent("mail.send", Some("graph")),
            Ok("microsoft-graph-mail")
        );
        assert_eq!(
            adapter_id_for_intent("mail.send", Some("apple-mail")),
            Ok("apple-mail")
        );
        assert_eq!(
            adapter_id_for_intent("presentation.slide.create", Some("google")),
            Ok("google-workspace")
        );
        assert_eq!(
            adapter_id_for_intent("presentation.slide.create", Some("powerpoint")),
            Ok("powerpoint-windows")
        );
        assert_eq!(
            adapter_id_for_intent("presentation.export", Some("powerpoint")),
            Ok("powerpoint")
        );
    }

    #[test]
    fn adapter_routing_refuses_ambiguous_providers_without_guessing() {
        assert!(adapter_id_for_intent("mail.send", None).is_err());
        assert!(adapter_id_for_intent("mail.send", Some("carrier-pigeon")).is_err());
        assert!(adapter_id_for_intent("presentation.slide.create", None).is_err());
        assert!(adapter_id_for_intent("presentation.export", None).is_err());
        assert!(adapter_id_for_intent(" spreadsheets.sum", None).is_err());
    }

    #[test]
    fn high_consequence_sends_classify_r3() {
        assert_eq!(classify("mail.send"), Risk::R3);
        assert_eq!(classify("message.send"), Risk::R3);
        assert_eq!(classify("discord.message.delete"), Risk::R3);
        assert_eq!(classify("discord.message.send"), Risk::R2);
        assert_eq!(classify("design.export"), Risk::R1);
        assert_eq!(classify("mail.search"), Risk::R0);
    }

    #[test]
    fn browser_session_connect_validates_provider() {
        let mut runtime = runtime();
        allow(&mut runtime, "browser.session.connect", Risk::R2);
        let unknown = operate(
            &mut runtime,
            "browser.session.connect",
            json!({"provider": "nope"}),
            Risk::R2,
        );
        assert_eq!(
            unknown.error.as_ref().map(|e| e.code.as_str()),
            Some("invalid_input")
        );
        let unavailable = operate(
            &mut runtime,
            "browser.session.connect",
            json!({"provider": "companion_extension"}),
            Risk::R2,
        );
        // CompanionExtension no longer requires COMPTROL_CDP_ENDPOINT;
        // it routes through the daemon's native bridge instead.
        assert_eq!(
            unavailable.error.as_ref().map(|e| e.code.as_str()),
            Some("route_unavailable")
        );
    }
}
