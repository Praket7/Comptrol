#![deny(unsafe_code)]

//! Typed, parameterized, cryptographically fingerprinted Comptrol
//! workflows. This crate owns the only workflow executors: the node graph
//! (`WorkflowExecutor`), the speculative batch runner (`run_speculative`),
//! the flat op list (`execute_ops`), and the promoted recipe catalog
//! (`recipes`).

pub mod recipes;

use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
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
/// The flat op form accepted by `workflow.execute`. It lives here, beside
/// the node graph executor, so both workflow dialects run in one crate
/// instead of two interpreters drifting apart in the runtime.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkflowOp {
    Sense { key: String, value: Value },
    Assert { key: String, equals: Value },
    Set { key: String, value: Value },
    Wait { milliseconds: u64 },
    Return { value: Value },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OpError {
    #[error("workflow assertion failed for {0}")]
    AssertionFailed(String),
    #[error("workflow op exceeded the {0}ms wait ceiling")]
    WaitTooLong(u64),
    #[error("workflow was cancelled before the next op")]
    Cancelled,
}

/// The longest single op sleep. A caller can ask for more, but the
/// interpreter clamps instead of stalling the caller indefinitely.
pub const MAX_OP_WAIT_MS: u64 = 60_000;

/// Interpret the flat op form against a bounded memory map.
///
/// This is the same executor the node graph uses, expressed as straight
/// line ops for callers that do not need branching. It is a pure function
/// of `ops` and the sleep hook, so a test can pass a no-op waiter and
/// never actually block.
pub fn execute_ops<W>(ops: &[WorkflowOp], mut wait: W) -> Result<Value, OpError>
where
    W: FnMut(u64) -> Result<(), OpError>,
{
    let mut memory = BTreeMap::<String, Value>::new();
    let mut index = 0usize;
    while index < ops.len() {
        match &ops[index] {
            WorkflowOp::Sense { key, value } | WorkflowOp::Set { key, value } => {
                memory.insert(key.clone(), value.clone());
                index += 1;
            }
            WorkflowOp::Assert { key, equals } => {
                if memory.get(key) != Some(equals) {
                    return Err(OpError::AssertionFailed(key.clone()));
                }
                index += 1;
            }
            WorkflowOp::Wait { milliseconds } => {
                let requested = *milliseconds;
                if requested > MAX_OP_WAIT_MS {
                    return Err(OpError::WaitTooLong(requested));
                }
                wait(requested)?;
                index += 1;
            }
            WorkflowOp::Return { value } => return Ok(value.clone()),
        }
    }
    Ok(Value::Null)
}

/// The result of an op list plus the memory it left behind, so a caller
/// can assert on intermediate state rather than only the return value.
pub fn execute_ops_traced<W>(
    ops: &[WorkflowOp],
    mut wait: W,
) -> (Result<Value, OpError>, BTreeMap<String, Value>)
where
    W: FnMut(u64) -> Result<(), OpError>,
{
    let mut memory = BTreeMap::<String, Value>::new();
    let mut index = 0usize;
    let outcome = loop {
        let Some(op) = ops.get(index) else {
            break Ok(Value::Null);
        };
        match op {
            WorkflowOp::Sense { key, value } | WorkflowOp::Set { key, value } => {
                memory.insert(key.clone(), value.clone());
            }
            WorkflowOp::Assert { key, equals } => {
                if memory.get(key) != Some(equals) {
                    break Err(OpError::AssertionFailed(key.clone()));
                }
            }
            WorkflowOp::Wait { milliseconds } => {
                if *milliseconds > MAX_OP_WAIT_MS {
                    break Err(OpError::WaitTooLong(*milliseconds));
                }
                if let Err(error) = wait(*milliseconds) {
                    break Err(error);
                }
            }
            WorkflowOp::Return { value } => break Ok(value.clone()),
        }
        index += 1;
    };
    (outcome, memory)
}

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

// ---------------------------------------------------------------------------
// P4.2 speculative batches: N steps with per-step postconditions and rollback
// hints, executed optimistically. On failure the caller receives the durable
// state of every executed step plus a reconcile plan instead of an opaque
// error - this is the "Classroom in 1 operate call" lever from BUILD_PLAN_V3.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SpeculativeStep {
    pub intent: String,
    #[serde(default)]
    pub params: Value,
    /// Per-step postcondition; when present the step is only "verified" after
    /// the independent check passes.
    #[serde(default)]
    pub postcondition: Option<Value>,
    /// Rollback hint consumed by the reconcile plan when a later step fails.
    #[serde(default)]
    pub rollback_hint: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SpeculativeStepOutcome {
    pub index: usize,
    pub intent: String,
    /// "succeeded" | "failed" | "postcondition_failed" | "not_run"
    pub state: String,
    pub result: Value,
    pub postcondition_passed: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SpeculativeOutcome {
    /// "completed" | "failed_reconcilable"
    pub state: String,
    pub steps: Vec<SpeculativeStepOutcome>,
    pub failed_at: Option<usize>,
    /// Rollback hints for executed steps, newest first. Empty when nothing
    /// needs undoing.
    pub reconcile_plan: Vec<Value>,
}

impl SpeculativeOutcome {
    pub fn completed(&self) -> bool {
        self.state == "completed"
    }
}

/// Run a speculative batch optimistically with deadline and cancellation
/// checks between steps. `action` performs one step and returns its result;
/// `verify` evaluates a step postcondition against that result.
pub fn run_speculative<A, V, C>(
    steps: &[SpeculativeStep],
    deadline_ms: u64,
    mut action: A,
    mut verify: V,
    mut cancelled: C,
) -> SpeculativeOutcome
where
    A: FnMut(usize, &str, &Value) -> Result<Value, String>,
    V: FnMut(&Value, &Value) -> bool,
    C: FnMut() -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
    let mut outcomes = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        if cancelled() || std::time::Instant::now() >= deadline {
            return fail_speculative(outcomes, steps, index);
        }
        match action(index, &step.intent, &step.params) {
            Ok(result) => {
                let postcondition_passed = step
                    .postcondition
                    .as_ref()
                    .map(|criterion| verify(criterion, &result));
                if postcondition_passed == Some(false) {
                    outcomes.push(SpeculativeStepOutcome {
                        index,
                        intent: step.intent.clone(),
                        state: "postcondition_failed".to_owned(),
                        result,
                        postcondition_passed: Some(false),
                    });
                    return fail_speculative(outcomes, steps, index);
                }
                outcomes.push(SpeculativeStepOutcome {
                    index,
                    intent: step.intent.clone(),
                    state: "succeeded".to_owned(),
                    result,
                    postcondition_passed,
                });
            }
            Err(message) => {
                outcomes.push(SpeculativeStepOutcome {
                    index,
                    intent: step.intent.clone(),
                    state: "failed".to_owned(),
                    result: Value::String(message),
                    postcondition_passed: None,
                });
                return fail_speculative(outcomes, steps, index);
            }
        }
    }
    SpeculativeOutcome {
        state: "completed".to_owned(),
        steps: outcomes,
        failed_at: None,
        reconcile_plan: Vec::new(),
    }
}

fn fail_speculative(
    mut steps: Vec<SpeculativeStepOutcome>,
    planned: &[SpeculativeStep],
    failed_at: usize,
) -> SpeculativeOutcome {
    // Everything after the failure was never attempted; report it honestly.
    for (index, planned_step) in planned.iter().enumerate().skip(steps.len()) {
        steps.push(SpeculativeStepOutcome {
            index,
            intent: planned_step.intent.clone(),
            state: "not_run".to_owned(),
            result: Value::Null,
            postcondition_passed: None,
        });
    }
    // Reconcile plan: undo executed work newest-first using its rollback hint.
    // Steps without hints are reported as manual-review entries so the caller
    // never silently loses track of a change.
    let mut reconcile_plan = Vec::new();
    for outcome in steps.iter().rev() {
        // Succeeded steps changed state and get their rollback hint. The
        // failed step itself may have partially applied (dispatch-unknown),
        // so it is reported as manual review instead of being dropped.
        let rollbackable = outcome.state == "succeeded";
        if !rollbackable && outcome.state != "failed" && outcome.state != "postcondition_failed" {
            continue;
        }
        let mut entry = Map::from_iter([
            ("index".to_owned(), Value::from(outcome.index as u64)),
            ("intent".to_owned(), Value::String(outcome.intent.clone())),
        ]);
        match planned
            .get(outcome.index)
            .and_then(|step| step.rollback_hint.clone())
            .filter(|hint| !hint.is_null())
        {
            Some(hint) if rollbackable => {
                entry.insert("action".to_owned(), Value::String("rollback".to_owned()));
                entry.insert("rollback_hint".to_owned(), hint);
            }
            _ => {
                entry.insert(
                    "action".to_owned(),
                    Value::String("manual_review".to_owned()),
                );
            }
        }
        reconcile_plan.push(Value::Object(entry));
    }
    SpeculativeOutcome {
        state: "failed_reconcilable".to_owned(),
        steps,
        failed_at: Some(failed_at),
        reconcile_plan,
    }
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

    fn spec_step(intent: &str, rollback_hint: Option<Value>) -> SpeculativeStep {
        SpeculativeStep {
            intent: intent.to_owned(),
            params: Value::Null,
            postcondition: None,
            rollback_hint,
        }
    }

    #[test]
    fn speculative_batch_completes_and_returns_all_outcomes() {
        let steps = vec![spec_step("a", None), spec_step("b", None)];
        let outcome = run_speculative(
            &steps,
            5_000,
            |_index, intent, _params| Ok(Value::String(intent.to_owned())),
            |_criterion, _result| true,
            || false,
        );
        assert!(outcome.completed());
        assert_eq!(outcome.steps.len(), 2);
        assert_eq!(outcome.steps[0].state, "succeeded");
        assert!(outcome.reconcile_plan.is_empty());
    }

    #[test]
    fn speculative_failure_returns_executed_state_and_reconcile_plan() {
        let steps = vec![
            spec_step("first", Some(json!({"intent": "first.undo"}))),
            spec_step("second", None),
            spec_step("third", None),
        ];
        let outcome = run_speculative(
            &steps,
            5_000,
            |_index, intent, _params| {
                if intent == "second" {
                    Err("boom".to_owned())
                } else {
                    Ok(Value::Bool(true))
                }
            },
            |_criterion, _result| true,
            || false,
        );
        assert!(!outcome.completed());
        assert_eq!(outcome.state, "failed_reconcilable");
        assert_eq!(outcome.failed_at, Some(1));
        assert_eq!(outcome.steps[0].state, "succeeded");
        assert_eq!(outcome.steps[1].state, "failed");
        assert_eq!(outcome.steps[2].state, "not_run");
        // Newest first: second has no hint (manual review), first rolls back.
        assert_eq!(outcome.reconcile_plan.len(), 2);
        assert_eq!(outcome.reconcile_plan[0]["intent"], "second");
        assert_eq!(outcome.reconcile_plan[0]["action"], "manual_review");
        assert_eq!(outcome.reconcile_plan[1]["intent"], "first");
        assert_eq!(outcome.reconcile_plan[1]["action"], "rollback");
        assert_eq!(
            outcome.reconcile_plan[1]["rollback_hint"],
            json!({"intent": "first.undo"})
        );
    }

    #[test]
    fn speculative_postcondition_failure_stops_the_batch() {
        let mut criterion = spec_step("a", None);
        criterion.postcondition = Some(json!({"equals": 1}));
        let steps = vec![criterion, spec_step("b", None)];
        let outcome = run_speculative(
            &steps,
            5_000,
            |_index, _intent, _params| Ok(json!(2)),
            |criterion, result| criterion.get("equals") == Some(result),
            || false,
        );
        assert_eq!(outcome.state, "failed_reconcilable");
        assert_eq!(outcome.steps[0].state, "postcondition_failed");
        assert_eq!(outcome.steps[0].postcondition_passed, Some(false));
        assert_eq!(outcome.steps[1].state, "not_run");
    }

    #[test]
    fn speculative_cancellation_marks_remaining_steps_not_run() {
        let steps = vec![spec_step("a", None), spec_step("b", None)];
        let outcome = run_speculative(
            &steps,
            5_000,
            |_index, _intent, _params| Ok(Value::Null),
            |_criterion, _result| true,
            || true,
        );
        assert!(!outcome.completed());
        assert_eq!(outcome.steps.len(), 2);
        assert!(outcome.steps.iter().all(|step| step.state == "not_run"));
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

    #[test]
    fn flat_ops_execute_against_memory_and_return_early() {
        let ops = vec![
            WorkflowOp::Sense {
                key: "ready".to_owned(),
                value: json!(true),
            },
            WorkflowOp::Assert {
                key: "ready".to_owned(),
                equals: json!(true),
            },
            WorkflowOp::Return {
                value: json!({"done": true}),
            },
            // Unreachable: the interpreter must stop at the first return.
            WorkflowOp::Set {
                key: "after".to_owned(),
                value: json!(1),
            },
        ];
        let (outcome, memory) = execute_ops_traced(&ops, |_| Ok(()));
        assert_eq!(outcome.expect("ops return"), json!({"done": true}));
        assert_eq!(memory.get("ready"), Some(&json!(true)));
        assert!(!memory.contains_key("after"));
    }

    #[test]
    fn flat_ops_refuse_an_unsatisfied_assertion_and_report_the_key() {
        let ops = vec![
            WorkflowOp::Sense {
                key: "ready".to_owned(),
                value: json!(false),
            },
            WorkflowOp::Assert {
                key: "ready".to_owned(),
                equals: json!(true),
            },
        ];
        assert_eq!(
            execute_ops(&ops, |_| Ok(())),
            Err(OpError::AssertionFailed("ready".to_owned()))
        );
    }

    #[test]
    fn flat_ops_bound_the_wait_and_never_block_the_caller() {
        // The ceiling is enforced by refusing, not by silently sleeping for
        // the caller's unbounded request.
        assert_eq!(
            execute_ops(
                &[WorkflowOp::Wait {
                    milliseconds: MAX_OP_WAIT_MS + 1
                }],
                |_| Ok(())
            ),
            Err(OpError::WaitTooLong(MAX_OP_WAIT_MS + 1))
        );
        let mut requested = Vec::new();
        assert_eq!(
            execute_ops(
                &[
                    WorkflowOp::Wait { milliseconds: 10 },
                    WorkflowOp::Wait { milliseconds: 20 }
                ],
                |ms| {
                    requested.push(ms);
                    Ok(())
                }
            ),
            Ok(Value::Null)
        );
        assert_eq!(requested, vec![10, 20]);
    }

    #[test]
    fn flat_ops_propagate_a_wait_hook_failure() {
        assert_eq!(
            execute_ops(&[WorkflowOp::Wait { milliseconds: 5 }], |_| Err(
                OpError::Cancelled
            )),
            Err(OpError::Cancelled)
        );
    }
}
