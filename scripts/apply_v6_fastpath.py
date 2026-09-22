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
        raise RuntimeError(f"{path}: expected one exact match, found {count}: {old[:100]!r}")
    write(path, text.replace(old, new, 1))


def regex_once(path: str, pattern: str, replacement: str, flags: int = re.S) -> None:
    text = read(path)
    updated, count = re.subn(pattern, replacement, text, count=1, flags=flags)
    if count != 1:
        raise RuntimeError(f"{path}: expected one regex match, found {count}: {pattern[:120]!r}")
    write(path, updated)


# ---------------------------------------------------------------------------
# 1. Browser Bridge: event-driven local wakeups + durable SQLite fallback.
# ---------------------------------------------------------------------------
bridge = "crates/comptrol-core/src/browser_bridge.rs"
replace_once(
    bridge,
    "use std::sync::atomic::{AtomicU64, Ordering};\nuse std::thread;\nuse std::time::{Duration, SystemTime, UNIX_EPOCH};",
    "use std::sync::atomic::{AtomicU64, Ordering};\nuse std::sync::{Condvar, Mutex, OnceLock};\nuse std::time::{Duration, SystemTime, UNIX_EPOCH};",
)
replace_once(
    bridge,
    "static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);\n",
    """static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Process-local wakeup channel for the durable browser bridge queue. SQLite
/// remains the source of truth across crashes, while active daemon threads use
/// this signal to avoid fixed-interval polling on the hot path.
fn bridge_signal() -> &'static (Mutex<u64>, Condvar) {
    static SIGNAL: OnceLock<(Mutex<u64>, Condvar)> = OnceLock::new();
    SIGNAL.get_or_init(|| (Mutex::new(0), Condvar::new()))
}

fn notify_bridge_waiters() {
    let (generation, changed) = bridge_signal();
    if let Ok(mut value) = generation.lock() {
        *value = value.wrapping_add(1);
        changed.notify_all();
    }
}
""",
)
replace_once(
    bridge,
    """        self.connection
            .execute(
                \"INSERT INTO bridge_commands
                 (request_id, command_type, payload, state, created_at_ms, attempts)
                 VALUES (?1, ?2, ?3, 'pending', ?4, 0)\",
                params![request_id, command_type, payload, now_ms()],
            )
            .map_err(|error| sqlite_error(\"enqueue browser bridge command\", error))?;
        Ok(request_id)
""",
    """        self.connection
            .execute(
                \"INSERT INTO bridge_commands
                 (request_id, command_type, payload, state, created_at_ms, attempts)
                 VALUES (?1, ?2, ?3, 'pending', ?4, 0)\",
                params![request_id, command_type, payload, now_ms()],
            )
            .map_err(|error| sqlite_error(\"enqueue browser bridge command\", error))?;
        notify_bridge_waiters();
        Ok(request_id)
""",
)
replace_once(
    bridge,
    """        let changed = self
            .connection
            .execute(
                \"UPDATE bridge_commands
                 SET state = 'completed', result = ?2, completed_at_ms = ?3, leased_until_ms = NULL
                 WHERE request_id = ?1\",
                params![request_id, encoded, now_ms()],
            )
            .map_err(|error| sqlite_error(\"store browser bridge result\", error))?;
        Ok(changed > 0)
""",
    """        let changed = self
            .connection
            .execute(
                \"UPDATE bridge_commands
                 SET state = 'completed', result = ?2, completed_at_ms = ?3, leased_until_ms = NULL
                 WHERE request_id = ?1\",
                params![request_id, encoded, now_ms()],
            )
            .map_err(|error| sqlite_error(\"store browser bridge result\", error))?;
        if changed > 0 {
            notify_bridge_waiters();
        }
        Ok(changed > 0)
""",
)
regex_once(
    bridge,
    r"    pub fn wait_result\(&self, request_id: &str, timeout: Duration\) -> io::Result<Value> \{.*?\n    \}\n\n    pub fn store_targets",
    """    pub fn wait_result(&self, request_id: &str, timeout: Duration) -> io::Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(result) = self.result(request_id)? {
                return Ok(result);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(\"browser bridge command {request_id} timed out\"),
                ));
            }

            // Capture the generation before the second result read so a result
            // written between the DB query and the wait cannot be missed.
            let (generation, changed) = bridge_signal();
            let observed = *generation
                .lock()
                .map_err(|_| io::Error::other(\"browser bridge signal lock poisoned\"))?;
            if let Some(result) = self.result(request_id)? {
                return Ok(result);
            }
            let guard = generation
                .lock()
                .map_err(|_| io::Error::other(\"browser bridge signal lock poisoned\"))?;
            if *guard != observed {
                continue;
            }
            let (_guard, wait) = changed
                .wait_timeout(guard, remaining)
                .map_err(|_| io::Error::other(\"browser bridge signal lock poisoned\"))?;
            if wait.timed_out() && self.result(request_id)?.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(\"browser bridge command {request_id} timed out\"),
                ));
            }
        }
    }

    /// Lease commands immediately when present, otherwise sleep on the
    /// process-local queue signal until a submit wakes us or the bounded long
    /// poll expires. This keeps SQLite durable without putting fixed sleeps on
    /// every browser command.
    pub fn wait_pending(
        &mut self,
        limit: usize,
        lease_duration: Duration,
        timeout: Duration,
    ) -> io::Result<Vec<BridgeCommand>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let (generation, changed) = bridge_signal();
            let observed = *generation
                .lock()
                .map_err(|_| io::Error::other(\"browser bridge signal lock poisoned\"))?;
            let commands = self.lease_pending(limit, lease_duration)?;
            if !commands.is_empty() || timeout.is_zero() {
                return Ok(commands);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(Vec::new());
            }
            let guard = generation
                .lock()
                .map_err(|_| io::Error::other(\"browser bridge signal lock poisoned\"))?;
            if *guard != observed {
                continue;
            }
            let (_guard, wait) = changed
                .wait_timeout(guard, remaining)
                .map_err(|_| io::Error::other(\"browser bridge signal lock poisoned\"))?;
            if wait.timed_out() {
                return Ok(Vec::new());
            }
        }
    }

    pub fn store_targets""",
)

# ---------------------------------------------------------------------------
# 2. Browser Bridge daemon endpoint: bounded long-poll instead of 200ms host
#    polling. Compatibility is preserved when wait_ms is absent.
# ---------------------------------------------------------------------------
main_rs = "crates/comptrol-core/src/main.rs"
regex_once(
    main_rs,
    r"    if request_line\.starts_with\(\"POST /browser/command/poll \"\) \{.*?\n    \}\n    if request_line\.starts_with\(\"POST /browser/command/result \"\)",
    """    if request_line.starts_with(\"POST /browser/command/poll \") {
        let request_body: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
        let wait_ms = request_body
            .get(\"wait_ms\")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(5_000);
        let commands = {
            let mut queue = command_queue.lock().expect(\"command queue lock poisoned\");
            let _ = queue.record_heartbeat(Some(\"comptrol.browser.bridge/0.1.0\"));
            queue.wait_pending(
                64,
                Duration::from_secs(60),
                Duration::from_millis(wait_ms),
            )
        };
        return match commands {
            Ok(commands) => write_http_response(
                stream,
                200,
                \"OK\",
                \"application/json\",
                serde_json::to_vec(&json!({
                    \"ok\": true,
                    \"commands\": commands,
                    \"count\": commands.len(),
                    \"wait_ms\": wait_ms,
                }))
                .unwrap_or_default(),
                None,
            ),
            Err(error) => write_http_response(
                stream,
                500,
                \"Internal Server Error\",
                \"application/json\",
                serde_json::to_vec(&json!({\"ok\": false, \"error\": error.to_string()}))
                    .unwrap_or_default(),
                None,
            ),
        };
    }
    if request_line.starts_with(\"POST /browser/command/result \")""",
)

# ---------------------------------------------------------------------------
# 3. Native host: one long-polling request stays in flight; daemon identity is
#    cached briefly after HMAC proof, while every bridge request stays signed
#    with a unique nonce.
# ---------------------------------------------------------------------------
host = "extensions/comptrol-browser-bridge/native_host.py"
replace_once(
    host,
    "POLL_INTERVAL_MS = 200\nPOLL_INTERVAL_MAX_MS = 2000\n",
    "LONG_POLL_MS = 1000\nERROR_BACKOFF_MS = 50\nDAEMON_IDENTITY_CACHE_SECONDS = 5.0\n",
)
replace_once(
    host,
    "_daemon_identity_lock = threading.Lock()\n\n\ndef _daemon_post_raw",
    "_daemon_identity_lock = threading.Lock()\n_daemon_identity_verified_until = 0.0\n_daemon_identity_token_digest = None\n\n\ndef _daemon_post_raw",
)
regex_once(
    host,
    r"def verify_daemon_identity\(\):\n    \"\"\"Verify the process currently bound to the daemon port knows the secret\.\"\"\"\n.*?\n\n\ndef daemon_post",
    """def verify_daemon_identity():
    \"\"\"Verify the loopback daemon, reusing a short-lived successful proof.\"\"\"
    global _daemon_identity_verified_until, _daemon_identity_token_digest
    with _daemon_identity_lock:
        token = load_bridge_token()
        if not token:
            return False
        token_digest = hashlib.sha256(token.encode(\"ascii\")).digest()
        now = time.monotonic()
        if (
            _daemon_identity_token_digest == token_digest
            and now < _daemon_identity_verified_until
        ):
            return True
        nonce = secrets.token_hex(32)
        response = _daemon_post_raw(\"/browser-auth/challenge\", {\"nonce\": nonce})
        proof = response.get(\"proof\") if isinstance(response, dict) else None
        expected = hmac.new(
            token.encode(\"ascii\"),
            (PROTOCOL_VERSION + \"\\0\" + nonce).encode(\"ascii\"),
            hashlib.sha256,
        ).hexdigest()
        verified = (
            response.get(\"ok\") is True
            and response.get(\"protocol\") == PROTOCOL_VERSION
            and isinstance(proof, str)
            and hmac.compare_digest(proof.lower(), expected.lower())
        )
        if verified:
            _daemon_identity_token_digest = token_digest
            _daemon_identity_verified_until = now + DAEMON_IDENTITY_CACHE_SECONDS
        else:
            _daemon_identity_token_digest = None
            _daemon_identity_verified_until = 0.0
        return verified


def daemon_post""",
)
regex_once(
    host,
    r"def command_poll_loop\(native_port_ref\):\n    \"\"\"Poll leased daemon commands and keep the active bridge heartbeat fresh\.\"\"\"\n.*?\n\n\ndef main\(\):",
    """def command_poll_loop(native_port_ref):
    \"\"\"Long-poll commands and keep the active bridge heartbeat fresh.\"\"\"
    last_heartbeat = 0.0

    while True:
        if not native_port_ref.get(\"connected\", False):
            time.sleep(0.05)
            continue

        now = time.monotonic()
        if now - last_heartbeat >= 2.0:
            heartbeat = daemon_post(\"/browser/extension/heartbeat\", {
                \"protocol\": PROTOCOL_VERSION,
            })
            if heartbeat.get(\"ok\"):
                last_heartbeat = now

        # The daemon blocks this request on an in-process queue signal. The
        # request returns immediately when a command is submitted and after a
        # bounded timeout when idle, eliminating the old 200 ms pickup tax.
        response = daemon_post(
            \"/browser/command/poll\",
            {\"wait_ms\": LONG_POLL_MS},
        )
        if not response.get(\"ok\"):
            time.sleep(ERROR_BACKOFF_MS / 1000.0)
            continue

        commands = response.get(\"commands\", [])
        if not commands:
            # Old daemons/probe servers can return immediately instead of long
            # polling. Avoid a busy loop while retaining a fast compatibility
            # path.
            time.sleep(0.01)
            continue

        for cmd in commands:
            command_type = cmd.get(\"command_type\", \"\")
            request_id = cmd.get(\"request_id\", \"\")
            payload = cmd.get(\"payload\", {})
            extension_msg = {
                \"type\": command_type,
                \"requestId\": request_id,
                **payload,
            }
            try:
                write_message(extension_msg)
            except Exception as e:
                daemon_post(\"/browser/command/result\", {
                    \"request_id\": request_id,
                    \"ok\": False,
                    \"error\": {
                        \"type\": \"send_failed\",
                        \"details\": str(e),
                    },
                })


def main():""",
)

# ---------------------------------------------------------------------------
# 4. App registry result compression: query + pagination + compact default.
#    The full record remains available with detail=true.
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
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    match comptrol_app_registry::registry::installed_entries() {
        Ok(list) => entries.extend(list),
        Err(error) => errors.push(error.to_string()),
    }
    if !query.trim().is_empty() {
        let needle = query.to_lowercase();
        entries.retain(|entry| {
            entry.id.to_lowercase().contains(&needle)
                || entry.display_name.to_lowercase().contains(&needle)
        });
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries.dedup_by(|a, b| a.id == b.id);
    let total = entries.len();
    let apps = entries
        .into_iter()
        .skip(offset)
        .take(limit)
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

fn app_launch_with_resource""",
)

# ---------------------------------------------------------------------------
# 5. Compact browser observations: delta mode with stable semantic identities.
# ---------------------------------------------------------------------------
browser = "crates/comptrol-core/src/browser.rs"
replace_once(
    browser,
    "use std::collections::HashMap;",
    "use std::collections::{BTreeMap, HashMap};",
)
insert_marker = "pub fn cdp_upload(\n"
text = read(browser)
if text.count(insert_marker) != 1:
    raise RuntimeError("browser.rs: cdp_upload insertion marker mismatch")
delta_code = r'''
#[derive(Clone)]
struct CompactDeltaCacheEntry {
    snapshot_revision: u64,
    elements: BTreeMap<String, Value>,
}

fn compact_delta_cache() -> &'static Mutex<HashMap<String, CompactDeltaCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CompactDeltaCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn compact_element_map(elements: &[Value]) -> BTreeMap<String, Value> {
    let mut seen = HashMap::<String, usize>::new();
    let mut mapped = BTreeMap::new();
    for element in elements {
        let text = |key: &str| element.get(key).and_then(Value::as_str).unwrap_or("");
        let base = if !text("test_id").is_empty() {
            format!("test:{}", text("test_id"))
        } else if !text("id").is_empty() {
            format!("id:{}", text("id"))
        } else {
            format!(
                "{}|{}|{}|{}|{}",
                text("tag"),
                text("role"),
                text("name"),
                text("type"),
                element
                    .get("href_present")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            )
        };
        let ordinal = seen.entry(base.clone()).or_insert(0);
        let key = format!("{base}#{ordinal}");
        *ordinal += 1;
        mapped.insert(key, element.clone());
    }
    mapped
}

/// Return a compact baseline or, when the caller supplies the immediately
/// previous snapshot revision, only semantic element changes. This reduces
/// model context growth while preserving the full compact snapshot as a
/// recovery path when the baseline is missing or stale.
pub fn compact_snapshot_delta(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: &str,
    since_snapshot_revision: Option<u64>,
    limit: usize,
) -> Result<Value, ComptrolError> {
    let mut full = compact_snapshot(
        endpoint,
        target_id,
        browser_context_id,
        revision,
        limit,
    )?;
    let elements = full
        .get("snapshot")
        .and_then(|snapshot| snapshot.get("elements"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let current = compact_element_map(&elements);
    let cache_key = format!("{endpoint}\0{target_id}\0{browser_context_id}");
    let mut cache = compact_delta_cache().lock().map_err(|_| ComptrolError {
        code: "browser_protocol_error".to_owned(),
        message: "Compact snapshot cache lock was poisoned".to_owned(),
        recovery: Some("Retry with a fresh compact snapshot".to_owned()),
    })?;
    if cache.len() > 128 && !cache.contains_key(&cache_key) {
        cache.clear();
    }
    let previous = cache.get(&cache_key).cloned();
    let next_revision = previous
        .as_ref()
        .map(|entry| entry.snapshot_revision.saturating_add(1))
        .unwrap_or(1);

    let can_delta = previous
        .as_ref()
        .zip(since_snapshot_revision)
        .is_some_and(|(entry, requested)| entry.snapshot_revision == requested);
    if can_delta {
        let previous = previous.as_ref().expect("checked above");
        let added = current
            .iter()
            .filter(|(key, _)| !previous.elements.contains_key(*key))
            .map(|(key, value)| json!({"key": key, "element": value}))
            .collect::<Vec<_>>();
        let removed = previous
            .elements
            .iter()
            .filter(|(key, _)| !current.contains_key(*key))
            .map(|(key, value)| json!({"key": key, "element": value}))
            .collect::<Vec<_>>();
        let changed = current
            .iter()
            .filter_map(|(key, value)| {
                previous
                    .elements
                    .get(key)
                    .filter(|old| *old != value)
                    .map(|old| json!({"key": key, "before": old, "after": value}))
            })
            .collect::<Vec<_>>();
        let unchanged = current.len().saturating_sub(added.len() + changed.len());
        cache.insert(
            cache_key,
            CompactDeltaCacheEntry {
                snapshot_revision: next_revision,
                elements: current,
            },
        );
        return Ok(json!({
            "target_id": target_id,
            "browser_context_id": browser_context_id,
            "revision": revision,
            "snapshot_revision": next_revision,
            "previous_snapshot_revision": since_snapshot_revision,
            "observation": "compact_actionable_delta",
            "delta": true,
            "added": added,
            "removed": removed,
            "changed": changed,
            "unchanged_count": unchanged,
            "typed_values_included": false,
            "pixels_included": false,
            "verified": true
        }));
    }

    cache.insert(
        cache_key,
        CompactDeltaCacheEntry {
            snapshot_revision: next_revision,
            elements: current,
        },
    );
    full["snapshot_revision"] = json!(next_revision);
    full["previous_snapshot_revision"] = json!(since_snapshot_revision);
    full["delta"] = json!(false);
    full["baseline_reason"] = json!(if since_snapshot_revision.is_some() {
        "requested baseline missing or stale"
    } else {
        "initial baseline"
    });
    Ok(full)
}

'''
write(browser, text.replace(insert_marker, delta_code + insert_marker, 1))

# Route compact_snapshot through adaptive/delta mode without adding a new tool.
regex_once(
    core,
    r"    if request\.intent == \"browser\.cdp\.compact_snapshot\" \{.*?\n    \}\n    if request\.intent == \"browser\.cdp\.screenshot\"",
    """    if request.intent == \"browser.cdp.compact_snapshot\" {
        let limit = request
            .params
            .get(\"limit\")
            .and_then(Value::as_u64)
            .unwrap_or(64)
            .clamp(1, 160) as usize;
        let mode = request
            .params
            .get(\"mode\")
            .and_then(Value::as_str)
            .unwrap_or(\"auto\");
        if !matches!(mode, \"auto\" | \"compact\" | \"delta\") {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: \"invalid_input\".to_owned(),
                    message: \"Compact browser observation mode must be auto, compact, or delta\"
                        .to_owned(),
                    recovery: Some(\"Use auto unless a full compact baseline is explicitly needed\".to_owned()),
                },
            );
        }
        let since = request
            .params
            .get(\"since_snapshot_revision\")
            .and_then(Value::as_u64);
        let observed = if mode == \"compact\" {
            browser::compact_snapshot(
                &endpoint.to_string_lossy(),
                target_id,
                browser_context_id,
                revision,
                limit,
            )
        } else {
            browser::compact_snapshot_delta(
                &endpoint.to_string_lossy(),
                target_id,
                browser_context_id,
                revision,
                since,
                limit,
            )
        };
        return match observed {
            Ok(data) => success(
                request,
                operation_id,
                \"browser_protocol\",
                EffectState::None,
                VerificationState::Verified,
                data,
            ),
            Err(error) => browser_failure(request, operation_id, error),
        };
    }
    if request.intent == \"browser.cdp.screenshot\"""",
)

# Mark local multi-step workflows explicitly as zero-model-turn execution so
# clients can prefer them for repeated tasks and measure the benefit.
replace_once(
    core,
    '        json!({"steps": completed, "step_count": completed.len(), "verified": true}),',
    '        json!({"steps": completed, "step_count": completed.len(), "verified": true, "local_execution": true, "model_turns": 0}),',
)

# ---------------------------------------------------------------------------
# 6. Benchmark statistics: empirical nearest-rank percentiles. Small sample
#    quantiles must never exceed the slowest observed iteration.
# ---------------------------------------------------------------------------
bench = "bench/runners/run_matrix.py"
replace_once(
    bench,
    "\n\ndef run_task(\n",
    """


def percentile_nearest_rank(values: List[float], percentile: float) -> float:
    \"\"\"Empirical nearest-rank percentile, bounded by observed samples.\"\"\"
    if not values:
        return 0.0
    ordered = sorted(values)
    rank = max(1, min(len(ordered), int((len(ordered) * percentile + 0.9999999999))))
    return ordered[rank - 1]


def run_task(
""",
)
regex_once(
    bench,
    r'        "p50_latency_ms": round\(statistics\.median\(\[r\["latency_ms"\] for r in rows\]\), 2\) if rows else 0,\n        "p95_latency_ms": round\(statistics\.quantiles\(\[r\["latency_ms"\] for r in rows\], n=20\)\[18\] if len\(rows\) >= 2 else \(rows\[0\]\["latency_ms"\] if rows else 0\), 2\),',
    '''        "sample_count": len(rows),
        "min_latency_ms": round(min([r["latency_ms"] for r in rows]), 2) if rows else 0,
        "p50_latency_ms": round(statistics.median([r["latency_ms"] for r in rows]), 2) if rows else 0,
        "p90_latency_ms": round(percentile_nearest_rank([r["latency_ms"] for r in rows], 0.90), 2),
        "p95_latency_ms": round(percentile_nearest_rank([r["latency_ms"] for r in rows], 0.95), 2),
        "p99_latency_ms": round(percentile_nearest_rank([r["latency_ms"] for r in rows], 0.99), 2),
        "max_latency_ms": round(max([r["latency_ms"] for r in rows]), 2) if rows else 0,''',
    flags=0,
)

# ---------------------------------------------------------------------------
# 7. Browser Bridge conformance: prove long-poll use and proof reuse.
# ---------------------------------------------------------------------------
conformance = "scripts/browser_bridge_conformance.py"
replace_once(
    conformance,
    """    if any(item[\"token\"] for item in good_seen):
        raise SystemExit(f\"native host sent legacy bearer token: {good_seen}\")

    print(
""",
    """    if any(item[\"token\"] for item in good_seen):
        raise SystemExit(f\"native host sent legacy bearer token: {good_seen}\")
    polls = [item for item in good_seen if item[\"path\"] == \"/browser/command/poll\"]
    if not polls or not any(item[\"body\"].get(\"wait_ms\", 0) >= 500 for item in polls):
        raise SystemExit(f\"native host did not request bounded long polling: {good_seen}\")
    challenges = [item for item in good_seen if item[\"path\"] == \"/browser-auth/challenge\"]
    signed_bridge = [item for item in good_seen if item[\"path\"].startswith(\"/browser/\") and item[\"signature_valid\"]]
    if len(signed_bridge) > 1 and len(challenges) >= len(signed_bridge):
        raise SystemExit(f\"daemon identity proof was not reused: {good_seen}\")

    print(
""",
)
replace_once(
    conformance,
    '                "verified_daemon_hmac_authentication": "passed",\n',
    '                "verified_daemon_hmac_authentication": "passed",\n                "bounded_long_poll": "passed",\n                "daemon_identity_proof_cache": "passed",\n',
)

# ---------------------------------------------------------------------------
# 8. Skill policy: make compact deltas + local workflows the default strategy.
# ---------------------------------------------------------------------------
skill = "plugins/comptrol/skills/comptrol-verified-control/SKILL.md"
skill_text = read(skill)
needle = "- For dynamic browser forms, prefer one `browser.cdp.workflow` with semantic fill/click steps over repeated model round trips.\n"
if needle not in skill_text:
    raise RuntimeError("skill guidance insertion marker missing")
skill_text = skill_text.replace(
    needle,
    needle
    + "- For browser observation, call `browser.cdp.compact_snapshot` with `mode=auto`; retain `snapshot_revision` and pass it back as `since_snapshot_revision` so subsequent observations return semantic deltas. Escalate to a full accessibility snapshot only when compact state is ambiguous, and to pixels only when semantic state cannot ground the action.\n"
    + "- Treat verified `browser.cdp.workflow` executions as local macros: batch deterministic multi-step browser work into one call and keep model turns at zero inside the workflow.\n"
    + "- Use `app.list` with a narrow `query` and the default compact page; request `detail=true` only for the exact app that needs full metadata.\n",
    1,
)
write(skill, skill_text)

print("V6 FastPath patch applied")
