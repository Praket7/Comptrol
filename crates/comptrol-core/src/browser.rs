use crate::{
    BrowserTarget, ComptrolError, MAX_PROTOCOL_BYTES, bind_browser_target,
    browser_bridge::{BridgeStore, COMPANION_BRIDGE_ENDPOINT, DEFAULT_HEALTH_MAX_AGE},
};
// Compatibility facade: new browser callers should use the async persistent
// multiplexer. The event-only compatibility helpers below are retained while
// their predicates are moved onto the browser EventHub.
pub use comptrol_browser::{
    BlockingBrowserManager, BrowserCommand, BrowserConnection, BrowserError, BrowserManager,
    FrameGraph, FrameRecord, TargetGraph, TargetRecord,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct TargetCacheEntry {
    observed_at: Instant,
    targets: Vec<BrowserTarget>,
}

type TargetCaches = HashMap<String, TargetCacheEntry>;

fn target_caches() -> &'static Mutex<TargetCaches> {
    static CACHES: OnceLock<Mutex<TargetCaches>> = OnceLock::new();
    CACHES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn is_companion_bridge(endpoint: &str) -> bool {
    endpoint == COMPANION_BRIDGE_ENDPOINT
}

fn open_bridge_store() -> Result<BridgeStore, ComptrolError> {
    BridgeStore::open(&crate::default_state_dir()).map_err(|error| ComptrolError {
        code: "browser_bridge_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some(
            "Start the Comptrol daemon and reconnect the Browser Bridge extension".to_owned(),
        ),
    })
}

fn bridge_command(
    command_type: &str,
    payload: Value,
    timeout: Duration,
) -> Result<Value, ComptrolError> {
    let mut store = open_bridge_store()?;
    let health = store
        .health(DEFAULT_HEALTH_MAX_AGE)
        .map_err(|error| ComptrolError {
            code: "browser_bridge_unavailable".to_owned(),
            message: error.to_string(),
            recovery: Some("Reconnect the Browser Bridge extension".to_owned()),
        })?;
    if !health.active {
        return Err(ComptrolError {
            code: "browser_bridge_unavailable".to_owned(),
            message: "The Browser Bridge has no recent extension heartbeat".to_owned(),
            recovery: Some(
                "Open Chrome with the Comptrol Browser Bridge extension enabled".to_owned(),
            ),
        });
    }
    let request_id = store
        .submit(command_type, payload)
        .map_err(|error| ComptrolError {
            code: if error.kind() == io::ErrorKind::WouldBlock {
                "browser_bridge_busy"
            } else {
                "browser_bridge_unavailable"
            }
            .to_owned(),
            message: error.to_string(),
            recovery: Some("Retry after the bridge drains pending commands".to_owned()),
        })?;
    let response = store
        .wait_result(&request_id, timeout)
        .map_err(|error| ComptrolError {
            code: if error.kind() == io::ErrorKind::TimedOut {
                "browser_bridge_timeout"
            } else {
                "browser_bridge_unavailable"
            }
            .to_owned(),
            message: error.to_string(),
            recovery: Some("Verify the Browser Bridge extension is connected and retry".to_owned()),
        })?;
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(ComptrolError {
            code: "browser_protocol_error".to_owned(),
            message: response
                .get("error")
                .map(Value::to_string)
                .unwrap_or_else(|| "Browser Bridge command failed".to_owned()),
            recovery: Some("Inspect the exact browser target and retry".to_owned()),
        })
    }
}

fn bridge_targets() -> Result<Vec<BrowserTarget>, ComptrolError> {
    let store = open_bridge_store()?;
    let values = store.targets().map_err(|error| ComptrolError {
        code: "browser_bridge_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Wait for the extension to publish a fresh target snapshot".to_owned()),
    })?;
    Ok(values
        .into_iter()
        .filter_map(|value| {
            let id = value
                .get("id")
                .or_else(|| value.get("targetId"))
                .and_then(Value::as_str)?
                .to_owned();
            let url = value.get("url").and_then(Value::as_str).map(str::to_owned);
            let revision = value
                .get("revision")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    Some(format!(
                        "bridge:{id}:{}",
                        url.as_deref().unwrap_or_default()
                    ))
                });
            Some(BrowserTarget {
                id,
                target_type: Some(
                    value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("page")
                        .to_owned(),
                ),
                browser_context_id: Some(
                    value
                        .get("browserContextId")
                        .or_else(|| value.get("browser_context_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        .to_owned(),
                ),
                url,
                title: value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                revision,
                web_socket_url: None,
            })
        })
        .collect())
}

pub fn discover(endpoint: &str) -> Result<Vec<BrowserTarget>, ComptrolError> {
    if is_companion_bridge(endpoint) {
        let targets = bridge_targets()?;
        if let Ok(mut caches) = target_caches().lock() {
            caches.insert(
                endpoint.to_owned(),
                TargetCacheEntry {
                    observed_at: Instant::now(),
                    targets: targets.clone(),
                },
            );
        }
        return Ok(targets);
    }
    let targets = if is_websocket_endpoint(endpoint) {
        let value = protocol_call(endpoint, "Target.getTargets", json!({}))?;
        parse_cdp_target_infos(&value)?
    } else {
        let value = get_json(endpoint, "/json/list").map_err(|error| ComptrolError {
            code: "browser_unavailable".to_owned(),
            message: error.to_string(),
            recovery: Some("Start a supported browser with remote debugging enabled".to_owned()),
        })?;
        parse_targets(&value).ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser returned an invalid target list".to_owned(),
            recovery: Some("Inspect the configured DevTools endpoint".to_owned()),
        })?
    };
    if let Ok(mut caches) = target_caches().lock() {
        caches.insert(
            endpoint.to_owned(),
            TargetCacheEntry {
                observed_at: Instant::now(),
                targets: targets.clone(),
            },
        );
    }
    Ok(targets)
}

fn discover_cached(endpoint: &str) -> Result<Vec<BrowserTarget>, ComptrolError> {
    if is_companion_bridge(endpoint) {
        return discover(endpoint);
    }
    if let Ok(caches) = target_caches().lock()
        && let Some(entry) = caches.get(endpoint)
        && entry.observed_at.elapsed() <= Duration::from_secs(2)
    {
        return Ok(entry.targets.clone());
    }
    discover(endpoint)
}

/// Return the current target snapshot, reusing the event-invalidated cache for
/// normal actions. Explicit inspection still uses [`discover`] when callers
/// request a fresh browser inventory.
pub fn discover_cached_targets(endpoint: &str) -> Result<Vec<BrowserTarget>, ComptrolError> {
    discover_cached(endpoint)
}

/// Recovery-only target readback used when a browser navigation invalidates
/// the compatibility bridge's cached session before its event update arrives.
/// Ordinary operations remain event-driven and do not call `/json/list`.
pub fn fresh_target_url(endpoint: &str, target_id: &str) -> Result<Option<String>, ComptrolError> {
    Ok(discover(endpoint)?
        .into_iter()
        .find(|target| target.id == target_id)
        .and_then(|target| target.url))
}

/// Decide whether the requested navigation postcondition already holds so
/// the fast path can skip `Page.navigate` entirely (the ESPN problem:
/// dynamic sites must not be reloaded when the requested state is live).
///
/// The check compares the current URL against the requested URL (exact or
/// `url_contains`) and optionally evaluates an in-page readiness
/// expression. Returns the observation needed to answer without mutating.
/// Read the event-maintained live target state from the persistent graph
/// (the TargetStateCache read path). Returns None when the target is not
/// present in the graph; callers fall back to discovery only for recovery.
pub fn live_target_state(
    endpoint: &str,
    target_id: &str,
) -> Result<Option<comptrol_browser::TargetRecord>, ComptrolError> {
    if is_companion_bridge(endpoint) {
        let target = discover(endpoint)?
            .into_iter()
            .find(|target| target.id == target_id);
        return Ok(target.map(|target| comptrol_browser::TargetRecord {
            id: target.id,
            target_type: target.target_type.unwrap_or_else(|| "page".to_owned()),
            browser_context_id: target.browser_context_id,
            session_id: None,
            url: target.url,
            title: target.title,
            opener_id: None,
            attached: true,
            generation: 0,
            revision: target
                .revision
                .unwrap_or_else(|| "bridge:unknown".to_owned()),
        }));
    }
    let browser_web_socket_url = browser_websocket_endpoint(endpoint)?;
    bridge()
        .target_state(&browser_web_socket_url, target_id)
        .map_err(|error| ComptrolError {
            code: "browser_protocol_error".to_owned(),
            message: error.to_string(),
            recovery: Some("Refresh the browser connection and retry".to_owned()),
        })
}

pub fn ensure_state(
    endpoint: &str,
    target_id: &str,
    browser_context_id: Option<&str>,
    revision: Option<&str>,
    url: Option<&str>,
    url_contains: Option<&str>,
    ready_expression: Option<&str>,
) -> Result<Value, ComptrolError> {
    // Prefer the event-maintained live graph (no network); only fall back
    // to discovery when the target is absent or the caller bound it with a
    // legacy revision that the graph cannot confirm.
    let graph_state = live_target_state(endpoint, target_id)?;
    let targets = discover_cached(endpoint)?;
    let target = crate::bind_browser_target(&targets, target_id, browser_context_id, revision)?;
    let current_url = graph_state
        .as_ref()
        .and_then(|record| record.url.clone())
        .or_else(|| target.url.clone())
        .unwrap_or_default();

    // URL identity gate: exact match or containment, mirroring the
    // navigate verification criteria so a skipped navigation proves the
    // same postcondition a performed one would have.
    let url_satisfied = match (url, url_contains) {
        (Some(expected), Some(fragment)) => {
            current_url == expected || current_url.contains(fragment)
        }
        (Some(expected), None) => current_url == expected,
        (None, Some(fragment)) => current_url.contains(fragment),
        (None, None) => true,
    };

    // Optional readiness probe on the live document (SPA state that a URL
    // alone cannot prove). Only runs when the URL gate already passed.
    let mut ready = Option::<Value>::None;
    if url_satisfied && let Some(expression) = ready_expression {
        let result = cdp_call(
            endpoint,
            target_id,
            browser_context_id,
            target.revision.as_deref(),
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
        )?;
        // The bridge already unwraps the CDP response envelope, so the
        // evaluation payload lives at result.result.value (matching the
        // shape every other Runtime.evaluate caller observes).
        let value = result
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        // An unknown expression result (null) must count as *not passed*
        // rather than silently granting readiness; boolean true and
        // non-empty values count as readiness signals.
        let passed = value.as_bool() == Some(true)
            || (value.is_string() && !value.as_str().unwrap_or_default().is_empty())
            || (value.is_number() && value.as_f64().unwrap_or_default() != 0.0);
        ready = Some(json!({ "expression": expression, "value": value, "passed": passed }));
    }

    Ok(json!({
        "current_url": current_url,
        "url_requested": url,
        "url_contains": url_contains,
        "url_satisfied": url_satisfied,
        "ready": ready,
        "satisfied": url_satisfied && ready.as_ref().map(|r| r["passed"] == json!(true)).unwrap_or(true),
        "revision": target.revision,
    }))
}

fn invalidate_target_cache(endpoint: &str) {
    if let Ok(mut caches) = target_caches().lock() {
        caches.remove(endpoint);
    }
}

pub fn parse_targets(value: &Value) -> Option<Vec<BrowserTarget>> {
    value.as_array()?.iter().map(parse_target).collect()
}

pub fn fixture_submit(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: &str,
    idempotency_key: &str,
    message: &str,
) -> Result<Value, ComptrolError> {
    let targets = discover_cached(endpoint)?;
    bind_browser_target(
        &targets,
        target_id,
        Some(browser_context_id),
        Some(revision),
    )?;
    if [target_id, browser_context_id, revision, idempotency_key]
        .iter()
        .any(|value| {
            value
                .chars()
                .any(|character| matches!(character, '\r' | '\n'))
        })
    {
        return Err(ComptrolError {
            code: "invalid_input".to_owned(),
            message: "Browser identity headers cannot contain line breaks".to_owned(),
            recovery: None,
        });
    }
    let (status, value) = request_json(
        endpoint,
        "POST",
        "/submit",
        &[
            ("X-Comptrol-Target-Id", target_id),
            ("X-Comptrol-Browser-Context", browser_context_id),
            ("X-Comptrol-Idempotency-Key", idempotency_key),
        ],
        Some(&json!({ "message": message }).to_string()),
    )
    .map_err(|error| ComptrolError {
        code: "browser_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Inspect the browser target and retry once it is healthy".to_owned()),
    })?;
    if status != 200 {
        return Err(ComptrolError {
            code: value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("browser_request_failed")
                .to_owned(),
            message: "The browser fixture refused the request".to_owned(),
            recovery: Some("Refresh the target identity before retrying".to_owned()),
        });
    }
    Ok(value)
}

pub fn fixture_state(endpoint: &str) -> Result<Value, ComptrolError> {
    get_json(endpoint, "/state").map_err(|error| ComptrolError {
        code: "browser_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Inspect the browser fixture before reconciling".to_owned()),
    })
}

pub fn open_tab(
    endpoint: &str,
    url: &str,
    background: bool,
    browser_context_id: Option<&str>,
) -> Result<Value, ComptrolError> {
    validate_url(url)?;
    if is_companion_bridge(endpoint) {
        return bridge_command(
            "open_tab",
            json!({
                "url": url,
                "background": background,
                "browserContextId": browser_context_id.unwrap_or("default"),
            }),
            Duration::from_secs(10),
        );
    }
    if !is_websocket_endpoint(endpoint) && !background && browser_context_id.is_none() {
        let path = format!("/json/new?{}", encode_new_tab_url(url));
        let (status, value) =
            request_json(endpoint, "PUT", &path, &[], None).map_err(|error| ComptrolError {
                code: "browser_unavailable".to_owned(),
                message: error.to_string(),
                recovery: Some("Start the visible browser with local DevTools enabled".to_owned()),
            })?;
        if status != 200 {
            return Err(ComptrolError {
                code: "browser_request_failed".to_owned(),
                message: "The browser refused to open a visible tab".to_owned(),
                recovery: Some("Inspect the local browser endpoint".to_owned()),
            });
        }
        let target = parse_target(&value).ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not return the opened tab identity".to_owned(),
            recovery: Some("Inspect the browser target list".to_owned()),
        })?;
        return Ok(json!({
            "target": target,
            "visibility": "foreground",
            "profile": "attached_existing_browser",
            "account_state": "same_browser_profile",
            "mouse": "untouched",
            "clipboard": "untouched",
            "verified": true
        }));
    }
    let web_socket_url = browser_websocket_endpoint(endpoint)?;
    let mut create_params = json!({
        "url": url,
        "background": background,
        "focus": !background,
        "newWindow": false
    });
    if let Some(browser_context_id) = real_context_id(browser_context_id) {
        create_params["browserContextId"] = json!(browser_context_id);
    }
    let created = protocol_call(&web_socket_url, "Target.createTarget", create_params)?;
    let target_id = created
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not return the opened tab identity".to_owned(),
            recovery: Some("Inspect the browser target list".to_owned()),
        })?;
    wait_for_event(&web_socket_url, Duration::from_secs(2), |event| {
        event.get("method").and_then(Value::as_str) == Some("Target.targetCreated")
            && event
                .get("params")
                .and_then(|params| params.get("targetInfo"))
                .and_then(|target| target.get("targetId"))
                .and_then(Value::as_str)
                == Some(target_id)
    })?;
    invalidate_target_cache(endpoint);
    let targets = discover(endpoint)?;
    if let Some(target) = targets.into_iter().find(|target| target.id == target_id) {
        if browser_context_id
            .is_some_and(|expected| target.browser_context_id.as_deref() != Some(expected))
        {
            return Err(ComptrolError {
                code: "stale_reference".to_owned(),
                message: "The browser created the tab in a different browser context".to_owned(),
                recovery: Some("Inspect browser contexts and open the tab again".to_owned()),
            });
        }
        return Ok(json!({
            "target": target,
            "visibility": if background { "background" } else { "foreground" },
            "profile": "attached_existing_browser",
            "account_state": "same_browser_profile",
            "mouse": "untouched",
            "clipboard": "untouched",
            "verified": true
        }));
    }
    Err(ComptrolError {
        code: "verification_failed".to_owned(),
        message: "The browser did not expose the new background tab".to_owned(),
        recovery: Some("Inspect browser targets before retrying".to_owned()),
    })
}

pub fn launch_chrome_tab(url: &str) -> Result<Value, ComptrolError> {
    validate_url(url)?;
    let status = if cfg!(target_os = "macos") {
        Command::new("open")
            .args(["-a", "Google Chrome", url])
            .status()
    } else if cfg!(target_os = "windows") {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Process -FilePath $env:COMPTROL_CHROME_URL",
            ])
            .env("COMPTROL_CHROME_URL", url)
            .status()
    } else if cfg!(target_os = "linux") {
        Command::new("xdg-open").arg(url).status()
    } else {
        return Err(ComptrolError {
            code: "unsupported_surface".to_owned(),
            message: "Chrome launcher control is unsupported on this operating system".to_owned(),
            recovery: Some("Use the configured browser DevTools route".to_owned()),
        });
    };
    match status {
        Ok(status) if status.success() => Ok(json!({
            "browser": "Google Chrome",
            "url": url,
            "visibility": "foreground",
            "profile": "existing_default_browser_profile",
            "account_state": "preserved_by_browser",
            "postcondition": "launcher_accepted",
            "mouse": "untouched",
            "clipboard": "untouched"
        })),
        Ok(status) => Err(ComptrolError {
            code: "launch_failed".to_owned(),
            message: format!("Chrome launcher returned {status}"),
            recovery: Some("Check that Google Chrome is installed".to_owned()),
        }),
        Err(error) => Err(ComptrolError {
            code: "launch_unavailable".to_owned(),
            message: error.to_string(),
            recovery: Some("Check that the native browser launcher is available".to_owned()),
        }),
    }
}

pub fn validate_url(url: &str) -> Result<(), ComptrolError> {
    if url.is_empty()
        || url
            .chars()
            .any(|character| character == '\r' || character == '\n')
        || !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("about:"))
    {
        return Err(ComptrolError {
            code: "invalid_input".to_owned(),
            message: "Browser tabs accept only http, https, or about URLs".to_owned(),
            recovery: Some("Provide a safe browser URL".to_owned()),
        });
    }
    Ok(())
}

pub fn close_tab(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: &str,
) -> Result<Value, ComptrolError> {
    let targets = discover_cached(endpoint)?;
    if is_companion_bridge(endpoint) {
        let target = crate::bind_browser_target(
            &targets,
            target_id,
            Some(browser_context_id),
            Some(revision),
        )?;
        let result = bridge_command(
            "close_tab",
            json!({"targetId": target.id}),
            Duration::from_secs(10),
        )?;
        invalidate_target_cache(endpoint);
        return Ok(result);
    }
    let target = crate::bind_browser_target(
        &targets,
        target_id,
        Some(browser_context_id),
        Some(revision),
    )?;
    let web_socket_url = browser_websocket_endpoint(endpoint)?;
    let value = protocol_call(
        &web_socket_url,
        "Target.closeTarget",
        json!({ "targetId": target_id }),
    )?;
    wait_for_event(&web_socket_url, Duration::from_secs(2), |event| {
        event.get("method").and_then(Value::as_str) == Some("Target.targetDestroyed")
            && event
                .get("params")
                .and_then(|params| params.get("targetId"))
                .and_then(Value::as_str)
                == Some(target_id)
    })?;
    invalidate_target_cache(endpoint);
    if discover(endpoint)
        .map(|remaining| remaining.iter().all(|item| item.id != target_id))
        .unwrap_or(false)
    {
        return Ok(json!({
            "target": target,
            "closed": true,
            "response": value,
            "mouse": "untouched",
            "clipboard": "untouched",
            "verified": true
        }));
    }
    Err(ComptrolError {
        code: "verification_failed".to_owned(),
        message: "The browser did not confirm that the exact target closed".to_owned(),
        recovery: Some("Inspect browser targets before retrying".to_owned()),
    })
}

pub fn history(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: &str,
    forward: bool,
) -> Result<Value, ComptrolError> {
    let targets = discover_cached(endpoint)?;
    if is_companion_bridge(endpoint) {
        let target = crate::bind_browser_target(
            &targets,
            target_id,
            Some(browser_context_id),
            Some(revision),
        )?;
        let result = bridge_command(
            "history",
            json!({"targetId": target.id, "forward": forward}),
            Duration::from_secs(10),
        )?;
        invalidate_target_cache(endpoint);
        return Ok(result);
    }
    let target = crate::bind_browser_target(
        &targets,
        target_id,
        Some(browser_context_id),
        Some(revision),
    )?;
    let current = cdp_call(
        endpoint,
        target_id,
        Some(browser_context_id),
        target.revision.as_deref(),
        "Page.getNavigationHistory",
        json!({}),
    )?;
    let current_index = current
        .get("currentIndex")
        .and_then(Value::as_i64)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not return a navigation history index".to_owned(),
            recovery: Some("Inspect the exact browser target again".to_owned()),
        })?;
    let entries = current
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not return navigation history entries".to_owned(),
            recovery: Some("Inspect the exact browser target again".to_owned()),
        })?;
    let destination_index = if forward {
        current_index.saturating_add(1)
    } else {
        current_index.saturating_sub(1)
    };
    let Some(destination) = entries.get(destination_index as usize) else {
        return Err(ComptrolError {
            code: "history_unavailable".to_owned(),
            message: if forward {
                "The exact browser target has no forward history".to_owned()
            } else {
                "The exact browser target has no back history".to_owned()
            },
            recovery: Some(
                "Inspect the current target before requesting another history step".to_owned(),
            ),
        });
    };
    let entry_id = destination
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser history entry has no numeric id".to_owned(),
            recovery: Some("Inspect the exact browser target again".to_owned()),
        })?;
    cdp_call(
        endpoint,
        target_id,
        Some(browser_context_id),
        target.revision.as_deref(),
        "Page.navigateToHistoryEntry",
        json!({ "entryId": entry_id }),
    )?;
    // The navigation command rides the persistent browser-level connection,
    // so its lifecycle event arrives there too. Waiting on a fresh page-level
    // socket here would miss the event and time out.
    let browser_event_socket = browser_websocket_endpoint(endpoint)?;
    wait_for_event(&browser_event_socket, Duration::from_secs(2), |event| {
        matches!(
            event.get("method").and_then(Value::as_str),
            Some("Page.frameNavigated") | Some("Page.navigatedWithinDocument")
        )
    })?;
    let observed = cdp_call(
        endpoint,
        target_id,
        Some(browser_context_id),
        None,
        "Page.getNavigationHistory",
        json!({}),
    )?;
    if observed.get("currentIndex").and_then(Value::as_i64) == Some(destination_index) {
        return Ok(json!({
            "direction": if forward { "forward" } else { "back" },
            "entry": destination,
            "current_index": destination_index,
            "verified": true,
            "wait": "protocol_event",
            "mouse": "untouched",
            "clipboard": "untouched"
        }));
    }
    Err(ComptrolError {
        code: "verification_failed".to_owned(),
        message: "The browser did not confirm the requested history step".to_owned(),
        recovery: Some(
            "Inspect the target and current navigation history before retrying".to_owned(),
        ),
    })
}

/// Wait for a navigation event on the already-bound target, then verify the
/// final URL from a fresh target observation. This avoids interval polling in
/// compact workflows while preserving exact target identity at the finish gate.
pub fn wait_for_url(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    contains: &str,
    timeout: Duration,
) -> Result<Value, ComptrolError> {
    if contains.is_empty() || contains.chars().any(|character| character.is_control()) {
        return Err(ComptrolError {
            code: "invalid_input".to_owned(),
            message: "URL postconditions must contain non-control text".to_owned(),
            recovery: None,
        });
    }
    if is_companion_bridge(endpoint) {
        let deadline = Instant::now() + timeout;
        loop {
            let targets = discover(endpoint)?;
            let target =
                crate::bind_browser_target(&targets, target_id, Some(browser_context_id), None)?;
            if target
                .url
                .as_deref()
                .is_some_and(|url| url.contains(contains))
            {
                return Ok(json!({
                    "url": target.url,
                    "wait": "bridge_target_snapshot",
                    "verified": true,
                }));
            }
            if Instant::now() >= deadline {
                return Err(ComptrolError {
                    code: "verification_failed".to_owned(),
                    message: format!("Browser URL did not contain {contains}"),
                    recovery: Some(
                        "Inspect the target and retry with a bounded postcondition".to_owned(),
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let targets = discover_cached(endpoint)?;
    let target = crate::bind_browser_target(&targets, target_id, Some(browser_context_id), None)?;
    if target
        .url
        .as_deref()
        .is_some_and(|url| url.contains(contains))
    {
        return Ok(json!({
            "url": target.url,
            "wait": "state_observation",
            "verified": true
        }));
    }
    let web_socket_url = target.web_socket_url.ok_or_else(|| ComptrolError {
        code: "browser_protocol_invalid".to_owned(),
        message: "The target did not provide a websocket debugger URL".to_owned(),
        recovery: Some("Inspect browser targets again".to_owned()),
    })?;
    let event = wait_for_event(&web_socket_url, timeout, |event| {
        let method = event.get("method").and_then(Value::as_str);
        matches!(
            method,
            Some("Page.frameNavigated") | Some("Page.navigatedWithinDocument")
        )
    })?;
    invalidate_target_cache(endpoint);
    let observed = discover(endpoint)?;
    let bound = crate::bind_browser_target(&observed, target_id, Some(browser_context_id), None)?;
    if bound
        .url
        .as_deref()
        .is_some_and(|url| url.contains(contains))
    {
        return Ok(json!({
            "url": bound.url,
            "event": event,
            "wait": "protocol_event",
            "verified": true
        }));
    }
    Err(ComptrolError {
        code: "verification_failed".to_owned(),
        message: format!("Browser URL did not contain {contains}"),
        recovery: Some("Inspect the target and retry with a bounded postcondition".to_owned()),
    })
}

/// Capture bounded visual evidence for one exact target. The image itself is
/// intentionally not returned through MCP by default: callers get a stable
/// digest and encoded size that can be compared during target-scoped recovery
/// without flooding the control channel with pixels.
#[allow(clippy::too_many_arguments)]
pub fn capture_screenshot(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: &str,
    format: &str,
    quality: Option<u64>,
    clip: Value,
    include_pixels: bool,
) -> Result<Value, ComptrolError> {
    let viewport = cdp_call(
        endpoint,
        target_id,
        Some(browser_context_id),
        Some(revision),
        "Runtime.evaluate",
        json!({
            "expression": "(() => ({ width: Math.max(1, Math.floor(window.innerWidth)), height: Math.max(1, Math.floor(window.innerHeight)), device_pixel_ratio: window.devicePixelRatio }))()",
            "returnByValue": true
        }),
    )?;
    let viewport = viewport
        .get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .unwrap_or_else(|| json!({"width": 0, "height": 0}));
    let params = json!({
        "format": format,
        "captureBeyondViewport": false,
        "fromSurface": true,
        "quality": quality,
        "clip": clip
    });
    let value = cdp_call(
        endpoint,
        target_id,
        Some(browser_context_id),
        Some(revision),
        "Page.captureScreenshot",
        params,
    )?;
    let data = value
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not return screenshot data".to_owned(),
            recovery: Some("Inspect the exact browser target and screenshot support".to_owned()),
        })?;
    if data.len() > MAX_PROTOCOL_BYTES {
        return Err(ComptrolError {
            code: "browser_message_too_large".to_owned(),
            message: "The browser screenshot exceeded the bounded evidence size".to_owned(),
            recovery: Some("Use a smaller clip or a lower quality setting".to_owned()),
        });
    }
    let mut digest = Sha256::new();
    digest.update(data.as_bytes());
    let digest = digest.finalize();
    let digest_hex = format!("{digest:x}");
    let width = viewport.get("width").and_then(Value::as_u64).unwrap_or(0);
    let height = viewport.get("height").and_then(Value::as_u64).unwrap_or(0);
    let capture_id = format!(
        "{target_id}:{browser_context_id}:{revision}:{format}:{width}x{height}:{digest_hex}"
    );
    let mut result = json!({
        "target_id": target_id,
        "browser_context_id": browser_context_id,
        "revision": revision,
        "format": format,
        "encoded_bytes": data.len(),
        "sha256_base64_payload": digest_hex,
        "capture_id": capture_id,
        "viewport": viewport,
        "clip": clip,
        "verified": true,
        "evidence": if include_pixels { "target_scoped_pixels" } else { "target_scoped_visual_digest" }
    });
    if include_pixels {
        result["data_base64"] = json!(data);
    }
    Ok(result)
}

/// Click one coordinate only when it is bound to an immediately verifiable
/// screenshot capture. A changed pixel digest or viewport refuses the action
/// as stale instead of guessing against moved page geometry.
#[allow(clippy::too_many_arguments)]
pub fn coordinate_click(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: &str,
    capture_id: &str,
    x: f64,
    y: f64,
    button: &str,
) -> Result<Value, ComptrolError> {
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return Err(ComptrolError {
            code: "invalid_input".to_owned(),
            message: "Coordinate clicks require finite non-negative viewport coordinates"
                .to_owned(),
            recovery: None,
        });
    }
    if !matches!(button, "none" | "left" | "middle" | "right") {
        return Err(ComptrolError {
            code: "invalid_input".to_owned(),
            message: "Coordinate clicks support none, left, middle, or right buttons".to_owned(),
            recovery: None,
        });
    }
    let current = capture_screenshot(
        endpoint,
        target_id,
        browser_context_id,
        revision,
        "png",
        None,
        Value::Null,
        false,
    )?;
    let width = current["viewport"]["width"].as_u64().unwrap_or(0) as f64;
    let height = current["viewport"]["height"].as_u64().unwrap_or(0) as f64;
    if x >= width || y >= height || current["capture_id"].as_str() != Some(capture_id) {
        return Err(ComptrolError {
            code: "stale_geometry".to_owned(),
            message: "The screenshot geometry no longer matches the requested coordinate"
                .to_owned(),
            recovery: Some("Capture a fresh screenshot and retry with its capture_id".to_owned()),
        });
    }
    let targets = discover_cached(endpoint)?;
    let target = crate::bind_browser_target(
        &targets,
        target_id,
        Some(browser_context_id),
        Some(revision),
    )?;
    let target_revision = target.revision.clone();
    cdp_call(
        endpoint,
        target_id,
        Some(browser_context_id),
        target_revision.as_deref(),
        "Input.dispatchMouseEvent",
        json!({"type":"mousePressed","x":x,"y":y,"button":button,"clickCount":1}),
    )?;
    cdp_call(
        endpoint,
        target_id,
        Some(browser_context_id),
        target_revision.as_deref(),
        "Input.dispatchMouseEvent",
        json!({"type":"mouseReleased","x":x,"y":y,"button":button,"clickCount":1}),
    )?;
    Ok(json!({
        "clicked": true,
        "x": x,
        "y": y,
        "button": button,
        "capture_id": capture_id,
        "viewport": current["viewport"],
        "verified": false,
        "verification": "dispatch_only",
        "evidence": "fresh_pixel_capture_geometry"
    }))
}

fn protocol_call(
    web_socket_url: &str,
    method: &str,
    params: Value,
) -> Result<Value, ComptrolError> {
    bridge()
        .command(web_socket_url, method, params)
        .map_err(|error| ComptrolError {
            code: "browser_protocol_error".to_owned(),
            message: error.to_string(),
            recovery: Some("Inspect the browser connection and retry".to_owned()),
        })
}

/// Shared persistent browser bridge accessor. One long-lived multiplexer is
/// created per process; every browser wait and command routes through it so
/// warm paths never open a second socket or poll discovery.
pub fn bridge() -> &'static BlockingBrowserManager {
    static BRIDGE: OnceLock<BlockingBrowserManager> = OnceLock::new();
    BRIDGE.get_or_init(BlockingBrowserManager::new)
}

fn wait_for_event<F>(
    web_socket_url: &str,
    timeout: Duration,
    predicate: F,
) -> Result<Value, ComptrolError>
where
    F: Fn(&Value) -> bool,
{
    let bridge = bridge();
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ComptrolError {
                code: "verification_failed".to_owned(),
                message: "The browser did not emit the expected protocol event in time".to_owned(),
                recovery: Some("Inspect the exact browser target before retrying".to_owned()),
            });
        }
        let value = bridge
            .next_event(
                web_socket_url,
                remaining.as_millis().min(u64::MAX as u128) as u64,
            )
            .map_err(|error| ComptrolError {
                code: if matches!(error, BrowserError::Timeout) {
                    "verification_failed"
                } else {
                    "browser_protocol_error"
                }
                .to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect the browser connection and retry".to_owned()),
            })?;
        if predicate(&value) {
            return Ok(value);
        }
    }
}

fn encode_new_tab_url(url: &str) -> String {
    url.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~:/?&=%".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub fn cdp_call(
    endpoint: &str,
    target_id: &str,
    browser_context_id: Option<&str>,
    revision: Option<&str>,
    method: &str,
    params: Value,
) -> Result<Value, ComptrolError> {
    let targets = discover_cached(endpoint)?;
    if is_companion_bridge(endpoint) {
        let target = crate::bind_browser_target(&targets, target_id, browser_context_id, revision)?;
        let result = bridge_command(
            "cdp_command",
            json!({
                "targetId": target.id,
                "method": method,
                "params": params,
            }),
            Duration::from_secs(15),
        )?;
        if matches!(method, "Page.navigate" | "Page.navigateToHistoryEntry") {
            invalidate_target_cache(endpoint);
        }
        return Ok(result);
    }
    let target = match crate::bind_browser_target(&targets, target_id, browser_context_id, revision)
    {
        Ok(target) => target,
        Err(error) if matches!(error.code.as_str(), "stale_reference" | "target_gone") => {
            invalidate_target_cache(endpoint);
            let refreshed = discover(endpoint)?;
            crate::bind_browser_target(&refreshed, target_id, browser_context_id, revision)?
        }
        Err(error) => return Err(error),
    };
    let browser_web_socket_url = browser_websocket_endpoint(endpoint)?;
    let result = bridge()
        .target_command_legacy_revision(
            &browser_web_socket_url,
            target_id,
            target.revision.as_deref(),
            method,
            params,
        )
        .map_err(|error| ComptrolError {
            code: match error {
                BrowserError::StaleReference(_) => "stale_reference",
                BrowserError::Closed => "browser_disconnected",
                _ => "browser_protocol_error",
            }
            .to_owned(),
            message: error.to_string(),
            recovery: Some("Refresh the live target graph and retry".to_owned()),
        });
    if matches!(method, "Page.navigate" | "Page.navigateToHistoryEntry") {
        invalidate_target_cache(endpoint);
    }
    result
}

pub fn cdp_frame_call(
    endpoint: &str,
    frame_id: &str,
    generation: u64,
    revision: u64,
    method: &str,
    params: Value,
) -> Result<Value, ComptrolError> {
    if is_companion_bridge(endpoint) {
        return Err(ComptrolError {
            code: "route_unavailable".to_owned(),
            message: format!(
                "Frame-scoped {method} requires the direct CDP frame graph; the companion bridge does not expose a stable frame-to-tab binding"
            ),
            recovery: Some("Use a direct CDP endpoint for frame-scoped evaluation".to_owned()),
        });
    }
    let browser_web_socket_url = browser_websocket_endpoint(endpoint)?;
    bridge()
        .frame_command(
            &browser_web_socket_url,
            frame_id,
            generation,
            revision,
            method,
            params,
        )
        .map_err(|error| ComptrolError {
            code: match error {
                BrowserError::StaleReference(_) => "stale_reference",
                BrowserError::Closed => "browser_disconnected",
                _ => "browser_protocol_error",
            }
            .to_owned(),
            message: error.to_string(),
            recovery: Some("Refresh the live frame graph and retry".to_owned()),
        })
}

fn browser_websocket_endpoint(endpoint: &str) -> Result<String, ComptrolError> {
    if is_websocket_endpoint(endpoint) {
        return Ok(endpoint.to_owned());
    }
    let version = get_json(endpoint, "/json/version").map_err(|error| ComptrolError {
        code: "browser_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Inspect the browser debugger endpoint".to_owned()),
    })?;
    version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("ws://"))
        .map(str::to_owned)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser version response did not contain a local websocket".to_owned(),
            recovery: Some("Start Chrome with a supported local debugger endpoint".to_owned()),
        })
}

/// Wait for a requested visible-page text postcondition using a bounded,
/// data-only query. Caller text is JSON-escaped before entering the expression.
pub fn wait_for_text(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    text: &str,
    timeout: Duration,
) -> Result<Value, ComptrolError> {
    if text.trim().is_empty() || text.chars().any(char::is_control) {
        return Err(ComptrolError {
            code: "invalid_input".to_owned(),
            message: "Text postconditions must contain non-control text".to_owned(),
            recovery: None,
        });
    }
    let needle = serde_json::to_string(text).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: None,
    })?;
    let expression = format!(
        "(() => {{ const text = {needle}; const body = document.body?.innerText || ''; const index = body.indexOf(text); return {{ matched: index >= 0, count: index < 0 ? 0 : body.split(text).length - 1 }}; }})()"
    );
    let deadline = Instant::now() + timeout;
    loop {
        let result = cdp_call(
            endpoint,
            target_id,
            Some(browser_context_id),
            None,
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
        )?;
        let value = result
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        if value.get("matched").and_then(Value::as_bool) == Some(true) {
            return Ok(json!({
                "text": text,
                "count": value.get("count").and_then(Value::as_u64).unwrap_or(1),
                "verified": true,
                "wait": "visible_document_text"
            }));
        }
        if Instant::now() >= deadline {
            return Err(ComptrolError {
                code: "verification_failed".to_owned(),
                message: format!("The visible page did not contain the requested text: {text}"),
                recovery: Some(
                    "Inspect the exact page and use a current semantic locator".to_owned(),
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn session_endpoint() -> &'static Mutex<Option<String>> {
    static ENDPOINT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    ENDPOINT.get_or_init(|| Mutex::new(None))
}

/// Return the live browser endpoint selected for this Comptrol process.
/// A permissioned Chrome session is stored here after its native consent and
/// CDP handshake succeed; it is never written into process-wide environment
/// state, where concurrent operations could silently switch profiles.
pub fn active_endpoint() -> Option<String> {
    session_endpoint()
        .lock()
        .ok()
        .and_then(|endpoint| endpoint.clone())
        .or_else(|| std::env::var("COMPTROL_CDP_ENDPOINT").ok())
        .filter(|endpoint| !endpoint.is_empty())
}

pub fn set_active_endpoint(endpoint: String) {
    if let Ok(mut current) = session_endpoint().lock() {
        *current = Some(endpoint);
    }
}

fn is_websocket_endpoint(endpoint: &str) -> bool {
    endpoint.starts_with("ws://") || endpoint.starts_with("wss://")
}

fn parse_cdp_target_infos(value: &Value) -> Result<Vec<BrowserTarget>, ComptrolError> {
    let target_infos = value
        .get("targetInfos")
        .and_then(Value::as_array)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "Chrome returned no targetInfos for Target.getTargets".to_owned(),
            recovery: Some("Inspect the permissioned Chrome WebSocket".to_owned()),
        })?;
    let values = target_infos
        .iter()
        .filter_map(|target| {
            Some(json!({
                "id": target.get("targetId")?.as_str()?,
                "type": target.get("type").and_then(Value::as_str),
                "browserContextId": target.get("browserContextId").and_then(Value::as_str),
                "url": target.get("url").and_then(Value::as_str),
                "title": target.get("title").and_then(Value::as_str),
            }))
        })
        .collect::<Vec<_>>();
    parse_targets(&json!(values)).ok_or_else(|| ComptrolError {
        code: "browser_protocol_invalid".to_owned(),
        message: "Chrome returned an invalid target list".to_owned(),
        recovery: Some("Inspect the permissioned Chrome WebSocket".to_owned()),
    })
}

/// Resolve and click a semantic locator in one bounded browser transaction.
///
/// The locator is intentionally data-only. The page receives no caller-supplied
/// JavaScript, and the element is resolved again inside the action transaction so
/// React-style rerenders cannot leave us holding a stale DOM node.
pub fn semantic_click(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: Option<&str>,
    locator: &Value,
    timeout_ms: u64,
) -> Result<Value, ComptrolError> {
    comptrol_browser::Locator::from_value(locator).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: Some(
            "Use one supported locator identity and refine ambiguous matches".to_owned(),
        ),
    })?;
    let locator_json = serde_json::to_string(locator).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: None,
    })?;
    let timeout_ms = timeout_ms.clamp(100, 30_000);
    let expression = format!(
        r#"(async () => {{
            const locator = {locator_json};
            const deadline = performance.now() + {timeout_ms};
            const normalize = value => String(value || '').replace(/\\s+/g, ' ').trim();
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
            const roleOf = element => element.getAttribute('role') ||
                (element.tagName === 'A' ? 'link' :
                 element.tagName === 'BUTTON' ? 'button' :
                 element.tagName === 'INPUT' ? 'textbox' : '');
            const roots = () => {{
                const pending = [document];
                const seen = [];
                while (pending.length) {{
                    const root = pending.shift();
                    seen.push(root);
                    for (const element of root.querySelectorAll('*')) {{
                        if (element.shadowRoot) pending.push(element.shadowRoot);
                        if (element.tagName === 'IFRAME') {{
                            try {{ if (element.contentDocument) pending.push(element.contentDocument); }} catch (_) {{}}
                        }}
                    }}
                }}
                return seen;
            }};
            const all = selector => roots().flatMap(root => [...root.querySelectorAll(selector)]);
            const candidates = () => {{
                let elements;
                if (locator.selector) {{
                    elements = all(locator.selector);
                }} else if (locator.test_id) {{
                    elements = all('[data-testid], [data-test-id]');
                }} else {{
                    elements = all('button, a, input, select, textarea, [role], [tabindex]');
                }}
                return elements.filter(element => {{
                    if (locator.test_id &&
                        element.getAttribute('data-testid') !== locator.test_id &&
                        element.getAttribute('data-test-id') !== locator.test_id) return false;
                    if (locator.role && roleOf(element) !== locator.role) return false;
                    if (locator.name && nameOf(element) !== normalize(locator.name)) return false;
                    if (locator.text && !normalize(element.innerText || element.textContent).includes(normalize(locator.text))) return false;
                    if (locator.href_contains && !(element.href || '').includes(locator.href_contains)) return false;
                    return true;
                }});
            }};
            const actionable = element => {{
                if (!element.isConnected || element.disabled || element.getAttribute('aria-disabled') === 'true') return false;
                const style = getComputedStyle(element);
                const rect = element.getBoundingClientRect();
                if (style.display === 'none' || style.visibility === 'hidden' || Number(style.opacity) === 0 || !rect.width || !rect.height) return false;
                const x = Math.max(0, Math.min(innerWidth - 1, rect.left + rect.width / 2));
                const y = Math.max(0, Math.min(innerHeight - 1, rect.top + rect.height / 2));
                const hit = document.elementFromPoint(x, y);
                const host = element.getRootNode() && element.getRootNode().host;
                return hit === element || Boolean(hit && element.contains(hit)) ||
                    Boolean(host && (hit === host || host.contains(hit)));
            }};
            const stable = async element => {{
                const first = element.getBoundingClientRect();
                await new Promise(requestAnimationFrame);
                const second = element.getBoundingClientRect();
                return first.left === second.left && first.top === second.top &&
                    first.width === second.width && first.height === second.height;
            }};
            while (performance.now() < deadline) {{
                const matches = candidates();
                if (matches.length === 1) {{
                    const element = matches[0];
                    element.scrollIntoView({{ block: 'center', inline: 'nearest' }});
                    if (actionable(element) && await stable(element) && actionable(element)) {{
                        element.click();
                        return {{
                            clicked: true,
                            matches: 1,
                            role: roleOf(element),
                            name: nameOf(element),
                            actionability: {{
                                attached: true,
                                visible: true,
                                stable: true,
                                enabled: true,
                                receives_events: true,
                                unobscured: true
                            }}
                        }};
                    }}
                }} else if (matches.length > 1) {{
                    return {{ clicked: false, reason: 'ambiguous_locator', matches: matches.length }};
                }}
                await new Promise(resolve => setTimeout(resolve, 25));
            }}
            return {{ clicked: false, reason: 'not_actionable', matches: candidates().length }};
        }})()"#
    );
    let mut last_error = None;
    for attempt in 0..=1 {
        let current_revision = if attempt == 0 { revision } else { None };
        match cdp_call(
            endpoint,
            target_id,
            Some(browser_context_id),
            current_revision,
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true
            }),
        ) {
            Ok(data) => {
                let value = data
                    .get("result")
                    .and_then(|result| result.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if value.get("clicked").and_then(Value::as_bool) == Some(true) {
                    return Ok(json!({
                        "result": data,
                        "semantic_locator": locator,
                        "attempts": attempt + 1,
                        "actionability": value.get("actionability").cloned().unwrap_or(Value::Null),
                        "verified": true
                    }));
                }
                return Err(ComptrolError {
                    code: value
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("verification_failed")
                        .to_owned(),
                    message: format!(
                        "Semantic locator did not resolve to one actionable element: {value}"
                    ),
                    recovery: Some(
                        "Inspect the current accessibility tree and refine the locator".to_owned(),
                    ),
                });
            }
            Err(error) if error.code == "stale_reference" && attempt == 0 => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| ComptrolError {
        code: "stale_reference".to_owned(),
        message: "The browser target changed during semantic click".to_owned(),
        recovery: Some("Inspect browser targets and retry once".to_owned()),
    }))
}

pub fn semantic_fill(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: Option<&str>,
    locator: &Value,
    value: &str,
    timeout_ms: u64,
) -> Result<Value, ComptrolError> {
    comptrol_browser::Locator::from_value(locator).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: Some(
            "Use one supported locator identity and refine ambiguous matches".to_owned(),
        ),
    })?;
    if value.len() > 256 * 1024 {
        return Err(ComptrolError {
            code: "invalid_input".to_owned(),
            message: "Semantic fill value exceeds the 256 KiB bound".to_owned(),
            recovery: None,
        });
    }
    let locator_json = serde_json::to_string(locator).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: None,
    })?;
    let value_json = serde_json::to_string(value).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: None,
    })?;
    let timeout_ms = timeout_ms.clamp(100, 30_000);
    let expression = format!(
        r#"(async () => {{
            const locator = {locator_json};
            const desired = {value_json};
            const deadline = performance.now() + {timeout_ms};
            const normalize = value => String(value || '').replace(/\s+/g, ' ').trim();
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
            const roleOf = element => element.getAttribute('role') ||
                (element.tagName === 'TEXTAREA' ? 'textbox' :
                 element.tagName === 'INPUT' ? 'textbox' :
                 element.tagName === 'SELECT' ? 'combobox' : '');
            const roots = () => {{
                const pending = [document];
                const seen = [];
                while (pending.length) {{
                    const root = pending.shift();
                    seen.push(root);
                    for (const element of root.querySelectorAll('*')) {{
                        if (element.shadowRoot) pending.push(element.shadowRoot);
                        if (element.tagName === 'IFRAME') {{
                            try {{ if (element.contentDocument) pending.push(element.contentDocument); }} catch (_) {{}}
                        }}
                    }}
                }}
                return seen;
            }};
            const all = selector => roots().flatMap(root => [...root.querySelectorAll(selector)]);
            const candidates = () => {{
                let elements;
                if (locator.selector) {{
                    elements = all(locator.selector);
                }} else if (locator.test_id) {{
                    elements = all('[data-testid], [data-test-id]');
                }} else {{
                    elements = all('input, textarea, select, [contenteditable="true"], [role="textbox"], [role="combobox"]');
                }}
                return elements.filter(element => {{
                    if (locator.test_id &&
                        element.getAttribute('data-testid') !== locator.test_id &&
                        element.getAttribute('data-test-id') !== locator.test_id) return false;
                    if (locator.role && roleOf(element) !== locator.role) return false;
                    if (locator.name && nameOf(element) !== normalize(locator.name)) return false;
                    if (locator.text && !normalize(element.innerText || element.textContent).includes(normalize(locator.text))) return false;
                    if (locator.href_contains && !(element.href || '').includes(locator.href_contains)) return false;
                    return true;
                }});
            }};
            const actionable = element => {{
                if (!element.isConnected || element.disabled || element.getAttribute('aria-disabled') === 'true') return false;
                if (element.readOnly) return false;
                const style = getComputedStyle(element);
                const rect = element.getBoundingClientRect();
                return style.display !== 'none' && style.visibility !== 'hidden' &&
                    Number(style.opacity) !== 0 && rect.width > 0 && rect.height > 0;
            }};
            const stable = async element => {{
                const first = element.getBoundingClientRect();
                await new Promise(requestAnimationFrame);
                const second = element.getBoundingClientRect();
                return first.left === second.left && first.top === second.top &&
                    first.width === second.width && first.height === second.height;
            }};
            const setNativeValue = (element, next) => {{
                if (element instanceof HTMLSelectElement) {{
                    element.value = next;
                }} else if ('value' in element) {{
                    const proto = element instanceof HTMLTextAreaElement
                        ? HTMLTextAreaElement.prototype
                        : HTMLInputElement.prototype;
                    const descriptor = Object.getOwnPropertyDescriptor(proto, 'value');
                    if (descriptor && descriptor.set) descriptor.set.call(element, next);
                    else element.value = next;
                }} else if (element.isContentEditable) {{
                    element.textContent = next;
                }} else {{
                    throw new Error('target is not editable');
                }}
                element.dispatchEvent(new InputEvent('input', {{ bubbles: true, composed: true, inputType: 'insertText', data: null }}));
                element.dispatchEvent(new Event('change', {{ bubbles: true, composed: true }}));
            }};
            const readValue = element => element.isContentEditable
                ? (element.textContent || '')
                : String(element.value ?? '');
            while (performance.now() < deadline) {{
                const matches = candidates();
                if (matches.length === 1) {{
                    const element = matches[0];
                    element.scrollIntoView({{ block: 'center', inline: 'nearest' }});
                    if (actionable(element) && await stable(element) && actionable(element)) {{
                        element.focus();
                        setNativeValue(element, desired);
                        await Promise.resolve();
                        await new Promise(requestAnimationFrame);
                        const verified = readValue(element) === desired;
                        return {{
                            filled: verified,
                            matches: 1,
                            role: roleOf(element),
                            name: nameOf(element),
                            value_length: desired.length,
                            verified,
                            actionability: {{ attached: true, visible: true, stable: true, enabled: true }}
                        }};
                    }}
                }} else if (matches.length > 1) {{
                    return {{ filled: false, reason: 'ambiguous_locator', matches: matches.length, verified: false }};
                }}
                await new Promise(resolve => setTimeout(resolve, 25));
            }}
            return {{ filled: false, reason: 'not_actionable', matches: candidates().length, verified: false }};
        }})()"#
    );
    let mut last_error = None;
    for attempt in 0..=1 {
        let current_revision = if attempt == 0 { revision } else { None };
        match cdp_call(
            endpoint,
            target_id,
            Some(browser_context_id),
            current_revision,
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
        ) {
            Ok(data) => {
                let result = data
                    .get("result")
                    .and_then(|result| result.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if result.get("verified").and_then(Value::as_bool) == Some(true) {
                    return Ok(json!({
                        "semantic_locator": locator,
                        "attempts": attempt + 1,
                        "role": result.get("role").cloned().unwrap_or(Value::Null),
                        "name": result.get("name").cloned().unwrap_or(Value::Null),
                        "value_length": result.get("value_length").cloned().unwrap_or(Value::Null),
                        "actionability": result.get("actionability").cloned().unwrap_or(Value::Null),
                        "verified": true
                    }));
                }
                return Err(ComptrolError {
                    code: result
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("verification_failed")
                        .to_owned(),
                    message: format!(
                        "Semantic locator did not resolve to one editable element: {result}"
                    ),
                    recovery: Some(
                        "Inspect compact browser state and refine the locator".to_owned(),
                    ),
                });
            }
            Err(error) if error.code == "stale_reference" && attempt == 0 => {
                last_error = Some(error)
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| ComptrolError {
        code: "stale_reference".to_owned(),
        message: "The browser target changed during semantic fill".to_owned(),
        recovery: Some("Inspect browser targets and retry once".to_owned()),
    }))
}

pub fn compact_snapshot(
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
            const roots = () => {{
                const pending = [document];
                const seen = [];
                while (pending.length) {{
                    const root = pending.shift();
                    seen.push(root);
                    for (const element of root.querySelectorAll('*')) {{
                        if (element.shadowRoot) pending.push(element.shadowRoot);
                        if (element.tagName === 'IFRAME') {{
                            try {{ if (element.contentDocument) pending.push(element.contentDocument); }} catch (_) {{}}
                        }}
                    }}
                }}
                return seen;
            }};
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
            return {{
                title: normalize(document.title),
                element_count: visible.length,
                returned: elements.length,
                truncated: visible.length > elements.length,
                elements
            }};
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

pub fn cdp_upload(
    endpoint: &str,
    target_id: &str,
    browser_context_id: Option<&str>,
    revision: Option<&str>,
    selector: &str,
    path: &Path,
) -> Result<Value, ComptrolError> {
    let file_name_text = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| ComptrolError {
            code: "invalid_input".to_owned(),
            message: "Upload path needs a valid file name".to_owned(),
            recovery: None,
        })?;
    let targets = discover_cached(endpoint)?;
    let target = crate::bind_browser_target(&targets, target_id, browser_context_id, revision)?;
    let target_revision = target.revision.clone();
    let document = cdp_call(
        endpoint,
        target_id,
        browser_context_id,
        target_revision.as_deref(),
        "DOM.getDocument",
        json!({ "depth": -1 }),
    )?;
    let root_id = document
        .get("root")
        .and_then(|root| root.get("nodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not return a document root".to_owned(),
            recovery: Some("Refresh the browser target".to_owned()),
        })?;
    let node = cdp_call(
        endpoint,
        target_id,
        browser_context_id,
        target_revision.as_deref(),
        "DOM.querySelector",
        json!({ "nodeId": root_id, "selector": selector }),
    )?;
    let node_id = node
        .get("nodeId")
        .and_then(Value::as_u64)
        .filter(|id| *id != 0)
        .ok_or_else(|| ComptrolError {
            code: "target_gone".to_owned(),
            message: "The browser upload control was not found".to_owned(),
            recovery: Some("Refresh the page and inspect the exact upload selector".to_owned()),
        })?;
    cdp_call(
        endpoint,
        target_id,
        browser_context_id,
        target_revision.as_deref(),
        "DOM.setFileInputFiles",
        json!({ "nodeId": node_id, "files": [path] }),
    )?;
    let selector = serde_json::to_string(selector).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: None,
    })?;
    let file_name = serde_json::to_string(file_name_text).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: None,
    })?;
    let verification = cdp_call(
        endpoint,
        target_id,
        browser_context_id,
        None,
        "Runtime.evaluate",
        json!({
            "expression": format!("(() => {{ const files = document.querySelector({selector}).files; return files.length === 1 && files[0].name === {file_name}; }})()"),
            "returnByValue": true
        }),
    )?;
    if verification
        .get("result")
        .and_then(|result| result.get("value"))
        != Some(&Value::Bool(true))
    {
        return Err(ComptrolError {
            code: "verification_failed".to_owned(),
            message: "The browser did not confirm the selected file".to_owned(),
            recovery: Some("Inspect the upload control and retry once".to_owned()),
        });
    }
    Ok(json!({
        "path": path,
        "file_name": file_name_text,
        // The DOM readback above proved the selection; transfer and persistence
        // remain unproven and are tracked by the stage map below.
        "verified": true,
        "stage": "selected",
        "stages": {
            "selected": true,
            "transfer_started": false,
            "transfer_completed": false,
            "application_accepted": false,
            "persisted": false
        },
        "verification_level": "surface_state",
        "evidence": "DOM file input readback"
    }))
}

pub fn cdp_download(
    endpoint: &str,
    target_id: &str,
    browser_context_id: Option<&str>,
    revision: Option<&str>,
    selector: &str,
    download_dir: &Path,
    expected_name: &str,
) -> Result<Value, ComptrolError> {
    if expected_name.is_empty()
        || Path::new(expected_name)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(expected_name)
    {
        return Err(ComptrolError {
            code: "invalid_input".to_owned(),
            message: "Download file name must be a nonempty local file name".to_owned(),
            recovery: None,
        });
    }
    fs_create_dir(download_dir)?;
    if is_companion_bridge(endpoint) {
        let targets = discover_cached(endpoint)?;
        let target = crate::bind_browser_target(&targets, target_id, browser_context_id, revision)?;
        let result = bridge_command(
            "download_file",
            json!({
                "targetId": target.id,
                "selector": selector,
                "fileName": expected_name,
            }),
            Duration::from_secs(30),
        )?;
        let source = result
            .get("path")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
            .ok_or_else(|| ComptrolError {
                code: "browser_protocol_invalid".to_owned(),
                message: "The Browser Bridge download result did not include a file path"
                    .to_owned(),
                recovery: Some("Inspect Chrome downloads and retry".to_owned()),
            })?;
        let destination = download_dir.join(expected_name);
        std::fs::copy(&source, &destination).map_err(|error| ComptrolError {
            code: "browser_download_failed".to_owned(),
            message: format!("failed to copy verified browser download into sandbox: {error}"),
            recovery: Some("Verify the browser download completed and retry".to_owned()),
        })?;
        return Ok(json!({
            "guid": result.get("downloadId").cloned().unwrap_or(Value::Null),
            "path": destination,
            "file_name": expected_name,
            "verified": destination.is_file(),
            "source": "companion_extension",
        }));
    }
    let targets = discover_cached(endpoint)?;
    let target = crate::bind_browser_target(&targets, target_id, browser_context_id, revision)?;
    let target_revision = target.revision.clone();
    let browser_web_socket_url = browser_websocket_endpoint(endpoint)?;
    // Real Chrome omits browserContextId for default-context targets and
    // rejects unknown GUIDs, so the canonical "default" label must never be
    // sent as if it were a real context GUID.
    let mut download_behavior = json!({
        "behavior": "allow",
        "downloadPath": download_dir,
        // Chrome only emits Browser.downloadWillBegin/downloadProgress when
        // events are explicitly enabled on this behavior binding.
        "eventsEnabled": true
    });
    if let Some(context_id) = real_context_id(browser_context_id) {
        download_behavior["browserContextId"] = json!(context_id);
    }
    bridge()
        .command(
            &browser_web_socket_url,
            "Browser.setDownloadBehavior",
            download_behavior,
        )
        .map_err(|error| ComptrolError {
            code: "browser_protocol_error".to_owned(),
            message: error.to_string(),
            recovery: Some("Refresh the browser connection and retry".to_owned()),
        })?;
    let selector = serde_json::to_string(selector).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: None,
    })?;
    cdp_call(
        endpoint,
        target_id,
        browser_context_id,
        target_revision.as_deref(),
        "Runtime.evaluate",
        json!({
            "expression": format!("(() => {{ const link = document.querySelector({selector}); if (!link) throw new Error('download target missing'); link.click(); return true; }})()"),
            "returnByValue": true,
            "awaitPromise": true
        }),
    )?;
    let download = wait_for_event(&browser_web_socket_url, Duration::from_secs(10), |event| {
        event.get("method").and_then(Value::as_str) == Some("Browser.downloadWillBegin")
    })?;
    let guid = download
        .get("params")
        .and_then(|params| params.get("guid"))
        .and_then(Value::as_str)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "Browser download event did not contain a GUID".to_owned(),
            recovery: Some("Inspect the browser download protocol events".to_owned()),
        })?
        .to_owned();
    let progress = wait_for_event(&browser_web_socket_url, Duration::from_secs(30), |event| {
        event.get("method").and_then(Value::as_str) == Some("Browser.downloadProgress")
            && event
                .get("params")
                .and_then(|params| params.get("guid"))
                .and_then(Value::as_str)
                == Some(guid.as_str())
            && event
                .get("params")
                .and_then(|params| params.get("state"))
                .and_then(Value::as_str)
                == Some("completed")
    })?;
    let expected_path = download_dir.join(expected_name);
    if !expected_path.is_file() {
        return Err(ComptrolError {
            code: "verification_failed".to_owned(),
            message: "Browser reported download completion but the expected file is missing"
                .to_owned(),
            recovery: Some("Reconcile the completed download GUID before retrying".to_owned()),
        });
    }
    let bytes = std::fs::metadata(&expected_path)
        .map_err(browser_file_error)?
        .len();
    Ok(json!({
        "path": expected_path,
        "file_name": expected_name,
        "bytes": bytes,
        "guid": guid,
        "state": progress["params"]["state"],
        "wait_strategy": "browser_download_events",
        "verified": true
    }))
}

fn fs_create_dir(path: &Path) -> Result<(), ComptrolError> {
    std::fs::create_dir_all(path).map_err(browser_file_error)
}

fn browser_file_error(error: io::Error) -> ComptrolError {
    ComptrolError {
        code: "browser_filesystem_failed".to_owned(),
        message: error.to_string(),
        recovery: Some("Inspect the Comptrol sandbox and retry".to_owned()),
    }
}

/// Chrome omits `browserContextId` for default-context targets, so Comptrol
/// normalizes that absence to the canonical label "default". That label is an
/// identity alias for binding and reporting only. It must never be forwarded
/// to Chrome as if it were a real context GUID, which Chrome rejects.
pub fn real_context_id(browser_context_id: Option<&str>) -> Option<&str> {
    match browser_context_id {
        Some("default") | Some("") => None,
        other => other,
    }
}

fn parse_target(value: &Value) -> Option<BrowserTarget> {
    let url = value.get("url").and_then(Value::as_str).map(str::to_owned);
    Some(BrowserTarget {
        id: value.get("id")?.as_str()?.to_owned(),
        target_type: value.get("type").and_then(Value::as_str).map(str::to_owned),
        browser_context_id: value
            .get("browserContextId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some("default".to_owned())),
        revision: value
            .get("revision")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| url.as_ref().map(|value| format!("url:{value}"))),
        url,
        title: value
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned),
        web_socket_url: value
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn get_json(endpoint: &str, path: &str) -> io::Result<Value> {
    let (status, value) = request_json(endpoint, "GET", path, &[], None)?;
    if status != 200 {
        return Err(io::Error::other(
            "browser endpoint returned a non success status",
        ));
    }
    Ok(value)
}

fn request_json(
    endpoint: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> io::Result<(u16, Value)> {
    let authority = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .and_then(|value| value.split('/').next())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid browser endpoint"))?;
    let (host, port) = authority.rsplit_once(':').ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "browser endpoint needs a port")
    })?;
    let host_name = host.trim_matches(|character| character == '[' || character == ']');
    if !matches!(host_name, "localhost" | "127.0.0.1" | "::1") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "browser endpoint must be loopback",
        ));
    }
    let mut stream = TcpStream::connect((
        host_name,
        port.parse::<u16>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid browser endpoint port")
        })?,
    ))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let body = body.unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nAccept: application/json\r\n"
    )?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    if !body.is_empty() {
        write!(
            stream,
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )?;
    }
    write!(stream, "\r\n{body}")?;
    let response = read_http_response(&mut stream)?;
    let response = String::from_utf8_lossy(&response);
    let (header, body) = response.split_once("\r\n\r\n").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "browser response has no body")
    })?;
    let status = header
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid browser status"))?;
    let body = if header.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        })
    }) {
        decode_chunked(body)?
    } else if let Some(length) = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.eq_ignore_ascii_case("content-length"))
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    }) {
        body[..length.min(body.len())].to_owned()
    } else {
        body.to_owned()
    };
    let value = serde_json::from_str(&body).map_err(|error| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{error} body={body:?}"))
    })?;
    Ok((status, value))
}

fn read_http_response(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(size) => {
                response.extend_from_slice(&chunk[..size]);
                if response.len() > MAX_PROTOCOL_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "browser response exceeds protocol size limit",
                    ));
                }
                let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let header = String::from_utf8_lossy(&response[..header_end]);
                if let Some(length) = header.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    (name.eq_ignore_ascii_case("content-length"))
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                }) {
                    if response.len() >= header_end + 4 + length {
                        break;
                    }
                } else if response.windows(5).any(|window| window == b"0\r\n\r\n") {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(response)
}

fn decode_chunked(mut body: &str) -> io::Result<String> {
    let mut decoded = String::new();
    loop {
        let (size, rest) = body
            .split_once("\r\n")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk header"))?;
        let size = usize::from_str_radix(size.trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size"))?;
        if size == 0 {
            return Ok(decoded);
        }
        if size > MAX_PROTOCOL_BYTES || decoded.len().saturating_add(size) > MAX_PROTOCOL_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunked browser response exceeds protocol size limit",
            ));
        }
        if rest.len() < size + 2 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short chunk"));
        }
        decoded.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_endpoint_is_loopback_only() {
        let error = discover("http://example.com:9222").expect_err("remote endpoint");
        assert_eq!(error.code, "browser_unavailable");
    }

    #[test]
    fn open_tab_rejects_unsafe_urls_before_connecting() {
        let error = open_tab("http://127.0.0.1:9222", "javascript:alert(1)", false, None)
            .expect_err("unsafe URL");
        assert_eq!(error.code, "invalid_input");
    }

    #[test]
    fn target_parser_keeps_exact_identity_fields() {
        let targets = parse_targets(&json!([{
            "id": "tab",
            "type": "page",
            "browserContextId": "context",
            "url": "http://127.0.0.1/",
            "title": "fixture",
            "revision": "rev",
            "webSocketDebuggerUrl": "ws://127.0.0.1/devtools/page/tab"
        }]))
        .expect("target list");
        assert_eq!(targets[0].id, "tab");
        assert_eq!(targets[0].browser_context_id.as_deref(), Some("context"));
        assert_eq!(targets[0].revision.as_deref(), Some("rev"));
    }

    #[test]
    fn permissioned_cdp_target_infos_keep_live_page_identity() {
        let targets = parse_cdp_target_infos(&json!({
            "targetInfos": [{
                "targetId": "classroom-tab",
                "type": "page",
                "browserContextId": "profile-context",
                "url": "https://classroom.google.com/",
                "title": "Google Classroom"
            }]
        }))
        .expect("CDP target list");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "classroom-tab");
        assert_eq!(targets[0].target_type.as_deref(), Some("page"));
        assert_eq!(
            targets[0].browser_context_id.as_deref(),
            Some("profile-context")
        );
        assert_eq!(
            targets[0].url.as_deref(),
            Some("https://classroom.google.com/")
        );
    }

    #[test]
    fn real_context_id_strips_canonical_default_label() {
        // Chrome omits browserContextId for default-context targets; the
        // canonical "default" label must never cross the wire as a GUID.
        assert_eq!(real_context_id(Some("default")), None);
        assert_eq!(real_context_id(Some("")), None);
        assert_eq!(real_context_id(None), None);
        assert_eq!(real_context_id(Some("ABC123")), Some("ABC123"));
    }
}
