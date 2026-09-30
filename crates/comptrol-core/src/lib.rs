#![deny(unsafe_code)]

pub mod adapters;
pub mod browser;
pub mod browser_bridge;
pub mod checkpoints;
pub mod chrome_autostart;
pub mod events;
pub mod geometry;
pub mod integration;
pub mod intent_schema;
pub mod mcp;
pub mod pairing;
pub mod restore;
pub mod restore_native;
pub mod setup;
pub mod terminal;
pub mod trace;

use comptrol_adapter_host::{AdapterHost, AdapterHostConfig};
use comptrol_adapter_sdk::{AdapterManifest, HealthState};
use comptrol_browser::{DownloadStage, DownloadTransaction, UploadTransaction};
pub use comptrol_verification::{
    VerificationCriterion, VerificationEvidence, VerificationLevel, VerificationReport,
    VerificationSource, VerificationState as StructuredVerificationState,
};
use comptrol_workflow::{
    SpeculativeStep, Workflow, WorkflowExecutor, WorkflowNode, run_speculative,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
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
pub const SERVER_VERSION: &str = "0.1.67";

/// P3.3: daemon ownership reporting. Set by `comptrol daemon` while it owns
/// the runtime; doctor reports `resident` and the live MCP client count so
/// multi-client sharing is observable instead of assumed.
pub static DAEMON_RESIDENT: AtomicBool = AtomicBool::new(false);
pub static DAEMON_CLIENTS: AtomicUsize = AtomicUsize::new(0);
pub const MAX_PROTOCOL_BYTES: usize = 1024 * 1024;

pub const FIRST_PARTY_ADAPTER_INTENTS: &[&str] = &[
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
    "blender.scene.object.delete",
    "blender.scene.create_2d_rocket",
    "blender.scene.copy_2d_rocket_to_3d",
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
    "presentation.slide.duplicate",
    "presentation.text.replace",
    "presentation.text.style",
    "presentation.export",
    "presentation.read",
    "presentation.batch_edit",
    "presentation.desktop.open",
    "presentation.desktop.batch_edit",
    "presentation.shape.text.set",
    "presentation.shape.textbox.create",
    "presentation.shape.image.insert",
    "presentation.shape.delete",
    "presentation.shape.geometry.set",
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
    "design.batch_edit",
    "design.export",
];

fn is_first_party_adapter_intent(intent: &str) -> bool {
    FIRST_PARTY_ADAPTER_INTENTS.contains(&intent)
}

/// Every non-adapter intent that `operate` dispatches. This is the
/// canonical list of callable core surfaces: the capability catalog
/// advertises exactly these names (plus the adapter list above), and a
/// conformance test fails if an intent is dispatched but never
/// advertised, or advertised but never dispatched. Both halves used to
/// drift silently, which is how a live route ended up unreachable.
pub const CORE_INTENTS: &[&str] = &[
    "system.ping",
    "workflow.execute",
    "workflow.speculate",
    "recipe.run",
    "desktop.terminal",
    "desktop.explorer",
    "desktop.observe",
    "platform.broker.observe",
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
    "settings.write",
    "software.search",
    "software.describe",
    "software.install",
    "software.update",
    "software.uninstall",
    "popup.inspect",
    "popup.dismiss",
    "browser.session.list",
    "browser.session.connect",
    "browser.ensure_session",
    "browser.chrome.open_tab",
    "browser.chrome.restore_recent",
    "browser.chrome.reopen_closed_group",
    "command.run",
    "windows.uia.inspect",
    "windows.uia.press",
    "windows.uia.set_value",
    "linux.atspi.press",
    "linux.atspi.set_value",
    "macos.ax.press",
    "macos.ax.set_value",
    "browser.fixture.submit",
    "browser.cdp.reopen_closed_group",
    "browser.cdp.discovery",
    "browser.cdp.wait_for",
    "browser.cdp.accessibility_snapshot",
    "browser.cdp.evaluate",
    "browser.cdp.frame_evaluate",
    "browser.cdp.ensure_state",
    "browser.cdp.navigate",
    "browser.cdp.upload",
    "browser.cdp.download",
    "browser.cdp.fill",
    "browser.cdp.click",
    "browser.cdp.focus",
    "browser.cdp.open_tab",
    "browser.cdp.close_tab",
    "browser.cdp.activate_tab",
    "browser.cdp.history_back",
    "browser.cdp.history_forward",
    "browser.cdp.semantic_click",
    "browser.cdp.semantic_fill",
    "browser.cdp.compact_snapshot",
    "browser.cdp.workflow",
    "browser.cdp.screenshot",
    "browser.cdp.coordinate_click",
    "browser.cdp.type_text",
    "browser.cdp.press_key",
    "browser.cdp.dialog",
];

/// Catalog names that describe a route family rather than a callable
/// intent. They are reported so a client can see the surface exists, but
/// calling them is a client error, so they live in their own catalog
/// section instead of being mistaken for dispatchable names.
pub const CAPABILITY_FAMILIES: &[&str] = &[
    "browser.cdp",
    "browser.cdp.history",
    "daemon.ipc",
    "desktop.semantic_input",
    "linux.atspi.semantic",
    "windows.uia.semantic",
];

/// Catalog rows that are detected surfaces rather than operations. Note
/// that `platform.broker.observe` is deliberately absent: it is a callable
/// intent that happens to share the prefix, so classification uses this
/// list instead of a name prefix.
pub const PLATFORM_OBSERVATIONS: &[&str] = &[
    "platform.macos.ax",
    "platform.windows.uia",
    "platform.linux.atspi",
    "platform.linux.x11",
    "platform.linux.wayland",
];

/// The callable surface, as the capability catalog reports it: every core
/// dispatch intent plus every first party adapter intent. Anything else in
/// the catalog is an observation or a family label.
pub fn callable_intent_catalog() -> Vec<Capability> {
    let mut result = capabilities();
    for intent in CORE_INTENTS {
        if !result.iter().any(|capability| capability.name == *intent) {
            result.push(Capability {
                name: (*intent).to_owned(),
                available: policy_reaches(intent),
                risk: classify(intent),
                route: "core".to_owned(),
                note: format!(
                    "Dispatchable core intent; required gates {:?}",
                    EnvGates::default().required_gates(intent)
                ),
            });
        }
    }
    result.sort_by(|left, right| left.name.cmp(&right.name));
    result
}

/// Whether the active policy would authorize this intent. Used for the
/// catalog rows that have no bespoke availability probe.
pub fn policy_reaches(intent: &str) -> bool {
    let policy = Policy::from_environment();
    policy.authorize(intent, classify(intent)).is_ok()
}

fn adapter_root() -> PathBuf {
    if let Some(root) = std::env::var_os("COMPTROL_ADAPTER_ROOT") {
        return PathBuf::from(root);
    }
    // Release archives and the npm package place `adapters/` beside the
    // executable or at a nearby package root. Find the shipped bundle so an
    // installed MCP does not depend on its current working directory.
    if let Ok(executable) = std::env::current_exe() {
        for parent in executable.ancestors().skip(1).take(5) {
            let candidate = parent.join("adapters");
            if candidate.join("blender").join("adapter.toml").is_file() {
                return candidate;
            }
        }
    }
    let state_bundle = default_state_dir().join("adapters");
    if state_bundle.join("blender").join("adapter.toml").is_file() {
        return state_bundle;
    }
    PathBuf::from("adapters")
}

fn adapter_python() -> PathBuf {
    if let Some(python) = std::env::var_os("COMPTROL_ADAPTER_PYTHON") {
        return PathBuf::from(python);
    }
    let local_venv = default_state_dir().join(if cfg!(windows) {
        "venv/Scripts/python.exe"
    } else {
        "venv/bin/python"
    });
    if local_venv.is_file() {
        local_venv
    } else {
        PathBuf::from(if cfg!(windows) {
            "python.exe"
        } else {
            "python3"
        })
    }
}

fn is_creative_adapter_intent(intent: &str) -> bool {
    intent.starts_with("blender.")
        || intent.starts_with("video.")
        || intent.starts_with("design.")
        || intent.starts_with("presentation.")
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
                "configured": browser::active_endpoint().is_some(),
                "address": browser::active_endpoint(),
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
    {
        return Err(ComptrolError {
            code: "stale_reference".to_owned(),
            message: "The browser target context changed".to_owned(),
            recovery: Some("Inspect browser targets and bind again".to_owned()),
        });
    }
    // K4: generation-tolerant revalidation. Revisions embed the tab URL as
    // their tail (`bridge:<profile:tab>:<generation>:<url>`); live SPAs bump
    // the generation constantly (status changes, same-URL reloads,
    // pushState), so an exact revision match is a losing race on real pages.
    // When only the generation counter moved — the pinned tail equals the
    // live URL — the document is the same class and the operation re-resolves
    // its locator at dispatch anyway, so revalidate instead of refusing. A
    // URL change or an unparsable revision still refuses.
    if let Some(expected) = revision {
        let actual = target.revision.as_deref().unwrap_or("");
        if actual != expected {
            let live_url = target.url.as_deref().unwrap_or("");
            let generation_only_drift = !live_url.is_empty()
                && expected.starts_with("bridge:")
                && expected.ends_with(&format!(":{live_url}"));
            if !generation_only_drift {
                return Err(ComptrolError {
                    code: "stale_reference".to_owned(),
                    message: "The browser target context or revision changed".to_owned(),
                    recovery: Some("Inspect browser targets and bind again".to_owned()),
                });
            }
        }
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
            allow_app_launch: true,
            max_risk: Risk::R2,
            allowed_intents: HashSet::from([
                "system.ping".to_owned(),
                "desktop.observe".to_owned(),
                "platform.broker.observe".to_owned(),
                "browser.cdp.wait_for".to_owned(),
                "browser.cdp.accessibility_snapshot".to_owned(),
                "browser.cdp.discovery".to_owned(),
                "browser.cdp.reopen_closed_group".to_owned(),
                "workflow.execute".to_owned(),
                "workflow.speculate".to_owned(),
                "recipe.run".to_owned(),
                "app.resolve".to_owned(),
                "app.list".to_owned(),
                "app.launch".to_owned(),
                "browser.chrome.open_tab".to_owned(),
                "permission.status".to_owned(),
                "popup.inspect".to_owned(),
                "browser.session.list".to_owned(),
            ]),
        }
    }
}

/// Every environment switch that can widen a [`Policy`], captured as data
/// instead of read inline. Keeping the gate set explicit is what makes it
/// possible to prove the capability gate and the policy allowlist agree:
/// they are two independent gates, and the bug that shipped
/// `policy_denied` on a capability-gated intent came from editing one
/// without the other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EnvGates {
    pub sandbox_writes: bool,
    pub desktop_notify: bool,
    pub macos_ax: bool,
    pub app_launch: bool,
    pub app_close: bool,
    pub software: bool,
    pub software_install: bool,
    pub settings: bool,
    pub popup: bool,
    pub commands: bool,
    pub desktop_explorer: bool,
    pub windows_uia: bool,
    pub linux_atspi: bool,
    pub browser_fixture: bool,
    pub all_intents: bool,
    pub browser_cdp: bool,
    pub adapters: bool,
    pub creative_adapters: bool,
    pub high_consequence_adapters: bool,
    pub mail_send: bool,
}

impl EnvGates {
    /// Every policy gate with the field that carries it, so
    /// `comptrol setup`, the docs, and the conformance test all walk the
    /// same list instead of keeping private copies.
    pub const NAMES: &'static [(&'static str, &'static str)] = &[
        ("COMPTROL_ALLOW_SANDBOX_WRITES", "sandbox_writes"),
        ("COMPTROL_ALLOW_DESKTOP_NOTIFY", "desktop_notify"),
        ("COMPTROL_ALLOW_MACOS_AX", "macos_ax"),
        ("COMPTROL_ALLOW_APP_LAUNCH", "app_launch"),
        ("COMPTROL_ALLOW_APP_CLOSE", "app_close"),
        ("COMPTROL_ALLOW_SOFTWARE", "software"),
        ("COMPTROL_ALLOW_SOFTWARE_INSTALL", "software_install"),
        ("COMPTROL_ALLOW_SETTINGS", "settings"),
        ("COMPTROL_ALLOW_POPUP", "popup"),
        ("COMPTROL_ALLOW_COMMANDS", "commands"),
        ("COMPTROL_ALLOW_DESKTOP_EXPLORER", "desktop_explorer"),
        ("COMPTROL_ALLOW_WINDOWS_UIA", "windows_uia"),
        ("COMPTROL_ALLOW_LINUX_ATSPI", "linux_atspi"),
        ("COMPTROL_ALLOW_BROWSER_FIXTURE", "browser_fixture"),
        ("COMPTROL_ALLOW_ALL_INTENTS", "all_intents"),
        ("COMPTROL_ALLOW_BROWSER_CDP", "browser_cdp"),
        ("COMPTROL_ALLOW_ADAPTERS", "adapters"),
        ("COMPTROL_ALLOW_CREATIVE_ADAPTERS", "creative_adapters"),
        (
            "COMPTROL_ALLOW_HIGH_CONSEQUENCE_ADAPTERS",
            "high_consequence_adapters",
        ),
        ("COMPTROL_ALLOW_MAIL_SEND", "mail_send"),
    ];

    pub fn from_environment() -> Self {
        Self {
            sandbox_writes: env_enabled("COMPTROL_ALLOW_SANDBOX_WRITES"),
            desktop_notify: env_enabled("COMPTROL_ALLOW_DESKTOP_NOTIFY"),
            macos_ax: env_enabled("COMPTROL_ALLOW_MACOS_AX"),
            app_launch: env_enabled("COMPTROL_ALLOW_APP_LAUNCH"),
            app_close: env_enabled("COMPTROL_ALLOW_APP_CLOSE"),
            software: env_enabled("COMPTROL_ALLOW_SOFTWARE"),
            software_install: env_enabled("COMPTROL_ALLOW_SOFTWARE_INSTALL"),
            settings: env_enabled("COMPTROL_ALLOW_SETTINGS"),
            popup: env_enabled("COMPTROL_ALLOW_POPUP"),
            commands: env_enabled("COMPTROL_ALLOW_COMMANDS"),
            desktop_explorer: env_enabled("COMPTROL_ALLOW_DESKTOP_EXPLORER"),
            windows_uia: env_enabled("COMPTROL_ALLOW_WINDOWS_UIA"),
            linux_atspi: env_enabled("COMPTROL_ALLOW_LINUX_ATSPI"),
            browser_fixture: env_enabled("COMPTROL_ALLOW_BROWSER_FIXTURE"),
            all_intents: env_enabled("COMPTROL_ALLOW_ALL_INTENTS"),
            browser_cdp: env_enabled("COMPTROL_ALLOW_BROWSER_CDP"),
            adapters: env_enabled("COMPTROL_ALLOW_ADAPTERS"),
            creative_adapters: env_enabled("COMPTROL_ALLOW_CREATIVE_ADAPTERS"),
            high_consequence_adapters: env_enabled("COMPTROL_ALLOW_HIGH_CONSEQUENCE_ADAPTERS"),
            mail_send: env_enabled("COMPTROL_ALLOW_MAIL_SEND"),
        }
    }

    /// A gate set with exactly one switch on or off. Returns `None` when
    /// the variable is not a recognized policy gate, which is itself a
    /// failure signal for callers that enumerate gate names.
    pub fn with(var: &str, value: bool) -> Option<Self> {
        let mut gates = Self::default();
        for (name, field) in Self::NAMES {
            if *name != var {
                continue;
            }
            match *field {
                "sandbox_writes" => gates.sandbox_writes = value,
                "desktop_notify" => gates.desktop_notify = value,
                "macos_ax" => gates.macos_ax = value,
                "app_launch" => gates.app_launch = value,
                "app_close" => gates.app_close = value,
                "software" => gates.software = value,
                "software_install" => gates.software_install = value,
                "settings" => gates.settings = value,
                "popup" => gates.popup = value,
                "commands" => gates.commands = value,
                "desktop_explorer" => gates.desktop_explorer = value,
                "windows_uia" => gates.windows_uia = value,
                "linux_atspi" => gates.linux_atspi = value,
                "browser_fixture" => gates.browser_fixture = value,
                "all_intents" => gates.all_intents = value,
                "browser_cdp" => gates.browser_cdp = value,
                "adapters" => gates.adapters = value,
                "creative_adapters" => gates.creative_adapters = value,
                "high_consequence_adapters" => gates.high_consequence_adapters = value,
                "mail_send" => gates.mail_send = value,
                _ => return None,
            }
            return Some(gates);
        }
        None
    }

    /// Every switch on. The most permissive policy the runtime can build,
    /// so a test can compare "what the catalog advertises" against
    /// "what policy can ever authorize" in one shot.
    pub fn all() -> Self {
        Self {
            sandbox_writes: true,
            desktop_notify: true,
            macos_ax: true,
            app_launch: true,
            app_close: true,
            software: true,
            software_install: true,
            settings: true,
            popup: true,
            commands: true,
            desktop_explorer: true,
            windows_uia: true,
            linux_atspi: true,
            browser_fixture: true,
            all_intents: true,
            browser_cdp: true,
            adapters: true,
            creative_adapters: true,
            high_consequence_adapters: true,
            mail_send: true,
        }
    }

    /// The environment switches that must be on before an intent can be
    /// authorized. `comptrol setup` prints these per intent so an
    /// operator never has to guess which switch unlocks a route.
    pub fn required_gates(self, intent: &str) -> &'static [&'static str] {
        GateRequirement::for_intent(intent, self)
    }
}

/// Declares the minimum gate set that makes an intent authorizable.
/// `comptrol_conformance` asserts that this table and the real
/// `Policy::from_gates` agree for every advertised intent, so a new
/// gate-gated intent cannot ship allowlist-less.
pub struct GateRequirement;

impl GateRequirement {
    pub fn for_intent(intent: &str, _gates: EnvGates) -> &'static [&'static str] {
        const NONE: &[&str] = &[];
        const SANDBOX: &[&str] = &["COMPTROL_ALLOW_SANDBOX_WRITES"];
        const NOTIFY: &[&str] = &["COMPTROL_ALLOW_DESKTOP_NOTIFY"];
        const MACOS_AX: &[&str] = &["COMPTROL_ALLOW_MACOS_AX"];
        const APP_LAUNCH: &[&str] = &["COMPTROL_ALLOW_APP_LAUNCH"];
        const APP_CLOSE: &[&str] = &["COMPTROL_ALLOW_APP_CLOSE"];
        const SOFTWARE: &[&str] = &["COMPTROL_ALLOW_SOFTWARE"];
        const SOFTWARE_INSTALL: &[&str] = &["COMPTROL_ALLOW_SOFTWARE_INSTALL"];
        const SETTINGS: &[&str] = &["COMPTROL_ALLOW_SETTINGS"];
        const POPUP: &[&str] = &["COMPTROL_ALLOW_POPUP"];
        const COMMANDS: &[&str] = &["COMPTROL_ALLOW_COMMANDS"];
        const DESKTOP_EXPLORER: &[&str] = &["COMPTROL_ALLOW_DESKTOP_EXPLORER"];
        const WINDOWS_UIA: &[&str] = &["COMPTROL_ALLOW_WINDOWS_UIA"];
        const LINUX_ATSPI: &[&str] = &["COMPTROL_ALLOW_LINUX_ATSPI"];
        const BROWSER_FIXTURE: &[&str] = &["COMPTROL_ALLOW_BROWSER_FIXTURE"];
        const BROWSER_CDP: &[&str] = &["COMPTROL_ALLOW_BROWSER_CDP"];
        const ADAPTERS: &[&str] = &["COMPTROL_ALLOW_ADAPTERS"];
        // `COMPTROL_ALLOW_ADAPTERS` is a superset that already unlocks every
        // creative adapter intent, so the creative switch is the narrow way
        // in, not the only one. Listing both keeps the declaration honest
        // about what a minimal configuration needs.
        const CREATIVE: &[&str] = &[
            "COMPTROL_ALLOW_CREATIVE_ADAPTERS",
            "COMPTROL_ALLOW_ADAPTERS",
        ];
        const HIGH_CONSEQUENCE: &[&str] = &[
            "COMPTROL_ALLOW_ADAPTERS",
            "COMPTROL_ALLOW_HIGH_CONSEQUENCE_ADAPTERS",
        ];
        const MAIL_SEND: &[&str] = &["COMPTROL_ALLOW_ADAPTERS", "COMPTROL_ALLOW_MAIL_SEND"];
        // Anything the default policy already allows needs no switch, so it
        // reports NONE. Checking the real default allowlist rather than a
        // hand-copied list is what keeps this table honest.
        if Policy::default().allowed_intents.contains(intent) {
            return NONE;
        }
        match intent {
            "recipe.run" => NONE,
            "filesystem.write" | "filesystem.copy" | "filesystem.restore_checkpoint" => SANDBOX,
            "desktop.notify" => NOTIFY,
            "macos.ax.press" | "macos.ax.set_value" => MACOS_AX,
            "desktop.open_app" | "app.open_resource" | "app.focus" => APP_LAUNCH,
            "app.close" => APP_CLOSE,
            "software.search" | "software.describe" => SOFTWARE,
            "software.install" | "software.update" | "software.uninstall" => SOFTWARE_INSTALL,
            "settings.get" | "settings.set" | "settings.write" | "permission.request" => SETTINGS,
            "popup.dismiss" => POPUP,
            "command.run" | "desktop.terminal" => COMMANDS,
            "desktop.explorer" => DESKTOP_EXPLORER,
            "windows.uia.press" | "windows.uia.set_value" | "windows.uia.inspect" => WINDOWS_UIA,
            "linux.atspi.press" | "linux.atspi.set_value" => LINUX_ATSPI,
            "browser.fixture.submit" => BROWSER_FIXTURE,
            "browser.chrome.restore_recent" | "browser.chrome.reopen_closed_group" => MACOS_AX,
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
            | "browser.cdp.activate_tab"
            | "browser.cdp.history_back"
            | "browser.cdp.history_forward"
            | "browser.cdp.semantic_click"
            | "browser.cdp.semantic_fill"
            | "browser.cdp.compact_snapshot"
            | "browser.cdp.workflow"
            | "browser.cdp.screenshot"
            | "browser.cdp.coordinate_click"
            | "browser.cdp.type_text"
            | "browser.cdp.press_key"
            | "browser.cdp.dialog"
            | "browser.ensure_session"
            | "browser.session.connect" => BROWSER_CDP,
            "obs.recording.start"
            | "obs.recording.stop"
            | "discord.message.delete"
            | "message.send" => HIGH_CONSEQUENCE,
            "mail.send" => MAIL_SEND,
            intent if is_first_party_adapter_intent(intent) => {
                if classify(intent) > Risk::R2 {
                    HIGH_CONSEQUENCE
                } else if is_creative_adapter_intent(intent) {
                    CREATIVE
                } else {
                    ADAPTERS
                }
            }
            _ => NONE,
        }
    }
}

impl Policy {
    pub fn from_environment() -> Self {
        Self::from_gates(EnvGates::from_environment())
    }

    /// Pure function of the gate set, so a test can ask "if an operator
    /// sets exactly this switch, which intents become reachable?" without
    /// mutating the test process environment.
    pub fn from_gates(gates: EnvGates) -> Self {
        let mut policy = Self::default();
        if gates.sandbox_writes {
            policy.allow_sandbox_writes = true;
            policy.max_risk = policy.max_risk.max(Risk::R1);
            policy.allowed_intents.insert("filesystem.write".to_owned());
            policy.allowed_intents.insert("filesystem.copy".to_owned());
            policy
                .allowed_intents
                .insert("filesystem.restore_checkpoint".to_owned());
        }
        if gates.desktop_notify {
            policy.allow_desktop_notify = true;
            policy.max_risk = policy.max_risk.max(Risk::R1);
            policy.allowed_intents.insert("desktop.notify".to_owned());
        }
        if gates.macos_ax {
            policy.max_risk = policy.max_risk.max(Risk::R2);
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
        if gates.app_launch {
            policy.allow_app_launch = true;
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy.allowed_intents.insert("desktop.open_app".to_owned());
            policy.allowed_intents.insert("app.launch".to_owned());
            policy.allowed_intents.insert("app.resolve".to_owned());
            policy.allowed_intents.insert("app.list".to_owned());
            policy
                .allowed_intents
                .insert("app.open_resource".to_owned());
            policy.allowed_intents.insert("app.focus".to_owned());
        }
        if gates.app_close {
            policy.max_risk = policy.max_risk.max(Risk::R3);
            policy.allowed_intents.insert("app.close".to_owned());
        }
        if gates.software {
            policy.max_risk = policy.max_risk.max(Risk::R1);
            policy.allowed_intents.insert("software.search".to_owned());
            policy
                .allowed_intents
                .insert("software.describe".to_owned());
        }
        if gates.software_install {
            policy.max_risk = policy.max_risk.max(Risk::R3);
            policy.allowed_intents.insert("software.install".to_owned());
            policy.allowed_intents.insert("software.update".to_owned());
            policy
                .allowed_intents
                .insert("software.uninstall".to_owned());
        }
        if gates.settings {
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
        if gates.popup {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy.allowed_intents.insert("popup.inspect".to_owned());
            policy.allowed_intents.insert("popup.dismiss".to_owned());
        }
        if gates.commands {
            policy.max_risk = policy.max_risk.max(Risk::R3);
            policy.allowed_intents.insert("command.run".to_owned());
            // The terminal route runs the same allowlisted executables, so
            // it inherits the same gate rather than inventing a looser one.
            policy.allowed_intents.insert("desktop.terminal".to_owned());
        }
        if gates.desktop_explorer {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy.allowed_intents.insert("desktop.explorer".to_owned());
        }
        if gates.windows_uia {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy
                .allowed_intents
                .insert("windows.uia.press".to_owned());
            policy
                .allowed_intents
                .insert("windows.uia.set_value".to_owned());
            policy
                .allowed_intents
                .insert("windows.uia.inspect".to_owned());
        }
        if gates.linux_atspi {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy
                .allowed_intents
                .insert("linux.atspi.press".to_owned());
            policy
                .allowed_intents
                .insert("linux.atspi.set_value".to_owned());
        }
        if gates.browser_fixture {
            policy.max_risk = policy.max_risk.max(Risk::R1);
            policy
                .allowed_intents
                .insert("browser.fixture.submit".to_owned());
        }
        if gates.all_intents {
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
                "workflow.speculate".to_owned(),
                "recipe.run".to_owned(),
                "app.resolve".to_owned(),
                "app.list".to_owned(),
                "permission.status".to_owned(),
                "popup.inspect".to_owned(),
                "browser.session.list".to_owned(),
                "browser.session.connect".to_owned(),
                "browser.ensure_session".to_owned(),
            ]);
        }
        if gates.browser_cdp {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            policy.allowed_intents.extend([
                // These operations only inspect browser targets or expose
                // their accessible names. Keep them available with CDP
                // inspection enabled, without opening the mutation routes.
                // ensure_session is a read-only readiness probe: route
                // resolution plus one bounded channel round trip.
                "browser.ensure_session".to_owned(),
                "browser.cdp.discovery".to_owned(),
                "browser.cdp.accessibility_snapshot".to_owned(),
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
                "browser.cdp.activate_tab".to_owned(),
                "browser.cdp.history_back".to_owned(),
                "browser.cdp.history_forward".to_owned(),
                "browser.cdp.semantic_click".to_owned(),
                "browser.cdp.semantic_fill".to_owned(),
                "browser.cdp.compact_snapshot".to_owned(),
                "browser.cdp.workflow".to_owned(),
                "browser.cdp.screenshot".to_owned(),
                "browser.cdp.coordinate_click".to_owned(),
                "browser.cdp.type_text".to_owned(),
                "browser.cdp.press_key".to_owned(),
                "browser.cdp.dialog".to_owned(),
                "browser.session.connect".to_owned(),
            ]);
        }
        if gates.adapters {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            for intent in FIRST_PARTY_ADAPTER_INTENTS {
                if classify(intent) <= Risk::R2 {
                    policy.allowed_intents.insert((*intent).to_owned());
                }
            }
            if gates.high_consequence_adapters {
                policy.max_risk = Risk::R3;
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
        if gates.mail_send && gates.adapters {
            policy.max_risk = Risk::R3;
            policy.allowed_intents.insert("mail.send".to_owned());
        }
        if gates.creative_adapters {
            policy.max_risk = policy.max_risk.max(Risk::R2);
            for intent in FIRST_PARTY_ADAPTER_INTENTS {
                if is_creative_adapter_intent(intent) && classify(intent) <= Risk::R2 {
                    policy.allowed_intents.insert((*intent).to_owned());
                }
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
    #[serde(default)]
    pub request_fingerprint: Option<String>,
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
            request_fingerprint: Some(request_fingerprint(request)),
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
            request_fingerprint: Some(request_fingerprint(request)),
            intent: request.intent.clone(),
            risk: effective_risk(request),
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
    state_dir: PathBuf,
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
    /// Persistent consent store. A load failure blocks consent-gated actions.
    pub consent: Option<comptrol_consent::ConsentStore>,
    consent_error: Option<String>,
    /// Human action broker tracking paused operations awaiting user
    /// approval (UAC, polkit, TCC, browser consent...).
    pub human_actions: comptrol_consent::HumanActionBroker,
    operation_cancel: Option<Arc<AtomicBool>>,
    adapter_hosts: HashMap<String, AdapterHost>,
    idempotent: HashMap<String, ActionResult>,
    idempotency_fingerprints: HashMap<String, String>,
    idempotency_conflicts: HashSet<String>,
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
                .map_err(|error| {
                    io::Error::other(format!("route latency samples read: {error}"))
                })?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                })
                .map_err(|error| {
                    io::Error::other(format!("route latency samples rows: {error}"))
                })?;
            for row in rows {
                let (route, latency_ms) = row.map_err(|error| {
                    io::Error::other(format!("route latency sample row: {error}"))
                })?;
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
        let mut idempotency_fingerprints = HashMap::new();
        let mut idempotency_conflicts = HashSet::new();
        for record in operations.completed() {
            if let (Some(key), Some(result)) = (&record.idempotency_key, &record.result) {
                let Some(fingerprint) = record.request_fingerprint.as_ref() else {
                    idempotent.remove(key);
                    idempotency_fingerprints.remove(key);
                    idempotency_conflicts.insert(key.clone());
                    continue;
                };
                if idempotency_conflicts.contains(key) {
                    continue;
                }
                if idempotency_fingerprints
                    .get(key)
                    .is_some_and(|existing| existing != fingerprint)
                {
                    idempotent.remove(key);
                    idempotency_fingerprints.remove(key);
                    idempotency_conflicts.insert(key.clone());
                } else {
                    idempotent.insert(key.clone(), result.clone());
                    idempotency_fingerprints.insert(key.clone(), fingerprint.clone());
                }
            }
        }
        let (consent, consent_error) =
            match comptrol_consent::ConsentStore::open(state_dir.join("consent.jsonl")) {
                Ok(store) => (Some(store), None),
                Err(error) => (None, Some(error.to_string())),
            };
        Ok(Self {
            state_dir: state_dir.clone(),
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
            consent,
            consent_error,
            human_actions: comptrol_consent::HumanActionBroker::with_path(
                state_dir.join("human_actions.json"),
            ),
            operation_cancel: None,
            adapter_hosts: HashMap::new(),
            idempotent,
            idempotency_fingerprints,
            idempotency_conflicts,
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
        let fingerprint = request_fingerprint(&request);
        if let Some(previous) = request
            .idempotency_key
            .as_ref()
            .and_then(|key| self.idempotent.get(key))
        {
            if self
                .idempotency_conflicts
                .contains(request.idempotency_key.as_deref().unwrap_or_default())
                || self
                    .idempotency_fingerprints
                    .get(request.idempotency_key.as_deref().unwrap_or_default())
                    != Some(&fingerprint)
            {
                return idempotency_conflict(&request, operation_id);
            }
            let mut replay = previous.clone();
            replay.recovery = RecoveryState::IdempotentReplay;
            return replay;
        }
        if let Some(key) = request.idempotency_key.as_deref()
            && (self.idempotency_conflicts.contains(key)
                || self.operations.records.values().any(|record| {
                    record.idempotency_key.as_deref() == Some(key)
                        && record.request_fingerprint.as_deref() != Some(&fingerprint)
                }))
        {
            return idempotency_conflict(&request, operation_id);
        }
        let risk = effective_risk(&request);
        // Asking for a browser operation is what starts the local browser;
        // starting one also turns on the CDP policy gate that exists to
        // serve it, so the cached policy snapshot is refreshed before route
        // selection and authorization see the request.
        if chrome_autostart::intent_needs_browser(&request.intent) {
            let gate_was_open = env_enabled("COMPTROL_ALLOW_BROWSER_CDP");
            if chrome_autostart::ensure() && !gate_was_open {
                self.policy = Policy::from_environment();
            }
        }
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
        // Schema validation runs after the policy gate on purpose: an
        // intent the local policy forbids is refused as `policy_denied`
        // without first describing what parameters it would have accepted.
        if let Err(message) = intent_schema::validate_params(&request.intent, &request.params) {
            let result = ActionResult::refused(
                &request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message,
                    recovery: intent_schema::schema_for(&request.intent).map(|schema| {
                        format!("Use the intent_schema tool for {}", schema["intent"])
                    }),
                },
            );
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
        if risk.mutation() && consent_gate_applies(&request.intent) {
            let store_unavailable = self.consent.is_none();
            let allowed = self.consent.as_ref().is_some_and(|store| {
                matches!(
                    store.authorize(
                        &request.intent,
                        &request.intent,
                        to_consent_risk(risk),
                        consent_resource.as_deref(),
                    ),
                    comptrol_consent::ConsentDecision::Allowed { .. }
                )
            });
            if !allowed {
                let result = ActionResult::refused(
                    &request,
                    operation_id,
                    ComptrolError {
                        code: if store_unavailable {
                            "consent_store_unavailable".to_owned()
                        } else {
                            "consent_required".to_owned()
                        },
                        message: if store_unavailable {
                            "Persistent consent records could not be verified".to_owned()
                        } else {
                            format!("No active consent grant covers {}", request.intent)
                        },
                        recovery: Some(if store_unavailable {
                            "Repair or recreate the local consent store before retrying this action"
                                .to_owned()
                        } else {
                            "Run local setup to grant this capability, or ask the user to approve it".to_owned()
                        }),
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
        let mut result = match request.intent.as_str() {
            "system.ping" => success(
                &request,
                operation_id,
                "native",
                EffectState::None,
                VerificationState::Verified,
                json!({
                    "ready": true,
                    "protocol": PROTOCOL_VERSION,
                    "runtime_fingerprint": runtime_fingerprint()
                }),
            ),
            "workflow.execute" => execute_workflow_request(self, &request, operation_id),
            "workflow.speculate" => execute_speculate_request(self, &request, operation_id),
            "recipe.run" => execute_recipe_request(self, &request, operation_id),
            "desktop.terminal" => terminal::terminal_run(&request, operation_id),
            "desktop.explorer" => terminal::explorer_open(&request, operation_id),
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
            "browser.session.connect" => {
                browser_session_connect(&request, operation_id, &self.state_dir)
            }
            "browser.ensure_session" => {
                browser_ensure_session(&request, operation_id, &self.state_dir)
            }
            "browser.chrome.open_tab" => browser_chrome_open_tab(&request, operation_id),
            "browser.chrome.restore_recent" | "browser.chrome.reopen_closed_group" => {
                browser_chrome_restore_recent(&request, operation_id)
            }
            "command.run" => command_run(&request, operation_id),
            "windows.uia.inspect" | "windows.uia.press" | "windows.uia.set_value" => {
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
            | "browser.cdp.activate_tab"
            | "browser.cdp.history_back"
            | "browser.cdp.history_forward"
            | "browser.cdp.semantic_click"
            | "browser.cdp.semantic_fill"
            | "browser.cdp.type_text"
            | "browser.cdp.press_key"
            | "browser.cdp.compact_snapshot"
            | "browser.cdp.workflow"
            | "browser.cdp.screenshot"
            | "browser.cdp.coordinate_click"
            | "browser.cdp.dialog"
            | "browser.cdp.discovery"
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
        let route_execution_ms = route_started.elapsed().as_secs_f64() * 1_000.0;
        if let Some(data) = result.data.as_object_mut() {
            data.insert(
                "_comptrol_timing".to_owned(),
                json!({"action_and_verification_ms":route_execution_ms}),
            );
        }
        self.record_route_outcome(&result, route_execution_ms);
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
                    None => json!({ "available": false, "reason": self.consent_error, "mutations_blocked": true }),
                },
                "human_actions": {
                    "pending": self.human_actions.all().iter().filter(|action| action.resolution.is_none()).count(),
                    "total": self.human_actions.all().len(),
                    "requests": self.human_actions.all(),
                },
            }),
            "capabilities" => capability_catalog(),
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
                if let Some(endpoint) = browser::active_endpoint() {
                    match browser::discover(&endpoint) {
                        Ok(targets) => {
                            json!({ "endpoint": endpoint, "targets": targets, "transport": "direct_cdp" })
                        }
                        Err(error) => {
                            json!({ "endpoint": endpoint, "error": error, "transport": "direct_cdp" })
                        }
                    }
                } else {
                    let health = browser_bridge::BridgeStore::open(&default_state_dir())
                        .and_then(|store| store.health(browser_bridge::DEFAULT_HEALTH_MAX_AGE));
                    match health {
                        Ok(health) if health.active => {
                            match browser::discover(browser_bridge::COMPANION_BRIDGE_ENDPOINT) {
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
                            }
                        }
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
            }
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
                let fingerprint = record.request_fingerprint.clone();
                if let Err(error) = self.operations.reconciled(record, result.clone()) {
                    return json!({ "state": "unknown", "operation_id": operation_id, "error": { "code": "recovery_write_failed", "message": error.to_string() } });
                }
                self.cache_idempotency(idempotency_key, fingerprint, result.clone());
                return json!({ "state": "reconciled", "result": result });
            }
        }
        if record.intent == "browser.fixture.submit"
            && let (Some(endpoint), Some(key)) = (
                browser::active_endpoint(),
                record.idempotency_key.as_deref(),
            )
        {
            match browser::fixture_state(&endpoint) {
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
                    let fingerprint = record.request_fingerprint.clone();
                    if let Err(error) = self.operations.reconciled(record, result.clone()) {
                        return json!({ "state": "unknown", "operation_id": operation_id, "error": { "code": "recovery_write_failed", "message": error.to_string() } });
                    }
                    self.cache_idempotency(idempotency_key, fingerprint, result.clone());
                    return json!({ "state": "reconciled", "result": result });
                }
                Ok(_) => {}
                Err(error) => {
                    return json!({ "state": "unknown", "operation_id": operation_id, "error": error });
                }
            }
        }
        // F03 closure: a dispatched/unknown browser workflow reconciles by
        // RE-OBSERVING the goal postcondition on the live page. The original
        // actor readback died with the caller, so the reconciled envelope is
        // a fresh independent observation (delivery/effect reflect the
        // observed page, never the vanished execution's self-report).
        if record.intent == "browser.cdp.workflow"
            && let (Some(goal), Some(target_id), Some(context_id)) = (
                record.metadata.get("reconcile_goal"),
                record
                    .metadata
                    .get("reconcile_target_id")
                    .and_then(Value::as_str),
                record
                    .metadata
                    .get("reconcile_browser_context_id")
                    .and_then(Value::as_str),
            )
        {
            let endpoint = browser::active_endpoint()
                .unwrap_or_else(|| browser_bridge::COMPANION_BRIDGE_ENDPOINT.to_owned());
            let expression = match goal.get("kind").and_then(Value::as_str) {
                Some("text") => goal
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(|text| serde_json::to_string(text).ok())
                    .map(|needle| {
                        // Same hidden-tab rule as wait_for_text: innerText is
                        // layout-dependent and empty for background tabs.
                        format!(
                            "(() => {{ const body = document.hidden ? (document.body?.textContent || '') : (document.body?.innerText || document.body?.textContent || ''); return body.includes({needle}); }})()"
                        )
                    }),
                Some("url") => goal
                    .get("contains")
                    .and_then(Value::as_str)
                    .map(|needle| {
                        format!(
                            "location.href.includes({})",
                            serde_json::to_string(needle).unwrap_or_else(|_| "''".to_owned())
                        )
                    }),
                _ => None,
            };
            let observation = match expression {
                Some(expression) => browser::cdp_call(
                    &endpoint,
                    target_id,
                    Some(context_id),
                    None,
                    "Runtime.evaluate",
                    json!({ "expression": expression, "returnByValue": true }),
                ),
                None => Err(ComptrolError {
                    code: "reconcile_unsupported".to_owned(),
                    message: "Workflow record has no observable final goal".to_owned(),
                    recovery: Some(
                        "Observe the page directly and resolve the operation manually".to_owned(),
                    ),
                }),
            };
            let met = observation
                .as_ref()
                .ok()
                .and_then(|data| {
                    data.get("result")
                        .and_then(|result| result.get("value"))
                        .cloned()
                })
                .map(|value| value == Value::Bool(true))
                .unwrap_or(false);
            if met {
                let result = ActionResult {
                    operation_id: record.operation_id.clone(),
                    intent: record.intent.clone(),
                    route: "recovery_observation".to_owned(),
                    target: record.target.clone(),
                    preflight: "reconciled".to_owned(),
                    delivery: DeliveryState::Delivered,
                    effect: EffectState::Changed,
                    verification: VerificationState::Verified,
                    disturbance: json!({ "foreground_changed": false, "mouse": "untouched", "clipboard": "untouched" }),
                    recovery: RecoveryState::None,
                    data: json!({
                        "reconciled": true,
                        "goal": goal,
                        "observed_on": target_id,
                        "basis": "independent_readback_after_recovery",
                    }),
                    error: None,
                };
                let idempotency_key = record.idempotency_key.clone();
                let fingerprint = record.request_fingerprint.clone();
                if let Err(error) = self.operations.reconciled(record, result.clone()) {
                    return json!({ "state": "unknown", "operation_id": operation_id, "error": { "code": "recovery_write_failed", "message": error.to_string() } });
                }
                self.cache_idempotency(idempotency_key, fingerprint, result.clone());
                return json!({ "state": "reconciled", "result": result });
            }
            // Goal not yet met (or target gone): stay honestly unknown. The
            // late command may still land; repeated reconcile is safe because
            // this branch is read-only.
            let error = observation.err();
            return json!({
                "state": "unknown",
                "operation_id": operation_id,
                "reconcile_required": true,
                "error": error.map(|err| json!({ "code": err.code, "message": err.message })),
                "note": "goal not observed yet; operation remains pending reconciliation",
            });
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
                let fingerprint = record.request_fingerprint.clone();
                if let Err(error) = self.operations.reconciled(record, result.clone()) {
                    return json!({ "state": "unknown", "operation_id": operation_id, "error": { "code": "recovery_write_failed", "message": error.to_string() } });
                }
                self.cache_idempotency(idempotency_key, fingerprint, result.clone());
                return json!({ "state": "reconciled", "result": result });
            }
        }
        json!({ "state": "unknown", "operation_id": operation_id, "error": "operation_unknown", "reconcile_required": true })
    }

    fn next_operation_id(&mut self) -> String {
        self.sequence += 1;
        format!("op-{}-{}", now_ms(), self.sequence)
    }

    fn cache_idempotency(
        &mut self,
        key: Option<String>,
        fingerprint: Option<String>,
        result: ActionResult,
    ) {
        let Some(key) = key else { return };
        let Some(fingerprint) = fingerprint else {
            self.idempotent.remove(&key);
            self.idempotency_fingerprints.remove(&key);
            self.idempotency_conflicts.insert(key);
            return;
        };
        if self
            .idempotency_fingerprints
            .get(&key)
            .is_some_and(|existing| existing != &fingerprint)
        {
            self.idempotent.remove(&key);
            self.idempotency_fingerprints.remove(&key);
            self.idempotency_conflicts.insert(key);
            return;
        }
        if !self.idempotency_conflicts.contains(&key) {
            self.idempotency_fingerprints
                .insert(key.clone(), fingerprint);
            self.idempotent.insert(key, result);
        }
    }

    fn remember(&mut self, request: &OperationRequest, result: ActionResult) {
        if let Some(key) = request.idempotency_key.as_ref()
            && !matches!(&result.delivery, DeliveryState::Unknown)
            && !matches!(&result.recovery, RecoveryState::RequiresReconciliation)
        {
            self.cache_idempotency(
                Some(key.clone()),
                Some(request_fingerprint(request)),
                result.clone(),
            );
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
        // P4.6: trace spans via MCP. The operation_id doubles as the trace_id;
        // `inspect kind:events` (kind: "trace.span") shows per-step spans and
        // the benchmark harness can attribute latency per step from them.
        self.events.emit(
            "trace.span",
            json!({
                "trace_id": result.operation_id,
                "span": "operate",
                "intent": result.intent,
                "route": result.route,
                "verification": result.verification,
                "delivery": result.delivery,
                "effect": result.effect,
            }),
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

/// Whether an executable is in the local command allowlist.
///
/// Both command routes (`command.run` and `desktop.terminal`) go through
/// this one check. When they each had their own copy, the terminal route
/// shipped without one, which would have made it a way to run any
/// executable the policy switch had been opened for.
pub fn command_program_allowed(program: &str) -> bool {
    match std::env::var("COMPTROL_COMMAND_ALLOWLIST") {
        Ok(allowlist) => program_in_allowlist(program, &allowlist),
        Err(_) => false,
    }
}

/// The allowlist rule itself, separated from the environment so it can be
/// exercised without mutating process state.
pub fn program_in_allowlist(program: &str, allowlist: &str) -> bool {
    allowlist
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|allowed| allowed == program)
}

pub fn classify(intent: &str) -> Risk {
    match intent {
        "system.ping" | "desktop.observe" | "platform.broker.observe" | "workflow.execute" => {
            Risk::R0
        }
        // The batch wrapper only orchestrates; every step re-enters operate()
        // and is classified, consent-gated, and policy-checked on its own.
        "workflow.speculate" | "recipe.run" => Risk::R1,
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
        "desktop.terminal" | "desktop.explorer" => Risk::R2,
        "windows.uia.inspect" => Risk::R0,
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
        | "browser.cdp.semantic_fill"
        | "browser.cdp.coordinate_click"
        | "browser.cdp.type_text"
        | "browser.cdp.press_key" => Risk::R2,
        "browser.cdp.screenshot" | "browser.cdp.compact_snapshot" => Risk::R0,
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
        "browser.cdp.discovery"
        | "browser.cdp.wait_for"
        | "browser.cdp.accessibility_snapshot"
        | "browser.cdp.reopen_closed_group" => Risk::R0,
        _ => Risk::R2,
    }
}

fn effective_risk(request: &OperationRequest) -> Risk {
    classify(&request.intent).max(request.risk.unwrap_or(Risk::R0))
}

fn request_fingerprint(request: &OperationRequest) -> String {
    #[derive(Serialize)]
    struct CanonicalRequest<'a> {
        intent: &'a str,
        target: &'a Option<Target>,
        params: &'a Value,
        postcondition: &'a Option<Value>,
        risk: Risk,
        dry_run: bool,
        background: &'a Option<String>,
    }

    use sha2::{Digest, Sha256};
    let request = CanonicalRequest {
        intent: &request.intent,
        target: &request.target,
        params: &request.params,
        postcondition: &request.postcondition,
        risk: effective_risk(request),
        dry_run: request.dry_run,
        background: &request.background,
    };
    let bytes = serde_json::to_vec(&request).expect("operation request is serializable");
    format!("v1:{:x}", Sha256::digest(bytes))
}

fn idempotency_conflict(request: &OperationRequest, operation_id: String) -> ActionResult {
    ActionResult::refused(
        request,
        operation_id,
        ComptrolError {
            code: "idempotency_conflict".to_owned(),
            message: "This idempotency key is already bound to a different or unverifiable request"
                .to_owned(),
            recovery: Some("Use a new idempotency key for a changed request".to_owned()),
        },
    )
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
    let root = adapter_root();
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
    // The adapter's manifest declares how long one operation may run, and the
    // host deadline must never be shorter than that. The old flat five-second
    // default killed work the adapter was built to finish -- a Blender rocket
    // recipe legitimately runs for about ten seconds -- and then left the
    // adapter route dead. A caller may still ask for a tighter bound.
    let declared_budget = manifest.max_operation_ms;
    let host_timeout_ms = request
        .params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(declared_budget)
        .min(declared_budget)
        .max(100);
    // A host whose connection died is replaced rather than reused: its reader
    // thread already consumed the pipe, so every later request would fail
    // against it while the route stayed broken until a restart.
    let reusable = runtime
        .adapter_hosts
        .get(adapter_name)
        .is_some_and(AdapterHost::connection_is_alive);
    let adapter_host_startup_ms = if !reusable {
        runtime.adapter_hosts.remove(adapter_name);
        let adapter_startup = Instant::now();
        let python = adapter_python();
        let script = root.join(adapter_name).join("src").join("adapter.py");
        let config = AdapterHostConfig {
            manifest,
            executable: python,
            arguments: vec![script.to_string_lossy().into_owned()],
            instance_id: format!("{adapter_name}-{operation_id}"),
            max_frame_bytes: comptrol_adapter_sdk::MAX_FRAME_BYTES,
            timeout_ms: host_timeout_ms,
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
        adapter_startup.elapsed().as_secs_f64() * 1000.0
    } else {
        0.0
    };
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
    let adapter_execution_started = Instant::now();
    let response = if let Some(cancellation) = runtime.operation_cancel.as_ref() {
        host.request_with_cancel("execute", &resource, payload, Some(token), || {
            cancellation.load(Ordering::Acquire)
        })
    } else {
        host.request("execute", &resource, payload, Some(token))
    };
    let adapter_execution_ms = adapter_execution_started.elapsed().as_secs_f64() * 1000.0;
    // Any error, or a connection that did not survive the exchange, means the
    // adapter process is no longer trustworthy: drop it so the next operation
    // starts a fresh one. Retrying against a half-read pipe is what made one
    // timeout disable an adapter until the daemon restarted.
    let connection_survived = runtime
        .adapter_hosts
        .get(adapter_name)
        .is_some_and(AdapterHost::connection_is_alive);
    if response.is_err() || !connection_survived {
        runtime.adapter_hosts.remove(adapter_name);
    }
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
                json!({"adapter": adapter_name, "health": response.health, "payload": response.payload, "verified": verified, "isolation": isolation_dimensions.clone(), "timings_ms":{"adapter_host_startup_ms":adapter_host_startup_ms,"adapter_execution_and_verification_ms":adapter_execution_ms}}),
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
            data: json!({"adapter": adapter_name, "health": response.health, "payload": response.payload, "isolation": isolation_dimensions, "timings_ms":{"adapter_host_startup_ms":adapter_host_startup_ms,"adapter_execution_and_verification_ms":adapter_execution_ms}}),
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
    if request.intent == "browser.cdp.workflow" {
        // Recovery evidence: enough to re-observe the goal postcondition on
        // the live page after a crash, without replaying any step.
        if let Some(target_id) = request.params.get("target_id").and_then(Value::as_str) {
            metadata["reconcile_target_id"] = json!(target_id);
        }
        if let Some(context) = request
            .params
            .get("browser_context_id")
            .and_then(Value::as_str)
        {
            metadata["reconcile_browser_context_id"] = json!(context);
        }
        if let Some(steps) = request.params.get("steps").and_then(Value::as_array) {
            let final_goal = steps.iter().rev().find_map(|step| {
                let action = step.get("action").and_then(Value::as_str)?;
                match action {
                    "wait_text" => Some(json!({
                        "kind": "text",
                        "text": step.get("text").cloned()?,
                    })),
                    "wait_url" => Some(json!({
                        "kind": "url",
                        "contains": step.get("contains").cloned()?,
                    })),
                    "navigate" => Some(json!({
                        "kind": "url",
                        "contains": step
                            .get("url_contains")
                            .cloned()
                            .or_else(|| step.get("url").cloned())?,
                    })),
                    _ => None,
                }
            });
            if let Some(goal) = final_goal {
                metadata["reconcile_goal"] = goal;
            }
        }
        if let Some(goal) = request.postcondition.as_ref() {
            metadata["reconcile_postcondition"] = goal.clone();
        }
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
        "browser.ensure_session",
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
        "recipe.run" => Some((
            "recipe",
            true,
            "Promoted, replay-gated recipes re-enter the normal dispatch path per step; each step keeps its own policy and consent gate",
        )),
        "workflow.speculate" => Some((
            "workflow",
            true,
            "Bounded speculative batch route with per-step postconditions",
        )),
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
        "browser.ensure_session" => Some((
            "browser_session_broker",
            true,
            "Read-only readiness probe with a bounded channel round trip",
        )),
        "browser.chrome.open_tab" => Some((
            "browser_launcher",
            cfg!(any(
                target_os = "windows",
                target_os = "macos",
                target_os = "linux"
            )),
            "Opens a URL in the existing default browser profile; page verification needs a local browser connection",
        )),
        "browser.chrome.restore_recent" | "browser.chrome.reopen_closed_group" => Some((
            "chrome_restore",
            true,
            "Chrome restore uses the native recently-closed surface when reachable and otherwise reconstructs only when explicitly allowed",
        )),
        "desktop.terminal" => Some((
            "terminal",
            env_enabled("COMPTROL_ALLOW_COMMANDS")
                && std::env::var_os("COMPTROL_COMMAND_ROOT").is_some()
                && std::env::var_os("COMPTROL_COMMAND_ALLOWLIST").is_some(),
            "Terminal commands require the same allowlist policy and command root as command.run; no shell is involved",
        )),
        "desktop.explorer" => Some((
            "file_explorer",
            cfg!(any(
                target_os = "windows",
                target_os = "macos",
                target_os = "linux"
            )),
            "Revealing a path uses the platform file manager and never mutates the file",
        )),
        "command.run" => Some((
            "process_argv",
            env_enabled("COMPTROL_ALLOW_COMMANDS")
                && std::env::var_os("COMPTROL_COMMAND_ROOT").is_some(),
            "Command execution requires an allowlist policy and command root",
        )),
        "windows.uia.inspect" | "windows.uia.press" | "windows.uia.set_value" => Some((
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
            (env_enabled("COMPTROL_ALLOW_ADAPTERS")
                || (env_enabled("COMPTROL_ALLOW_CREATIVE_ADAPTERS")
                    && is_creative_adapter_intent(value)))
                && adapter_root().is_dir(),
            "First party application adapters require an explicit policy and a shipped adapter bundle",
        )),
        "browser.cdp.frame_evaluate" => Some((
            "browser_protocol",
            browser::active_endpoint().is_some() && env_enabled("COMPTROL_ALLOW_BROWSER_CDP"),
            "Frame-scoped CDP requires the direct event-maintained frame graph; the companion bridge intentionally refuses this route until it can prove a stable frame binding",
        )),
        value if value.starts_with("browser.cdp.") => Some((
            "browser_protocol",
            (browser::active_endpoint().is_some() || browser_bridge::bridge_is_active())
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

pub fn route_catalog() -> Vec<RoutePlan> {
    [
        "system.ping",
        "desktop.observe",
        "platform.broker.observe",
        "workflow.execute",
        "workflow.speculate",
        "recipe.run",
        "desktop.terminal",
        "desktop.explorer",
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
        "browser.ensure_session",
        "browser.chrome.open_tab",
        "browser.chrome.restore_recent",
        "browser.chrome.reopen_closed_group",
        "command.run",
        "windows.uia.inspect",
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
        "browser.cdp.semantic_fill",
        "browser.cdp.compact_snapshot",
        "browser.cdp.screenshot",
        "browser.cdp.coordinate_click",
        "browser.cdp.type_text",
        "browser.cdp.press_key",
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

/// P4.4: run one promoted recipe as a single operation.
///
/// A recipe is a stored, parameter-lifted workflow. Running it here means
/// the caller makes one `operate` call instead of reconstructing the task
/// step by step. Every step still re-enters `operate`, so each one is
/// classified, policy-checked, and consent-gated on its own: a recipe is
/// a shorter conversation, not a wider door.
fn execute_recipe_request(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    let Some(recipe_id) = request.params.get("recipe").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "recipe.run needs params.recipe with an exact recipe id".to_owned(),
                recovery: Some("List recipes with the recipe_list tool".to_owned()),
            },
        );
    };
    let Some(recipe) = comptrol_workflow::recipes::catalog()
        .into_iter()
        .find(|recipe| recipe.workflow.id == recipe_id)
    else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "unknown_recipe".to_owned(),
                message: format!("No recipe named {recipe_id}"),
                recovery: Some("List recipes with the recipe_list tool".to_owned()),
            },
        );
    };
    // Promotion is earned by recorded, independently verified replays. An
    // unpromoted recipe is refused with its own state rather than being
    // silently run as if it were proven. The bar is one verified replay
    // in a clean fixture with independent verification.
    if !recipe.is_promoted(1) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "recipe_not_promoted".to_owned(),
                message: format!(
                    "Recipe {recipe_id} has {} verified replays and is not promoted",
                    recipe.evidence.verified_runs
                ),
                recovery: Some(
                    "Record the task with recipe_record, then promote it after independent verification"
                        .to_owned(),
                ),
            },
        );
    }
    let supplied = request
        .params
        .get("parameters")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let workflow = match recipe.bind(&supplied) {
        Ok(workflow) => workflow,
        Err(message) => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message,
                    recovery: Some("Inspect the recipe's declared parameters".to_owned()),
                },
            );
        }
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
        max_steps: workflow.nodes.len().saturating_mul(4).saturating_add(4),
    };
    match executor.run(&workflow) {
        Ok(value) => success(
            request,
            operation_id,
            "recipe",
            EffectState::Changed,
            VerificationState::Verified,
            json!({
                "recipe": recipe_id,
                "recipe_fingerprint": workflow.fingerprint,
                "steps": workflow.nodes.len(),
                "result": value
            }),
        ),
        Err(error) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "recipe_execution_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Observe the current state and run the recipe cold".to_owned()),
            },
        ),
    }
}

/// P4.2: `workflow.speculate` - run N steps optimistically with per-step
/// postconditions and rollback hints. Every step re-enters `operate()`, so
/// intent-level classification, consent, and policy apply unchanged. On
/// failure the result carries the durable state of executed steps plus a
/// reconcile plan instead of an opaque error (Gate 3: Classroom in 1 call).
fn execute_speculate_request(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
    let steps_value = match request.params.get("steps") {
        Some(Value::Array(steps)) if !steps.is_empty() => steps.clone(),
        _ => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "workflow.speculate needs a non-empty steps array".to_owned(),
                    recovery: Some(
                        "Each step is {intent, params, postcondition?, rollback_hint?}".to_owned(),
                    ),
                },
            );
        }
    };
    let Ok(steps) = serde_json::from_value::<Vec<SpeculativeStep>>(Value::Array(steps_value))
    else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Speculative steps did not match the closed schema".to_owned(),
                recovery: Some(
                    "Each step is {intent, params, postcondition?, rollback_hint?}".to_owned(),
                ),
            },
        );
    };
    let deadline_ms = request
        .params
        .get("deadline_ms")
        .and_then(Value::as_u64)
        .unwrap_or(30_000)
        .clamp(1_000, 300_000);
    let target = request.target.clone();
    let background = request.background.clone();
    let mut step_results = Vec::new();
    let mut intents = Vec::new();
    let outcome = {
        let step_results = &mut step_results;
        let intents = &mut intents;
        run_speculative(
            &steps,
            deadline_ms,
            |_index, intent: &str, params: &Value| {
                let step_outcome = runtime.operate(OperationRequest {
                    intent: intent.to_owned(),
                    target: target.clone(),
                    params: params.clone(),
                    postcondition: None,
                    risk: None,
                    idempotency_key: None,
                    dry_run: false,
                    background: background.clone(),
                });
                let failed = step_outcome.error.is_some()
                    || step_outcome.verification == VerificationState::Failed;
                let value = serde_json::to_value(&step_outcome).unwrap_or(Value::Null);
                step_results.push(value.clone());
                intents.push(intent.to_owned());
                if failed {
                    let message = step_outcome
                        .error
                        .map(|error| error.message)
                        .unwrap_or_else(|| "step verification failed".to_owned());
                    Err(message)
                } else {
                    Ok(value)
                }
            },
            |criterion: &Value, observed: &Value| match criterion.get("equals") {
                Some(expected) => {
                    observed.get("data") == Some(expected)
                        || observed.get("verification") == Some(expected)
                }
                None => criterion == observed,
            },
            || false,
        )
    };
    let data = json!({
        "batch_state": outcome.state,
        "failed_at": outcome.failed_at,
        "steps": outcome.steps,
        "executed_results": step_results,
        "reconcile_plan": outcome.reconcile_plan,
        "verified_by": if outcome.completed() && steps.iter().all(|step| step.postcondition.is_some()) {
            "postcondition_readback"
        } else {
            "actor_dispatch"
        },
    });
    if outcome.completed() {
        // P4.3: comptrol-verification owns postcondition evaluation and is the
        // only code that may conclude `verified`. Each declared postcondition
        // becomes a required criterion evaluated against its step outcome;
        // dispatch-only evidence can never upgrade the state.
        let mut report = VerificationReport::new(VerificationLevel::ApplicationState);
        for (step, step_outcome) in steps.iter().zip(outcome.steps.iter()) {
            if let Some(criterion) = step.postcondition.as_ref() {
                let passed = step_outcome.postcondition_passed == Some(true);
                report.criteria.push(VerificationCriterion {
                    id: format!("step_{}_{}", step_outcome.index, step_outcome.intent),
                    required: true,
                    expected: criterion.clone(),
                    observed: step_outcome.result.clone(),
                    passed,
                    source: VerificationSource::IndependentFixture,
                });
            }
        }
        if !report.criteria.is_empty() {
            report.evidence.push(VerificationEvidence {
                source: VerificationSource::IndependentFixture,
                kind: "speculative_batch_postconditions".to_owned(),
                reference: Some(operation_id.clone()),
                details: json!({ "steps": outcome.steps.len() }),
            });
        }
        let report = report.finalize();
        let verified = report.is_verified();
        let mut data = data;
        if let Some(object) = data.as_object_mut() {
            object.insert(
                "verification_report".to_owned(),
                serde_json::to_value(&report).unwrap_or(Value::Null),
            );
        }
        return success(
            request,
            operation_id,
            "workflow",
            EffectState::Changed,
            if verified {
                VerificationState::Verified
            } else {
                VerificationState::Unverified
            },
            data,
        );
    }
    ActionResult {
        operation_id,
        intent: request.intent.clone(),
        route: "workflow".to_owned(),
        target: request.target.clone(),
        preflight: "passed".to_owned(),
        delivery: DeliveryState::Delivered,
        effect: EffectState::Unknown,
        verification: VerificationState::Failed,
        disturbance: json!({ "foreground_changed": false }),
        recovery: RecoveryState::RequiresReconciliation,
        data,
        error: Some(ComptrolError {
            code: "speculative_batch_failed".to_owned(),
            message: format!(
                "Speculative batch failed at step {:?}; executed state and reconcile plan are in data",
                outcome.failed_at
            ),
            recovery: Some(
                "Apply data.reconcile_plan newest-first or re-observe and retry the remaining steps"
                    .to_owned(),
            ),
        }),
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
    let Some(endpoint) = browser::active_endpoint() else {
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
        &endpoint,
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

/// The endpoint one browser operation should use.
///
/// A local Chrome is started here, on first use, rather than when the
/// process starts: a session that only touches the desktop, the terminal,
/// or an adapter must never make a browser window appear.
fn browser_cdp_endpoint() -> Option<std::ffi::OsString> {
    if let Some(endpoint) = browser::active_endpoint() {
        return Some(std::ffi::OsString::from(endpoint));
    }
    if browser_bridge::bridge_is_active() {
        return Some(std::ffi::OsString::from(
            browser_bridge::COMPANION_BRIDGE_ENDPOINT,
        ));
    }
    if chrome_autostart::ensure()
        && let Some(endpoint) = browser::active_endpoint()
    {
        return Some(std::ffi::OsString::from(endpoint));
    }
    browser_bridge::bridge_is_active()
        .then(|| std::ffi::OsString::from(browser_bridge::COMPANION_BRIDGE_ENDPOINT))
}

fn browser_cdp_action(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(endpoint) = browser_cdp_endpoint() else {
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
    if request.intent == "browser.cdp.discovery" {
        return match browser::discover(&endpoint.to_string_lossy()) {
            Ok(targets) => {
                let pages = targets
                    .into_iter()
                    .filter(|target| target.target_type.as_deref() == Some("page"))
                    .map(|target| {
                        json!({
                            "id": target.id,
                            "type": target.target_type,
                            "browser_context_id": target.browser_context_id,
                            "url": target.url,
                            "title": target.title,
                            "revision": target.revision,
                        })
                    })
                    .collect::<Vec<_>>();
                success(
                    request,
                    operation_id,
                    "browser_protocol",
                    EffectState::None,
                    VerificationState::Verified,
                    json!({ "targets": pages, "verified": true }),
                )
            }
            Err(error) => browser_failure(request, operation_id, error),
        };
    }
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
    if request.intent == "browser.cdp.semantic_fill" {
        return browser_cdp_semantic_fill(request, operation_id, &endpoint);
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
    let Some(revision) = request
        .params
        .get("revision")
        .and_then(Value::as_str)
        .or_else(|| (request.intent == "browser.cdp.wait_for").then_some(""))
    else {
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
    // Read-only observation re-binds to the live revision before dispatch.
    // An SPA that changes URL between discovery and read (a redirect that
    // lands mid-load, a pushState) otherwise turns every read during the
    // load into stale_reference, which made hands-off flows impossible.
    // Mutations keep the caller-pinned revision so they can never act on a
    // page state other than the one the caller observed.
    let revision: String = if matches!(
        request.intent.as_str(),
        "browser.cdp.compact_snapshot"
            | "browser.cdp.accessibility_snapshot"
            | "browser.cdp.wait_for"
            | "browser.cdp.screenshot"
    ) {
        browser::discover(&endpoint.to_string_lossy())
            .ok()
            .and_then(|targets| {
                targets
                    .iter()
                    .find(|target| {
                        target.id == target_id
                            && target.browser_context_id.as_deref() == Some(browser_context_id)
                    })
                    .and_then(|target| target.revision.clone())
            })
            .unwrap_or_else(|| revision.to_owned())
    } else {
        revision.to_owned()
    };
    let revision: &str = &revision;
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
    if request.intent == "browser.cdp.activate_tab" {
        // Foreground disclosure: activating a tab may bring its window forward,
        // so the result states the disturbance explicitly instead of hiding it.
        let deactivate = request
            .params
            .get("deactivate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !deactivate && request.background.as_deref() == Some("strict_background") {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "background_unavailable".to_owned(),
                    message: "Activating a tab can change which page is visible on screen".to_owned(),
                    recovery: Some(
                        "Use background-safe read and semantic actions, or drop strict_background knowingly".to_owned(),
                    ),
                },
            );
        }
        return match browser::activate_tab(
            &endpoint.to_string_lossy(),
            target_id,
            browser_context_id,
            revision,
            deactivate,
        ) {
            Ok(data) => success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                VerificationState::Verified,
                json!({
                    "activation": data,
                    "disturbance": {
                        "foreground_changed": !deactivate,
                        "disclosed": true,
                        "mouse": "untouched",
                        "clipboard": "untouched"
                    }
                }),
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
    if request.intent == "browser.cdp.compact_snapshot" {
        let limit = request
            .params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(64)
            .clamp(1, 160) as usize;
        return match browser::compact_snapshot(
            &endpoint.to_string_lossy(),
            target_id,
            browser_context_id,
            revision,
            limit,
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
    // Real trusted-input routes: type into the focused element (canvas
    // editors included) and press named keys. The caller places the caret
    // first via semantic_click or coordinate_click.
    if request.intent == "browser.cdp.type_text" {
        let Some(text) = request.params.get("text").and_then(Value::as_str) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "type_text needs a text parameter".to_owned(),
                    recovery: None,
                },
            );
        };
        return match browser::type_text(
            &endpoint.to_string_lossy(),
            target_id,
            browser_context_id,
            Some(revision),
            text,
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
    if request.intent == "browser.cdp.press_key" {
        let Some(key) = request.params.get("key").and_then(Value::as_str) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "press_key needs a key parameter".to_owned(),
                    recovery: Some("Named keys: Enter, Tab, Escape, Backspace, Delete, arrows, Home, End, PageUp, PageDown, or a single character; optional modifiers array (ctrl/alt/shift/meta)".to_owned()),
                },
            );
        };
        let modifiers = request.params.get("modifiers").and_then(Value::as_array);
        return match browser::press_key(
            &endpoint.to_string_lossy(),
            target_id,
            browser_context_id,
            Some(revision),
            key,
            modifiers,
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
                            "final_url": state.get("current_url").cloned().unwrap_or(Value::Null),
                            "reason": "requested state already live on the exact target",
                            "ensure_state": state,
                            "verification": {
                                "level": "surface_state",
                                "source": "browser_target_state",
                                "criterion": "final_url_matches_request",
                                "passed": true
                            },
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
            // Honesty contract: the in-page readback (unique match +
            // actionability + clicked confirmation) proves the DISPATCH
            // happened on the right element, but it is produced by the same
            // evaluate that performed the action. It attests delivery, not the
            // user-visible outcome.
            //
            // C5: when the caller supplies a postcondition ({"text_contains":
            // ...} or {"url_contains": ...}), we verify through a SECOND,
            // independent channel — a fresh target observation (discover) for
            // url_contains, or a separate Runtime.evaluate for text_contains —
            // and only then mark Verified.
            let verified = data.get("verified").and_then(Value::as_bool) == Some(true);
            let postcondition = request.postcondition.as_ref();
            let url_contains = postcondition
                .and_then(|value| value.get("url_contains"))
                .and_then(Value::as_str);
            let text_contains = postcondition
                .and_then(|value| value.get("text_contains"))
                .and_then(Value::as_str);
            let mut independent = json!({
                "requested": postcondition.is_some(),
                "checked": false,
            });
            let mut independently_verified = false;
            if verified && (url_contains.is_some() || text_contains.is_some()) {
                let endpoint_string = endpoint.to_string_lossy().into_owned();
                let deadline = Instant::now()
                    + Duration::from_millis(
                        request
                            .params
                            .get("timeout_ms")
                            .and_then(Value::as_u64)
                            .unwrap_or(8_000)
                            .clamp(100, 30_000),
                    );
                loop {
                    let mut ok = true;
                    if let Some(contains) = url_contains {
                        // Second channel: fresh discovery snapshot, not the
                        // actor's evaluate result.
                        ok = browser::discover(&endpoint_string)
                            .ok()
                            .is_some_and(|targets| {
                                targets.iter().any(|target| {
                                    target.id == *target_id
                                        && target
                                            .url
                                            .as_deref()
                                            .is_some_and(|url| browser::url_matches(url, contains))
                                })
                            });
                    }
                    if text_contains.is_some() {
                        // Second channel: a separate evaluate that only reads.
                        let expression = format!(
                            "document.body && document.body.innerText.includes({})",
                            serde_json::to_string(text_contains.unwrap_or_default())
                                .unwrap_or_else(|_| "''".to_owned())
                        );
                        ok &= browser::cdp_call(
                            &endpoint_string,
                            target_id,
                            Some(browser_context_id),
                            None,
                            "Runtime.evaluate",
                            json!({ "expression": expression, "returnByValue": true }),
                        )
                        .ok()
                        .and_then(|data| {
                            data.get("result")
                                .and_then(|result| result.get("value"))
                                .cloned()
                        })
                        .map(|value| value == Value::Bool(true))
                        .unwrap_or(false);
                    }
                    independently_verified = ok;
                    if ok || Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                independent["checked"] = json!(true);
                independent["met"] = json!(independently_verified);
            }
            success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                if independently_verified {
                    VerificationState::Verified
                } else if postcondition.is_some() {
                    VerificationState::Failed
                } else {
                    VerificationState::Unverified
                },
                json!({
                    "dispatch": data,
                    "postcondition": if independently_verified {
                        "verified"
                    } else if postcondition.is_some() {
                        "failed"
                    } else {
                        "none"
                    },
                    "independent_verification": independent,
                    "verification": if independently_verified {
                        "independent_second_channel_readback"
                    } else if verified {
                        "actor_attested_in_page_readback"
                    } else {
                        "unverified"
                    }
                }),
            )
        }
        Err(error) => browser_failure(request, operation_id, error),
    }
}

fn browser_cdp_semantic_fill(
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
                message: "Semantic browser fills need a target id".to_owned(),
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
                message: "Semantic browser fills need a locator object".to_owned(),
                recovery: Some(
                    "Provide role/name, text, test_id, href_contains, or selector".to_owned(),
                ),
            },
        );
    };
    let Some(value) = request.params.get("value").and_then(Value::as_str) else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Semantic browser fills need a string value".to_owned(),
                recovery: None,
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
    match browser::semantic_fill(
        &endpoint.to_string_lossy(),
        target_id,
        browser_context_id,
        revision,
        locator,
        value,
        timeout,
    ) {
        Ok(data) => {
            // Same honesty contract as semantic_click: the in-page value
            // readback is actor-attested proof of dispatch, not independent
            // outcome verification.
            let verified = data.get("verified").and_then(Value::as_bool) == Some(true);
            success(
                request,
                operation_id,
                "browser_protocol",
                EffectState::Changed,
                VerificationState::Unverified,
                json!({
                    "dispatch": data,
                    "value_length": value.chars().count(),
                    "verification": if verified {
                        "actor_attested_in_page_readback"
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
    Fill {
        locator: Value,
        value: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    WaitUrl {
        contains: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    WaitText {
        text: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
}

fn browser_cdp_workflow(
    request: &OperationRequest,
    operation_id: String,
    endpoint: &std::ffi::OsStr,
) -> ActionResult {
    let Some(steps_value) = request.params.get("steps") else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Browser workflows need a steps array".to_owned(),
                recovery: Some(
                    "Use data-only navigate, click, fill, wait_url, and wait_text steps".to_owned(),
                ),
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
                recovery: Some(
                    "Use data-only navigate, click, fill, wait_url, and wait_text steps".to_owned(),
                ),
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
    let target_resolution = Instant::now();
    let targets = match browser::discover_cached_targets(&endpoint) {
        Ok(targets) => targets,
        Err(error) => return browser_failure(request, operation_id, error),
    };
    let (target, target_was_resolved) = if let Some(target_id) =
        request.params.get("target_id").and_then(Value::as_str)
    {
        let browser_context_id = request
            .params
            .get("browser_context_id")
            .and_then(Value::as_str);
        match crate::bind_browser_target(
            &targets,
            target_id,
            browser_context_id,
            request.params.get("revision").and_then(Value::as_str),
        ) {
            Ok(target) => (target, false),
            Err(error) => return browser_failure(request, operation_id, error),
        }
    } else {
        let Some(url_match) = request
            .params
            .get("target_url_contains")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Browser workflows need target_id or target_url_contains".to_owned(),
                    recovery: Some("Provide an exact id or a unique URL/title matcher".to_owned()),
                },
            );
        };
        let title_match = request
            .params
            .get("target_title_contains")
            .and_then(Value::as_str);
        let context_match = request
            .params
            .get("browser_context_id")
            .and_then(Value::as_str);
        match unique_workflow_target(&targets, url_match, title_match, context_match) {
            Ok(target) => (target, true),
            Err(error) => return browser_failure(request, operation_id, error),
        }
    };
    let target_id = target.id.as_str();
    let Some(browser_context_id) = target.browser_context_id.as_deref() else {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "target_context_missing".to_owned(),
                message: "Selected browser target has no browser context identity".to_owned(),
                recovery: Some("Inspect the live browser target inventory".to_owned()),
            },
        );
    };
    // F07: per-span timings so latency attribution is measured, not guessed.
    let target_resolution_ms = target_resolution.elapsed().as_secs_f64() * 1000.0;
    let mut revision = target.revision;
    let mut completed = Vec::with_capacity(steps.len());
    let mut step_timings = Vec::with_capacity(steps.len());
    let mut postcondition_verified = false;
    let mut observed_change = false;
    for (index, step) in steps.iter().enumerate() {
        let step_started = Instant::now();
        let result = match step {
            BrowserWorkflowStep::Navigate {
                url,
                url_contains,
                timeout_ms,
            } => {
                if let Err(error) = browser::validate_url(url) {
                    return browser_failure(request, operation_id, error);
                }
                let live = browser::ensure_state(
                    &endpoint,
                    target_id,
                    Some(browser_context_id),
                    revision.as_deref(),
                    Some(url),
                    url_contains.as_deref(),
                    None,
                );
                if let Ok(state) = live
                    && state.get("satisfied").and_then(Value::as_bool) == Some(true)
                {
                    postcondition_verified = true;
                    Ok(json!({
                        "action": "navigate",
                        "url": url,
                        "navigated": false,
                        "reason": "requested state already live",
                        "ensure_state": state
                    }))
                } else {
                    match browser::cdp_call(
                        &endpoint,
                        target_id,
                        Some(browser_context_id),
                        revision.as_deref(),
                        "Page.navigate",
                        json!({"url": url}),
                    ) {
                        Ok(data) => {
                            observed_change = true;
                            let expected = url_contains.clone().unwrap_or_else(|| {
                                url.split_once("://")
                                    .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
                                    .unwrap_or(url)
                                    .to_owned()
                            });
                            let observed = match browser_wait_for_url(
                                &endpoint,
                                target_id,
                                browser_context_id,
                                &expected,
                                timeout_ms.unwrap_or(2_000),
                            ) {
                                Ok(observed) => observed,
                                Err(error) => return browser_failure(request, operation_id, error),
                            };
                            postcondition_verified = true;
                            Ok(json!({
                                "action":"navigate",
                                "url": url,
                                "navigated": true,
                                "protocol": data,
                                "postcondition": observed
                            }))
                        }
                        Err(error) => Err(error),
                    }
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
            )
            .inspect(|_| observed_change = true),
            BrowserWorkflowStep::Fill {
                locator,
                value,
                timeout_ms,
            } => browser::semantic_fill(
                &endpoint,
                target_id,
                browser_context_id,
                revision.as_deref(),
                locator,
                value,
                timeout_ms.unwrap_or(1_500).clamp(100, 10_000),
            )
            .inspect(|_| observed_change = true),
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
            .map(|observed| {
                postcondition_verified = true;
                json!({"action":"wait_url", "contains": contains, "observed": observed})
            }),
            BrowserWorkflowStep::WaitText { text, timeout_ms } => browser::wait_for_text(
                &endpoint,
                target_id,
                browser_context_id,
                text,
                Duration::from_millis(timeout_ms.unwrap_or(2_000).clamp(100, 30_000)),
            )
            .map(|observed| {
                postcondition_verified = true;
                json!({"action":"wait_text", "observed": observed})
            }),
        };
        let step_elapsed_ms = step_started.elapsed().as_secs_f64() * 1000.0;
        let data = match result {
            Ok(data) => data,
            Err(error) => return browser_failure(request, operation_id, error),
        };
        // Actor acknowledgement cannot prove the goal, even in a one-step
        // workflow. Require a final independent observation after mutations.
        if !matches!(
            step,
            BrowserWorkflowStep::Navigate { .. }
                | BrowserWorkflowStep::WaitUrl { .. }
                | BrowserWorkflowStep::WaitText { .. }
        ) {
            postcondition_verified = false;
        }
        // Per-step revision refresh through a full bridge discovery; timed
        // separately so its cost is visible in the result envelope.
        let refresh_started = Instant::now();
        let refreshed_revision =
            browser::discover_cached_targets(&endpoint)
                .ok()
                .and_then(|current| {
                    crate::bind_browser_target(&current, target_id, Some(browser_context_id), None)
                        .ok()
                        .map(|bound| bound.revision)
                });
        let revision_refresh_ms = refresh_started.elapsed().as_secs_f64() * 1000.0;
        if let Some(bound) = refreshed_revision {
            revision = bound;
        }
        step_timings.push(json!({
            "index": index,
            "step_ms": step_elapsed_ms,
            "revision_refresh_ms": revision_refresh_ms,
        }));
        completed.push(json!({"index": index, "result": data}));
    }
    success(
        request,
        operation_id,
        "browser_protocol",
        if observed_change {
            EffectState::Changed
        } else {
            EffectState::None
        },
        if postcondition_verified {
            VerificationState::Verified
        } else {
            VerificationState::Unverified
        },
        json!({
            "steps": completed,
            "step_count": completed.len(),
            "step_timings_ms": step_timings,
            "target_resolution_ms": target_resolution_ms,
            "verified": postcondition_verified,
            "target_revision": revision,
            "verification_basis": if !postcondition_verified {
                "no_goal_postcondition"
            } else {
                "observed_workflow_postcondition"
            },
            "target_resolved_internally": target_was_resolved,
            "target_match": if target_was_resolved {
                json!({
                    "url_contains": request.params.get("target_url_contains"),
                    "title_contains": request.params.get("target_title_contains")
                })
            } else {
                Value::Null
            }
        }),
    )
}

fn unique_workflow_target(
    targets: &[BrowserTarget],
    url_match: &str,
    title_match: Option<&str>,
    context_match: Option<&str>,
) -> Result<BrowserTarget, ComptrolError> {
    let candidates = targets
        .iter()
        .filter(|candidate| candidate.target_type.as_deref() == Some("page"))
        .filter(|candidate| {
            candidate
                .url
                .as_deref()
                .is_some_and(|url| browser::url_matches(url, url_match))
        })
        .filter(|candidate| {
            title_match.is_none_or(|title_match| {
                candidate
                    .title
                    .as_deref()
                    .is_some_and(|title| title.contains(title_match))
            })
        })
        .filter(|candidate| {
            context_match
                .is_none_or(|context| candidate.browser_context_id.as_deref() == Some(context))
        })
        .cloned()
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        return Ok(candidates.into_iter().next().expect("one target"));
    }
    let code = if candidates.is_empty() {
        "target_missing"
    } else {
        "ambiguous_target"
    };
    Err(ComptrolError {
        code: code.to_owned(),
        message: format!(
            "Browser target matcher resolved to {} page targets",
            candidates.len()
        ),
        recovery: Some(
            "Narrow target_url_contains, target_title_contains, or browser_context_id".to_owned(),
        ),
    })
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
            let expression = match browser_wait_expression(&request.params) {
                Ok(expression) => expression,
                Err(message) => {
                    return ActionResult::refused(
                        request,
                        operation_id,
                        ComptrolError {
                            code: "invalid_input".to_owned(),
                            message: message.to_owned(),
                            recovery: None,
                        },
                    );
                }
            };
            (expression, true)
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
            // The read/action was dispatched. A false postcondition cannot
            // be represented as a failed preflight or "not attempted".
            let mut result = success(
                request,
                operation_id,
                "browser_protocol",
                if request.intent == "browser.cdp.wait_for" {
                    EffectState::None
                } else {
                    EffectState::Unknown
                },
                VerificationState::Failed,
                json!({"result": data, "postcondition": "failed"}),
            );
            result.error = Some(ComptrolError {
                code: "verification_failed".to_owned(),
                message: "The browser did not confirm the requested DOM condition".to_owned(),
                recovery: Some("Inspect the exact page state before retrying".to_owned()),
            });
            if request.intent != "browser.cdp.wait_for" {
                result.recovery = RecoveryState::RequiresReconciliation;
            }
            return result;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn browser_wait_expression(params: &Value) -> Result<String, &'static str> {
    let mut conditions = Vec::new();
    for (key, expression) in [
        ("url_contains", "location.href"),
        ("text_contains", "(document.body?.innerText ?? '')"),
    ] {
        if let Some(value) = params.get(key).and_then(Value::as_str) {
            let value = serde_json::to_string(value).map_err(|_| "Invalid wait string")?;
            conditions.push(format!("{expression}.includes({value})"));
        }
    }
    if let Some(expression) = params.get("ready_expression").and_then(Value::as_str) {
        conditions.push(format!("Boolean({expression})"));
    }
    if !conditions.is_empty() {
        if params.get("selector").is_some() {
            let mut dom = params.clone();
            for key in ["url_contains", "text_contains", "ready_expression"] {
                dom.as_object_mut().unwrap().remove(key);
            }
            conditions.push(browser_wait_expression(&dom)?);
        }
        return Ok(conditions
            .into_iter()
            .map(|value| format!("({value})"))
            .collect::<Vec<_>>()
            .join(" && "));
    }
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .ok_or("Browser wait needs a selector")?;
    let property = params
        .get("property")
        .and_then(Value::as_str)
        .unwrap_or("textContent");
    if property == "readyState" {
        if selector != "document" {
            return Err("Browser readyState wait requires selector document");
        }
        let expected = params
            .get("equals")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "interactive" | "complete"))
            .ok_or("Browser readyState wait needs equals interactive or complete")?;
        return Ok(format!("document.readyState === {expected:?}"));
    }
    if !matches!(
        property,
        "textContent" | "value" | "title" | "href" | "checked" | "disabled"
    ) {
        return Err("Browser wait property is not allowlisted");
    }
    let condition = if let Some(expected) = params.get("equals") {
        let expected = serde_json::to_string(expected)
            .map_err(|_| "Browser wait value is not serializable")?;
        format!("JSON.stringify(element[{property:?}]) === JSON.stringify({expected})")
    } else if let Some(expected) = params.get("contains").and_then(Value::as_str) {
        let expected = serde_json::to_string(expected)
            .map_err(|_| "Browser wait value is not serializable")?;
        format!("String(element[{property:?}] ?? '').includes({expected})")
    } else {
        return Err("Browser wait needs equals or contains");
    };
    let selector =
        serde_json::to_string(selector).map_err(|_| "Browser selector is not serializable")?;
    Ok(format!(
        "(() => {{ const element = document.querySelector({selector}); return Boolean(element) && {condition}; }})()"
    ))
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
        "browser_dispatch_failed"
            | "browser_response_failed"
            | "browser_unavailable"
            | "browser_bridge_timeout"
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
    } else if cfg!(target_os = "windows") {
        // C4 compact output: callers can bound the inventory (default 256);
        // min_windows=true drops invisible windows to shrink tool responses.
        let max_windows = request
            .params
            .get("max_windows")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(256);
        let min_windows_only = request
            .params
            .get("visible_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut inventory =
            match comptrol_platform_windows::enumerate_top_level_windows_bounded(max_windows) {
                Ok(inventory) => inventory,
                Err(error) => {
                    data["process_observation"] = json!("unavailable");
                    data["diagnostic"] = json!(error);
                    return success(
                        request,
                        operation_id,
                        "platform_observe",
                        EffectState::None,
                        VerificationState::Unverified,
                        data,
                    );
                }
            };
        if min_windows_only
            && let Some(windows) = inventory.get_mut("windows").and_then(Value::as_array_mut)
        {
            windows.retain(|window| window.get("visible").and_then(Value::as_bool) == Some(true));
            inventory["window_count"] = json!(windows.len());
            inventory["visible_only"] = json!(true);
        }
        data["windows"] = inventory["windows"].clone();
        data["window_count"] = inventory["window_count"].clone();
        data["truncated"] = inventory["truncated"].clone();
        data["window_enumeration"] = inventory["enumeration"].clone();
        data["process_observation"] = json!("verified");
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
    let relative_path = Path::new(relative);
    if !sandbox_relative(relative) {
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
    let path = checkpoints.sandbox_path().join(relative_path);
    if let Err(error) =
        checkpoints.write_sandbox_file(&operation_id, relative_path, content.as_bytes())
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
    sandbox_copy_at(request, operation_id, checkpoints)
}

fn sandbox_copy_at(
    request: &OperationRequest,
    operation_id: String,
    checkpoints: &CheckpointStore,
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
    let source_path = Path::new(source);
    let destination_path = Path::new(destination);
    let Ok(bytes) = checkpoints.read_sandbox_file(source_path) else {
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
    if let Err(error) = checkpoints.write_sandbox_file(&operation_id, destination_path, &bytes) {
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
    let destination = checkpoints.sandbox_path().join(destination_path);
    let source = checkpoints.sandbox_path().join(source_path);
    let verified = checkpoints
        .read_sandbox_file(destination_path)
        .is_ok_and(|copied| copied == bytes);
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
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && !path.to_string_lossy().contains('\\')
        && path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
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
/// through the native mechanism, correlate a top-level window, and wait
/// for a non-empty UIA observation before reporting content-ready.
#[cfg(windows)]
fn windows_app_matches_window(
    app: &comptrol_app_registry::AppEntry,
    window: &Value,
    _process_id: Option<u32>,
    exact_window_handle: Option<u64>,
) -> bool {
    if window["visible"].as_bool() != Some(true) {
        return false;
    }
    if exact_window_handle.is_some_and(|handle| window["window_handle"].as_u64() != Some(handle)) {
        return false;
    }
    if exact_window_handle.is_some() && windows_packaged_host_matches_app(app, window) {
        return true;
    }
    let aumid_matches = window["app_user_model_id"]
        .as_str()
        .is_some_and(|aumid| aumid.eq_ignore_ascii_case(&app.id));
    let executable_matches = app.executable.as_deref().is_some_and(|expected| {
        window["executable"]
            .as_str()
            .is_some_and(|actual| actual.eq_ignore_ascii_case(&expected.to_string_lossy()))
    });
    aumid_matches || executable_matches
}

#[cfg(windows)]
fn windows_packaged_host_matches_app(
    app: &comptrol_app_registry::AppEntry,
    window: &Value,
) -> bool {
    app.id.contains('!')
        && window["visible"].as_bool() == Some(true)
        && window["class_name"].as_str() == Some("ApplicationFrameWindow")
        && window["executable"]
            .as_str()
            .is_some_and(|path| path.ends_with("\\ApplicationFrameHost.exe"))
        && window["title"]
            .as_str()
            .is_some_and(|title| title.eq_ignore_ascii_case(&app.display_name))
}

#[cfg(windows)]
fn windows_app_windows(
    app: &comptrol_app_registry::AppEntry,
    process_id: Option<u32>,
    exact_window_handle: Option<u64>,
) -> Result<Vec<Value>, String> {
    let inventory = comptrol_platform_windows::enumerate_top_level_windows()?;
    let inventory_windows = inventory["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    if exact_window_handle.is_some() {
        return Ok(inventory_windows
            .iter()
            .filter(|window| {
                windows_app_matches_window(app, window, process_id, exact_window_handle)
            })
            .cloned()
            .collect());
    }
    if let Some(process_id) = process_id {
        let exact_process = inventory_windows
            .iter()
            .filter(|window| {
                window["visible"].as_bool() == Some(true)
                    && window["process_id"].as_u64() == Some(process_id as u64)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !exact_process.is_empty() {
            return Ok(exact_process);
        }
    }
    Ok(inventory_windows
        .iter()
        .filter(|window| windows_app_matches_window(app, window, None, None))
        .cloned()
        .collect())
}

#[cfg(windows)]
fn windows_foreground_packaged_host(app: &comptrol_app_registry::AppEntry) -> Option<Value> {
    if !app.id.contains('!') {
        return None;
    }
    let foreground = comptrol_platform_windows::foreground_window_handle()?;
    let inventory = comptrol_platform_windows::enumerate_top_level_windows().ok()?;
    inventory["windows"]
        .as_array()?
        .iter()
        .find(|candidate| {
            candidate["window_handle"].as_u64() == Some(foreground)
                && windows_packaged_host_matches_app(app, candidate)
        })
        .cloned()
}

#[cfg(windows)]
fn windows_app_readiness(
    app: &comptrol_app_registry::AppEntry,
    process_id: Option<u32>,
    exact_window_handle: Option<u64>,
    timeout_ms: u64,
    wait_for_content: bool,
) -> (Value, bool) {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms.clamp(100, 30_000));
    let mut first_window_ms = None;
    let mut last_observation = Value::Null;
    let mut event_wakeups = 0usize;
    let mut fallback_waits = 0usize;
    loop {
        // The packaged-app activation API returns the app PID, while UIA's
        // usable root may be the foreground ApplicationFrameHost window. This
        // correlation is permitted only during an explicit AUMID launch and
        // only for the exact foreground host with the expected title/class.
        if let (Some(activation_pid), Some(host)) =
            (process_id, windows_foreground_packaged_host(app))
            && let (Some(host_pid), Some(host_handle)) =
                (host["process_id"].as_u64(), host["window_handle"].as_u64())
            && let Ok(data) =
                comptrol_platform_windows::execute(comptrol_platform_windows::Request {
                    process_id: host_pid as u32,
                    window_handle: Some(host_handle),
                    name: None,
                    automation_id: None,
                    role: None,
                    action: comptrol_platform_windows::Action::Inspect,
                    value: None,
                    expected_attribute: None,
                    expected_value: None,
                    match_index: None,
                    expected_match_count: None,
                    max_nodes: 96,
                    allow_physical_click: false,
                })
            && data["control_count"].as_u64().unwrap_or(0) > 0
        {
            return (
                json!({
                                "ready":true,
                                "stage":"content_ready",
                                "readiness_source":"foreground_application_frame_host_uia",
                                "window":host,
                                "activation_process_id":activation_pid,
                                "controls_matched":data["matched_count"],
                                "controls_actionable":data["actionable_count"],
                                "window_poll_ms":first_window_ms,
                                "content_observed_ms":started.elapsed().as_secs_f64()*1000.0,
                                "ui_observation_ms":data["elapsed_ms"],
                                "event_wakeups":event_wakeups,
                                "fallback_waits":fallback_waits
                }),
                true,
            );
        }
        let windows = match windows_app_windows(app, process_id, exact_window_handle) {
            Ok(windows) => windows,
            Err(error) => {
                return (
                    json!({"ready":false,"stage":"window_inventory_failed","error":error}),
                    false,
                );
            }
        };
        if windows.len() > 1 {
            return (
                json!({
                    "ready":false,
                    "stage":"ambiguous_windows",
                    "candidate_window_handles":windows.iter().filter_map(|window| window["window_handle"].as_u64()).collect::<Vec<_>>(),
                    "window_poll_ms":started.elapsed().as_secs_f64()*1000.0
                }),
                false,
            );
        }
        if let Some(window) = windows.first() {
            first_window_ms.get_or_insert_with(|| started.elapsed().as_secs_f64() * 1000.0);
            let Some(window_handle) = window["window_handle"].as_u64() else {
                return (
                    json!({"ready":false,"stage":"window_handle_missing"}),
                    false,
                );
            };
            let Some(owner_pid) = window["process_id"].as_u64() else {
                return (
                    json!({"ready":false,"stage":"window_process_identity_missing"}),
                    false,
                );
            };
            let observation =
                comptrol_platform_windows::execute(comptrol_platform_windows::Request {
                    process_id: owner_pid as u32,
                    window_handle: Some(window_handle),
                    name: None,
                    automation_id: None,
                    role: None,
                    action: comptrol_platform_windows::Action::Inspect,
                    value: None,
                    expected_attribute: None,
                    expected_value: None,
                    match_index: None,
                    expected_match_count: None,
                    max_nodes: 96,
                    allow_physical_click: false,
                });
            match observation {
                Ok(data) if data["control_count"].as_u64().unwrap_or(0) > 0 => {
                    return (
                        json!({
                            "ready":true,
                            "stage":"content_ready",
                            "readiness_source":"win_event_wake_with_bounded_inventory_fallback_and_uia_observation",
                            "window":window,
                            "controls_matched":data["matched_count"],
                            "controls_actionable":data["actionable_count"],
                            "window_poll_ms":first_window_ms,
                            "content_observed_ms":started.elapsed().as_secs_f64()*1000.0,
                            "ui_observation_ms":data["elapsed_ms"]
                            ,"event_wakeups":event_wakeups
                            ,"fallback_waits":fallback_waits
                        }),
                        true,
                    );
                }
                Ok(data) => {
                    last_observation = data;
                    // Packaged Windows apps can expose their usable UIA tree
                    // under the foreground ApplicationFrameHost HWND instead
                    // of their AUMID-bearing CoreWindow. Correlate only the
                    // exact current foreground host, and only after this app's
                    // unique AUMID window was found above. Never infer a host
                    // from title alone or choose among stale host HWNDs.
                }
                Err(error) if error.starts_with("uia_worker_timeout:") => {
                    last_observation = json!({"error":"provider_timeout"});
                }
                Err(error) => last_observation = json!({"error":error}),
            }
            if !wait_for_content {
                return (
                    json!({
                        "ready":false,
                        "stage":"window_created_content_unready",
                        "readiness_source":"one_fresh_uia_observation_for_reused_window",
                        "window":window,
                        "last_observation":last_observation,
                        "window_poll_ms":first_window_ms,
                        "elapsed_ms":started.elapsed().as_secs_f64()*1000.0
                    }),
                    false,
                );
            }
        }
        if Instant::now() >= deadline {
            return (
                json!({
                    "ready":false,
                    "stage":if first_window_ms.is_some() { "window_created_content_unready" } else { "window_not_found" },
                    "readiness_source":"win_event_wake_with_bounded_inventory_fallback",
                    "window":windows.first(),
                    "last_observation":last_observation,
                    "window_poll_ms":first_window_ms,
                    "event_wakeups":event_wakeups,
                    "fallback_waits":fallback_waits,
                    "elapsed_ms":started.elapsed().as_secs_f64()*1000.0
                }),
                false,
            );
        }
        let remaining_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(250) as u32;
        if comptrol_platform_windows::wait_for_window_event(remaining_ms.max(1)) {
            event_wakeups += 1;
        } else {
            fallback_waits += 1;
        }
    }
}

/// The registry refuses ambiguous or missing apps instead of guessing.
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
    let instance_policy = request
        .params
        .get("instance_policy")
        .and_then(Value::as_str)
        .unwrap_or("reuse_unique");
    let exact_window_handle = request.params.get("window_handle").and_then(Value::as_u64);
    if let Err(message) = validate_app_instance_policy(instance_policy, exact_window_handle) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message,
                recovery: Some("Use reuse_unique for exact-window targeting; otherwise choose reuse_unique, launch_new, or error_if_running".to_owned()),
            },
        );
    }
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
    let resolution_started = Instant::now();
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
    let app_resolution_ms = resolution_started.elapsed().as_millis() as u64;
    #[cfg(windows)]
    let readiness_timeout_ms = request
        .params
        .get("readiness_timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(10_000)
        .clamp(100, 30_000);
    #[cfg(windows)]
    if resource == comptrol_app_registry::Resource::None && instance_policy != "launch_new" {
        let running = match windows_app_windows(&resolved, None, exact_window_handle) {
            Ok(windows) => windows,
            Err(error) => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "window_inventory_failed".to_owned(),
                        message: error,
                        recovery: Some("Refresh the Windows window inventory and retry".to_owned()),
                    },
                );
            }
        };
        if instance_policy == "error_if_running" && !running.is_empty() {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "app_already_running".to_owned(),
                    message: "The requested application already has a matching window".to_owned(),
                    recovery: Some(
                        "Use instance_policy=reuse_unique or select a distinct app instance"
                            .to_owned(),
                    ),
                },
            );
        }
        if instance_policy == "reuse_unique" && !running.is_empty() {
            if running.len() != 1 {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "target_ambiguous".to_owned(),
                        message: format!(
                            "{} matching application windows are running; refusing to guess",
                            running.len()
                        ),
                        recovery: Some(
                            "Inspect the window inventory and pass the exact window_handle"
                                .to_owned(),
                        ),
                    },
                );
            }
            let window = &running[0];
            let hwnd = window["window_handle"].as_u64().unwrap_or_default();
            let pid = window["process_id"].as_u64().unwrap_or_default() as u32;
            let created = window["process_created_at_100ns"].as_u64();
            let focused = if request.background.as_deref() == Some("prefer_background") {
                Value::Null
            } else {
                match comptrol_platform_windows::focus_window_handle(hwnd, pid, created) {
                    Ok(result) => result,
                    Err(error) => {
                        return ActionResult::refused(
                            request,
                            operation_id,
                            ComptrolError {
                                code: "app_focus_failed".to_owned(),
                                message: error,
                                recovery: Some("Confirm that the exact existing window is still active and retry".to_owned()),
                            },
                        );
                    }
                }
            };
            let (readiness, ready) = windows_app_readiness(
                &resolved,
                Some(pid),
                Some(hwnd),
                readiness_timeout_ms,
                false,
            );
            return success(
                request,
                operation_id,
                "app_window_reuse",
                EffectState::None,
                if ready {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                json!({
                    "app": resolved.id,
                    "instance_policy": instance_policy,
                    "existing_window": window,
                    "focused": focused,
                    "readiness": readiness,
                    "timings_ms": {"app_resolution_ms": app_resolution_ms},
                    "mouse": "untouched",
                    "clipboard": "untouched"
                }),
            );
        }
    }
    #[cfg(windows)]
    if exact_window_handle.is_some()
        && instance_policy == "reuse_unique"
        && resource == comptrol_app_registry::Resource::None
    {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "target_gone".to_owned(),
                message: "The requested exact window is not a visible window for this app"
                    .to_owned(),
                recovery: Some("Refresh the exact window inventory and retry".to_owned()),
            },
        );
    }
    let mut launch_request = comptrol_app_registry::LaunchRequest::new(resolved.clone());
    launch_request.resource = resource;
    launch_request.background = request.background.as_deref() == Some("prefer_background");
    match comptrol_app_registry::launch_verified(&launch_request) {
        Ok((outcome, verification)) => {
            #[cfg(windows)]
            let (readiness, ready) = windows_app_readiness(
                &resolved,
                outcome.pid,
                exact_window_handle,
                readiness_timeout_ms,
                true,
            );
            #[cfg(not(windows))]
            let (readiness, ready) = (
                json!({"ready":false,"stage":"platform_readiness_unavailable"}),
                false,
            );
            success(
                request,
                operation_id,
                "app_registry_launch",
                EffectState::Changed,
                if ready {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                json!({
                    "app": outcome.app_id,
                    "route": outcome.route,
                    "pid": outcome.pid,
                    "process_verification": verification,
                    "instance_policy": instance_policy,
                    "readiness": readiness,
                    "timings_ms": {"app_resolution_ms":app_resolution_ms, "launch":outcome.timings_ms},
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

fn validate_app_instance_policy(
    instance_policy: &str,
    exact_window_handle: Option<u64>,
) -> Result<(), String> {
    if !matches!(
        instance_policy,
        "reuse_unique" | "launch_new" | "error_if_running"
    ) {
        return Err(format!(
            "unsupported app instance policy: {instance_policy}"
        ));
    }
    if exact_window_handle.is_some() && instance_policy != "reuse_unique" {
        return Err(
            "window_handle selects an existing window and requires instance_policy=reuse_unique"
                .to_owned(),
        );
    }
    Ok(())
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
    match comptrol_app_registry::registry::installed_entries() {
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
    launch_request.settle_ms = request
        .params
        .get("settle_ms")
        .and_then(Value::as_u64)
        .unwrap_or(300)
        .clamp(200, 2_000);
    launch_request.background = request.background.as_deref() == Some("prefer_background");
    // P5.4: `window_state` chooses how the launched window is left. Hidden
    // keeps a desktop app out of the user's way while its controls remain
    // reachable through UIA.
    let window_state = match request
        .params
        .get("window_state")
        .and_then(Value::as_str)
        .unwrap_or("normal")
    {
        "hidden" => comptrol_app_registry::WindowState::Hidden,
        "minimized" => comptrol_app_registry::WindowState::Minimized,
        "off_desktop" => comptrol_app_registry::WindowState::OffDesktop,
        "normal" => comptrol_app_registry::WindowState::Normal,
        other => {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: format!(
                        "unknown window_state {other}; use normal, hidden, minimized, or off_desktop"
                    ),
                    recovery: None,
                },
            );
        }
    };
    match comptrol_app_registry::launch_verified(&launch_request) {
        Ok((outcome, verification)) => {
            // P5.3: a PID is not a surface. Correlate the launch with the
            // window that actually appeared, and report the real outcome
            // rather than upgrading delivery into verification.
            let correlate_options = comptrol_app_registry::CorrelateOptions {
                deadline_ms: request
                    .params
                    .get("surface_timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(5_000)
                    .clamp(200, 30_000),
                state: window_state,
                virtual_desktop: None,
            };
            let correlation = outcome
                .pid
                .map(|pid| comptrol_app_registry::correlate(pid, correlate_options));
            let surface = correlation.as_ref().and_then(|result| result.as_ref().ok());
            let verified = matches!(
                verification,
                comptrol_app_registry::LaunchVerification::Verified
            ) && surface.is_some();
            let surface_report = match surface {
                Some(surface) => {
                    comptrol_app_registry::surface::surface_payload(surface, window_state)
                }
                None => json!({
                    "window_handle": Value::Null,
                    "window_state": window_state,
                    "correlation": match correlation {
                        Some(Err(error)) => error.as_str(),
                        Some(Ok(_)) => "correlated",
                        None => "no_pid",
                    },
                    "note": "No window was correlated to this launch; a tray-only or service launch reports this rather than claiming a surface",
                }),
            };
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
                    "surface": surface_report,
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
    #[cfg(not(windows))]
    {
        return app_launch_with_resource(request, operation_id, false, "app_registry_activate");
    }
    #[cfg(windows)]
    {
        if request.background.as_deref() == Some("strict_background") {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "background_unavailable".to_owned(),
                    message: "Focusing an application requires foreground posture".to_owned(),
                    recovery: Some("Use foreground_allowed or foreground_required".to_owned()),
                },
            );
        }
        if request.params.get("resource").is_some() {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message:
                        "app.focus does not accept a resource; use app.open_resource to open one"
                            .to_owned(),
                    recovery: None,
                },
            );
        }
        let Some(query) = request.params.get("app").and_then(Value::as_str) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "app.focus needs an exact app identity or display name".to_owned(),
                    recovery: Some(
                        "Inspect the app registry and use one exact identity".to_owned(),
                    ),
                },
            );
        };
        let app = match comptrol_app_registry::registry::resolve(query) {
            Ok(app) => app,
            Err(error) => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "app_not_resolved".to_owned(),
                        message: error.to_string(),
                        recovery: Some("List installed apps and use one exact identity".to_owned()),
                    },
                );
            }
        };
        let window_handle = request.params.get("window_handle").and_then(Value::as_u64);
        if let Some(hwnd) = window_handle {
            let host = comptrol_platform_windows::enumerate_top_level_windows()
                .ok()
                .and_then(|inventory| {
                    inventory["windows"]
                        .as_array()?
                        .iter()
                        .find(|window| {
                            window["window_handle"].as_u64() == Some(hwnd)
                                && windows_packaged_host_matches_app(&app, window)
                        })
                        .cloned()
                });
            if let Some(window) = host {
                let pid = window["process_id"].as_u64().unwrap_or_default() as u32;
                let created = window["process_created_at_100ns"].as_u64();
                return match comptrol_platform_windows::focus_window_handle(hwnd, pid, created) {
                    Ok(result) => success(
                        request,
                        operation_id,
                        "windows_exact_packaged_host_focus",
                        EffectState::Changed,
                        VerificationState::Verified,
                        json!({"app":app.id,"window":result,"mouse":"untouched","clipboard":"untouched"}),
                    ),
                    Err(error) => ActionResult::refused(
                        request,
                        operation_id,
                        ComptrolError {
                            code: "app_focus_failed".to_owned(),
                            message: error,
                            recovery: Some("Windows may deny foreground activation; use the app or retry after a user action".to_owned()),
                        },
                    ),
                };
            }
        }
        let windows = match windows_app_windows(&app, None, window_handle) {
            Ok(windows) => windows,
            Err(error) => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "window_inventory_failed".to_owned(),
                        message: error,
                        recovery: Some("Refresh the Windows window inventory and retry".to_owned()),
                    },
                );
            }
        };
        let window = match windows.as_slice() {
            [] => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "target_missing".to_owned(),
                        message: "No visible window matches the exact registered app identity"
                            .to_owned(),
                        recovery: Some(
                            "Refresh the window inventory and retry with an exact window handle"
                                .to_owned(),
                        ),
                    },
                );
            }
            [window] => window,
            candidates => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "target_ambiguous".to_owned(),
                        message: format!(
                            "{} visible windows match this app; refusing to guess",
                            candidates.len()
                        ),
                        recovery: Some(
                            "Inspect the exact app windows and pass window_handle".to_owned(),
                        ),
                    },
                );
            }
        };
        let Some(hwnd) = window["window_handle"].as_u64() else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "window_identity_incomplete".to_owned(),
                    message: "The matching app window has no native HWND".to_owned(),
                    recovery: Some("Refresh the Windows window inventory and retry".to_owned()),
                },
            );
        };
        let Some(pid) = window["process_id"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
        else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "window_identity_incomplete".to_owned(),
                    message: "The matching app window has no valid process identity".to_owned(),
                    recovery: Some("Refresh the Windows window inventory and retry".to_owned()),
                },
            );
        };
        let created = window["process_created_at_100ns"].as_u64();
        match comptrol_platform_windows::focus_window_handle(hwnd, pid, created) {
            Ok(result) => success(
                request,
                operation_id,
                "windows_exact_app_window_focus",
                EffectState::Changed,
                VerificationState::Verified,
                json!({"app":app.id,"window":result,"mouse":"untouched","clipboard":"untouched"}),
            ),
            Err(error) => ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "app_focus_failed".to_owned(),
                    message: error,
                    recovery: Some("Windows may deny foreground activation; use the app or retry after a user action".to_owned()),
                },
            ),
        }
    }
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
            "browser_cdp_configured": browser::active_endpoint().is_some(),
            "browser_bridge_active": browser_bridge::bridge_is_active(),
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
            "notification" | "notifications" => {
                "x-apple.systempreferences:com.apple.preference.notifications".to_owned()
            }
            _ => return None,
        }),
        "windows" => Some(match permission {
            "accessibility" => "ms-settings:privacy-accessibility".to_owned(),
            "microphone" => "ms-settings:privacy-microphone".to_owned(),
            "notification" | "notifications" => "ms-settings:privacy-notifications".to_owned(),
            "screen_recording" => "ms-settings:privacy-screencapture".to_owned(),
            _ => return None,
        }),
        "linux" => Some(match permission {
            "notification" | "notifications" => "gnome-control-center notifications".to_owned(),
            _ => return None,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod permission_surface_tests {
    use super::permission_surface_for;

    /// The `permission.request` schema advertises exactly these values; each
    /// platform must map them to its documented surface or refuse honestly
    /// with None. This is the drift class that once left the schema's own
    /// `notification` value unmapped on every platform.
    #[test]
    fn permission_surfaces_cover_the_documented_enum() {
        let expected: &[(&str, bool)] = if cfg!(target_os = "macos") {
            &[
                ("accessibility", true),
                ("screen_recording", true),
                ("automation", true),
                ("notification", true),
            ]
        } else if cfg!(windows) {
            &[
                ("accessibility", true),
                ("screen_recording", true),
                ("automation", false),
                ("notification", true),
            ]
        } else {
            &[
                ("accessibility", false),
                ("screen_recording", false),
                ("automation", false),
                ("notification", true),
            ]
        };
        for (permission, mapped) in expected {
            assert_eq!(
                permission_surface_for(permission).is_some(),
                *mapped,
                "permission {permission} surface mapping drifted from the documented enum"
            );
        }
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
                name: Some(action_label.as_str()),
                automation_id: None,
                role: Some("AXButton"),
                action: comptrol_platform_macos::Action::Press,
                value: None,
                expected_attribute: None,
                expected_value: None,
                match_index: None,
                expected_match_count: None,
                max_nodes: 128,
                allow_physical_click: false,
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
                if let Some(endpoint) = browser::active_endpoint() {
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
                        std::ffi::OsStr::new(&endpoint),
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
                    comptrol_browser::SessionProvider::CompanionExtension => "ordinary signed-in tabs and tab groups through the authenticated extension bridge",
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
fn browser_session_connect(
    request: &OperationRequest,
    operation_id: String,
    state_dir: &Path,
) -> ActionResult {
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
            if browser::active_endpoint().is_none() {
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
            let ws_url = match comptrol_browser::connect_permissioned_auto_connect() {
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
            let version = browser::bridge().command(&ws_url, "Browser.getVersion", json!({}));
            let product = match version {
                Ok(value) => value
                    .get("product")
                    .and_then(Value::as_str)
                    .filter(|product| product.starts_with("Chrome/"))
                    .map(str::to_owned),
                Err(error) => {
                    return ActionResult::refused(
                        request,
                        operation_id,
                        ComptrolError {
                            code: "route_unavailable".to_owned(),
                            message: format!(
                                "Chrome's permissioned WebSocket did not complete a live CDP handshake: {error}"
                            ),
                            recovery: Some(
                                "Keep Chrome running with Remote Debugging enabled and accept its native Allow prompt when shown".to_owned(),
                            ),
                        },
                    );
                }
            };
            let Some(product) = product else {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "browser_protocol_invalid".to_owned(),
                        message: "The permissioned WebSocket did not identify a Chrome browser"
                            .to_owned(),
                        recovery: Some(
                            "Inspect the active Chrome remote debugging session".to_owned(),
                        ),
                    },
                );
            };
            browser::set_active_endpoint(ws_url);
            success(
                request,
                operation_id,
                "browser_session_broker",
                EffectState::Changed,
                VerificationState::Verified,
                json!({
                    "provider": "chrome_permissioned_auto_connect",
                    "status": "connected_via_permissioned_auto_connect",
                    "browser_product": product,
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
            let mut store = match browser_bridge::BridgeStore::open(state_dir) {
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
                    last_round_trip_ms: None,
                    last_round_trip_at_ms: None,
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

/// C6: the low-command contract. One call resolves the best available browser
/// route, proves the command channel with a live round trip, and returns the
/// current surface (targets) so the next call can act immediately. Replaces
/// the list → connect → discover → act dance with a single idempotent read.
fn browser_ensure_session(
    request: &OperationRequest,
    operation_id: String,
    state_dir: &Path,
) -> ActionResult {
    let mut route_report = Vec::new();
    // Route 1: explicit CDP endpoint (policy-gated, consent is per-connection).
    if let Some(endpoint) = browser::active_endpoint() {
        route_report.push(json!({ "route": "explicit_cdp_endpoint", "endpoint": endpoint, "state": "configured" }));
    }
    // Route 2: companion extension — prove it with a live round trip.
    let store_result = browser_bridge::BridgeStore::open(state_dir);
    if let Ok(store) = store_result {
        let health = store
            .health(browser_bridge::DEFAULT_HEALTH_MAX_AGE)
            .unwrap_or(browser_bridge::BridgeHealth {
                active: false,
                last_heartbeat_ms: None,
                target_count: 0,
                last_round_trip_ms: None,
                last_round_trip_at_ms: None,
            });
        let channel = store
            .health_round_trip(browser_bridge::DEFAULT_HEALTH_MAX_AGE)
            .ok();
        if channel.as_ref().is_some_and(|health| health.active) {
            // Channel proven; return the live surface in the same call.
            let targets = store
                .targets()
                .unwrap_or_default()
                .into_iter()
                .take(16)
                .collect::<Vec<_>>();
            return success(
                request,
                operation_id,
                "companion_extension",
                EffectState::None,
                VerificationState::Verified,
                json!({
                    "route": "companion_extension",
                    "ready": true,
                    "channel": channel,
                    "surface": {
                        "endpoint": "comptrol+bridge://local",
                        "targets": targets,
                        "target_count": targets.len()
                    },
                    "next": "Use browser.cdp.* intents against comptrol+bridge://local"
                }),
            );
        }
        route_report.push(json!({
            "route": "companion_extension",
            "state": if health.active { "host_alive_channel_unproven" } else { "inactive" },
            "channel": channel,
            "recovery": if health.active {
                "The extension service worker is not answering. Reload the extension at chrome://extensions, then retry; the wake bus may also recover it automatically."
            } else {
                "Open Chrome with the Comptrol Browser Bridge extension enabled and selected"
            }
        }));
    }
    // Nothing proved ready: report the strongest route state honestly with
    // its recovery path. (ComptrolError carries code/message/recovery; richer
    // per-route detail is visible in inspect kind:doctor → browser.channel.)
    let _ = route_report;
    ActionResult::refused(
        request,
        operation_id,
        ComptrolError {
            code: "browser_not_ready".to_owned(),
            message: "No browser route could be verified ready in one ensure_session call"
                .to_owned(),
            recovery: Some(
                "Reload the Browser Bridge extension at chrome://extensions if Chrome is running; once the channel round trip succeeds this call returns the live surface".to_owned(),
            ),
        },
    )
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
    let endpoint = browser::active_endpoint();
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
    if !command_program_allowed(program) {
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

fn uia_worker_unknown(
    request: &OperationRequest,
    operation_id: String,
    inspecting: bool,
    message: String,
) -> ActionResult {
    let timed_out = message.starts_with("uia_worker_timeout:");
    ActionResult {
        operation_id,
        intent: request.intent.clone(),
        route: "windows_uia_supervised_worker".to_owned(),
        target: request.target.clone(),
        preflight: "worker_result_unknown".to_owned(),
        delivery: DeliveryState::Unknown,
        effect: if inspecting {
            EffectState::None
        } else {
            EffectState::Unknown
        },
        verification: VerificationState::Unverified,
        disturbance: json!({
            "foreground_changed": "unknown",
            "mouse": "unknown",
            "clipboard": "untouched",
            "posture": request.background.as_deref().unwrap_or("foreground_allowed")
        }),
        recovery: if inspecting {
            RecoveryState::None
        } else {
            RecoveryState::RequiresReconciliation
        },
        data: json!({"worker_terminated": timed_out}),
        error: Some(ComptrolError {
            code: if timed_out {
                "uia_provider_timeout"
            } else {
                "uia_worker_stopped"
            }
            .to_owned(),
            message,
            recovery: Some(if inspecting {
                "The blocked UIA worker was stopped. Refresh the exact surface and inspect again."
                    .to_owned()
            } else {
                "Do not repeat this mutation yet. Inspect or reconcile the exact target state before retrying."
                    .to_owned()
            }),
        }),
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
    let key_sequence = request.params.get("key_sequence").and_then(Value::as_array);
    let inspecting = request.intent == "windows.uia.inspect";
    if !inspecting && key_sequence.is_none() && name.is_none() && automation_id.is_none() {
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
    if let Some(sequence) = key_sequence {
        if !matches!(
            request.background.as_deref(),
            Some("foreground_allowed" | "foreground_required")
        ) {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "foreground_required".to_owned(),
                    message: "key_sequence requires foreground_allowed or foreground_required"
                        .to_owned(),
                    recovery: Some(
                        "Focus the exact target window and retry with foreground_allowed"
                            .to_owned(),
                    ),
                },
            );
        }
        let Some(window_handle) = request.params.get("window_handle").and_then(Value::as_u64)
        else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "key_sequence requires an exact window_handle".to_owned(),
                    recovery: None,
                },
            );
        };
        let keys: Option<Vec<String>> = sequence
            .iter()
            .map(Value::as_str)
            .map(|v| v.map(str::to_owned))
            .collect();
        let Some(keys) = keys else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "key_sequence must contain only string key tokens".to_owned(),
                    recovery: None,
                },
            );
        };
        return match comptrol_platform_windows::send_key_sequence(process_id as u32, window_handle, &keys) {
            Ok(data) => success(
                request,
                operation_id,
                "windows_uia_key_sequence",
                EffectState::Changed,
                VerificationState::Unverified,
                data,
            ),
            Err(message) => ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "keyboard_input_refused".to_owned(),
                    message,
                    recovery: Some("Confirm the exact target window is foreground and use an allowed key token".to_owned()),
                },
            ),
        };
    }
    #[cfg(windows)]
    if inspecting || std::env::var("COMPTROL_WINDOWS_UIA_LEGACY").as_deref() != Ok("1") {
        let action = if inspecting {
            comptrol_platform_windows::Action::Inspect
        } else if request.intent.ends_with("press") {
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
            window_handle: request.params.get("window_handle").and_then(Value::as_u64),
            name,
            automation_id,
            role: request.params.get("role").and_then(Value::as_str),
            action,
            value: request.params.get("value").and_then(Value::as_str),
            expected_attribute,
            expected_value,
            match_index: request
                .params
                .get("match_index")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            expected_match_count: request
                .params
                .get("expected_match_count")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            max_nodes: request
                .params
                .get("max_nodes")
                .and_then(Value::as_u64)
                .unwrap_or(128) as usize,
            allow_physical_click: matches!(
                request.background.as_deref(),
                Some("foreground_allowed" | "foreground_required")
            ),
        });
        return match result {
            Ok(data) if inspecting => success(
                request,
                operation_id,
                "windows_uia_inspect",
                EffectState::None,
                if data.get("verified").and_then(Value::as_bool) == Some(true) {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                data,
            ),
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
            Err(message)
                if message.starts_with("uia_worker_timeout:")
                    || message.starts_with("uia_worker_stopped:") =>
            {
                uia_worker_unknown(request, operation_id, inspecting, message)
            }
            Err(message) => ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: if message.contains("ambiguous") {
                        "target_ambiguous"
                    } else if message.contains("missing") {
                        "target_gone"
                    } else if message.contains("not_actionable") || message.contains("disabled") {
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
    let name = request.params.get("name").and_then(Value::as_str);
    let automation_id = request.params.get("automation_id").and_then(Value::as_str);
    if name.is_none() && automation_id.is_none() {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "Linux AT SPI needs a name or automation_id".to_owned(),
                recovery: None,
            },
        );
    }
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
            automation_id,
            role: request.params.get("role").and_then(Value::as_str),
            action,
            value: request.params.get("value").and_then(Value::as_str),
            expected_attribute,
            expected_value,
            match_index: request
                .params
                .get("match_index")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            expected_match_count: request
                .params
                .get("expected_match_count")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            max_nodes: request
                .params
                .get("max_nodes")
                .and_then(Value::as_u64)
                .unwrap_or(128) as usize,
            allow_physical_click: matches!(
                request.background.as_deref(),
                Some("foreground_allowed" | "foreground_required")
            ),
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
        .env(
            "COMPTROL_ATSPI_NAME",
            name.or(automation_id).unwrap_or_default(),
        )
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
        name: Some(control),
        automation_id: request.params.get("automation_id").and_then(Value::as_str),
        role: request.params.get("role").and_then(Value::as_str),
        action,
        value,
        expected_attribute,
        expected_value,
        match_index: request
            .params
            .get("match_index")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        expected_match_count: request
            .params
            .get("expected_match_count")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        max_nodes: request
            .params
            .get("max_nodes")
            .and_then(Value::as_u64)
            .unwrap_or(128) as usize,
        allow_physical_click: matches!(
            request.background.as_deref(),
            Some("foreground_allowed" | "foreground_required")
        ),
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
    // Browser actions can run through either the explicitly configured direct
    // CDP endpoint or the authenticated companion bridge. Keep capability
    // discovery aligned with route planning and dispatch; otherwise a healthy
    // extension session is invisible to clients even though dispatch supports
    // it.
    let direct_browser_available = browser::active_endpoint().is_some();
    let companion_bridge_available = browser_bridge::bridge_is_active();
    let browser_session_available = direct_browser_available || companion_bridge_available;
    let browser_cdp_allowed = env_enabled("COMPTROL_ALLOW_BROWSER_CDP");
    let browser_cdp_available = browser_protocol_available(
        direct_browser_available,
        companion_bridge_available,
        browser_cdp_allowed,
        chrome_autostart::available(),
    );
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
            name: "browser.ensure_session".to_owned(),
            available: true,
            risk: Risk::R0,
            route: "browser_session_broker".to_owned(),
            note: "One-call readiness: resolves the best browser route, proves the command channel with a live round trip, and returns the current surface".to_owned(),
        },
        Capability {
            name: "browser.cdp.dialog".to_owned(),
            available: browser_cdp_allowed
                && browser::active_endpoint().is_some(),
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Handles a JavaScript dialog on one exact target; protected dialogs go through popup.dismiss".to_owned(),
        },
        Capability {
            name: "browser.chrome.open_tab".to_owned(),
            available: cfg!(any(
                target_os = "macos",
                target_os = "windows",
                target_os = "linux"
            )),
            risk: Risk::R2,
            route: "browser_launcher".to_owned(),
            note: "Opens a foreground Chrome tab in the existing default browser profile without an environment toggle; reports launcher acceptance unless a browser connection verifies page load".to_owned(),
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
            name: "desktop.terminal".to_owned(),
            available: env_enabled("COMPTROL_ALLOW_COMMANDS")
                && std::env::var_os("COMPTROL_COMMAND_ROOT").is_some()
                && std::env::var_os("COMPTROL_COMMAND_ALLOWLIST").is_some(),
            risk: Risk::R2,
            route: "terminal".to_owned(),
            note: "Runs allowlisted executables with structured argv and reads output back; no shell is involved and the window can be hidden"
                .to_owned(),
        },
        Capability {
            name: "desktop.explorer".to_owned(),
            available: cfg!(any(target_os = "windows", target_os = "macos", target_os = "linux"))
                && env_enabled("COMPTROL_ALLOW_DESKTOP_EXPLORER"),
            risk: Risk::R2,
            route: "file_explorer".to_owned(),
            note: "Reveals one exact path in the platform file manager without mutating the file"
                .to_owned(),
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
            available: browser_cdp_available,
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Direct CDP or an active authenticated companion bridge can serve browser actions; explicit browser policy is required".to_owned(),
        },
        Capability {
            name: "browser.cdp.open_tab".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Opens a visible or background tab in the existing local browser profile without mouse or clipboard input".to_owned(),
        },
        Capability {
            name: "browser.cdp.close_tab".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Closes one exact live page target after context and revision validation".to_owned(),
        },
        Capability {
            name: "browser.cdp.activate_tab".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Activates or deactivates one exact live page target; activation is a disclosed foreground change for rendering-dependent steps".to_owned(),
        },
        Capability {
            name: "browser.cdp.history".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Moves one exact live page target through bounded browser history without foreground input".to_owned(),
        },
        Capability {
            name: "browser.cdp.accessibility_snapshot".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R0,
            route: "browser_protocol".to_owned(),
            note: "Reads a bounded accessibility tree from one exact live page target".to_owned(),
        },
        Capability {
            name: "browser.cdp.compact_snapshot".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R0,
            route: "browser_protocol".to_owned(),
            note: "Reads a bounded list of visible actionable controls from one exact live page target".to_owned(),
        },
        Capability {
            name: "browser.cdp.screenshot".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R0,
            route: "browser_protocol".to_owned(),
            note: "Captures a bounded target-scoped visual digest without returning pixels through MCP".to_owned(),
        },
        Capability {
            name: "browser.cdp.coordinate_click".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Dispatches one coordinate click only when a fresh screenshot capture_id proves the viewport geometry is current".to_owned(),
        },
        Capability {
            name: "browser.cdp.focus".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R1,
            route: "browser_protocol".to_owned(),
            note: "Focuses one exact live page element without mouse or clipboard input".to_owned(),
        },
        Capability {
            name: "browser.cdp.semantic_click".to_owned(),
            available: browser_cdp_available,
            risk: Risk::R2,
            route: "browser_protocol".to_owned(),
            note: "Resolves a fresh semantic locator, checks visibility and overlay coverage, then retries once after a stale target revision".to_owned(),
        },
        Capability {
            name: "browser.cdp.workflow".to_owned(),
            available: browser_cdp_available,
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
            available: browser_session_available,
            risk: Risk::R0,
            route: "browser_protocol".to_owned(),
            note: "Discovers exact local browser targets without mutation".to_owned(),
        },
        Capability {
            name: "browser.fixture.submit".to_owned(),
            available: direct_browser_available
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
            available: (env_enabled("COMPTROL_ALLOW_ADAPTERS")
                || (env_enabled("COMPTROL_ALLOW_CREATIVE_ADAPTERS")
                    && is_creative_adapter_intent(intent)))
                && adapter_root().is_dir()
                && match *intent {
                    "mail.send" => env_enabled("COMPTROL_ALLOW_MAIL_SEND"),
                    "obs.recording.start"
                    | "obs.recording.stop"
                    | "discord.message.delete"
                    | "message.send" => env_enabled("COMPTROL_ALLOW_HIGH_CONSEQUENCE_ADAPTERS"),
                    _ => true,
                },
            risk: classify(intent),
            route: "isolated_adapter".to_owned(),
            note: "Runs through a bounded out of process first party adapter and requires application state verification".to_owned(),
        });
    }
    result.extend(platform_capabilities());
    result
}

/// Machine-readable catalog that keeps callable intent names separate from
/// platform readiness labels, which are observations rather than operations.
pub fn capability_catalog() -> Value {
    let all = callable_intent_catalog();
    let intents = all
        .iter()
        .filter(|capability| {
            !PLATFORM_OBSERVATIONS.contains(&capability.name.as_str())
                && !CAPABILITY_FAMILIES.contains(&capability.name.as_str())
        })
        .collect::<Vec<_>>();
    let families = all
        .iter()
        .filter(|capability| CAPABILITY_FAMILIES.contains(&capability.name.as_str()))
        .collect::<Vec<_>>();
    let platforms = all
        .iter()
        .filter(|capability| PLATFORM_OBSERVATIONS.contains(&capability.name.as_str()))
        .collect::<Vec<_>>();
    json!({
        "intents": intents,
        "capability_families": families,
        "platform_observations": platforms,
        "note": "Only entries in intents are callable operate intent names; capability_families describe a route family without a dispatchable name, and platform_observations describe detected surfaces."
    })
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

/// Whether a local browser route can be reached right now.
///
/// `auto_start` means this runtime can start the local browser on demand,
/// which both produces an endpoint and opens the CDP policy gate the
/// browser exists to serve. Reporting that honestly is what lets a client
/// try the browser route at all; the eager start used to fake it by
/// writing the gate at process start for every session.
fn browser_protocol_available(
    direct_endpoint: bool,
    companion_bridge: bool,
    policy: bool,
    auto_start: bool,
) -> bool {
    (direct_endpoint || companion_bridge || auto_start) && (policy || auto_start)
}

fn doctor(runtime: &Runtime) -> Value {
    let direct_browser_configured = browser::active_endpoint().is_some();
    let extension_registered = comptrol_browser::list_sessions().iter().any(|session| {
        session.provider == comptrol_browser::SessionProvider::CompanionExtension
            && session.available
    });
    // Channel truth: a heartbeat alone lied during live failure analysis (the
    // daemon recorded its own heartbeat posts while the extension service
    // worker was dead). "Alive" now means a completed extension round trip
    // (a bridge_ping answered by the actual service worker) within the health
    // window; host-heartbeat-only state is reported as degraded, not ready.
    let channel =
        browser_bridge::BridgeStore::open(&crate::default_state_dir()).and_then(|store| {
            store
                .health_round_trip(browser_bridge::DEFAULT_HEALTH_MAX_AGE)
                .map(|health| {
                    (
                        store.health(browser_bridge::DEFAULT_HEALTH_MAX_AGE).ok(),
                        Some(health),
                    )
                })
        });
    let (host_health, channel_health) = match channel {
        Ok((host, channel)) => (host, channel),
        Err(_) => (None, None),
    };
    let channel_alive = channel_health.as_ref().is_some_and(|health| health.active);
    let host_heartbeat_active = host_health.as_ref().is_some_and(|health| health.active);
    // K5: the page-op tier. A channel can answer pings while every in-page
    // command hangs (observed live), so health reports both tiers.
    let page_ops_report = browser_bridge::BridgeStore::open(&default_state_dir())
        .and_then(|store| store.page_ops_health())
        .unwrap_or_else(|_| json!({ "state": "unknown" }));
    let extension_active = channel_alive;
    let browser_ready = extension_active;
    let browser_status = if channel_alive {
        "connected"
    } else if host_heartbeat_active {
        // Exactly the zombie state observed live: something posts host
        // heartbeats, but the extension has not proven liveness. Never
        // report this as ready.
        "degraded_extension_unreachable"
    } else if direct_browser_configured {
        "configured_unverified"
    } else if extension_registered {
        "registered_channel_unverified"
    } else {
        "not_configured"
    };
    let channel_report = json!({
        "state": if channel_alive {
            "alive"
        } else if host_heartbeat_active {
            "degraded_extension_unreachable"
        } else {
            "down"
        },
        "host_heartbeat_active": host_heartbeat_active,
        "last_host_heartbeat_ms": host_health.and_then(|health| health.last_heartbeat_ms),
        "round_trip_active": channel_alive,
        "last_round_trip_ms": channel_health.as_ref().and_then(|health| health.last_round_trip_ms),
        "last_round_trip_at_ms": channel_health.as_ref().and_then(|health| health.last_round_trip_at_ms),
        "page_ops": page_ops_report,
        "meaning": "alive requires a service-worker-answered bridge_ping within the health window; host_heartbeat alone is daemon liveness only, and page_ops separates page-operation health from ping health"
    });
    json!({
        "server": SERVER_VERSION,
        "protocol": PROTOCOL_VERSION,
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "daemon": {
            "state": if DAEMON_RESIDENT.load(Ordering::Relaxed) { "resident" } else { "in_process" },
            "available": true,
            "resident": DAEMON_RESIDENT.load(Ordering::Relaxed),
            "clients": DAEMON_CLIENTS.load(Ordering::Relaxed),
        },
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
                None => json!({ "available": false, "reason": runtime.consent_error, "mutations_blocked": true }),
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
            "configured": direct_browser_configured || extension_registered,
            "ready": browser_ready,
            "status": browser_status,
            "channel": channel_report,
            "fixture_mutation": runtime.policy.allowed_intents.contains("browser.fixture.submit"),
            "extension": if extension_active { "ready" } else if extension_registered { "registered_channel_unverified" } else { "not_registered" },
            "persistent_multiplexer": "implemented_not_live_verified",
            "event_target_frame_graph": "implemented_not_live_verified",
            "chrome_native_restore": "implemented_not_live_verified",
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
        "readiness": readiness_report(runtime),
        "state_dir": state_dir()
    })
}

fn readiness_report(runtime: &Runtime) -> Value {
    let capabilities = capabilities()
        .into_iter()
        .map(|capability| {
            let history = runtime.route_history.get(&capability.route);
            let attempts = history.map_or(0, |stats| stats.attempts);
            let verified = history.map_or(0, |stats| stats.verified_successes);
            json!({
                "intent": capability.name,
                "route": capability.route,
                "environment_available": capability.available,
                "local_policy_allowed": runtime.policy.authorize(&capability.name, capability.risk).is_ok(),
                "live_status": if verified > 0 { "verified_here" } else if attempts > 0 { "attempted_not_verified" } else { "not_tested_here" },
                "attempt_count": attempts,
                "verified_successes": verified,
                "last_verified_at_ms": history.and_then(|stats| stats.last_success_at_ms),
                "note": capability.note
            })
        })
        .collect::<Vec<_>>();
    json!({
        "meaning": "environment_available describes local configuration only; verified_here means this runtime recorded a verified operation. Neither status proves an external application is installed.",
        "capabilities": capabilities
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
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile).join(".comptrol");
    }
    PathBuf::from(".comptrol")
}

fn runtime_fingerprint() -> Value {
    static FINGERPRINT: OnceLock<Value> = OnceLock::new();
    FINGERPRINT
        .get_or_init(|| {
            const POLICY_FLAGS: &[&str] = &[
                "COMPTROL_ALLOW_APP_LAUNCH",
                "COMPTROL_ALLOW_BROWSER_CDP",
                "COMPTROL_ALLOW_CREATIVE_ADAPTERS",
                "COMPTROL_ALLOW_SETTINGS",
                "COMPTROL_ALLOW_WINDOWS_UIA",
                "COMPTROL_AUTO_START_CHROME_CDP",
                "COMPTROL_CHROME_AUTO_CONNECT",
                "COMPTROL_WINDOWS_UIA",
            ];
            let effective_policy = POLICY_FLAGS
                .iter()
                .map(|name| ((*name).to_owned(), env_enabled(name)))
                .collect::<Vec<_>>();
            let policy_bytes = serde_json::to_vec(&effective_policy).unwrap_or_default();
            let policy_fingerprint = format!("{:x}", Sha256::digest(policy_bytes));
            let executable_path = std::env::current_exe().ok();
            let executable_sha256 = executable_path
                .as_ref()
                .and_then(|path| fs::read(path).ok())
                .map(|bytes| format!("{:x}", Sha256::digest(bytes)));
            let effective_policy = effective_policy
                .into_iter()
                .map(|(name, value)| (name, Value::Bool(value)))
                .collect::<serde_json::Map<_, _>>();
            json!({
                "server_version": SERVER_VERSION,
                "protocol_version": PROTOCOL_VERSION,
                "process_id": std::process::id(),
                "executable_path": executable_path,
                "executable_sha256": executable_sha256,
                "state_directory": state_dir(),
                "adapter_root": adapter_root(),
                "effective_policy": effective_policy,
                "policy_fingerprint": policy_fingerprint
            })
        })
        .clone()
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

// P4.1: the op interpreter itself lives in `comptrol-workflow` beside the
// node graph executor, so the two workflow dialects cannot drift. This is
// the runtime's error mapping on top of that single executor.
pub use comptrol_workflow::WorkflowOp;

pub fn execute_workflow(ops: &[WorkflowOp]) -> Result<Value, ComptrolError> {
    let outcome = comptrol_workflow::execute_ops(ops, |milliseconds| {
        std::thread::sleep(Duration::from_millis(milliseconds));
        Ok(())
    });
    outcome.map_err(|error| ComptrolError {
        code: match error {
            comptrol_workflow::OpError::AssertionFailed(_) => "verification_failed",
            comptrol_workflow::OpError::WaitTooLong(_) => "invalid_input",
            comptrol_workflow::OpError::Cancelled => "cancelled",
        }
        .to_owned(),
        message: error.to_string(),
        recovery: Some("Reobserve and compile a repaired branch".to_owned()),
    })
}

pub fn default_state_dir() -> PathBuf {
    state_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn packaged_app_focus_matches_exact_aumid_without_registered_executable() {
        let app = comptrol_app_registry::AppEntry {
            id: "sample.package_abcd!App".to_owned(),
            display_name: "Sample".to_owned(),
            platform: "windows".to_owned(),
            executable: None,
            launch_args: Vec::new(),
            version: None,
            metadata: Default::default(),
        };
        let window = json!({
            "visible": true,
            "window_handle": 42,
            "app_user_model_id": "sample.package_abcd!App",
            "process_id": 10
        });
        assert!(windows_app_matches_window(&app, &window, None, Some(42)));
        assert!(!windows_app_matches_window(&app, &window, None, Some(43)));

        let wrong_package = json!({
            "visible": true,
            "window_handle": 42,
            "app_user_model_id": "other.package_abcd!App",
            "process_id": 10
        });
        assert!(!windows_app_matches_window(
            &app,
            &wrong_package,
            None,
            Some(42)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn packaged_host_requires_exact_host_identity_and_app_title() {
        let app = comptrol_app_registry::AppEntry {
            id: "sample.package_abcd!App".to_owned(),
            display_name: "Sample".to_owned(),
            platform: "windows".to_owned(),
            executable: None,
            launch_args: Vec::new(),
            version: None,
            metadata: Default::default(),
        };
        let host = json!({
            "visible": true,
            "class_name": "ApplicationFrameWindow",
            "executable": "C:\\Windows\\System32\\ApplicationFrameHost.exe",
            "title": "Sample"
        });
        assert!(windows_packaged_host_matches_app(&app, &host));

        let wrong_title = json!({"visible":true,"class_name":"ApplicationFrameWindow","executable":"C:\\Windows\\System32\\ApplicationFrameHost.exe","title":"Other"});
        assert!(!windows_packaged_host_matches_app(&app, &wrong_title));
        let wrong_host = json!({"visible":true,"class_name":"OtherWindow","executable":"C:\\Windows\\System32\\other.exe","title":"Sample"});
        assert!(!windows_packaged_host_matches_app(&app, &wrong_host));
    }

    #[test]
    fn app_instance_policy_requires_exact_window_to_reuse_existing_instance() {
        assert!(validate_app_instance_policy("reuse_unique", Some(42)).is_ok());
        assert!(validate_app_instance_policy("launch_new", Some(42)).is_err());
        assert!(validate_app_instance_policy("error_if_running", Some(42)).is_err());
        assert!(validate_app_instance_policy("guess", None).is_err());
        assert!(validate_app_instance_policy("launch_new", None).is_ok());
    }

    #[test]
    fn uia_provider_timeout_marks_mutation_unknown_for_reconciliation() {
        let request = OperationRequest {
            intent: "windows.uia.press".to_owned(),
            target: None,
            params: json!({"process_id": 1234, "name":"Save"}),
            postcondition: None,
            risk: None,
            idempotency_key: Some("uia-timeout-case".to_owned()),
            dry_run: false,
            background: Some("foreground_allowed".to_owned()),
        };
        let result = uia_worker_unknown(
            &request,
            "uia-timeout-op".to_owned(),
            false,
            "uia_worker_timeout: provider exceeded deadline".to_owned(),
        );
        assert_eq!(result.delivery, DeliveryState::Unknown);
        assert_eq!(result.effect, EffectState::Unknown);
        assert_eq!(result.recovery, RecoveryState::RequiresReconciliation);
        assert_eq!(result.error.unwrap().code, "uia_provider_timeout");
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn route_p95_uses_nearest_rank_percentile() {
        let samples = (1..=20).map(|value| value as f64).collect::<VecDeque<_>>();
        assert_eq!(percentile_95(&samples), Some(19.0));
    }

    #[test]
    fn browser_wait_supports_bounded_document_readiness_checks() {
        assert_eq!(
            browser_wait_expression(&json!({
                "selector": "document",
                "property": "readyState",
                "equals": "complete"
            }))
            .expect("valid readyState wait"),
            "document.readyState === \"complete\""
        );
        assert!(
            browser_wait_expression(&json!({
                "selector": "#page",
                "property": "readyState",
                "equals": "complete"
            }))
            .is_err()
        );
        assert!(
            browser_wait_expression(&json!({
                "selector": "document",
                "property": "readyState",
                "equals": "loaded"
            }))
            .is_err()
        );
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
    fn runtime_fingerprint_hashes_the_running_executable_and_policy() {
        let fingerprint = runtime_fingerprint();
        let executable = std::env::current_exe().expect("test executable path");
        let executable_hash = format!("{:x}", Sha256::digest(fs::read(&executable).unwrap()));
        assert_eq!(fingerprint["server_version"], SERVER_VERSION);
        assert_eq!(fingerprint["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(
            fingerprint["executable_path"],
            executable.to_string_lossy().as_ref()
        );
        assert_eq!(fingerprint["executable_sha256"], executable_hash);
        assert_eq!(fingerprint["process_id"], std::process::id());
        assert_eq!(
            fingerprint["policy_fingerprint"].as_str().unwrap().len(),
            64
        );
        assert!(fingerprint["effective_policy"].is_object());
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
    fn browser_protocol_capability_requires_policy_and_a_live_transport() {
        assert!(!browser_protocol_available(false, false, false, false));
        assert!(!browser_protocol_available(false, false, true, false));
        assert!(!browser_protocol_available(true, false, false, false));
        assert!(browser_protocol_available(true, false, true, false));
        assert!(!browser_protocol_available(false, true, false, false));
        assert!(browser_protocol_available(false, true, true, false));
        // Auto-start alone is enough: it produces the endpoint and the gate.
        assert!(browser_protocol_available(false, false, false, true));
        assert!(browser_protocol_available(false, false, true, true));
    }

    #[test]
    fn default_policy_allows_opening_apps_and_urls_without_broad_control() {
        let policy = Policy::default();
        assert!(policy.allow_app_launch);
        assert!(policy.authorize("app.launch", Risk::R1).is_ok());
        assert!(
            policy
                .authorize("browser.chrome.open_tab", Risk::R2)
                .is_ok()
        );
        assert!(policy.authorize("app.open_resource", Risk::R2).is_err());
        assert!(policy.authorize("desktop.open_app", Risk::R2).is_err());
        assert!(policy.authorize("browser.cdp.navigate", Risk::R2).is_err());
        assert!(
            route_plan_for_intent("browser.chrome.open_tab", Value::Null, None).candidates[0]
                .feasible
        );
    }

    #[test]
    fn default_policy_routes_open_requests_without_environment_toggles() {
        let mut runtime = runtime();
        runtime.policy = Policy::default();
        for (intent, params, risk, expected_route) in [
            (
                "app.launch",
                json!({"app":"Blender"}),
                Risk::R1,
                "app_registry_launch",
            ),
            (
                "browser.chrome.open_tab",
                json!({"url":"https://example.test"}),
                Risk::R2,
                "browser_launcher",
            ),
        ] {
            let result = runtime.operate(OperationRequest {
                intent: intent.to_owned(),
                target: None,
                params,
                postcondition: None,
                risk: Some(risk),
                idempotency_key: None,
                dry_run: true,
                background: Some("foreground_allowed".to_owned()),
            });
            assert_eq!(result.preflight, "passed", "{intent}: {:?}", result.error);
            assert_eq!(result.route, expected_route);
        }
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
    fn recipe_run_refuses_an_unpromoted_recipe_and_an_unknown_one() {
        let mut runtime = runtime();
        // The closed schema rejects an unknown recipe name before dispatch,
        // so the caller learns it from a validated enum rather than from a
        // runtime lookup failure.
        let unknown = operate(
            &mut runtime,
            "recipe.run",
            json!({"recipe":"recipe.does_not_exist"}),
            Risk::R1,
        );
        assert_eq!(
            unknown.error.as_ref().map(|e| e.code.as_str()),
            Some("invalid_input")
        );
        // Every checked-in recipe ships with zero verified replays, so it
        // must refuse rather than pretend to be proven.
        let unpromoted = operate(
            &mut runtime,
            "recipe.run",
            json!({
                "recipe":"recipe.classroom_open_class",
                "parameters":{"account":"a@example.test","class_name":"Investment Club"}
            }),
            Risk::R1,
        );
        assert_eq!(
            unpromoted.error.as_ref().map(|e| e.code.as_str()),
            Some("recipe_not_promoted")
        );
        assert!(
            unpromoted
                .error
                .as_ref()
                .is_some_and(|e| e.message.contains("0 verified replays")),
            "the refusal must report the recipe's real promotion state"
        );
        // Promotion is checked before binding, so an unpromoted recipe
        // reports its promotion state even when its parameters are also
        // missing. That ordering is deliberate: telling a caller their
        // parameters are wrong when the real problem is "this recipe has
        // never been proven" would send them down the wrong path.
        let missing_params = operate(
            &mut runtime,
            "recipe.run",
            json!({"recipe":"recipe.classroom_open_class"}),
            Risk::R1,
        );
        assert_eq!(
            missing_params.error.as_ref().map(|e| e.code.as_str()),
            Some("recipe_not_promoted")
        );
    }

    #[test]
    fn recipes_are_discoverable_and_their_steps_reuse_dispatchable_intents() {
        // A recipe must never reach a route the caller could not reach
        // directly, so every step it names has to be a real intent.
        for recipe in comptrol_workflow::recipes::catalog() {
            assert!(intent_schema::schema_for("recipe.run").is_some());
            for node in recipe.workflow.nodes.values() {
                if let comptrol_workflow::WorkflowNode::Act { intent, .. } = node {
                    assert!(
                        CORE_INTENTS.contains(&intent.as_str()),
                        "recipe {} steps through {intent}, which is not a core intent",
                        recipe.workflow.id
                    );
                }
            }
        }
    }

    #[test]
    fn app_launch_rejects_an_unknown_window_state_before_launching_anything() {
        let mut runtime = runtime();
        allow(&mut runtime, "app.launch", Risk::R1);
        let result = operate(
            &mut runtime,
            "app.launch",
            json!({"app":"this-application-does-not-exist","window_state":"invisible"}),
            Risk::R1,
        );
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("invalid_input"),
            "an unknown window_state must be refused by the closed schema, not by a failed launch"
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
    fn caller_cannot_lower_mutation_risk_to_bypass_stop_latch() {
        let mut runtime = runtime();
        runtime.stop.engage().expect("stop");
        allow(&mut runtime, "filesystem.write", Risk::R1);
        let result = runtime.operate(OperationRequest {
            intent: "filesystem.write".to_owned(),
            target: None,
            params: json!({"path":"risk.txt","content":"blocked"}),
            postcondition: None,
            risk: Some(Risk::R0),
            idempotency_key: Some("risk-downgrade".to_owned()),
            dry_run: false,
            background: None,
        });
        assert_eq!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("stopped")
        );
    }

    #[test]
    fn idempotency_key_rejects_changed_request_and_replays_exact_request_after_restart() {
        let dir = std::env::temp_dir().join(format!("comptrol-idempotency-{}", now_ms()));
        let key = Some("stable-key".to_owned());
        let request = OperationRequest {
            intent: "system.ping".to_owned(),
            target: None,
            params: json!({"value":1}),
            postcondition: None,
            risk: None,
            idempotency_key: key.clone(),
            dry_run: false,
            background: None,
        };
        let mut runtime = Runtime::new(dir.clone()).expect("runtime");
        let first = runtime.operate(request.clone());
        let replay = runtime.operate(request.clone());
        assert!(matches!(replay.recovery, RecoveryState::IdempotentReplay));
        let mut changed = request.clone();
        changed.params = json!({"value":2});
        assert_eq!(
            runtime
                .operate(changed.clone())
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("idempotency_conflict")
        );
        drop(runtime);
        let mut restarted = Runtime::new(dir.clone()).expect("restart");
        assert!(matches!(
            restarted.operate(request).recovery,
            RecoveryState::IdempotentReplay
        ));
        assert_eq!(
            restarted
                .operate(changed)
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("idempotency_conflict")
        );
        assert!(!first.operation_id.is_empty());
        let _ = fs::remove_dir_all(dir);
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
        let result = sandbox_copy_at(&request, "copy-op".to_owned(), &checkpoints);
        assert_eq!(result.verification, VerificationState::Verified);
        assert_eq!(
            fs::read(sandbox.join("nested/copy.txt")).expect("copy"),
            b"copy me"
        );
        let mut invalid = request;
        invalid.params["source"] = json!("../outside.txt");
        let result = sandbox_copy_at(&invalid, "copy-invalid".to_owned(), &checkpoints);
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
            {"action":"wait_url","contains":"/done","timeout_ms":1200},
            {"action":"wait_text","text":"Task completed","timeout_ms":1200}
        ]))
        .expect("browser workflow schema");
        assert_eq!(steps.len(), 4);
        assert!(
            serde_json::from_value::<Vec<BrowserWorkflowStep>>(json!([
                {"action":"evaluate","expression":"alert(1)"}
            ]))
            .is_err()
        );
    }

    #[test]
    fn browser_bridge_timeout_is_unknown_and_requires_reconciliation() {
        let request = OperationRequest {
            intent: "browser.cdp.workflow".to_owned(),
            target: None,
            params: json!({"steps":[{"action":"click","locator":{"text":"Continue"}}]}),
            postcondition: None,
            risk: Some(Risk::R2),
            idempotency_key: Some("bridge-timeout-reconcile".to_owned()),
            dry_run: false,
            background: None,
        };
        let result = browser_failure(
            &request,
            "bridge-timeout-reconcile".to_owned(),
            ComptrolError {
                code: "browser_bridge_timeout".to_owned(),
                message: "command result timed out".to_owned(),
                recovery: Some("Inspect the live target before retrying".to_owned()),
            },
        );
        assert_eq!(result.preflight, "passed");
        assert_eq!(result.delivery, DeliveryState::Unknown);
        assert_eq!(result.effect, EffectState::Unknown);
        assert_eq!(result.verification, VerificationState::Unverified);
        assert_eq!(result.recovery, RecoveryState::RequiresReconciliation);
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
            request_fingerprint: Some(request_fingerprint(&request)),
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
            request_fingerprint: None,
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
            request_fingerprint: Some(request_fingerprint(&request)),
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
            request_fingerprint: Some(request_fingerprint(&request)),
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
    fn browser_binding_revalidates_generation_only_drift() {
        // K4: live SPAs bump the generation constantly; a pinned revision
        // whose URL tail matches the live URL is generation-only drift and
        // must revalidate, while a URL change must still refuse.
        let targets = vec![BrowserTarget {
            id: "profile-1:42".to_owned(),
            browser_context_id: Some("profile-1".to_owned()),
            target_type: Some("page".to_owned()),
            url: Some("https://classroom.google.com/u/2/h/st".to_owned()),
            title: Some("Home - Classroom".to_owned()),
            revision: Some(
                "bridge:profile-1:42:3:https://classroom.google.com/u/2/h/st".to_owned(),
            ),
            web_socket_url: None,
        }];
        assert_eq!(
            bind_browser_target(
                &targets,
                "profile-1:42",
                Some("profile-1"),
                Some("bridge:profile-1:42:3:https://classroom.google.com/u/2/h/st")
            )
            .expect("exact revision")
            .id,
            "profile-1:42"
        );
        assert_eq!(
            bind_browser_target(
                &targets,
                "profile-1:42",
                Some("profile-1"),
                Some("bridge:profile-1:42:9:https://classroom.google.com/u/2/h/st")
            )
            .expect("generation-only drift revalidates")
            .id,
            "profile-1:42"
        );
        assert_eq!(
            bind_browser_target(
                &targets,
                "profile-1:42",
                Some("profile-1"),
                Some("bridge:profile-1:42:9:https://classroom.google.com/u/2/c/abc")
            )
            .expect_err("url change")
            .code,
            "stale_reference"
        );
        assert_eq!(
            bind_browser_target(&targets, "profile-1:42", None, Some("revision-1"))
                .expect_err("strict non-bridge revision")
                .code,
            "stale_reference"
        );
    }

    #[test]
    fn browser_workflow_target_match_requires_one_exact_page_candidate() {
        let target = BrowserTarget {
            id: "tab-classroom".to_owned(),
            browser_context_id: Some("profile-school".to_owned()),
            target_type: Some("page".to_owned()),
            url: Some("https://classroom.google.com/u/2/h/st".to_owned()),
            title: Some("Home - Classroom".to_owned()),
            revision: Some("url:https://classroom.google.com/u/2/h/st".to_owned()),
            web_socket_url: None,
        };
        assert_eq!(
            unique_workflow_target(
                std::slice::from_ref(&target),
                "classroom.google.com",
                Some("Classroom"),
                Some("profile-school")
            )
            .expect("one exact page")
            .id,
            "tab-classroom"
        );
        assert_eq!(
            unique_workflow_target(
                std::slice::from_ref(&target),
                "classroom.google.com",
                None,
                Some("other-profile")
            )
            .expect_err("wrong context")
            .code,
            "target_missing"
        );
        let mut duplicate = target.clone();
        duplicate.id = "tab-classroom-2".to_owned();
        assert_eq!(
            unique_workflow_target(
                &[target.clone(), duplicate],
                "classroom.google.com",
                None,
                None
            )
            .expect_err("ambiguous classroom pages")
            .code,
            "ambiguous_target"
        );
        let mut chrome_internal = target;
        chrome_internal.id = "chrome-internal".to_owned();
        chrome_internal.target_type = Some("other".to_owned());
        assert_eq!(
            unique_workflow_target(&[chrome_internal], "classroom.google.com", None, None)
                .expect_err("non-page target")
                .code,
            "target_missing"
        );

        let account_chooser = BrowserTarget {
            id: "tab-account-chooser".to_owned(),
            browser_context_id: Some("profile-school".to_owned()),
            target_type: Some("page".to_owned()),
            url: Some("https://accounts.google.com/v3/signin?continue=https%3A%2F%2Fclassroom.google.com%2Fu%2F2%2Fh%2Fst".to_owned()),
            title: Some("Choose an account".to_owned()),
            revision: Some("url:account-chooser".to_owned()),
            web_socket_url: None,
        };
        assert_eq!(
            unique_workflow_target(&[account_chooser], "classroom.google.com", None, None)
                .expect_err("account chooser must not match Classroom destination query")
                .code,
            "target_missing"
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
        let source = dir.join("sandbox/note.txt");
        fs::create_dir_all(source.parent().expect("parent")).expect("checkpoint dir");
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
        let ping = report["readiness"]["capabilities"]
            .as_array()
            .expect("readiness list")
            .iter()
            .find(|entry| entry["intent"] == "system.ping")
            .expect("ping readiness");
        assert_eq!(ping["live_status"], "not_tested_here");
        assert_eq!(ping["environment_available"], true);
        runtime.operate(OperationRequest {
            intent: "system.ping".to_owned(),
            target: None,
            params: Value::Null,
            postcondition: None,
            risk: Some(Risk::R0),
            idempotency_key: None,
            dry_run: false,
            background: None,
        });
        let report = runtime.inspect("doctor");
        let ping = report["readiness"]["capabilities"]
            .as_array()
            .expect("readiness list")
            .iter()
            .find(|entry| entry["intent"] == "system.ping")
            .expect("ping readiness");
        assert_eq!(ping["live_status"], "verified_here");
        let consent = runtime.inspect("consent");
        assert!(consent["human_actions"]["pending"].is_u64());
    }

    #[test]
    fn corrupt_consent_store_fails_closed_and_reports_reason() {
        let dir = std::env::temp_dir().join(format!("comptrol-consent-corrupt-{}", now_ms()));
        fs::create_dir_all(&dir).expect("state");
        fs::write(dir.join("consent.jsonl"), b"not valid json\n").expect("corrupt store");
        let mut runtime = Runtime::new(dir.clone()).expect("runtime keeps running");
        allow(&mut runtime, "software.install", Risk::R3);
        let result = runtime.operate(OperationRequest {
            intent: "software.install".to_owned(),
            target: Some(Target {
                kind: "package".to_owned(),
                id: Some("test:package".to_owned()),
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
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("consent_store_unavailable")
        );
        let report = runtime.inspect("doctor");
        assert_eq!(report["consent"]["store"]["available"], json!(false));
        assert!(
            report["consent"]["store"]["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty())
        );
        let _ = fs::remove_dir_all(dir);
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
            let params = if intent == "app.open_resource" {
                json!({
                    "app":"example-app-id",
                    "resource":{"kind":"url","url":"https://example.com/"}
                })
            } else {
                json!({})
            };
            let result = operate(&mut runtime, intent, params, Risk::R3);
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
        let disconnected = operate(
            &mut runtime,
            "browser.session.connect",
            json!({"provider": "companion_extension"}),
            Risk::R2,
        );
        // The extension is registered on this machine, but this isolated
        // runtime must not borrow the user's default bridge state or connect
        // to their live browser session.
        assert_eq!(disconnected.error, None);
        assert_eq!(disconnected.verification, VerificationState::Unverified);
        assert_eq!(disconnected.data["status"], "registered_but_inactive");
        assert_eq!(disconnected.data["bridge_health"]["active"], false);
    }
}
