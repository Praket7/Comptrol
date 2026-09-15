#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

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
}
