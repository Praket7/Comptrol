#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

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
}
