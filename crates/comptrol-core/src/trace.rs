use crate::{ActionResult, OperationRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceMode {
    PrivacyMinimal,
    Developer,
    FixtureFull,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TraceEntry {
    pub request: OperationRequest,
    pub result: ActionResult,
    pub mode: TraceMode,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CompiledWorkflow {
    pub workflow_id: String,
    pub workflow_version: u32,
    pub intent: String,
    pub steps: Vec<CompiledStep>,
    pub preconditions: Vec<WorkflowPrecondition>,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CompiledStep {
    pub intent: String,
    pub params: Value,
    pub postcondition: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct WorkflowPrecondition {
    pub key: String,
    pub expected: Value,
}

pub fn compile_verified_trace(
    entries: &[TraceEntry],
    workflow_id: &str,
) -> Result<CompiledWorkflow, crate::ComptrolError> {
    if workflow_id.trim().is_empty() || entries.is_empty() {
        return Err(crate::ComptrolError {
            code: "workflow_validation_failed".to_owned(),
            message: "A workflow id and at least one trace entry are required".to_owned(),
            recovery: Some("Provide a verified non-empty trace".to_owned()),
        });
    }
    if entries.iter().any(|entry| {
        !matches!(
            entry.result.verification,
            crate::VerificationState::Verified
        ) || matches!(
            entry.result.effect,
            crate::EffectState::Unknown | crate::EffectState::NotAttempted
        ) || entry.result.error.is_some()
    }) {
        return Err(crate::ComptrolError {
            code: "workflow_validation_failed".to_owned(),
            message: "Only successful independently verified trace entries can be compiled"
                .to_owned(),
            recovery: Some("Capture a clean verified trace before compiling".to_owned()),
        });
    }
    let steps = entries
        .iter()
        .map(|entry| CompiledStep {
            intent: entry.request.intent.clone(),
            params: public_params(&entry.request.params),
            postcondition: entry.request.postcondition.clone(),
        })
        .collect::<Vec<_>>();
    let mut preconditions = Vec::new();
    for entry in entries {
        if let Some(target) = &entry.request.target {
            for (key, expected) in [
                ("target.kind", json!(target.kind)),
                (
                    "target.id",
                    target.id.clone().map_or(Value::Null, Value::String),
                ),
                (
                    "target.name",
                    target.name.clone().map_or(Value::Null, Value::String),
                ),
            ] {
                if expected != Value::Null {
                    preconditions.push(WorkflowPrecondition {
                        key: key.to_owned(),
                        expected,
                    });
                }
            }
        }
        for key in ["target_id", "browser_context_id", "revision"] {
            if let Some(value) = entry.request.params.get(key).and_then(Value::as_str) {
                preconditions.push(WorkflowPrecondition {
                    key: key.to_owned(),
                    expected: json!(value),
                });
            }
        }
    }
    preconditions.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then(left.expected.to_string().cmp(&right.expected.to_string()))
    });
    preconditions.dedup();
    let intent = entries[0].request.intent.clone();
    let fingerprint = workflow_fingerprint(&intent, &steps, &preconditions);
    Ok(CompiledWorkflow {
        workflow_id: workflow_id.to_owned(),
        workflow_version: 1,
        intent,
        steps,
        preconditions,
        fingerprint,
    })
}

pub fn validate_compiled_workflow(
    workflow: &CompiledWorkflow,
    observed: &Value,
) -> Result<(), crate::ComptrolError> {
    if workflow.workflow_version != 1
        || workflow.fingerprint
            != workflow_fingerprint(&workflow.intent, &workflow.steps, &workflow.preconditions)
    {
        return Err(crate::ComptrolError {
            code: "workflow_stale".to_owned(),
            message: "Compiled workflow fingerprint or version is stale".to_owned(),
            recovery: Some("Recompile from a fresh verified trace".to_owned()),
        });
    }
    for precondition in &workflow.preconditions {
        let actual = observed_value(observed, &precondition.key);
        if actual != Some(&precondition.expected) {
            return Err(crate::ComptrolError {
                code: "workflow_precondition_failed".to_owned(),
                message: format!("Workflow precondition failed for {}", precondition.key),
                recovery: Some("Observe the current target and use a cold route".to_owned()),
            });
        }
    }
    Ok(())
}

fn workflow_fingerprint(
    intent: &str,
    steps: &[CompiledStep],
    preconditions: &[WorkflowPrecondition],
) -> String {
    let payload = serde_json::to_vec(&(intent, steps, preconditions)).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    payload.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn observed_value<'a>(observed: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(value) = observed.get(key) {
        return Some(value);
    }
    key.split('.')
        .try_fold(observed, |value, part| value.get(part))
}

fn public_params(params: &Value) -> Value {
    let mut copy = params.clone();
    if let Value::Object(values) = &mut copy {
        for key in [
            "content",
            "value",
            "text",
            "body",
            "credential",
            "password",
            "token",
        ] {
            if values.contains_key(key) {
                values.insert(key.to_owned(), json!({"redacted": true}));
            }
        }
    }
    copy
}

#[derive(Clone, Debug)]
pub struct TraceRecorder {
    path: PathBuf,
    mode: TraceMode,
}

impl TraceRecorder {
    pub fn open(path: PathBuf, mode: TraceMode) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { path, mode })
    }

    pub fn append(&self, request: &OperationRequest, result: &ActionResult) -> io::Result<()> {
        let entry = TraceEntry {
            request: sanitize_request(request, self.mode),
            result: result.clone(),
            mode: self.mode,
        };
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        serde_json::to_writer(&mut file, &entry)?;
        file.write_all(b"\n")?;
        file.flush()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn read_trace(path: &Path) -> io::Result<Vec<TraceEntry>> {
    let file = std::fs::File::open(path)?;
    Ok(BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect())
}

fn sanitize_request(request: &OperationRequest, mode: TraceMode) -> OperationRequest {
    if matches!(mode, TraceMode::FixtureFull) {
        return request.clone();
    }
    let mut sanitized = request.clone();
    if let Value::Object(params) = &mut sanitized.params {
        for key in ["content", "value", "text", "body"] {
            if params.contains_key(key) {
                params.insert(key.to_owned(), json!({"redacted": true}));
            }
        }
    }
    if matches!(mode, TraceMode::PrivacyMinimal) {
        sanitized.postcondition = None;
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ActionResult, DeliveryState, EffectState, RecoveryState, Risk, Target, VerificationState,
    };

    fn verified_entry() -> TraceEntry {
        TraceEntry {
            request: OperationRequest {
                intent: "browser.cdp.click".to_owned(),
                target: Some(Target {
                    kind: "page".to_owned(),
                    id: Some("page-1".to_owned()),
                    name: None,
                }),
                params: json!({"target_id":"page-1", "revision":"r1", "value":"private"}),
                postcondition: Some(json!({"url":"https://example.test/done"})),
                risk: Some(Risk::R2),
                idempotency_key: Some("trace-1".to_owned()),
                dry_run: false,
                background: None,
            },
            result: ActionResult {
                operation_id: "op-1".to_owned(),
                intent: "browser.cdp.click".to_owned(),
                route: "browser_protocol".to_owned(),
                target: None,
                preflight: "passed".to_owned(),
                delivery: DeliveryState::Delivered,
                effect: EffectState::Changed,
                verification: VerificationState::Verified,
                disturbance: json!({"foreground_changed":false}),
                recovery: RecoveryState::None,
                data: Value::Null,
                error: None,
            },
            mode: TraceMode::FixtureFull,
        }
    }

    #[test]
    fn compiler_redacts_private_values_and_emits_preconditions() {
        let workflow =
            compile_verified_trace(&[verified_entry()], "bills-article").expect("compile");
        assert_eq!(workflow.workflow_version, 1);
        assert_eq!(workflow.steps[0].params["value"]["redacted"], true);
        assert!(
            workflow
                .preconditions
                .iter()
                .any(|item| item.key == "target.id")
        );
        assert!(
            workflow
                .preconditions
                .iter()
                .any(|item| item.key == "revision")
        );
        assert!(!workflow.fingerprint.is_empty());
    }

    #[test]
    fn validator_rejects_stale_fingerprint_and_changed_precondition() {
        let workflow = compile_verified_trace(&[verified_entry()], "stale-check").expect("compile");
        let mut stale = workflow.clone();
        stale.fingerprint = "0000000000000000".to_owned();
        assert_eq!(
            validate_compiled_workflow(&stale, &json!({}))
                .unwrap_err()
                .code,
            "workflow_stale"
        );
        assert_eq!(
            validate_compiled_workflow(
                &workflow,
                &json!({"target":{"id":"other"}, "revision":"r1"})
            )
            .unwrap_err()
            .code,
            "workflow_precondition_failed"
        );
        assert!(
            validate_compiled_workflow(
                &workflow,
                &json!({"target":{"kind":"page","id":"page-1"}, "target_id":"page-1", "revision":"r1"})
            )
            .is_ok()
        );
    }
}
