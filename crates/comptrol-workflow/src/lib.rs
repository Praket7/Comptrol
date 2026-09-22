#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorkflowParameter {
    pub name: String,
    pub parameter_type: String,
    pub sensitive: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorkflowTemplate {
    pub version: u32,
    pub intent: String,
    pub parameters: Vec<WorkflowParameter>,
    pub node: Value,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Workflow {
    pub id: String,
    pub version: u32,
    pub intent: String,
    pub parameters: Vec<WorkflowParameter>,
    pub fingerprint: String,
    pub start: String,
    pub nodes: BTreeMap<String, WorkflowNode>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReplayEvidence {
    pub clean_fixture: bool,
    pub independent_verification: bool,
    pub verified_runs: u32,
    pub fingerprint: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PromotionError {
    #[error("workflow replay did not use a clean fixture")]
    NotClean,
    #[error("workflow replay lacks independent verification")]
    NotIndependentlyVerified,
    #[error("workflow replay has too few verified runs")]
    TooFewVerifiedRuns,
    #[error("workflow replay fingerprint does not match the candidate")]
    FingerprintMismatch,
}

/// Promote only a candidate that was repeatedly verified in a clean fixture.
/// Promotion creates a new workflow version and never rewrites the candidate.
pub fn promote_candidate(
    candidate: &Workflow,
    evidence: &ReplayEvidence,
    minimum_verified_runs: u32,
) -> Result<Workflow, PromotionError> {
    if !evidence.clean_fixture {
        return Err(PromotionError::NotClean);
    }
    if !evidence.independent_verification {
        return Err(PromotionError::NotIndependentlyVerified);
    }
    if evidence.verified_runs < minimum_verified_runs.max(1) {
        return Err(PromotionError::TooFewVerifiedRuns);
    }
    if evidence.fingerprint != candidate.fingerprint {
        return Err(PromotionError::FingerprintMismatch);
    }
    let mut promoted = candidate.clone();
    promoted.version = candidate.version.saturating_add(1);
    promoted.id = format!("{}-promoted-v{}", candidate.id, promoted.version);
    Ok(promoted)
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PromotedWorkflowHost {
    pub workflows: BTreeMap<String, Workflow>,
}

/// Small durable host registry for promoted workflows. The runtime may place
/// this file inside its SQLite-backed state directory; the atomic replace
/// keeps a crash from producing a half-written active workflow set.
#[derive(Debug)]
pub struct WorkflowHostStore {
    path: PathBuf,
}

impl WorkflowHostStore {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            fs::write(&path, br#"{"workflows":{}}"#)?;
        }
        Ok(Self { path })
    }

    pub fn load(&self) -> io::Result<PromotedWorkflowHost> {
        let bytes = fs::read(&self.path)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }

    pub fn get(&self, id: &str) -> io::Result<Option<Workflow>> {
        let mut host = self.load()?;
        Ok(host.workflows.remove(id))
    }

    pub fn put(&self, workflow: Workflow) -> io::Result<()> {
        validate_workflow(&workflow).map_err(io::Error::other)?;
        let mut host = self.load()?;
        host.workflows.insert(workflow.id.clone(), workflow);
        let bytes = serde_json::to_vec_pretty(&host).map_err(io::Error::other)?;
        let temporary = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let mut file = fs::File::create(&temporary)?;
        use std::io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temporary, &self.path)
    }
}

/// Repair is deliberately explicit: a cold route supplies a replacement
/// candidate, and the active workflow is never silently rewritten in place.
pub fn repair_candidate(original: &Workflow, replacement: Workflow) -> Result<Workflow, String> {
    validate_workflow(&replacement)?;
    if original.intent != replacement.intent || original.parameters != replacement.parameters {
        return Err("repaired workflow changed intent or parameter schema".to_owned());
    }
    let mut repaired = replacement;
    repaired.version = original.version.saturating_add(1);
    repaired.id = format!("{}-repaired-v{}", original.id, repaired.version);
    Ok(repaired)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowNode {
    Observe {
        intent: String,
        next: Option<String>,
    },
    Assert {
        condition: Value,
        next: Option<String>,
    },
    Act {
        intent: String,
        params: Value,
        next: Option<String>,
    },
    Wait {
        event: String,
        timeout_ms: u64,
        next: Option<String>,
    },
    Verify {
        criterion: Value,
        next: Option<String>,
    },
    Checkpoint {
        name: String,
        next: Option<String>,
    },
    Branch {
        condition: Value,
        if_true: String,
        if_false: String,
    },
    Loop {
        body: String,
        next: Option<String>,
        max_iterations: u32,
    },
    ParallelRead {
        nodes: Vec<String>,
        next: Option<String>,
    },
    Return {
        value: Value,
    },
}

pub fn validate_workflow(workflow: &Workflow) -> Result<(), String> {
    if workflow.version == 0 || workflow.id.is_empty() || workflow.intent.is_empty() {
        return Err("workflow identity and positive version are required".to_owned());
    }
    if !workflow.nodes.contains_key(&workflow.start) {
        return Err("workflow start node is missing".to_owned());
    }
    if workflow.nodes.values().any(contains_executable_code) {
        return Err("workflow nodes cannot contain arbitrary executable code".to_owned());
    }
    for (id, node) in &workflow.nodes {
        if let WorkflowNode::Loop { max_iterations, .. } = node
            && (*max_iterations == 0 || *max_iterations > 10_000)
        {
            return Err(format!("loop {id} has an unsafe iteration bound"));
        }
        for next in node_edges(node) {
            if !workflow.nodes.contains_key(next) {
                return Err(format!("node {id} references missing node {next}"));
            }
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExecutionError {
    #[error("workflow node is missing: {0}")]
    MissingNode(String),
    #[error("workflow execution exceeded its step budget")]
    StepBudgetExceeded,
    #[error("workflow action failed at {node}: {message}")]
    ActionFailed { node: String, message: String },
    #[error("workflow verification failed at {node}")]
    VerificationFailed { node: String },
    #[error("workflow wait failed at {node}: {message}")]
    WaitFailed { node: String, message: String },
    #[error("workflow execution was cancelled")]
    Cancelled,
}

/// Execute a validated typed workflow without evaluating caller-supplied code.
///
/// Application-specific work remains behind callbacks owned by the host. The
/// executor only controls the closed state-machine transitions and enforces a
/// finite step budget, so a repaired or malformed workflow cannot run forever.
pub struct WorkflowExecutor<A, V, W>
where
    A: FnMut(&str, &Value) -> Result<Value, String>,
    V: FnMut(&Value, &Value) -> bool,
    W: FnMut(&str, u64) -> Result<(), String>,
{
    pub action: A,
    pub verify: V,
    pub wait: W,
    pub max_steps: usize,
}

impl<A, V, W> WorkflowExecutor<A, V, W>
where
    A: FnMut(&str, &Value) -> Result<Value, String>,
    V: FnMut(&Value, &Value) -> bool,
    W: FnMut(&str, u64) -> Result<(), String>,
{
    pub fn run(&mut self, workflow: &Workflow) -> Result<Value, ExecutionError> {
        self.run_with_cancel(workflow, || false)
    }

    /// Execute with a host-owned cancellation check. The callback is checked
    /// before every state transition, including bounded waits and branches,
    /// so task cancellation cannot leave a workflow running between nodes.
    pub fn run_with_cancel<C>(
        &mut self,
        workflow: &Workflow,
        mut cancelled: C,
    ) -> Result<Value, ExecutionError>
    where
        C: FnMut() -> bool,
    {
        validate_workflow(workflow).map_err(|message| ExecutionError::ActionFailed {
            node: workflow.start.clone(),
            message,
        })?;
        let mut current = workflow.start.clone();
        let mut last = Value::Null;
        let mut steps = 0usize;
        let mut loop_counts = BTreeMap::<String, u32>::new();
        loop {
            if cancelled() {
                return Err(ExecutionError::Cancelled);
            }
            steps = steps.saturating_add(1);
            if steps > self.max_steps.max(1) {
                return Err(ExecutionError::StepBudgetExceeded);
            }
            let node = workflow
                .nodes
                .get(&current)
                .ok_or_else(|| ExecutionError::MissingNode(current.clone()))?;
            match node {
                WorkflowNode::Observe { intent, next } => {
                    last = (self.action)(intent, &Value::Null).map_err(|message| {
                        ExecutionError::ActionFailed {
                            node: current.clone(),
                            message,
                        }
                    })?;
                    current = next
                        .clone()
                        .ok_or_else(|| ExecutionError::MissingNode(current.clone()))?;
                }
                WorkflowNode::Act {
                    intent,
                    params,
                    next,
                } => {
                    last = (self.action)(intent, params).map_err(|message| {
                        ExecutionError::ActionFailed {
                            node: current.clone(),
                            message,
                        }
                    })?;
                    current = next
                        .clone()
                        .ok_or_else(|| ExecutionError::MissingNode(current.clone()))?;
                }
                WorkflowNode::Assert { condition, next } => {
                    if !(self.verify)(condition, &last) {
                        return Err(ExecutionError::VerificationFailed { node: current });
                    }
                    current = next
                        .clone()
                        .ok_or_else(|| ExecutionError::MissingNode(current.clone()))?;
                }
                WorkflowNode::Verify { criterion, next } => {
                    if !(self.verify)(criterion, &last) {
                        return Err(ExecutionError::VerificationFailed { node: current });
                    }
                    current = next
                        .clone()
                        .ok_or_else(|| ExecutionError::MissingNode(current.clone()))?;
                }
                WorkflowNode::Wait {
                    event,
                    timeout_ms,
                    next,
                } => {
                    (self.wait)(event, *timeout_ms).map_err(|message| {
                        ExecutionError::WaitFailed {
                            node: current.clone(),
                            message,
                        }
                    })?;
                    current = next
                        .clone()
                        .ok_or_else(|| ExecutionError::MissingNode(current.clone()))?;
                }
                WorkflowNode::Checkpoint { next, .. } => {
                    current = next
                        .clone()
                        .ok_or_else(|| ExecutionError::MissingNode(current.clone()))?;
                }
                WorkflowNode::Branch {
                    if_true,
                    if_false,
                    condition,
                } => {
                    current = if (self.verify)(condition, &last) {
                        if_true.clone()
                    } else {
                        if_false.clone()
                    };
                }
                WorkflowNode::Loop {
                    body,
                    next,
                    max_iterations,
                } => {
                    let count = loop_counts.entry(current.clone()).or_default();
                    if *count >= *max_iterations {
                        current = next
                            .clone()
                            .ok_or_else(|| ExecutionError::MissingNode(current.clone()))?;
                    } else {
                        *count += 1;
                        current = body.clone();
                    }
                }
                WorkflowNode::ParallelRead { nodes, next } => {
                    for node_id in nodes {
                        let observe = workflow
                            .nodes
                            .get(node_id)
                            .ok_or_else(|| ExecutionError::MissingNode(node_id.clone()))?;
                        if let WorkflowNode::Observe { intent, .. } = observe {
                            last = (self.action)(intent, &Value::Null).map_err(|message| {
                                ExecutionError::ActionFailed {
                                    node: node_id.clone(),
                                    message,
                                }
                            })?;
                        } else {
                            return Err(ExecutionError::ActionFailed {
                                node: node_id.clone(),
                                message: "parallel_read accepts Observe nodes only".to_owned(),
                            });
                        }
                    }
                    current = next
                        .clone()
                        .ok_or_else(|| ExecutionError::MissingNode(current.clone()))?;
                }
                WorkflowNode::Return { value } => {
                    return Ok(if value.is_null() { last } else { value.clone() });
                }
            }
        }
    }
}

fn node_edges(node: &WorkflowNode) -> Vec<&str> {
    match node {
        WorkflowNode::Observe { next, .. }
        | WorkflowNode::Assert { next, .. }
        | WorkflowNode::Act { next, .. }
        | WorkflowNode::Wait { next, .. }
        | WorkflowNode::Verify { next, .. }
        | WorkflowNode::Checkpoint { next, .. }
        | WorkflowNode::ParallelRead { next, .. } => next.iter().map(String::as_str).collect(),
        WorkflowNode::Loop { body, next, .. } => {
            let mut edges = vec![body.as_str()];
            edges.extend(next.iter().map(String::as_str));
            edges
        }
        WorkflowNode::Branch {
            if_true, if_false, ..
        } => vec![if_true, if_false],
        WorkflowNode::Return { .. } => Vec::new(),
    }
}

fn contains_executable_code(node: &WorkflowNode) -> bool {
    let values = match node {
        WorkflowNode::Assert { condition, .. }
        | WorkflowNode::Verify {
            criterion: condition,
            ..
        }
        | WorkflowNode::Branch { condition, .. }
        | WorkflowNode::Return { value: condition } => vec![condition],
        WorkflowNode::Act { params, .. } => vec![params],
        _ => Vec::new(),
    };
    values.iter().any(|value| {
        value.get("python").is_some()
            || value.get("javascript").is_some()
            || value.get("shell").is_some()
            || value.get("eval").is_some()
    })
}

pub fn lift_parameter(
    params: &mut Map<String, Value>,
    field: &str,
    name: &str,
    parameter_type: &str,
) -> Option<WorkflowParameter> {
    let value = params.get(field)?.clone();
    if !value.is_string() && !value.is_number() && !value.is_boolean() {
        return None;
    }
    params.insert(field.to_owned(), serde_json::json!({ "param": name }));
    Some(WorkflowParameter {
        name: name.to_owned(),
        parameter_type: parameter_type.to_owned(),
        sensitive: true,
    })
}

pub fn structural_fingerprint(value: &Value) -> String {
    let canonical = canonical_json(value);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{}", hex_lower(&hasher.finalize()))
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort();
            let fields = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        canonical_json(&object[key])
                    )
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(array) => format!(
            "[{}]",
            array
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => value.to_string(),
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_lifting_removes_value_from_template() {
        let mut params =
            Map::from_iter([(String::from("value"), Value::String("secret".to_owned()))]);
        let parameter = lift_parameter(&mut params, "value", "name_value", "string").unwrap();
        assert_eq!(parameter.name, "name_value");
        assert_eq!(
            params["value"],
            serde_json::json!({ "param": "name_value" })
        );
    }

    #[test]
    fn fingerprint_is_order_independent_and_cryptographic() {
        let left = serde_json::json!({"b": 2, "a": 1});
        let right = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(
            structural_fingerprint(&left),
            structural_fingerprint(&right)
        );
        assert!(structural_fingerprint(&left).starts_with("sha256:"));
    }

    #[test]
    fn typed_workflow_rejects_unbounded_loops_and_code_payloads() {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "start".to_owned(),
            WorkflowNode::Loop {
                body: "start".to_owned(),
                next: None,
                max_iterations: 0,
            },
        );
        let workflow = Workflow {
            id: "demo".to_owned(),
            version: 1,
            intent: "demo".to_owned(),
            parameters: Vec::new(),
            fingerprint: "sha256:test".to_owned(),
            start: "start".to_owned(),
            nodes,
        };
        assert!(validate_workflow(&workflow).is_err());

        let mut nodes = BTreeMap::new();
        nodes.insert(
            "start".to_owned(),
            WorkflowNode::Act {
                intent: "browser.click".to_owned(),
                params: serde_json::json!({"javascript": "alert(1)"}),
                next: None,
            },
        );
        let workflow = Workflow { nodes, ..workflow };
        assert!(validate_workflow(&workflow).is_err());
    }

    #[test]
    fn executor_runs_typed_action_and_verification_before_return() {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "act".to_owned(),
            WorkflowNode::Act {
                intent: "fixture.write".to_owned(),
                params: serde_json::json!({"value": 7}),
                next: Some("verify".to_owned()),
            },
        );
        nodes.insert(
            "verify".to_owned(),
            WorkflowNode::Verify {
                criterion: serde_json::json!({"value": 7}),
                next: Some("return".to_owned()),
            },
        );
        nodes.insert(
            "return".to_owned(),
            WorkflowNode::Return { value: Value::Null },
        );
        let workflow = Workflow {
            id: "fixture".to_owned(),
            version: 1,
            intent: "fixture.write".to_owned(),
            parameters: Vec::new(),
            fingerprint: "sha256:test".to_owned(),
            start: "act".to_owned(),
            nodes,
        };
        let mut calls = 0;
        let mut executor = WorkflowExecutor {
            action: |intent: &str, params: &Value| {
                calls += 1;
                assert_eq!(intent, "fixture.write");
                Ok(params.clone())
            },
            verify: |criterion: &Value, observed: &Value| criterion == observed,
            wait: |_event: &str, _timeout: u64| Ok(()),
            max_steps: 10,
        };
        assert_eq!(
            executor.run(&workflow).unwrap(),
            serde_json::json!({"value": 7})
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn promotion_requires_clean_independent_replay() {
        let workflow = Workflow {
            id: "candidate".to_owned(),
            version: 1,
            intent: "fixture.read".to_owned(),
            parameters: Vec::new(),
            fingerprint: "sha256:fixture".to_owned(),
            start: "return".to_owned(),
            nodes: BTreeMap::from([(
                "return".to_owned(),
                WorkflowNode::Return { value: Value::Null },
            )]),
        };
        let evidence = ReplayEvidence {
            clean_fixture: true,
            independent_verification: true,
            verified_runs: 3,
            fingerprint: workflow.fingerprint.clone(),
        };
        let promoted = promote_candidate(&workflow, &evidence, 3).unwrap();
        assert_eq!(promoted.version, 2);
        assert_ne!(promoted.id, workflow.id);
        assert_eq!(workflow.version, 1);
    }

    #[test]
    fn promotion_rejects_fingerprint_drift() {
        let workflow = Workflow {
            id: "candidate".to_owned(),
            version: 1,
            intent: "fixture.read".to_owned(),
            parameters: Vec::new(),
            fingerprint: "sha256:fixture".to_owned(),
            start: "return".to_owned(),
            nodes: BTreeMap::from([(
                "return".to_owned(),
                WorkflowNode::Return { value: Value::Null },
            )]),
        };
        let evidence = ReplayEvidence {
            clean_fixture: true,
            independent_verification: true,
            verified_runs: 3,
            fingerprint: "sha256:changed".to_owned(),
        };
        assert_eq!(
            promote_candidate(&workflow, &evidence, 3),
            Err(PromotionError::FingerprintMismatch)
        );
    }

    #[test]
    fn promoted_workflow_host_survives_reopen() {
        let path = std::env::temp_dir().join(format!(
            "comptrol-workflow-host-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workflow = Workflow {
            id: "hosted".to_owned(),
            version: 2,
            intent: "fixture.read".to_owned(),
            parameters: Vec::new(),
            fingerprint: "sha256:hosted".to_owned(),
            start: "return".to_owned(),
            nodes: BTreeMap::from([(
                "return".to_owned(),
                WorkflowNode::Return { value: Value::Null },
            )]),
        };
        let store = WorkflowHostStore::open(&path).unwrap();
        store.put(workflow.clone()).unwrap();
        drop(store);
        let reopened = WorkflowHostStore::open(&path).unwrap();
        assert_eq!(reopened.get("hosted").unwrap(), Some(workflow));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn workflow_cancellation_stops_before_next_transition() {
        let workflow = Workflow {
            id: "cancelled".to_owned(),
            version: 1,
            intent: "fixture.read".to_owned(),
            parameters: Vec::new(),
            fingerprint: "sha256:cancelled".to_owned(),
            start: "return".to_owned(),
            nodes: BTreeMap::from([(
                "return".to_owned(),
                WorkflowNode::Return { value: Value::Null },
            )]),
        };
        let mut executor = WorkflowExecutor {
            action: |_intent: &str, _params: &Value| Ok(Value::Null),
            verify: |_criterion: &Value, _observed: &Value| true,
            wait: |_event: &str, _timeout: u64| Ok(()),
            max_steps: 4,
        };
        assert_eq!(
            executor.run_with_cancel(&workflow, || true),
            Err(ExecutionError::Cancelled)
        );
    }
}
