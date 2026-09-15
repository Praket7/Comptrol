#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationLevel {
    None,
    Delivery,
    SurfaceState,
    ApplicationState,
    PersistedArtifact,
    IndependentOutcome,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationSource {
    BrowserDom,
    BrowserAccessibility,
    BrowserNetwork,
    NativeAccessibility,
    AppAdapter,
    FileSystem,
    DocumentBackend,
    IndependentFixture,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VerificationCriterion {
    pub id: String,
    pub required: bool,
    pub expected: Value,
    pub observed: Value,
    pub passed: bool,
    pub source: VerificationSource,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VerificationEvidence {
    pub source: VerificationSource,
    pub kind: String,
    pub reference: Option<String>,
    pub details: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VerificationReport {
    pub level: VerificationLevel,
    pub state: VerificationState,
    pub criteria: Vec<VerificationCriterion>,
    pub evidence: Vec<VerificationEvidence>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    NotAttempted,
    Verified,
    Unverified,
    Failed,
}

impl VerificationReport {
    pub fn new(level: VerificationLevel) -> Self {
        Self {
            level,
            state: VerificationState::NotAttempted,
            criteria: Vec::new(),
            evidence: Vec::new(),
        }
    }

    pub fn finalize(mut self) -> Self {
        let required = self.criteria.iter().filter(|criterion| criterion.required);
        self.state = if self.criteria.is_empty() {
            VerificationState::Unverified
        } else if required.clone().any(|criterion| !criterion.passed) {
            VerificationState::Failed
        } else if self.criteria.iter().all(|criterion| criterion.passed) {
            VerificationState::Verified
        } else {
            VerificationState::Unverified
        };
        self
    }

    pub fn is_verified(&self) -> bool {
        self.state == VerificationState::Verified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_failure_cannot_be_reported_verified() {
        let report = VerificationReport {
            level: VerificationLevel::ApplicationState,
            state: VerificationState::NotAttempted,
            criteria: vec![VerificationCriterion {
                id: "saved".to_owned(),
                required: true,
                expected: Value::Bool(true),
                observed: Value::Bool(false),
                passed: false,
                source: VerificationSource::AppAdapter,
            }],
            evidence: Vec::new(),
        }
        .finalize();
        assert_eq!(report.state, VerificationState::Failed);
        assert!(!report.is_verified());
    }
}
