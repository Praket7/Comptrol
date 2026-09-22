from pathlib import Path


def read(path):
    return Path(path).read_text(encoding="utf-8")


def write(path, text):
    Path(path).write_text(text, encoding="utf-8")


def replace_once(text, old, new, label):
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


workflow_path = "crates/comptrol-workflow/src/lib.rs"
workflow = read(workflow_path)
workflow = replace_once(
    workflow,
    "pub struct WorkflowHostStore {\n    path: PathBuf,\n}",
    "#[derive(Debug)]\npub struct WorkflowHostStore {\n    path: PathBuf,\n}",
    "WorkflowHostStore Debug derive",
)
write(workflow_path, workflow)

core_path = "crates/comptrol-core/src/lib.rs"
core = read(core_path)
core = replace_once(
    core,
    "use comptrol_workflow::{Workflow, WorkflowExecutor, WorkflowNode};",
    "use comptrol_workflow::{\n    ReplayEvidence, Workflow, WorkflowExecutor, WorkflowHostStore, WorkflowNode, promote_candidate,\n};",
    "workflow imports",
)

operation_request = '''#[derive(Clone, Debug, Deserialize, Serialize)]
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
'''
parallel_spec = operation_request + '''
#[derive(Clone, Debug, Deserialize)]
struct ParallelReadSpec {
    intent: String,
    #[serde(default)]
    target: Option<Target>,
    #[serde(default)]
    params: Value,
}
'''
core = replace_once(core, operation_request, parallel_spec, "ParallelReadSpec")

core = replace_once(
    core,
    "    route_stats_db: Connection,\n    sequence: u64,",
    "    route_stats_db: Connection,\n    workflow_host: WorkflowHostStore,\n    sequence: u64,",
    "runtime workflow host field",
)

core = replace_once(
    core,
    "        Ok(Self {\n            policy: Policy::from_environment(),",
    "        let workflow_host = WorkflowHostStore::open(state_dir.join(\"promoted-workflows.json\"))?;\n        Ok(Self {\n            policy: Policy::from_environment(),",
    "workflow host initialization",
)
core = replace_once(
    core,
    "            route_history,\n            route_stats_db,\n            sequence: 0,",
    "            route_history,\n            route_stats_db,\n            workflow_host,\n            sequence: 0,",
    "workflow host assignment",
)

# Contextual route history must persist under the same contextual key used in memory.
start = core.index("    fn record_route_outcome(")
end = core.index("    /// Execute an operation while exposing the task cancellation latch", start)
record_block = core[start:end]
for old, new, label in [
    ("params![result.route, latency_ms]", "params![route_key, latency_ms]", "latency sample route key"),
    ("params![result.route]", "params![route_key]", "latency prune route key"),
    ("                result.route,\n                entry.attempts as i64,", "                route_key,\n                entry.attempts as i64,", "route stats route key"),
]:
    if record_block.count(old) != 1:
        raise RuntimeError(f"{label}: expected one match, found {record_block.count(old)}")
    record_block = record_block.replace(old, new, 1)
core = core[:start] + record_block + core[end:]

# Expose promoted workflow inventory without dumping parameter values.
inspect_marker = '''            "platform" => platform_diagnostics(),
'''
inspect_workflows = '''            "workflows" => match self.workflow_host.load() {
                Ok(host) => json!({
                    "promoted": host.workflows.values().map(|workflow| json!({
                        "id": workflow.id,
                        "version": workflow.version,
                        "intent": workflow.intent,
                        "fingerprint": workflow.fingerprint,
                        "parameter_count": workflow.parameters.len(),
                    })).collect::<Vec<_>>()
                }),
                Err(error) => json!({ "error": error.to_string() }),
            },
            "platform" => platform_diagnostics(),
'''
core = replace_once(core, inspect_marker, inspect_workflows, "workflow inspection")

execute_marker = '''fn execute_workflow_request(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
) -> ActionResult {
'''
execute_prefix = execute_marker + '''    if let Some(promote) = request.params.get("promote") {
        let Some(candidate_value) = promote.get("candidate") else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Workflow promotion needs a candidate workflow".to_owned(),
                    recovery: None,
                },
            );
        };
        let Some(evidence_value) = promote.get("evidence") else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "Workflow promotion needs replay evidence".to_owned(),
                    recovery: None,
                },
            );
        };
        let Ok(candidate) = serde_json::from_value::<Workflow>(candidate_value.clone()) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "workflow_validation_failed".to_owned(),
                    message: "Promotion candidate did not match the closed workflow schema".to_owned(),
                    recovery: Some("Compile the candidate from a verified trace".to_owned()),
                },
            );
        };
        let Ok(evidence) = serde_json::from_value::<ReplayEvidence>(evidence_value.clone()) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "workflow_validation_failed".to_owned(),
                    message: "Replay evidence did not match the promotion schema".to_owned(),
                    recovery: None,
                },
            );
        };
        let minimum_verified_runs = promote
            .get("minimum_verified_runs")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 100) as u32;
        let promoted = match promote_candidate(&candidate, &evidence, minimum_verified_runs) {
            Ok(promoted) => promoted,
            Err(error) => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "workflow_promotion_rejected".to_owned(),
                        message: error.to_string(),
                        recovery: Some("Replay the candidate in a clean fixture with independent verification".to_owned()),
                    },
                );
            }
        };
        if let Err(error) = runtime.workflow_host.put(promoted.clone()) {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "workflow_store_failed".to_owned(),
                    message: error.to_string(),
                    recovery: Some("Repair the local promoted workflow store".to_owned()),
                },
            );
        }
        return success(
            request,
            operation_id,
            "workflow_host",
            EffectState::None,
            VerificationState::Verified,
            json!({
                "promoted_workflow_id": promoted.id,
                "version": promoted.version,
                "intent": promoted.intent,
                "fingerprint": promoted.fingerprint,
                "minimum_verified_runs": minimum_verified_runs,
            }),
        );
    }
    if let Some(workflow_id) = request
        .params
        .get("promoted_workflow_id")
        .and_then(Value::as_str)
    {
        let workflow = match runtime.workflow_host.get(workflow_id) {
            Ok(Some(workflow)) => workflow,
            Ok(None) => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "workflow_not_found".to_owned(),
                        message: "The promoted workflow is not installed".to_owned(),
                        recovery: Some("Inspect promoted workflows or use a cold verified route".to_owned()),
                    },
                );
            }
            Err(error) => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "workflow_store_failed".to_owned(),
                        message: error.to_string(),
                        recovery: Some("Repair the local promoted workflow store".to_owned()),
                    },
                );
            }
        };
        if request
            .params
            .get("fingerprint")
            .and_then(Value::as_str)
            .is_some_and(|expected| expected != workflow.fingerprint)
        {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "workflow_fingerprint_mismatch".to_owned(),
                    message: "The promoted workflow fingerprint changed".to_owned(),
                    recovery: Some("Inspect the installed workflow before executing it".to_owned()),
                },
            );
        }
        return execute_typed_workflow(
            runtime,
            request,
            operation_id,
            workflow,
            "promoted_workflow",
        );
    }
    if let Some(parallel_reads) = request.params.get("parallel_reads") {
        let Ok(specs) = serde_json::from_value::<Vec<ParallelReadSpec>>(parallel_reads.clone()) else {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "invalid_input".to_owned(),
                    message: "parallel_reads did not match the closed read batch schema".to_owned(),
                    recovery: None,
                },
            );
        };
        return execute_parallel_read_batch(runtime, request, operation_id, specs);
    }
'''
core = replace_once(core, execute_marker, execute_prefix, "workflow fast-path branches")

resolve_marker = '''fn resolve_workflow_parameters(template: &Value, parameters: &Value) -> Value {
'''
helpers = r'''fn parallel_read_allowed(intent: &str) -> bool {
    matches!(
        intent,
        "system.ping"
            | "capability.search"
            | "desktop.observe"
            | "platform.broker.observe"
            | "app.resolve"
            | "app.list"
            | "permission.status"
            | "settings.get"
            | "popup.inspect"
            | "browser.session.list"
    )
}

fn execute_parallel_read_spec(spec: ParallelReadSpec, index: usize) -> ActionResult {
    let request = OperationRequest {
        intent: spec.intent,
        target: spec.target,
        params: spec.params,
        postcondition: None,
        risk: Some(Risk::R0),
        idempotency_key: None,
        dry_run: false,
        background: Some("strict_background".to_owned()),
    };
    let operation_id = format!("parallel-read-{index}");
    match request.intent.as_str() {
        "system.ping" => success(
            &request,
            operation_id,
            "native",
            EffectState::None,
            VerificationState::Verified,
            json!({ "ready": true, "protocol": PROTOCOL_VERSION }),
        ),
        "capability.search" => capability_search(&request, operation_id),
        "desktop.observe" => desktop_observe(&request, operation_id),
        "platform.broker.observe" => platform_broker_observe(&request, operation_id),
        "app.resolve" => app_resolve(&request, operation_id),
        "app.list" => app_list(&request, operation_id),
        "permission.status" => permission_status(&request, operation_id),
        "settings.get" => settings_get(&request, operation_id),
        "popup.inspect" => popup_inspect(&request, operation_id),
        "browser.session.list" => browser_session_list(&request, operation_id),
        _ => ActionResult::refused(
            &request,
            operation_id,
            ComptrolError {
                code: "parallel_read_denied".to_owned(),
                message: "Only explicitly allowlisted R0 reads can run in parallel".to_owned(),
                recovery: None,
            },
        ),
    }
}

fn execute_parallel_read_batch(
    runtime: &Runtime,
    request: &OperationRequest,
    operation_id: String,
    specs: Vec<ParallelReadSpec>,
) -> ActionResult {
    if specs.is_empty() || specs.len() > 8 {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "invalid_input".to_owned(),
                message: "parallel_reads accepts between 1 and 8 read operations".to_owned(),
                recovery: None,
            },
        );
    }
    if let Some(spec) = specs.iter().find(|spec| {
        !parallel_read_allowed(&spec.intent)
            || classify(&spec.intent) != Risk::R0
            || !runtime.policy.allowed_intents.contains(&spec.intent)
    }) {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "parallel_read_denied".to_owned(),
                message: format!("{} is not an authorized R0 parallel read", spec.intent),
                recovery: Some("Run mutating or privileged operations through the serialized verified route".to_owned()),
            },
        );
    }
    let handles = specs
        .into_iter()
        .enumerate()
        .map(|(index, spec)| std::thread::spawn(move || execute_parallel_read_spec(spec, index)))
        .collect::<Vec<_>>();
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.join() {
            Ok(result) => results.push(result),
            Err(_) => {
                return ActionResult::refused(
                    request,
                    operation_id,
                    ComptrolError {
                        code: "parallel_read_failed".to_owned(),
                        message: "A read worker terminated unexpectedly".to_owned(),
                        recovery: Some("Retry the reads serially".to_owned()),
                    },
                );
            }
        }
    }
    let verified = results
        .iter()
        .all(|result| result.error.is_none() && result.verification == VerificationState::Verified);
    if !verified {
        return ActionResult {
            operation_id,
            intent: request.intent.clone(),
            route: "workflow_parallel_read".to_owned(),
            target: request.target.clone(),
            preflight: "passed".to_owned(),
            delivery: DeliveryState::Delivered,
            effect: EffectState::None,
            verification: VerificationState::Failed,
            disturbance: json!({ "foreground_changed": false }),
            recovery: RecoveryState::None,
            data: json!({ "parallelism": results.len(), "results": results }),
            error: Some(ComptrolError {
                code: "parallel_read_failed".to_owned(),
                message: "At least one parallel read failed verification".to_owned(),
                recovery: Some("Inspect the failed read and retry only that observation".to_owned()),
            }),
        };
    }
    success(
        request,
        operation_id,
        "workflow_parallel_read",
        EffectState::None,
        VerificationState::Verified,
        json!({ "parallelism": results.len(), "results": results }),
    )
}

fn execute_typed_workflow(
    runtime: &mut Runtime,
    request: &OperationRequest,
    operation_id: String,
    mut workflow: Workflow,
    route: &str,
) -> ActionResult {
    let parameters = request
        .params
        .get("parameters")
        .cloned()
        .unwrap_or(Value::Null);
    for node in workflow.nodes.values_mut() {
        if let WorkflowNode::Act { params, .. } = node {
            *params = resolve_workflow_parameters(params, &parameters);
        }
    }
    let mutates = workflow.nodes.values().any(|node| {
        matches!(node, WorkflowNode::Act { intent, .. } if classify(intent).mutation())
    });
    let target = request.target.clone();
    let background = request.background.clone();
    let cancellation = runtime.operation_cancel.clone();
    let max_steps = workflow.nodes.len().saturating_mul(4).saturating_add(4);
    let workflow_id = workflow.id.clone();
    let workflow_version = workflow.version;
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
            if let Some(error) = result.error.as_ref() {
                return Err(error.message.clone());
            }
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
        max_steps,
    };
    let executed = if let Some(cancellation) = cancellation {
        executor.run_with_cancel(&workflow, || cancellation.load(Ordering::SeqCst))
    } else {
        executor.run(&workflow)
    };
    match executed {
        Ok(value) => success(
            request,
            operation_id,
            route,
            if mutates { EffectState::Changed } else { EffectState::None },
            VerificationState::Verified,
            json!({
                "workflow_id": workflow_id,
                "workflow_version": workflow_version,
                "result": value,
            }),
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
    }
}

'''
core = replace_once(core, resolve_marker, helpers + resolve_marker, "workflow helpers")

# Teach the verified-control skill to use safe read fan-out and promoted workflows.
skill_path = "plugins/comptrol/skills/comptrol-verified-control/SKILL.md"
skill = read(skill_path)
skill_anchor = "- Prefer typed/native batch operations before UI replay: Resolve `video.timeline.batch`, PowerPoint `presentation.desktop.batch_edit`, and Canva `design.batch_edit`.\n"
skill_add = skill_anchor + "- For independent preflight observations, use `workflow.execute` with `parallel_reads` (maximum 8); the runtime only accepts explicitly allowlisted R0 reads and never speculates mutations.\n- Promote a repeated workflow only with clean-fixture, independently verified replay evidence. Reuse it by `promoted_workflow_id` and include its fingerprint when available; fall back to a cold verified route on any mismatch.\n"
skill = replace_once(skill, skill_anchor, skill_add, "skill V6 guidance")
write(skill_path, skill)

# Lightweight regression tests for the new safety boundaries.
test_insert = r'''

    #[test]
    fn v6_parallel_read_allowlist_excludes_mutations() {
        assert!(parallel_read_allowed("system.ping"));
        assert!(parallel_read_allowed("app.list"));
        assert!(!parallel_read_allowed("app.launch"));
        assert!(!parallel_read_allowed("browser.cdp.click"));
    }

    #[test]
    fn v6_contextual_route_stats_persist_under_context_key() {
        let mut runtime = runtime();
        let request = OperationRequest {
            intent: "system.ping".to_owned(),
            target: Some(Target {
                kind: "fixture".to_owned(),
                id: Some("alpha".to_owned()),
                name: None,
            }),
            params: Value::Null,
            postcondition: None,
            risk: Some(Risk::R0),
            idempotency_key: None,
            dry_run: false,
            background: None,
        };
        let result = runtime.operate(request.clone());
        let key = route_history_key(&request, &result.route);
        assert!(runtime.route_history.contains_key(&key));
        let persisted: i64 = runtime
            .route_stats_db
            .query_row(
                "SELECT COUNT(*) FROM route_stats WHERE route_key = ?1",
                params![key],
                |row| row.get(0),
            )
            .expect("route row");
        assert_eq!(persisted, 1);
    }
'''
last = core.rfind("\n}")
if last == -1:
    raise RuntimeError("core tests closing brace missing")
core = core[:last] + test_insert + core[last:]

write(core_path, core)
print("V6 final execution pass applied")
