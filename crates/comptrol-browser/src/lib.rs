#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TargetRecord {
    pub id: String,
    pub target_type: String,
    pub browser_context_id: Option<String>,
    pub session_id: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub opener_id: Option<String>,
    pub attached: bool,
    pub generation: u64,
    pub revision: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct TargetGraph {
    pub generation: u64,
    pub targets: BTreeMap<String, TargetRecord>,
}

impl TargetGraph {
    pub fn apply_created(&mut self, target: TargetRecord) {
        self.targets.insert(target.id.clone(), target);
    }

    pub fn apply_changed(&mut self, id: &str, url: Option<String>, title: Option<String>) {
        if let Some(target) = self.targets.get_mut(id) {
            target.url = url;
            target.title = title;
            target.revision = format!("generation:{}:target:{}", self.generation, id);
        }
    }

    pub fn apply_destroyed(&mut self, id: &str) {
        self.targets.remove(id);
    }

    pub fn reconnect(&mut self) {
        self.generation = self.generation.saturating_add(1);
        for target in self.targets.values_mut() {
            target.attached = false;
            target.session_id = None;
            target.generation = self.generation;
            target.revision = format!("generation:{}:target:{}", self.generation, target.id);
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FrameRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub target_id: String,
    pub loader_id: Option<String>,
    pub execution_context_ids: Vec<u64>,
    pub generation: u64,
    pub revision: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct FrameGraph {
    pub frames: BTreeMap<String, FrameRecord>,
}

impl FrameGraph {
    pub fn upsert(&mut self, frame: FrameRecord) {
        self.frames.insert(frame.id.clone(), frame);
    }

    pub fn remove(&mut self, id: &str) {
        self.frames
            .retain(|frame_id, frame| frame_id != id && frame.parent_id.as_deref() != Some(id));
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BrowserCommand {
    pub id: u64,
    pub session_id: Option<String>,
    pub method: String,
    pub params: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> TargetRecord {
        TargetRecord {
            id: "tab".to_owned(),
            target_type: "page".to_owned(),
            browser_context_id: Some("default".to_owned()),
            session_id: Some("session".to_owned()),
            url: Some("https://example.test".to_owned()),
            title: Some("Example".to_owned()),
            opener_id: None,
            attached: true,
            generation: 0,
            revision: "generation:0:target:tab".to_owned(),
        }
    }

    #[test]
    fn reconnect_invalidates_sessions_and_increments_generation() {
        let mut graph = TargetGraph::default();
        graph.apply_created(target());
        graph.reconnect();
        let target = graph.targets.get("tab").unwrap();
        assert_eq!(graph.generation, 1);
        assert_eq!(target.session_id, None);
        assert!(!target.attached);
        assert_eq!(target.generation, 1);
    }
}
