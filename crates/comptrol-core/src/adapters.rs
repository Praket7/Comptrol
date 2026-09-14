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
            descriptors: vec![
                AdapterDescriptor {
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
                },
                AdapterDescriptor {
                    name: "comptrol.browser.cdp".to_owned(),
                    version: "0.1".to_owned(),
                    platforms: vec!["macos".to_owned(), "windows".to_owned(), "linux".to_owned()],
                    capabilities: vec![
                        "target_discovery".to_owned(),
                        "accessibility_snapshot".to_owned(),
                        "evaluate".to_owned(),
                        "navigate".to_owned(),
                        "fill".to_owned(),
                        "click".to_owned(),
                        "wait_for".to_owned(),
                        "upload".to_owned(),
                        "download".to_owned(),
                        "open_tab".to_owned(),
                        "close_tab".to_owned(),
                        "history".to_owned(),
                    ],
                    route: "browser_protocol".to_owned(),
                    risk: Risk::R2,
                    isolation: "loopback_policy_bound".to_owned(),
                },
                AdapterDescriptor {
                    name: "comptrol.macos.ax".to_owned(),
                    version: "0.1".to_owned(),
                    platforms: vec!["macos".to_owned()],
                    capabilities: vec![
                        "press".to_owned(),
                        "set_value".to_owned(),
                        "postcondition".to_owned(),
                    ],
                    route: "macos_ax".to_owned(),
                    risk: Risk::R2,
                    isolation: "osascript_bounded".to_owned(),
                },
                AdapterDescriptor {
                    name: "comptrol.macos.launch".to_owned(),
                    version: "0.1".to_owned(),
                    platforms: vec!["macos".to_owned()],
                    capabilities: vec!["open_app".to_owned()],
                    route: "launchservices".to_owned(),
                    risk: Risk::R2,
                    isolation: "argument_vector_bounded".to_owned(),
                },
            ],
        }
    }

    pub fn register(&mut self, descriptor: AdapterDescriptor) -> bool {
        if !descriptor.is_valid()
            || self
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

impl AdapterDescriptor {
    pub fn is_valid(&self) -> bool {
        !self.name.is_empty()
            && !self.version.is_empty()
            && !self.route.is_empty()
            && !self.isolation.is_empty()
            && self
                .name
                .chars()
                .all(|character| !character.is_control() && !character.is_whitespace())
            && !self.platforms.is_empty()
            && !self.capabilities.is_empty()
            && self
                .platforms
                .iter()
                .chain(self.capabilities.iter())
                .all(|value| !value.is_empty() && !value.chars().any(char::is_control))
    }
}
