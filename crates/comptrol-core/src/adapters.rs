use crate::Risk;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AdapterDescriptor {
    pub name: String,
    pub version: String,
    pub platforms: Vec<String>,
    pub capabilities: Vec<String>,
    pub route: String,
    pub risk: Risk,
    pub isolation: String,
}

#[derive(Clone, Debug, Default)]
pub struct AdapterRegistry {
    descriptors: Vec<AdapterDescriptor>,
}

impl AdapterRegistry {
    pub fn builtin() -> Self {
        Self {
            descriptors: vec![AdapterDescriptor {
                name: "comptrol.core".to_owned(),
                version: "0.1".to_owned(),
                platforms: vec!["macos".to_owned(), "windows".to_owned(), "linux".to_owned()],
                capabilities: vec![
                    "observe".to_owned(),
                    "policy".to_owned(),
                    "recovery".to_owned(),
                ],
                route: "native".to_owned(),
                risk: Risk::R0,
                isolation: "in_process_trusted".to_owned(),
            }],
        }
    }

    pub fn register(&mut self, descriptor: AdapterDescriptor) -> bool {
        if self
            .descriptors
            .iter()
            .any(|item| item.name == descriptor.name)
        {
            return false;
        }
        self.descriptors.push(descriptor);
        true
    }

    pub fn list(&self) -> &[AdapterDescriptor] {
        &self.descriptors
    }
}
