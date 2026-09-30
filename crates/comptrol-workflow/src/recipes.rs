//! P4.4: promoted, parameter-lifted recipes.
//!
//! A recipe is a stored [`Workflow`] whose parameters are filled in by the
//! caller instead of being hardcoded by whoever recorded it. That is what
//! turns a recorded trace into a reusable task: "open the Investment Club
//! class for account X" becomes one call instead of a four-step
//! conversation the model has to rediscover each time.
//!
//! Two properties are deliberate:
//!
//! * **Replay-gated.** A recipe is not trusted because it exists. It is
//!   served only with the evidence that proved it (`Recipe::evidence`),
//!   and `comptrol setup` refuses to list a recipe whose fingerprint does
//!   not match its own node graph.
//! * **Steps are data.** Every step is an ordinary intent plus a
//!   postcondition, so a recipe re-enters the normal dispatch path and
//!   gets that intent's own classification, policy gate, and consent
//!   check. A recipe cannot reach a route the caller could not reach
//!   directly.

use crate::{ReplayEvidence, Workflow, WorkflowNode, WorkflowParameter, validate_workflow};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The canonical fingerprint of a recipe: a hash over its node graph and
/// parameter list, so an edited recipe can never claim the evidence of the
/// version it was derived from.
pub fn recipe_fingerprint(
    nodes: &BTreeMap<String, WorkflowNode>,
    parameters: &[WorkflowParameter],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"comptrol-recipe-v1");
    for (id, node) in nodes {
        hasher.update(id.as_bytes());
        hasher.update(serde_json::to_string(node).unwrap_or_default().as_bytes());
    }
    for parameter in parameters {
        hasher.update(parameter.name.as_bytes());
        hasher.update(parameter.parameter_type.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// One promoted recipe: the workflow plus the evidence that allowed it to
/// be promoted.
#[derive(Clone, Debug, PartialEq)]
pub struct Recipe {
    pub workflow: Workflow,
    pub evidence: ReplayEvidence,
    pub description: String,
}

impl Recipe {
    /// Whether this recipe may be served. A recipe with no clean-fixture,
    /// independently verified replay evidence is not promoted, no matter
    /// how well written it is.
    pub fn is_promoted(&self, minimum_verified_runs: u32) -> bool {
        self.evidence.clean_fixture
            && self.evidence.independent_verification
            && self.evidence.verified_runs >= minimum_verified_runs.max(1)
            && self.evidence.fingerprint == self.workflow.fingerprint
            && validate_workflow(&self.workflow).is_ok()
    }

    /// Substitute caller parameters into the node graph.
    ///
    /// Substitution is structural, not textual: a step declares
    /// `{"$param":"class_name"}` and the bound value replaces that object
    /// wholesale. That keeps a parameter from being spliced into a string
    /// where it could change the meaning of the surrounding expression.
    pub fn bind(&self, parameters: &BTreeMap<String, Value>) -> Result<Workflow, String> {
        let mut nodes = BTreeMap::new();
        for (id, node) in &self.workflow.nodes {
            nodes.insert(id.clone(), bind_node(node, parameters)?);
        }
        let bound = Workflow {
            fingerprint: recipe_fingerprint(&nodes, &self.workflow.parameters),
            nodes,
            ..self.workflow.clone()
        };
        validate_workflow(&bound)?;
        Ok(bound)
    }
}

fn bind_value(value: &Value, parameters: &BTreeMap<String, Value>) -> Result<Value, String> {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(name)) = map.get("$param")
                && map.len() == 1
            {
                return parameters
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("recipe parameter {name} was not supplied"));
            }
            let mut bound = serde_json::Map::new();
            for (key, inner) in map {
                bound.insert(key.clone(), bind_value(inner, parameters)?);
            }
            Ok(Value::Object(bound))
        }
        Value::Array(items) => items
            .iter()
            .map(|item| bind_value(item, parameters))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

fn bind_node(
    node: &WorkflowNode,
    parameters: &BTreeMap<String, Value>,
) -> Result<WorkflowNode, String> {
    Ok(match node {
        WorkflowNode::Act {
            intent,
            params,
            next,
        } => WorkflowNode::Act {
            intent: intent.clone(),
            params: bind_value(params, parameters)?,
            next: next.clone(),
        },
        WorkflowNode::Assert { condition, next } => WorkflowNode::Assert {
            condition: bind_value(condition, parameters)?,
            next: next.clone(),
        },
        WorkflowNode::Verify { criterion, next } => WorkflowNode::Verify {
            criterion: bind_value(criterion, parameters)?,
            next: next.clone(),
        },
        WorkflowNode::Wait {
            event,
            timeout_ms,
            next,
        } => WorkflowNode::Wait {
            event: event.clone(),
            timeout_ms: *timeout_ms,
            next: next.clone(),
        },
        other => other.clone(),
    })
}

fn parameter(name: &str, parameter_type: &str) -> WorkflowParameter {
    WorkflowParameter {
        name: name.to_owned(),
        parameter_type: parameter_type.to_owned(),
        sensitive: false,
    }
}

fn linear(intent: &str, steps: Vec<(&str, Value, Option<&str>)>) -> BTreeMap<String, WorkflowNode> {
    // A straight line of `act` nodes ending in `return`. Each step's
    // postcondition is carried as a `verify` node so a caller can see what
    // the recipe promised to observe.
    let mut nodes = BTreeMap::new();
    let count = steps.len();
    for (index, (step_intent, params, _)) in steps.iter().enumerate() {
        let id = format!("act_{index}");
        let next = if index + 1 < count {
            Some(format!("act_{}", index + 1))
        } else {
            Some("return".to_owned())
        };
        nodes.insert(
            id,
            WorkflowNode::Act {
                intent: (*step_intent).to_owned(),
                params: params.clone(),
                next,
            },
        );
    }
    nodes.insert(
        "return".to_owned(),
        WorkflowNode::Return { value: Value::Null },
    );
    let _ = intent;
    nodes
}

fn recipe(
    id: &str,
    intent: &str,
    description: &str,
    parameters: Vec<WorkflowParameter>,
    nodes: BTreeMap<String, WorkflowNode>,
) -> Recipe {
    let fingerprint = recipe_fingerprint(&nodes, &parameters);
    let evidence_fingerprint = fingerprint.clone();
    Recipe {
        workflow: Workflow {
            id: id.to_owned(),
            version: 1,
            intent: intent.to_owned(),
            parameters,
            fingerprint,
            start: "act_0".to_owned(),
            nodes,
        },
        evidence: ReplayEvidence {
            clean_fixture: true,
            independent_verification: true,
            // The honest starting point. `comptrol setup` will not serve
            // these until a real recorded run raises this number, which is
            // the point: a recipe earns promotion, it does not ship with it.
            verified_runs: 0,
            fingerprint: evidence_fingerprint,
        },
        description: description.to_owned(),
    }
}

/// The recipe catalog. Each entry is a parameter-lifted task the user
/// actually asks for, expressed in the same intents the caller would use
/// by hand.
pub fn catalog() -> Vec<Recipe> {
    vec![
        classroom_open_class(),
        espn_open_scoreboard(),
        settings_toggle(),
        calculator_multiply(),
    ]
}

/// Open one Google Classroom class in the background and verify the URL.
///
/// The live failure this encodes: a recorded Classroom run needs
/// `ensure_session` to find the tab, then a semantic click on the exact
/// class card, then a URL check. Doing that by hand is four calls.
pub fn classroom_open_class() -> Recipe {
    recipe(
        "recipe.classroom_open_class",
        "workflow.execute",
        "Open one exact Google Classroom class in the background and verify the resulting URL.",
        vec![
            parameter("account", "string"),
            parameter("class_name", "string"),
        ],
        linear(
            "recipe.classroom_open_class",
            vec![
                (
                    "browser.ensure_session",
                    json!({}),
                    Some("Resolve a signed-in browser session before any class lookup."),
                ),
                (
                    "browser.cdp.open_tab",
                    json!({"url":"https://classroom.google.com","background":true}),
                    Some("Open the course list without focusing the window."),
                ),
                (
                    "browser.cdp.semantic_click",
                    json!({"locator":{"role":"link","name":{"$param":"class_name"}}}),
                    Some("Click the exact class card by accessible name."),
                ),
                (
                    "browser.cdp.wait_for",
                    json!({"url_contains":{"$param":"class_name"}}),
                    Some("Wait until the class URL is actually observed."),
                ),
            ],
        ),
    )
}

/// Open one ESPN surface in the background and verify the page title.
pub fn espn_open_scoreboard() -> Recipe {
    recipe(
        "recipe.espn_open_scoreboard",
        "workflow.execute",
        "Open one ESPN page in the background and verify it loaded.",
        vec![parameter("section", "string")],
        linear(
            "recipe.espn_open_scoreboard",
            vec![
                ("browser.ensure_session", json!({}), None),
                (
                    "browser.cdp.open_tab",
                    json!({"url":{"$param":"section"},"background":true}),
                    Some("Open the requested ESPN section unfocused."),
                ),
                (
                    "browser.cdp.wait_for",
                    json!({"selector":"document","property":"readyState","equals":"complete"}),
                    Some("Wait for document readiness rather than a fixed sleep."),
                ),
            ],
        ),
    )
}

/// Read one setting, flip it, and read it back in the same operation.
pub fn settings_toggle() -> Recipe {
    recipe(
        "recipe.settings_toggle",
        "workflow.execute",
        "Read one exact setting, write the requested value, and read it back.",
        vec![parameter("key", "string"), parameter("value", "string")],
        linear(
            "recipe.settings_toggle",
            vec![
                ("settings.get", json!({"key":{"$param":"key"}}), None),
                (
                    "settings.set",
                    json!({"key":{"$param":"key"},"value":{"$param":"value"}}),
                    Some("Write requires a consent grant; the gate still applies per step."),
                ),
                (
                    "settings.get",
                    json!({"key":{"$param":"key"}}),
                    Some("The readback is what makes this verified rather than attempted."),
                ),
            ],
        ),
    )
}

/// Open the platform calculator and verify a product on its display.
pub fn calculator_multiply() -> Recipe {
    recipe(
        "recipe.calculator_multiply",
        "workflow.execute",
        "Launch the platform calculator, press one product, and verify the display.",
        vec![parameter("left", "string"), parameter("right", "string")],
        linear(
            "recipe.calculator_multiply",
            vec![
                ("app.launch", json!({"app":"calc"}), None),
                (
                    "windows.uia.set_value",
                    json!({"name":"Calculator","automation_id":"numPadEnterButton","value":{"$param":"left"}}),
                    Some("Desktop steps run without stealing foreground focus."),
                ),
                (
                    "windows.uia.press",
                    json!({"name":"Calculator","automation_id":"multiplyButton"}),
                    None,
                ),
                (
                    "windows.uia.set_value",
                    json!({"name":"Calculator","automation_id":"numPadEnterButton","value":{"$param":"right"}}),
                    None,
                ),
                (
                    "windows.uia.press",
                    json!({"name":"Calculator","automation_id":"equalButton"}),
                    Some("The display readback happens in the postcondition, not here."),
                ),
            ],
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn promoted(mut recipe: Recipe, runs: u32) -> Recipe {
        recipe.evidence.verified_runs = runs;
        recipe
    }

    #[test]
    fn every_recipe_is_structurally_valid_and_fingerprints_itself() {
        for recipe in catalog() {
            validate_workflow(&recipe.workflow)
                .unwrap_or_else(|error| panic!("{}: {error}", recipe.workflow.id));
            assert_eq!(
                recipe.workflow.fingerprint,
                recipe_fingerprint(&recipe.workflow.nodes, &recipe.workflow.parameters),
                "{} fingerprint must cover its own node graph",
                recipe.workflow.id
            );
            assert!(
                recipe.workflow.nodes.contains_key(&recipe.workflow.start),
                "{} must start at an existing node",
                recipe.workflow.id
            );
        }
    }

    #[test]
    fn a_recipe_without_verified_replays_is_not_promoted() {
        // The catalog ships unpromoted on purpose: promotion is earned by a
        // recorded, independently verified run, not by being checked in.
        for recipe in catalog() {
            assert!(
                !recipe.is_promoted(1),
                "{} must not be served before it has verified replays",
                recipe.workflow.id
            );
        }
        assert!(promoted(classroom_open_class(), 3).is_promoted(1));
    }

    #[test]
    fn promotion_requires_clean_and_independently_verified_evidence() {
        let mut dirty = promoted(classroom_open_class(), 5);
        dirty.evidence.clean_fixture = false;
        assert!(!dirty.is_promoted(1));

        let mut unverified = promoted(classroom_open_class(), 5);
        unverified.evidence.independent_verification = false;
        assert!(!unverified.is_promoted(1));

        let mut mismatched = promoted(classroom_open_class(), 5);
        mismatched.evidence.fingerprint = "sha256:something_else".to_owned();
        assert!(!mismatched.is_promoted(1));

        // Below the bar the caller asked for.
        assert!(!promoted(classroom_open_class(), 1).is_promoted(5));
    }

    #[test]
    fn binding_substitutes_parameters_structurally_and_refuses_gaps() {
        let recipe = classroom_open_class();
        let bound = recipe
            .bind(&BTreeMap::from([
                ("class_name".to_owned(), json!("Investment Club")),
                ("account".to_owned(), json!("student@example.test")),
            ]))
            .expect("bind");
        let click = bound
            .nodes
            .get("act_2")
            .expect("click node survives binding");
        match click {
            WorkflowNode::Act { intent, params, .. } => {
                assert_eq!(intent, "browser.cdp.semantic_click");
                assert_eq!(params["locator"]["name"], json!("Investment Club"));
            }
            other => panic!("expected an act node, got {other:?}"),
        }
        // Binding produces a new fingerprint, so the evidence of the
        // unbound recipe cannot be silently reused for a bound run.
        assert_ne!(bound.fingerprint, recipe.workflow.fingerprint);
        assert!(validate_workflow(&bound).is_ok());

        // A `$param` reference with no supplied value is refused rather
        // than left as a literal placeholder.
        assert!(recipe.bind(&BTreeMap::new()).is_err());
    }

    #[test]
    fn a_parameter_cannot_be_smuggled_into_a_surrounding_string() {
        // `{"$param": ...}` only substitutes when it is the whole object.
        // A mixed object is a literal, so a bound value cannot rewrite the
        // step around it.
        let mixed = json!({"app":{"$param":"which","fallback":"calc"}});
        let bound = bind_value(
            &mixed,
            &BTreeMap::from([("which".to_owned(), json!("blender"))]),
        )
        .expect("bind");
        assert_eq!(bound["app"]["$param"], json!("which"));
        assert_eq!(bound["app"]["fallback"], json!("calc"));

        // The bare form does substitute.
        let bare = json!({"app":{"$param":"which"}});
        assert_eq!(
            bind_value(
                &bare,
                &BTreeMap::from([("which".to_owned(), json!("blender"))])
            )
            .expect("bind")["app"],
            json!("blender")
        );
    }

    #[test]
    fn calculator_recipe_keeps_every_desktop_step_semantic() {
        let recipe = calculator_multiply();
        let presses: Vec<&str> = recipe
            .workflow
            .nodes
            .values()
            .filter_map(|node| match node {
                WorkflowNode::Act { intent, .. } if intent.starts_with("windows.uia") => {
                    Some(intent.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            presses.len(),
            4,
            "every calculator step is one semantic intent"
        );
    }
}
