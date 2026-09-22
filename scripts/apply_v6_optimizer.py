#!/usr/bin/env python3
from __future__ import annotations

import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    (ROOT / path).write_text(text, encoding="utf-8")


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one exact match, found {count}: {old[:120]!r}")
    write(path, text.replace(old, new, 1))


def regex_once(path: str, pattern: str, replacement: str, flags: int = re.S) -> None:
    text = read(path)
    updated, count = re.subn(pattern, lambda _match: replacement, text, count=1, flags=flags)
    if count != 1:
        raise RuntimeError(f"{path}: expected one regex match, found {count}: {pattern[:140]!r}")
    write(path, updated)


# ---------------------------------------------------------------------------
# 1. App registry: retain the cached vector for compatibility, but add exact
#    indexes and a paginated substring search that clones only returned rows.
# ---------------------------------------------------------------------------
registry = "crates/comptrol-app-registry/src/registry.rs"
replace_once(
    registry,
    "use std::collections::BTreeMap;",
    "use std::collections::BTreeMap;",
)
replace_once(
    registry,
    """#[derive(Clone)]
struct CachedEntries {
    observed_at: Instant,
    entries: Vec<AppEntry>,
}
""",
    """#[derive(Clone)]
struct CachedEntries {
    observed_at: Instant,
    entries: Vec<AppEntry>,
    by_id: BTreeMap<String, usize>,
    by_name: BTreeMap<String, Vec<usize>>,
}

impl CachedEntries {
    fn new(entries: Vec<AppEntry>) -> Self {
        let mut by_id = BTreeMap::new();
        let mut by_name = BTreeMap::<String, Vec<usize>>::new();
        for (index, entry) in entries.iter().enumerate() {
            by_id.insert(entry.id.to_ascii_lowercase(), index);
            by_name
                .entry(entry.display_name.to_ascii_lowercase())
                .or_default()
                .push(index);
        }
        Self {
            observed_at: Instant::now(),
            entries,
            by_id,
            by_name,
        }
    }

    fn exact_matches(&self, query: &str) -> Vec<AppEntry> {
        let lower = query.to_ascii_lowercase();
        let mut indices = Vec::<usize>::new();
        if let Some(index) = self.by_id.get(&lower) {
            indices.push(*index);
        }
        if let Some(matches) = self.by_name.get(&lower) {
            indices.extend(matches.iter().copied());
        }
        indices.sort_unstable();
        indices.dedup();
        indices
            .into_iter()
            .filter_map(|index| self.entries.get(index).cloned())
            .collect()
    }
}
""",
)
replace_once(
    registry,
    """    if let Ok(mut cache) = registry_cache().write() {
        *cache = Some(CachedEntries {
            observed_at: Instant::now(),
            entries: entries.clone(),
        });
    }
    Ok(entries)
""",
    """    if let Ok(mut cache) = registry_cache().write() {
        *cache = Some(CachedEntries::new(entries.clone()));
    }
    Ok(entries)
""",
)
regex_once(
    registry,
    r"/// All exact matches for `query` \(id match or exact display-name match\)\.\npub fn resolve_all\(query: &str\) -> Result<Vec<AppEntry>, RegistryError> \{.*?\n\}\n",
    """/// All exact matches for `query` (id match or exact display-name match).
pub fn resolve_all(query: &str) -> Result<Vec<AppEntry>, RegistryError> {
    if let Ok(cache) = registry_cache().read()
        && let Some(cached) = cache.as_ref()
        && cached.observed_at.elapsed() <= REGISTRY_CACHE_TTL
    {
        return Ok(cached.exact_matches(query));
    }
    // Refresh once on the cold path, then use the index so warm resolution
    // never clones and scans the complete application inventory.
    let _ = installed_entries()?;
    let cache = registry_cache()
        .read()
        .map_err(|_| std::io::Error::other("application registry cache lock poisoned"))?;
    Ok(cache
        .as_ref()
        .map(|cached| cached.exact_matches(query))
        .unwrap_or_default())
}

/// Search the warm application snapshot without cloning every entry. Results
/// are deterministic and paginated so MCP callers can keep context bounded.
pub fn search(
    query: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<AppEntry>, usize), RegistryError> {
    let stale = registry_cache()
        .read()
        .ok()
        .and_then(|cache| cache.as_ref().map(|cached| cached.observed_at.elapsed() > REGISTRY_CACHE_TTL))
        .unwrap_or(true);
    if stale {
        let _ = installed_entries()?;
    }
    let cache = registry_cache()
        .read()
        .map_err(|_| std::io::Error::other("application registry cache lock poisoned"))?;
    let Some(cached) = cache.as_ref() else {
        return Ok((Vec::new(), 0));
    };
    let needle = query.trim().to_ascii_lowercase();
    let matching = cached
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            needle.is_empty()
                || entry.id.to_ascii_lowercase().contains(&needle)
                || entry.display_name.to_ascii_lowercase().contains(&needle)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let total = matching.len();
    let rows = matching
        .into_iter()
        .skip(offset)
        .take(limit.max(1))
        .filter_map(|index| cached.entries.get(index).cloned())
        .collect();
    Ok((rows, total))
}
""",
)

# ---------------------------------------------------------------------------
# 2. Browser observation: cache the actionable projection inside the page and
#    invalidate it with MutationObserver. Unchanged observations become a
#    constant-time object read instead of another full DOM/shadow-root scan.
# ---------------------------------------------------------------------------
browser = "crates/comptrol-core/src/browser.rs"
regex_once(
    browser,
    r"pub fn compact_snapshot\(\n    endpoint: &str,\n    target_id: &str,\n    browser_context_id: &str,\n    revision: &str,\n    limit: usize,\n\) -> Result<Value, ComptrolError> \{.*?\n\}\n\n#\[derive\(Clone\)\]\nstruct CompactDeltaCacheEntry",
    r'''pub fn compact_snapshot(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: &str,
    limit: usize,
) -> Result<Value, ComptrolError> {
    let limit = limit.clamp(1, 160);
    let expression = format!(
        r#"(() => {{
            const limit = {limit};
            const CACHE_KEY = '__comptrol_action_index_v2';
            const normalize = value => String(value || '').replace(/\s+/g, ' ').trim().slice(0, 160);
            const roleOf = element => element.getAttribute('role') ||
                (element.tagName === 'A' ? 'link' :
                 element.tagName === 'BUTTON' ? 'button' :
                 element.tagName === 'TEXTAREA' ? 'textbox' :
                 element.tagName === 'INPUT' ? 'textbox' :
                 element.tagName === 'SELECT' ? 'combobox' : '');
            const labelledByText = element => normalize(
                (element.getAttribute('aria-labelledby') || '')
                    .split(/\s+/).filter(Boolean)
                    .map(id => element.ownerDocument.getElementById(id))
                    .filter(Boolean)
                    .map(node => node.innerText || node.textContent || '')
                    .join(' ')
            );
            const labelsText = element => normalize(
                element.labels
                    ? [...element.labels].map(label => label.innerText || label.textContent || '').join(' ')
                    : ''
            );
            const nameOf = element => normalize(
                element.getAttribute('aria-label') ||
                labelledByText(element) ||
                labelsText(element) ||
                element.getAttribute('placeholder') ||
                element.getAttribute('title') ||
                element.innerText ||
                element.textContent
            );
            let state = globalThis[CACHE_KEY];
            if (!state || !state.observer) {{
                state = {{ dirty: true, cached: null, observed: new WeakSet(), observer: null, revision: 0 }};
                state.observer = new MutationObserver(() => {{ state.dirty = true; }});
                globalThis[CACHE_KEY] = state;
            }}
            const observeRoot = root => {{
                if (!root || state.observed.has(root)) return;
                try {{
                    state.observer.observe(root, {{
                        subtree: true,
                        childList: true,
                        attributes: true,
                        characterData: true,
                        attributeFilter: [
                            'role','aria-label','aria-labelledby','aria-disabled','disabled','readonly',
                            'hidden','style','class','id','data-testid','data-test-id','href','tabindex',
                            'contenteditable','type','placeholder','title'
                        ]
                    }});
                    state.observed.add(root);
                }} catch (_) {{}}
            }};
            const roots = () => {{
                const pending = [document];
                const seen = [];
                while (pending.length) {{
                    const root = pending.shift();
                    seen.push(root);
                    observeRoot(root);
                    for (const element of root.querySelectorAll('*')) {{
                        if (element.shadowRoot) pending.push(element.shadowRoot);
                        if (element.tagName === 'IFRAME') {{
                            try {{ if (element.contentDocument) pending.push(element.contentDocument); }} catch (_) {{}}
                        }}
                    }}
                }}
                return seen;
            }};
            if (!state.dirty && state.cached && state.cached.limit === limit) {{
                return {{ ...state.cached.value, cache_hit: true, action_index_revision: state.revision }};
            }}
            const candidates = roots().flatMap(root => [...root.querySelectorAll(
                'button, a, input, select, textarea, [contenteditable="true"], [role], [tabindex]'
            )]);
            const visible = candidates.filter(element => {{
                if (!element.isConnected) return false;
                const style = getComputedStyle(element);
                const rect = element.getBoundingClientRect();
                return style.display !== 'none' && style.visibility !== 'hidden' &&
                    Number(style.opacity) !== 0 && rect.width > 0 && rect.height > 0;
            }});
            const elements = visible.slice(0, limit).map(element => ({{
                tag: element.tagName.toLowerCase(),
                role: roleOf(element),
                name: nameOf(element),
                type: normalize(element.getAttribute('type')),
                test_id: normalize(element.getAttribute('data-testid') || element.getAttribute('data-test-id')),
                id: normalize(element.id),
                enabled: !(element.disabled || element.getAttribute('aria-disabled') === 'true'),
                editable: Boolean(element.isContentEditable || element.tagName === 'INPUT' || element.tagName === 'TEXTAREA' || element.tagName === 'SELECT'),
                href_present: Boolean(element.href)
            }}));
            state.revision += 1;
            state.dirty = false;
            state.cached = {{
                limit,
                value: {{
                    title: normalize(document.title),
                    element_count: visible.length,
                    returned: elements.length,
                    truncated: visible.length > elements.length,
                    elements
                }}
            }};
            return {{ ...state.cached.value, cache_hit: false, action_index_revision: state.revision }};
        }})()"#
    );
    let data = cdp_call(
        endpoint,
        target_id,
        Some(browser_context_id),
        Some(revision),
        "Runtime.evaluate",
        json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
    )?;
    let snapshot = data
        .get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({
        "target_id": target_id,
        "browser_context_id": browser_context_id,
        "revision": revision,
        "snapshot": snapshot,
        "observation": "compact_actionable_state",
        "typed_values_included": false,
        "pixels_included": false,
        "verified": true
    }))
}

#[derive(Clone)]
struct CompactDeltaCacheEntry''',
)

# ---------------------------------------------------------------------------
# 3. Core: app.list uses the indexed registry search; add compact capability
#    search so agents do not need to ingest the full catalog.
# ---------------------------------------------------------------------------
core = "crates/comptrol-core/src/lib.rs"
regex_once(
    core,
    r"fn app_list\(request: &OperationRequest, operation_id: String\) -> ActionResult \{.*?\n\}\n\nfn app_launch_with_resource",
    """fn app_list(request: &OperationRequest, operation_id: String) -> ActionResult {
    let query = request
        .params
        .get(\"query\")
        .and_then(Value::as_str)
        .unwrap_or(\"\");
    let offset = request
        .params
        .get(\"offset\")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let limit = request
        .params
        .get(\"limit\")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let detail = request
        .params
        .get(\"detail\")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (entries, total, errors) = match comptrol_app_registry::registry::search(query, offset, limit) {
        Ok((entries, total)) => (entries, total, Vec::<String>::new()),
        Err(error) => (Vec::new(), 0, vec![error.to_string()]),
    };
    let apps = entries
        .into_iter()
        .map(|entry| {
            if detail {
                serde_json::to_value(entry).unwrap_or(Value::Null)
            } else {
                json!({
                    \"id\": entry.id,
                    \"name\": entry.display_name,
                    \"platform\": entry.platform,
                })
            }
        })
        .collect::<Vec<_>>();
    let returned = apps.len();
    let next_offset = (offset + returned < total).then_some(offset + returned);
    success(
        request,
        operation_id,
        \"app_registry_read\",
        EffectState::None,
        VerificationState::Verified,
        json!({
            \"apps\": apps,
            \"count\": total,
            \"returned\": returned,
            \"offset\": offset,
            \"limit\": limit,
            \"next_offset\": next_offset,
            \"compact\": !detail,
            \"provider_errors\": errors,
        }),
    )
}

fn capability_search(request: &OperationRequest, operation_id: String) -> ActionResult {
    let query = request
        .params
        .get(\"query\")
        .and_then(Value::as_str)
        .unwrap_or(\"\")
        .trim()
        .to_ascii_lowercase();
    let offset = request
        .params
        .get(\"offset\")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let limit = request
        .params
        .get(\"limit\")
        .and_then(Value::as_u64)
        .unwrap_or(40)
        .clamp(1, 160) as usize;
    let detail = request
        .params
        .get(\"detail\")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let filtered = capabilities()
        .into_iter()
        .filter(|capability| {
            query.is_empty()
                || capability.name.to_ascii_lowercase().contains(&query)
                || capability.route.to_ascii_lowercase().contains(&query)
        })
        .collect::<Vec<_>>();
    let total = filtered.len();
    let capabilities = filtered
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|capability| {
            if detail {
                serde_json::to_value(capability).unwrap_or(Value::Null)
            } else {
                json!({
                    \"name\": capability.name,
                    \"available\": capability.available,
                    \"risk\": capability.risk,
                    \"route\": capability.route,
                })
            }
        })
        .collect::<Vec<_>>();
    let returned = capabilities.len();
    success(
        request,
        operation_id,
        \"native\",
        EffectState::None,
        VerificationState::Verified,
        json!({
            \"capabilities\": capabilities,
            \"count\": total,
            \"returned\": returned,
            \"offset\": offset,
            \"limit\": limit,
            \"next_offset\": (offset + returned < total).then_some(offset + returned),
            \"compact\": !detail,
        }),
    )
}

fn app_launch_with_resource""",
)

# Policy/default allow list and dispatch for capability.search.
replace_once(
    core,
    '                "system.ping".to_owned(),\n                "desktop.observe".to_owned(),',
    '                "system.ping".to_owned(),\n                "capability.search".to_owned(),\n                "desktop.observe".to_owned(),',
)
# COMPTROL_ALLOW_ALL_INTENTS has a second system.ping entry.
text = read(core)
needle = '                "system.ping".to_owned(),\n                "desktop.observe".to_owned(),'
if needle in text:
    text = text.replace(needle, '                "system.ping".to_owned(),\n                "capability.search".to_owned(),\n                "desktop.observe".to_owned(),', 1)
write(core, text)
replace_once(
    core,
    '            "system.ping" => success(\n',
    '            "capability.search" => capability_search(&request, operation_id),\n            "system.ping" => success(\n',
)
# R0 classifier.
replace_once(
    core,
    '        "system.ping" | "desktop.observe" | "platform.broker.observe" | "workflow.execute" => {',
    '        "system.ping" | "capability.search" | "desktop.observe" | "platform.broker.observe" | "workflow.execute" => {',
)

# ---------------------------------------------------------------------------
# 4. Contextual route learning: keep route-only stats as a fallback for older
#    installations, but learn per intent/provider/platform going forward.
# ---------------------------------------------------------------------------
replace_once(
    core,
    """fn route_plan_with_history(
    request: &OperationRequest,
    history: &HashMap<String, RouteHistory>,
    policy: Option<&Policy>,
) -> RoutePlan {
    let mut plan = route_plan(request, policy);
""",
    """fn route_history_key(request: &OperationRequest, route: &str) -> String {
    let provider = request
        .params
        .get(\"provider\")
        .and_then(Value::as_str)
        .unwrap_or(\"default\");
    format!(\"{}|{}|{}|{}\", route, request.intent, std::env::consts::OS, provider)
}

fn route_plan_with_history(
    request: &OperationRequest,
    history: &HashMap<String, RouteHistory>,
    policy: Option<&Policy>,
) -> RoutePlan {
    let mut plan = route_plan(request, policy);
""",
)
replace_once(
    core,
    """        if let Some(stats) = history.get(&candidate.route) {
            candidate.historical_success = stats.success_rate();
            candidate.expected_p95_ms = stats.p95_latency_ms.or(candidate.expected_p95_ms);
""",
    """        let contextual_key = route_history_key(request, &candidate.route);
        if let Some(stats) = history
            .get(&contextual_key)
            .or_else(|| history.get(&candidate.route))
        {
            candidate.historical_success = stats.success_rate();
            candidate.expected_p95_ms = stats.p95_latency_ms.or(candidate.expected_p95_ms);
""",
)
replace_once(
    core,
    "        self.record_route_outcome(&result, route_started.elapsed().as_secs_f64() * 1_000.0);",
    "        self.record_route_outcome(&request, &result, route_started.elapsed().as_secs_f64() * 1_000.0);",
)
replace_once(
    core,
    """    fn record_route_outcome(&mut self, result: &ActionResult, latency_ms: f64) {
        let entry = self.route_history.entry(result.route.clone()).or_default();
""",
    """    fn record_route_outcome(
        &mut self,
        request: &OperationRequest,
        result: &ActionResult,
        latency_ms: f64,
    ) {
        let route_key = route_history_key(request, &result.route);
        let entry = self.route_history.entry(route_key.clone()).or_default();
""",
)
# Persist contextual key instead of route-only key inside record_route_outcome.
# The record function contains three uses of result.route for persistence; only
# replace inside this function by bounded regex segment.
text = read(core)
start = text.index("    fn record_route_outcome(")
end = text.index("\n    fn next_operation_id", start)
segment = text[start:end]
segment = segment.replace("&result.route", "&route_key")
segment = segment.replace("result.route.clone()", "route_key.clone()")
text = text[:start] + segment + text[end:]
write(core, text)

# ---------------------------------------------------------------------------
# 5. Lightweight stuck/milestone monitor. It stores only hashes and counters,
#    never page text or typed content, and suggests escalation after repeated
#    no-progress outcomes without executing extra mutations.
# ---------------------------------------------------------------------------
replace_once(
    core,
    "const ROUTE_LATENCY_SAMPLE_CAP: usize = 256;\n",
    """const ROUTE_LATENCY_SAMPLE_CAP: usize = 256;

#[derive(Clone, Debug, Default)]
struct ProgressRecord {
    fingerprint: String,
    repeated: u32,
}

fn progress_records() -> &'static std::sync::Mutex<HashMap<String, ProgressRecord>> {
    static RECORDS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, ProgressRecord>>> =
        std::sync::OnceLock::new();
    RECORDS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn progress_control_signal(request: &OperationRequest, result: &ActionResult) -> Value {
    let target = request
        .target
        .as_ref()
        .and_then(|target| target.id.as_deref().or(target.name.as_deref()))
        .unwrap_or("default");
    let key = format!("{}|{}", request.intent, target);
    let selected_state = json!({
        "verification": result.verification,
        "effect": result.effect,
        "error": result.error.as_ref().map(|error| error.code.as_str()),
        "revision": result.data.get("revision"),
        "snapshot_revision": result.data.get("snapshot_revision"),
        "satisfied": result.data.get("satisfied"),
        "done": result.data.get("done"),
    });
    let bytes = serde_json::to_vec(&selected_state).unwrap_or_default();
    let fingerprint = format!("{:x}", Sha256::digest(bytes));
    let milestone = result.error.is_none()
        && result.verification == VerificationState::Verified
        && matches!(result.effect, EffectState::Changed);
    let mut records = match progress_records().lock() {
        Ok(records) => records,
        Err(_) => return json!({"stuck": false, "milestone": milestone}),
    };
    if records.len() > 256 && !records.contains_key(&key) {
        records.clear();
    }
    let record = records.entry(key).or_default();
    if milestone {
        record.fingerprint = fingerprint;
        record.repeated = 0;
    } else if record.fingerprint == fingerprint {
        record.repeated = record.repeated.saturating_add(1);
    } else {
        record.fingerprint = fingerprint;
        record.repeated = 0;
    }
    let stuck = record.repeated >= 2 && !milestone;
    json!({
        "stuck": stuck,
        "milestone": milestone,
        "repeat_count": record.repeated,
        "recommended": if stuck { "escalate_observation_or_replan" } else { "continue" },
    })
}
""",
)
replace_once(
    core,
    """        self.record_route_outcome(&request, &result, route_started.elapsed().as_secs_f64() * 1_000.0);
        self.remember(&request, result.clone());
        result
""",
    """        let mut result = result;
        let control = progress_control_signal(&request, &result);
        if let Some(data) = result.data.as_object_mut() {
            data.insert(\"control\".to_owned(), control);
        }
        self.record_route_outcome(&request, &result, route_started.elapsed().as_secs_f64() * 1_000.0);
        self.remember(&request, result.clone());
        result
""",
)

# ---------------------------------------------------------------------------
# 6. Skill defaults: use indexed capability search and respect stuck signal.
# ---------------------------------------------------------------------------
skill = "plugins/comptrol/skills/comptrol-verified-control/SKILL.md"
skill_text = read(skill)
marker = "Prefer `inspect` before a mutation when the target or capability is not already exact."
if marker not in skill_text:
    raise RuntimeError("skill marker missing")
skill_text = skill_text.replace(
    marker,
    "Prefer `capability.search` with a narrow query when the needed capability is not already known; reserve full `inspect` catalog reads for diagnostics. " + marker,
    1,
)
needle = "- Treat verified `browser.cdp.workflow` executions as local macros: batch deterministic multi-step browser work into one call and keep model turns at zero inside the workflow.\n"
if needle in skill_text:
    skill_text = skill_text.replace(
        needle,
        needle + "- Read the returned `control` signal. Continue normally on milestones; if `stuck=true`, escalate observation fidelity or replan instead of blindly repeating the same action.\n",
        1,
    )
write(skill, skill_text)

print("V6 optimizer pass applied")
