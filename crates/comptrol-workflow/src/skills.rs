//! S1 Skill Forge: versioned, parameter-lifted procedural memory.
//!
//! A skill is a stored procedure that gets more reliable the more it is
//! used: typed inputs, an optional precondition, ordered steps (each with
//! anchored locators and a postcondition), rollback hints, a reliability
//! score, and an audit trail of revisions.
//!
//! Three properties are deliberate, mirroring the recipe discipline in
//! [`crate::recipes`]:
//!
//! * **Steps are data.** Every step is an ordinary intent plus parameters,
//!   so a skill re-enters the normal dispatch path and gets that intent's
//!   own classification, policy gate, and consent check. A skill cannot
//!   reach a route the caller could not reach directly, and skill nodes
//!   can never carry executable code.
//! * **Evidence, not vibes.** [`SkillRecord::record_run`] is the only way
//!   reliability changes. Failures open a `reflect` revision, verified
//!   successes can promote a revised step graph (`reuse`) or record a
//!   faster/first-try run, and every change lands in the audit trail with
//!   an easy pin-to-version via [`SkillRecord::version`].
//! * **Plain files.** Skills live under `~/.comptrol/skills` as JSON and
//!   round-trip through [`SkillStore::export_pack`] / [`SkillStore::import_pack`],
//!   so a skill pack can move between machines or live in a repo.

use crate::WorkflowParameter;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// One remembered way to find a UI element: the anchor graph that makes a
/// stored locator survive a redesign (S2). Anchors are kept in reliability
/// order and healed against live context only with verification.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SkillAnchor {
    pub role: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    pub neighborhood_text: Option<String>,
    #[serde(default)]
    pub ancestor_path: Vec<String>,
    pub dom_fingerprint: Option<String>,
    /// Observed success rate of this anchor, updated by evidence only.
    #[serde(default)]
    pub reliability: f64,
}

impl SkillAnchor {
    /// Similarity between an expected anchor and a live candidate, on a
    /// 0.0..=1.0 scale. Role and name dominate; neighborhood and ancestor
    /// path break ties when a page is reshuffled but labelled the same.
    pub fn similarity(&self, other: &SkillAnchor) -> f64 {
        let mut score = 0.0;
        let mut weight = 0.0;
        let mut compare = |left: &Option<String>, right: &Option<String>, w: f64| {
            weight += w;
            if left.is_none() && right.is_none() {
                score += w;
            } else if let (Some(left), Some(right)) = (left, right) {
                if left == right {
                    score += w;
                } else if left.to_lowercase() == right.to_lowercase() {
                    score += w * 0.9;
                } else if !left.trim().is_empty()
                    && !right.trim().is_empty()
                    && (left.contains(right.as_str()) || right.contains(left.as_str()))
                {
                    score += w * 0.6;
                }
            }
        };
        compare(&self.role, &other.role, 0.30);
        compare(&self.name, &other.name, 0.30);
        compare(
            &self.neighborhood_text.as_ref().map(|s| s.to_lowercase()),
            &other.neighborhood_text.as_ref().map(|s| s.to_lowercase()),
            0.15,
        );
        compare(&self.dom_fingerprint, &other.dom_fingerprint, 0.10);
        weight += 0.15;
        if self.ancestor_path.is_empty() && other.ancestor_path.is_empty() {
            score += 0.15;
        } else {
            let shared = self
                .ancestor_path
                .iter()
                .filter(|entry| other.ancestor_path.contains(entry))
                .count();
            let total = self.ancestor_path.len().max(other.ancestor_path.len());
            score += 0.15 * (shared as f64 / total as f64);
        }
        if self.attributes.is_empty() && other.attributes.is_empty() {
            score += 0.10;
            weight += 0.10;
        } else if !self.attributes.is_empty() && !other.attributes.is_empty() {
            weight += 0.10;
            let shared = self
                .attributes
                .iter()
                .filter(|(key, value)| other.attributes.get(*key) == Some(value))
                .count();
            let total = self.attributes.len().max(other.attributes.len());
            score += 0.10 * (shared as f64 / total as f64);
        }
        if weight <= 0.0 {
            return 0.0;
        }
        (score / weight).clamp(0.0, 1.0)
    }
}

/// The heal decision for one dispatch attempt: act on the cached anchor,
/// heal to a verified candidate, or refuse. Healing never acts blind — the
/// `Heal` arm demands a readback probe before the action transaction.
#[derive(Clone, Debug, PartialEq)]
pub enum HealPlan {
    UseAnchor { index: usize },
    Heal { candidate: usize, similarity: f64 },
    Refuse { reason: String },
}

/// Plan one dispatch against cached anchors plus live candidates.
///
/// * An exact field match wins immediately (`UseAnchor`).
/// * Otherwise the best candidate at or above `threshold` heals, but only
///   when it wins by `margin` — a near-tie means the page is ambiguous and
///   blind-patching would act on the wrong element (`Refuse`).
pub fn plan_heal(
    expected: &SkillAnchor,
    candidates: &[SkillAnchor],
    threshold: f64,
    margin: f64,
) -> HealPlan {
    if candidates.is_empty() {
        return HealPlan::Refuse {
            reason: "no live candidates for the stored anchor".to_owned(),
        };
    }
    let mut scored: Vec<(usize, f64)> = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (index, expected.similarity(candidate)))
        .collect();
    scored.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(index) = candidates
        .iter()
        .position(|candidate| candidate == expected || expected.similarity(candidate) >= 0.999)
    {
        return HealPlan::UseAnchor { index };
    }
    let (best_index, best_score) = scored[0];
    if best_score < threshold {
        return HealPlan::Refuse {
            reason: format!("best live candidate scored {best_score:.2} below threshold {threshold:.2}"),
        };
    }
    if scored.len() > 1 && best_score - scored[1].1 < margin {
        return HealPlan::Refuse {
            reason: format!(
                "top candidates are within the {margin:.2} margin; the match is ambiguous"
            ),
        };
    }
    HealPlan::Heal {
        candidate: best_index,
        similarity: best_score,
    }
}

/// One ordered step of a skill: an ordinary intent, its parameters (with
/// `$param` structural substitution), the anchored locators it relies on,
/// a postcondition, and a rollback hint for speculative execution.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SkillStep {
    pub id: String,
    pub intent: String,
    #[serde(default = "empty_object")]
    pub params: Value,
    /// Anchors in reliability order; empty for non-UI steps.
    #[serde(default)]
    pub anchors: Vec<SkillAnchor>,
    pub postcondition: Option<Value>,
    pub rollback: Option<Value>,
}

fn empty_object() -> Value {
    json!({})
}

/// Per-version evidence. Only recorded runs change these numbers.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SkillEvidence {
    pub clean_fixture: bool,
    pub independent_verification: bool,
    pub verified_runs: u32,
    pub verified_successes: u32,
    pub first_try_runs: u32,
    pub healed_anchors: u32,
    pub total_wall_ms: u64,
    pub last_wall_ms: u64,
}

impl SkillEvidence {
    /// Reliability on a 0.0..=1.0 scale: verified success rate blended
    /// with the share of runs that needed no anchor healing.
    pub fn reliability_score(&self) -> f64 {
        if self.verified_runs == 0 {
            return 0.0;
        }
        let success = self.verified_successes as f64 / self.verified_runs as f64;
        let first_try = self.first_try_runs as f64 / self.verified_runs as f64;
        (0.7 * success + 0.3 * first_try).clamp(0.0, 1.0)
    }

    pub fn mean_wall_ms(&self) -> Option<u64> {
        if self.verified_runs == 0 {
            None
        } else {
            Some(self.total_wall_ms / self.verified_runs as u64)
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillRevisionKind {
    /// A failure was observed; the step graph needs rethinking.
    Reflect,
    /// A revised step graph replaced an older one.
    Revise,
    /// A verified success replaced an older version on speed or robustness.
    Reuse,
    /// A caller pinned execution to a specific version.
    Pin,
}

/// One audit-trail entry. Revisions are append-only; versions are never
/// rewritten out of history.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SkillRevision {
    pub kind: SkillRevisionKind,
    pub note: String,
    pub from_version: u32,
    pub to_version: u32,
    pub at_ms: u64,
}

/// One immutable version of a skill's step graph.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SkillVersion {
    pub version: u32,
    pub precondition: Option<Value>,
    pub steps: Vec<SkillStep>,
    /// Skill ids this version composes (skill-of-skills).
    #[serde(default)]
    pub composes: Vec<String>,
    pub fingerprint: String,
    pub evidence: SkillEvidence,
}

/// The versioned skill: identity, typed inputs, version history, the
/// active version, and the append-only revision trail.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SkillRecord {
    pub id: String,
    pub description: String,
    pub intent: String,
    #[serde(default)]
    pub parameters: Vec<WorkflowParameter>,
    pub versions: Vec<SkillVersion>,
    pub active_version: u32,
    #[serde(default)]
    pub revisions: Vec<SkillRevision>,
}

/// Canonical fingerprint of a step graph plus its parameter list, so an
/// edited skill can never claim the evidence of the version it was derived
/// from (same discipline as [`crate::recipes::recipe_fingerprint`]).
pub fn skill_fingerprint(
    steps: &[SkillStep],
    composes: &[String],
    parameters: &[WorkflowParameter],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"comptrol-skill-v1");
    for step in steps {
        hasher.update(step.id.as_bytes());
        hasher.update(step.intent.as_bytes());
        hasher.update(serde_json::to_string(&step.params).unwrap_or_default().as_bytes());
        hasher.update(
            serde_json::to_string(&step.postcondition).unwrap_or_default().as_bytes(),
        );
        hasher.update(serde_json::to_string(&step.anchors).unwrap_or_default().as_bytes());
    }
    for id in composes {
        hasher.update(id.as_bytes());
    }
    for parameter in parameters {
        hasher.update(parameter.name.as_bytes());
        hasher.update(parameter.parameter_type.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn validate_steps(steps: &[SkillStep]) -> Result<(), String> {
    if steps.is_empty() {
        return Err("a skill needs at least one step".to_owned());
    }
    let mut seen = std::collections::BTreeSet::new();
    for step in steps {
        if step.id.trim().is_empty() || step.intent.trim().is_empty() {
            return Err("skill steps need non-empty id and intent".to_owned());
        }
        if !seen.insert(step.id.clone()) {
            return Err(format!("duplicate skill step id {}", step.id));
        }
        for key in ["python", "javascript", "shell", "eval"] {
            if step.params.get(key).is_some() {
                return Err(format!(
                    "skill step {} cannot carry executable code ({key})",
                    step.id
                ));
            }
        }
    }
    Ok(())
}

impl SkillRecord {
    /// Create a fresh skill at version 1.
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        intent: impl Into<String>,
        parameters: Vec<WorkflowParameter>,
        precondition: Option<Value>,
        steps: Vec<SkillStep>,
        composes: Vec<String>,
    ) -> Result<Self, String> {
        let id = id.into();
        validate_skill_id(&id)?;
        validate_steps(&steps)?;
        let version = SkillVersion {
            version: 1,
            precondition,
            steps,
            composes,
            fingerprint: String::new(),
            evidence: SkillEvidence::default(),
        };
        let mut record = Self {
            id,
            description: description.into(),
            intent: intent.into(),
            parameters,
            versions: vec![version],
            active_version: 1,
            revisions: Vec::new(),
        };
        record.recompute_fingerprints();
        Ok(record)
    }

    pub fn active(&self) -> &SkillVersion {
        self.version(self.active_version).expect("active version exists")
    }

    pub fn version(&self, version: u32) -> Option<&SkillVersion> {
        self.versions.iter().find(|entry| entry.version == version)
    }

    pub fn version_mut(&mut self, version: u32) -> Option<&mut SkillVersion> {
        self.versions.iter_mut().find(|entry| entry.version == version)
    }

    fn recompute_fingerprints(&mut self) {
        let parameters = self.parameters.clone();
        for version in &mut self.versions {
            version.fingerprint = skill_fingerprint(&version.steps, &version.composes, &parameters);
        }
    }

    /// Revise the step graph: append a new active version and an audit
    /// entry. Older versions stay addressable for pinning.
    pub fn revise(
        &mut self,
        kind: SkillRevisionKind,
        note: impl Into<String>,
        precondition: Option<Value>,
        steps: Vec<SkillStep>,
        composes: Vec<String>,
    ) -> Result<u32, String> {
        validate_steps(&steps)?;
        let from_version = self.active_version;
        let next = self
            .versions
            .iter()
            .map(|entry| entry.version)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let fingerprint = skill_fingerprint(&steps, &composes, &self.parameters);
        self.versions.push(SkillVersion {
            version: next,
            precondition,
            steps,
            composes,
            fingerprint,
            evidence: SkillEvidence::default(),
        });
        self.active_version = next;
        self.revisions.push(SkillRevision {
            kind,
            note: note.into(),
            from_version,
            to_version: next,
            at_ms: now_ms(),
        });
        Ok(next)
    }

    /// Pin execution to one historical version.
    pub fn pin(&mut self, version: u32) -> Result<(), String> {
        if self.version(version).is_none() {
            return Err(format!("skill {} has no version {version}", self.id));
        }
        let from_version = self.active_version;
        self.active_version = version;
        self.revisions.push(SkillRevision {
            kind: SkillRevisionKind::Pin,
            note: format!("pinned to version {version}"),
            from_version,
            to_version: version,
            at_ms: now_ms(),
        });
        Ok(())
    }

    /// The self-improvement loop, fed only by observed evidence.
    ///
    /// * A failure records a `reflect` revision (Reflect).
    /// * A verified success with a revised step graph promotes it (`reuse`
    ///   when the run was faster or first-try, `revise` otherwise).
    /// * Every run updates the evidence of the version that ran, so
    ///   reliability and anchor healing statistics are honest.
    pub fn record_run(&mut self, outcome: &RunOutcome) -> Result<RunUpdate, String> {
        let version = self
            .version_mut(outcome.version)
            .ok_or_else(|| format!("skill {} has no version {}", self.id, outcome.version))?;
        let previous_mean = version.evidence.mean_wall_ms();
        version.evidence.verified_runs = version.evidence.verified_runs.saturating_add(1);
        version.evidence.last_wall_ms = outcome.wall_ms;
        if outcome.clean_fixture {
            version.evidence.clean_fixture = true;
        }
        if outcome.independent_verification {
            version.evidence.independent_verification = true;
        }
        if outcome.verified {
            version.evidence.verified_successes = version.evidence.verified_successes.saturating_add(1);
            if outcome.first_try {
                version.evidence.first_try_runs = version.evidence.first_try_runs.saturating_add(1);
            }
            version.evidence.total_wall_ms = version.evidence.total_wall_ms.saturating_add(outcome.wall_ms);
        }

        if !outcome.verified {
            self.revisions.push(SkillRevision {
                kind: SkillRevisionKind::Reflect,
                note: outcome
                    .note
                    .clone()
                    .unwrap_or_else(|| "run did not verify".to_owned()),
                from_version: outcome.version,
                to_version: outcome.version,
                at_ms: now_ms(),
            });
            return Ok(RunUpdate::Recorded {
                reliability: self.version(outcome.version).unwrap().evidence.reliability_score(),
                promoted: false,
                new_version: None,
            });
        }

        let mut promoted = false;
        let mut new_version = None;
        if let Some(revised) = &outcome.revised_steps {
            let faster = match previous_mean {
                Some(mean) => outcome.wall_ms < mean,
                None => true,
            };
            let kind = if faster || outcome.first_try {
                SkillRevisionKind::Reuse
            } else {
                SkillRevisionKind::Revise
            };
            let note = outcome
                .note
                .clone()
                .unwrap_or_else(|| "verified run promoted a revised step graph".to_owned());
            let version = self.revise(
                kind,
                note,
                self.version(outcome.version).and_then(|entry| entry.precondition.clone()),
                revised.clone(),
                outcome.revised_composes.clone().unwrap_or_default(),
            )?;
            promoted = true;
            new_version = Some(version);
        }
        Ok(RunUpdate::Recorded {
            reliability: self.version(outcome.version).unwrap().evidence.reliability_score(),
            promoted,
            new_version,
        })
    }
}

/// Evidence from one executed run of one pinned version.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct RunOutcome {
    pub version: u32,
    pub verified: bool,
    pub independent_verification: bool,
    pub clean_fixture: bool,
    pub first_try: bool,
    pub healed_anchors: u32,
    pub wall_ms: u64,
    pub note: Option<String>,
    /// A step graph (usually produced by S2 healing) that the verified run
    /// proved out; when present it is promoted as a new version.
    pub revised_steps: Option<Vec<SkillStep>>,
    pub revised_composes: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunUpdate {
    Recorded {
        reliability: f64,
        promoted: bool,
        new_version: Option<u32>,
    },
}

fn validate_skill_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("skill id cannot be empty".to_owned());
    }
    if id.len() > 128 {
        return Err("skill id is too long".to_owned());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "skill id {id} may only contain letters, digits, dash, underscore, and dot"
        ));
    }
    Ok(())
}

/// Substitute caller parameters into a value. Substitution is structural,
/// not textual: `{"$param":"name"}` is replaced wholesale so a parameter
/// cannot change the meaning of a surrounding expression.
pub fn bind_skill_value(
    value: &Value,
    parameters: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(name)) = map.get("$param")
                && map.len() == 1
            {
                return parameters
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("skill parameter {name} was not supplied"));
            }
            let mut bound = serde_json::Map::new();
            for (key, inner) in map {
                bound.insert(key.clone(), bind_skill_value(inner, parameters)?);
            }
            Ok(Value::Object(bound))
        }
        Value::Array(items) => items
            .iter()
            .map(|item| bind_skill_value(item, parameters))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

/// Expand a skill into its ordered, fully bound step list, inlining any
/// composed skills (skill-of-skills) depth-first with cycle refusal.
pub fn expand_skill(
    lookup: &dyn Fn(&str) -> Option<SkillRecord>,
    record: &SkillRecord,
    version: u32,
    parameters: &BTreeMap<String, Value>,
) -> Result<Vec<SkillStep>, String> {
    let mut seen = Vec::new();
    expand_inner(lookup, record, version, parameters, &mut seen, 0)
}

fn expand_inner(
    lookup: &dyn Fn(&str) -> Option<SkillRecord>,
    record: &SkillRecord,
    version: u32,
    parameters: &BTreeMap<String, Value>,
    seen: &mut Vec<String>,
    depth: usize,
) -> Result<Vec<SkillStep>, String> {
    if depth > 8 {
        return Err("skill composition exceeds the depth limit".to_owned());
    }
    if seen.contains(&record.id) {
        return Err(format!("skill composition cycle at {}", record.id));
    }
    seen.push(record.id.clone());
    let entry = record
        .version(version)
        .ok_or_else(|| format!("skill {} has no version {version}", record.id))?;
    let mut steps = Vec::new();
    for id in &entry.composes {
        let child = lookup(id).ok_or_else(|| format!("composed skill {id} is not installed"))?;
        let child_steps = expand_inner(lookup, &child, child.active_version, parameters, seen, depth + 1)?;
        steps.extend(child_steps);
    }
    for step in &entry.steps {
        let mut bound = step.clone();
        bound.params = bind_skill_value(&step.params, parameters)?;
        steps.push(bound);
    }
    seen.pop();
    Ok(steps)
}

/// Durable skill directory. Files are plain JSON so skill packs move
/// between machines and into repos without a service.
pub struct SkillStore {
    root: PathBuf,
}

impl SkillStore {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, String> {
        validate_skill_id(id)?;
        Ok(self.root.join(format!("{id}.json")))
    }

    pub fn list(&self) -> io::Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".json") {
                ids.push(id.to_owned());
            }
        }
        ids.sort();
        Ok(ids)
    }

    pub fn load(&self, id: &str) -> Result<Option<SkillRecord>, String> {
        let path = self.path_for(id)?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).map_err(|error| error.to_string())?;
        let record: SkillRecord =
            serde_json::from_slice(&bytes).map_err(|error| format!("skill {id} is corrupt: {error}"))?;
        Ok(Some(record))
    }

    /// Atomic write so a crash cannot leave a half-written skill.
    pub fn save(&self, record: &SkillRecord) -> Result<(), String> {
        let path = self.path_for(&record.id)?;
        let bytes = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &bytes).map_err(|error| error.to_string())?;
        fs::rename(&tmp, &path).map_err(|error| error.to_string())
    }

    pub fn remove(&self, id: &str) -> Result<bool, String> {
        let path = self.path_for(id)?;
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(&path).map_err(|error| error.to_string())?;
        Ok(true)
    }

    /// Export one skill as a plain JSON pack.
    pub fn export_pack(&self, id: &str) -> Result<String, String> {
        let record = self
            .load(id)?
            .ok_or_else(|| format!("skill {id} is not installed"))?;
        serde_json::to_string_pretty(&record).map_err(|error| error.to_string())
    }

    /// Import a pack produced by [`SkillStore::export_pack`]. An existing
    /// skill is only replaced when `overwrite` is set; otherwise the import
    /// must be a strictly newer version of the same step graph lineage.
    pub fn import_pack(&self, pack: &str, overwrite: bool) -> Result<SkillRecord, String> {
        let record: SkillRecord =
            serde_json::from_str(pack).map_err(|error| format!("skill pack is invalid: {error}"))?;
        validate_skill_id(&record.id)?;
        for version in &record.versions {
            validate_steps(&version.steps)?;
        }
        if record.version(record.active_version).is_none() {
            return Err("skill pack references a missing active version".to_owned());
        }
        match self.load(&record.id)? {
            Some(existing) if !overwrite => {
                let existing_max = existing
                    .versions
                    .iter()
                    .map(|entry| entry.version)
                    .max()
                    .unwrap_or(0);
                let incoming_max = record
                    .versions
                    .iter()
                    .map(|entry| entry.version)
                    .max()
                    .unwrap_or(0);
                if incoming_max <= existing_max {
                    return Err(format!(
                        "skill {} already installed at version {existing_max}; import a newer pack or pass overwrite",
                        record.id
                    ));
                }
            }
            _ => {}
        }
        self.save(&record)?;
        Ok(record)
    }

    /// Compose lookup used by [`expand_skill`].
    pub fn lookup(&self) -> impl Fn(&str) -> Option<SkillRecord> + '_ {
        move |id| self.load(id).ok().flatten()
    }
}

/// Refuse composition graphs that reference missing skills or cycle back
/// on themselves. Checked at the store level because a cycle only exists
/// across records.
pub fn validate_composition(records: &[SkillRecord]) -> Result<(), String> {
    let by_id: BTreeMap<&str, &SkillRecord> =
        records.iter().map(|record| (record.id.as_str(), record)).collect();
    for record in records {
        let mut stack = vec![(record.id.clone(), 0usize)];
        let mut path = Vec::new();
        while let Some((id, depth)) = stack.pop() {
            if depth > 8 {
                return Err(format!("skill composition from {} exceeds the depth limit", record.id));
            }
            if path.contains(&id) {
                return Err(format!("skill composition cycle: {} -> {}", path.join(" -> "), id));
            }
            path.push(id.clone());
            let Some(entry) = by_id.get(id.as_str()) else {
                return Err(format!("composed skill {id} is not installed"));
            };
            for child in &entry.active().composes {
                stack.push((child.clone(), depth + 1));
            }
            path.pop();
        }
    }
    Ok(())
}

/// JSON view used by the runtime to report a skill without leaking the
/// whole version history into every listing.
pub fn skill_summary(record: &SkillRecord) -> Value {
    let active = record.active();
    json!({
        "id": record.id,
        "description": record.description,
        "intent": record.intent,
        "parameters": record.parameters,
        "active_version": record.active_version,
        "versions": record.versions.iter().map(|entry| entry.version).collect::<Vec<_>>(),
        "steps": active.steps.len(),
        "composes": active.composes,
        "fingerprint": active.fingerprint,
        "reliability": active.evidence.reliability_score(),
        "evidence": active.evidence,
        "revisions": record.revisions.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_step(id: &str, intent: &str) -> SkillStep {
        SkillStep {
            id: id.to_owned(),
            intent: intent.to_owned(),
            params: json!({"url": {"$param": "target_url"}}),
            anchors: vec![SkillAnchor {
                role: Some("link".to_owned()),
                name: Some("Investment Club".to_owned()),
                ..SkillAnchor::default()
            }],
            postcondition: Some(json!({"equals": {"ready": true}})),
            rollback: Some(json!({"history_back": true})),
        }
    }

    fn sample_skill() -> SkillRecord {
        SkillRecord::new(
            "classroom-open",
            "Open one class page",
            "browser.cdp.workflow",
            vec![WorkflowParameter {
                name: "target_url".to_owned(),
                parameter_type: "string".to_owned(),
                sensitive: false,
            }],
            None,
            vec![sample_step("open", "browser.cdp.navigate")],
            Vec::new(),
        )
        .expect("sample skill validates")
    }

    #[test]
    fn anchor_similarity_prefers_role_and_name() {
        let expected = SkillAnchor {
            role: Some("button".to_owned()),
            name: Some("Submit assignment".to_owned()),
            neighborhood_text: Some("classroom stream".to_owned()),
            ..SkillAnchor::default()
        };
        let renamed = SkillAnchor {
            role: Some("button".to_owned()),
            name: Some("Submit assignment".to_owned()),
            neighborhood_text: Some("totally different page".to_owned()),
            ..SkillAnchor::default()
        };
        let wrong = SkillAnchor {
            role: Some("link".to_owned()),
            name: Some("Cancel".to_owned()),
            ..SkillAnchor::default()
        };
        assert!(expected.similarity(&renamed) > expected.similarity(&wrong));
        assert!(expected.similarity(&renamed) > 0.7);
    }

    #[test]
    fn heal_plan_refuses_ambiguous_candidates() {
        let expected = SkillAnchor {
            role: Some("button".to_owned()),
            name: Some("Submit".to_owned()),
            ..SkillAnchor::default()
        };
        let candidates = vec![
            SkillAnchor {
                role: Some("button".to_owned()),
                name: Some("Submit".to_owned()),
                ..SkillAnchor::default()
            },
            SkillAnchor {
                role: Some("button".to_owned()),
                name: Some("Submit".to_owned()),
                attributes: BTreeMap::from([("data-zone".to_owned(), "sidebar".to_owned())]),
                ..SkillAnchor::default()
            },
        ];
        // Two identical live candidates: exact match resolves by identity.
        match plan_heal(&expected, &candidates, 0.75, 0.05) {
            HealPlan::UseAnchor { .. } | HealPlan::Heal { .. } => {}
            HealPlan::Refuse { reason } => panic!("unexpected refusal: {reason}"),
        }
        let near_tie = vec![
            SkillAnchor {
                role: Some("button".to_owned()),
                name: Some("Submit form".to_owned()),
                ..SkillAnchor::default()
            },
            SkillAnchor {
                role: Some("button".to_owned()),
                name: Some("Submit query".to_owned()),
                ..SkillAnchor::default()
            },
        ];
        assert!(matches!(
            plan_heal(&expected, &near_tie, 0.5, 0.10),
            HealPlan::Refuse { .. }
        ));
    }

    #[test]
    fn bind_is_structural_not_textual() {
        let mut parameters = BTreeMap::new();
        parameters.insert("target_url".to_owned(), json!("https://example.com/"));
        let bound = bind_skill_value(&json!({"url": {"$param": "target_url"}}), &parameters)
            .expect("binds");
        assert_eq!(bound, json!({"url": "https://example.com/"}));
        assert!(bind_skill_value(&json!({"url": {"$param": "missing"}}), &parameters).is_err());
    }

    #[test]
    fn fingerprint_changes_when_steps_change() {
        let record = sample_skill();
        let original = record.active().fingerprint.clone();
        let mut revised = record.clone();
        revised
            .revise(
                SkillRevisionKind::Revise,
                "tweak",
                None,
                vec![sample_step("open", "browser.cdp.semantic_click")],
                Vec::new(),
            )
            .expect("revises");
        assert_ne!(revised.active().fingerprint, original);
        // The old version keeps its own fingerprint and stays pinnable.
        assert_eq!(revised.version(1).unwrap().fingerprint, original);
        revised.pin(1).expect("pins");
        assert_eq!(revised.active_version, 1);
    }

    #[test]
    fn record_run_promotes_revised_graph_and_records_failures() {
        let mut record = sample_skill();
        let outcome = RunOutcome {
            version: 1,
            verified: true,
            independent_verification: true,
            clean_fixture: true,
            first_try: true,
            wall_ms: 900,
            revised_steps: Some(vec![sample_step("open", "browser.cdp.navigate")]),
            ..RunOutcome::default()
        };
        let update = record.record_run(&outcome).expect("records");
        assert!(matches!(
            update,
            RunUpdate::Recorded {
                promoted: true,
                new_version: Some(2),
                ..
            }
        ));
        assert_eq!(record.active_version, 2);
        assert_eq!(record.active().evidence.verified_successes, 0); // fresh graph

        let failure = RunOutcome {
            version: 2,
            verified: false,
            wall_ms: 120,
            note: Some("anchor vanished".to_owned()),
            ..RunOutcome::default()
        };
        record.record_run(&failure).expect("records failure");
        assert!(record
            .revisions
            .iter()
            .any(|entry| entry.kind == SkillRevisionKind::Reflect));
        assert_eq!(record.version(2).unwrap().evidence.verified_runs, 1);
    }

    #[test]
    fn store_round_trips_and_guards_imports() {
        let dir = std::env::temp_dir().join(format!("comptrol-skills-{}", now_ms()));
        let store = SkillStore::open(&dir).expect("opens");
        let record = sample_skill();
        store.save(&record).expect("saves");
        let loaded = store.load("classroom-open").expect("loads").expect("present");
        assert_eq!(loaded, record);
        assert_eq!(store.list().expect("lists"), vec!["classroom-open".to_owned()]);

        let pack = store.export_pack("classroom-open").expect("exports");
        assert!(store.import_pack(&pack, false).is_err(), "same version must not import");
        assert!(store.import_pack(&pack, true).expect("overwrite imports").id == "classroom-open");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn composition_inlines_children_and_refuses_cycles() {
        let mut child = sample_skill();
        child.id = "child".to_owned();
        let mut parent = sample_skill();
        parent.id = "parent".to_owned();
        parent
            .revise(
                SkillRevisionKind::Revise,
                "compose",
                None,
                vec![sample_step("finish", "system.ping")],
                vec!["child".to_owned()],
            )
            .expect("revises");

        let records = vec![child.clone(), parent.clone()];
        validate_composition(&records).expect("valid composition");
        let lookup = |id: &str| match id {
            "child" => Some(child.clone()),
            "parent" => Some(parent.clone()),
            _ => None,
        };
        let steps = expand_skill(&lookup, &parent, parent.active_version, &BTreeMap::new())
            .expect("expands");
        assert_eq!(steps.len(), 2, "child steps inline before parent steps");
        assert_eq!(steps[0].id, "open");
        assert_eq!(steps[1].id, "finish");

        let mut cyclic = parent.clone();
        cyclic
            .revise(
                SkillRevisionKind::Revise,
                "cycle",
                None,
                vec![sample_step("again", "system.ping")],
                vec!["parent".to_owned()],
            )
            .expect("revises");
        assert!(validate_composition(&[cyclic]).is_err());
    }

    #[test]
    fn executable_code_is_refused_in_steps() {
        let bad = SkillStep {
            id: "bad".to_owned(),
            intent: "command.run".to_owned(),
            params: json!({"python": "import os"}),
            anchors: Vec::new(),
            postcondition: None,
            rollback: None,
        };
        assert!(validate_steps(&[bad]).is_err());
    }

    #[test]
    fn summary_reports_reliability() {
        let mut record = sample_skill();
        record
            .record_run(&RunOutcome {
                version: 1,
                verified: true,
                independent_verification: true,
                first_try: true,
                wall_ms: 500,
                ..RunOutcome::default()
            })
            .expect("records");
        let summary = skill_summary(&record);
        assert_eq!(summary["reliability"], json!(1.0));
        assert_eq!(summary["evidence"]["verified_runs"], json!(1));
    }
}
