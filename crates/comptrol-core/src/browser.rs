use crate::{BrowserTarget, ComptrolError, MAX_PROTOCOL_BYTES, bind_browser_target};
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};
use tungstenite::{Message, connect};

pub fn discover(endpoint: &str) -> Result<Vec<BrowserTarget>, ComptrolError> {
    let value = get_json(endpoint, "/json/list").map_err(|error| ComptrolError {
        code: "browser_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Start a supported browser with remote debugging enabled".to_owned()),
    })?;
    parse_targets(&value).ok_or_else(|| ComptrolError {
        code: "browser_protocol_invalid".to_owned(),
        message: "The browser returned an invalid target list".to_owned(),
        recovery: Some("Inspect the configured DevTools endpoint".to_owned()),
    })
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
    let targets = discover(endpoint)?;
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
    if !background && browser_context_id.is_none() {
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
    let version = get_json(endpoint, "/json/version").map_err(|error| ComptrolError {
        code: "browser_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Start the existing browser with local DevTools enabled".to_owned()),
    })?;
    let web_socket_url = version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not provide a browser websocket".to_owned(),
            recovery: Some("Use a Chrome endpoint that exposes the browser target".to_owned()),
        })?;
    let mut create_params = json!({
        "url": url,
        "background": background,
        "focus": !background,
        "newWindow": false
    });
    if let Some(browser_context_id) = browser_context_id {
        create_params["browserContextId"] = json!(browser_context_id);
    }
    let created = protocol_call(web_socket_url, "Target.createTarget", create_params)?;
    let target_id = created
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not return the background tab identity".to_owned(),
            recovery: Some("Inspect the browser target list".to_owned()),
        })?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(targets) = discover(endpoint)
            && let Some(target) = targets.into_iter().find(|target| target.id == target_id)
        {
            if browser_context_id
                .is_some_and(|expected| target.browser_context_id.as_deref() != Some(expected))
            {
                return Err(ComptrolError {
                    code: "stale_reference".to_owned(),
                    message: "The browser created the tab in a different browser context"
                        .to_owned(),
                    recovery: Some("Inspect browser contexts and open the tab again".to_owned()),
                });
            }
            return Ok(json!({
                "target": target,
                "visibility": "background",
                "profile": "attached_existing_browser",
                "account_state": "same_browser_profile",
                "mouse": "untouched",
                "clipboard": "untouched",
                "verified": true
            }));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(ComptrolError {
        code: "verification_failed".to_owned(),
        message: "The browser did not expose the new background tab".to_owned(),
        recovery: Some("Inspect browser targets before retrying".to_owned()),
    })
}

pub fn close_tab(
    endpoint: &str,
    target_id: &str,
    browser_context_id: &str,
    revision: &str,
) -> Result<Value, ComptrolError> {
    let targets = discover(endpoint)?;
    let target = crate::bind_browser_target(
        &targets,
        target_id,
        Some(browser_context_id),
        Some(revision),
    )?;
    let version = get_json(endpoint, "/json/version").map_err(|error| ComptrolError {
        code: "browser_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Inspect the existing browser websocket endpoint".to_owned()),
    })?;
    let web_socket_url = version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not provide a browser websocket".to_owned(),
            recovery: Some("Use a Chrome endpoint that exposes the browser target".to_owned()),
        })?;
    let value = protocol_call(
        web_socket_url,
        "Target.closeTarget",
        json!({ "targetId": target_id }),
    )?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
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
        std::thread::sleep(Duration::from_millis(25));
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
    let current = cdp_call(
        endpoint,
        target_id,
        Some(browser_context_id),
        Some(revision),
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
        Some(revision),
        "Page.navigateToHistoryEntry",
        json!({ "entryId": entry_id }),
    )?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(observed) = cdp_call(
            endpoint,
            target_id,
            Some(browser_context_id),
            Some(revision),
            "Page.getNavigationHistory",
            json!({}),
        ) && observed.get("currentIndex").and_then(Value::as_i64) == Some(destination_index)
        {
            return Ok(json!({
                "direction": if forward { "forward" } else { "back" },
                "entry": destination,
                "current_index": destination_index,
                "verified": true,
                "mouse": "untouched",
                "clipboard": "untouched"
            }));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(ComptrolError {
        code: "verification_failed".to_owned(),
        message: "The browser did not confirm the requested history step".to_owned(),
        recovery: Some(
            "Inspect the target and current navigation history before retrying".to_owned(),
        ),
    })
}

fn protocol_call(
    web_socket_url: &str,
    method: &str,
    params: Value,
) -> Result<Value, ComptrolError> {
    if !web_socket_url.starts_with("ws://") {
        return Err(ComptrolError {
            code: "browser_transport_unsupported".to_owned(),
            message: "Only local unencrypted DevTools websocket endpoints are enabled".to_owned(),
            recovery: Some("Use a local browser endpoint".to_owned()),
        });
    }
    let (mut socket, _) = connect(web_socket_url).map_err(browser_connect_error)?;
    socket
        .send(Message::Text(
            json!({ "id": 1, "method": method, "params": params })
                .to_string()
                .into(),
        ))
        .map_err(browser_dispatch_error)?;
    loop {
        let message = socket.read().map_err(browser_response_error)?;
        let Message::Text(text) = message else {
            continue;
        };
        if text.len() > MAX_PROTOCOL_BYTES {
            return Err(ComptrolError {
                code: "browser_message_too_large".to_owned(),
                message: format!(
                    "Browser protocol messages are limited to {MAX_PROTOCOL_BYTES} bytes"
                ),
                recovery: Some("Inspect the browser target".to_owned()),
            });
        }
        let value: Value = serde_json::from_str(&text).map_err(|error| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: error.to_string(),
            recovery: Some("Inspect the browser protocol version".to_owned()),
        })?;
        if value.get("id").and_then(Value::as_u64) != Some(1) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(ComptrolError {
                code: "browser_command_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect the browser target and retry once".to_owned()),
            });
        }
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
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
    let targets = discover(endpoint)?;
    let target = crate::bind_browser_target(&targets, target_id, browser_context_id, revision)?;
    let Some(web_socket_url) = target.web_socket_url else {
        return Err(ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The target did not provide a websocket debugger URL".to_owned(),
            recovery: Some("Inspect browser targets again".to_owned()),
        });
    };
    if !web_socket_url.starts_with("ws://") {
        return Err(ComptrolError {
            code: "browser_transport_unsupported".to_owned(),
            message: "Only local unencrypted DevTools websocket endpoints are enabled".to_owned(),
            recovery: Some(
                "Use a local browser endpoint or configure a trusted transport".to_owned(),
            ),
        });
    }
    let (mut socket, _) = connect(web_socket_url).map_err(|error| ComptrolError {
        code: "browser_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Start the browser target and retry".to_owned()),
    })?;
    socket
        .send(Message::Text(
            json!({ "id": 1, "method": method, "params": params })
                .to_string()
                .into(),
        ))
        .map_err(|error| ComptrolError {
            code: "browser_dispatch_failed".to_owned(),
            message: error.to_string(),
            recovery: Some("Reconnect to the exact browser target".to_owned()),
        })?;
    loop {
        let message = socket.read().map_err(|error| ComptrolError {
            code: "browser_response_failed".to_owned(),
            message: error.to_string(),
            recovery: Some("Inspect the browser target before retrying".to_owned()),
        })?;
        let Message::Text(text) = message else {
            continue;
        };
        if text.len() > MAX_PROTOCOL_BYTES {
            return Err(ComptrolError {
                code: "browser_message_too_large".to_owned(),
                message: format!(
                    "Browser protocol messages are limited to {MAX_PROTOCOL_BYTES} bytes"
                ),
                recovery: Some("Inspect the target and retry with a bounded response".to_owned()),
            });
        }
        let value: Value = serde_json::from_str(&text).map_err(|error| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: error.to_string(),
            recovery: Some("Inspect the browser protocol version".to_owned()),
        })?;
        if value.get("id").and_then(Value::as_u64) != Some(1) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(ComptrolError {
                code: "browser_command_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Refresh the target and retry once".to_owned()),
            });
        }
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
    }
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
    let targets = discover(endpoint)?;
    let target = crate::bind_browser_target(&targets, target_id, browser_context_id, revision)?;
    let Some(web_socket_url) = target.web_socket_url else {
        return Err(ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The target did not provide a websocket debugger URL".to_owned(),
            recovery: Some("Inspect browser targets again".to_owned()),
        });
    };
    if !web_socket_url.starts_with("ws://") {
        return Err(ComptrolError {
            code: "browser_transport_unsupported".to_owned(),
            message: "Only local unencrypted DevTools websocket endpoints are enabled".to_owned(),
            recovery: Some(
                "Use a local browser endpoint or configure a trusted transport".to_owned(),
            ),
        });
    }
    let (mut socket, _) = connect(web_socket_url).map_err(|error| ComptrolError {
        code: "browser_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Start the browser target and retry".to_owned()),
    })?;
    let mut command_id = 0_u64;
    let mut call = |method: &str, params: Value| -> Result<Value, ComptrolError> {
        command_id += 1;
        let id = command_id;
        socket
            .send(Message::Text(
                json!({ "id": id, "method": method, "params": params })
                    .to_string()
                    .into(),
            ))
            .map_err(|error| ComptrolError {
                code: "browser_dispatch_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Reconnect to the exact browser target".to_owned()),
            })?;
        loop {
            let message = socket.read().map_err(|error| ComptrolError {
                code: "browser_response_failed".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect the browser target before retrying".to_owned()),
            })?;
            let Message::Text(text) = message else {
                continue;
            };
            if text.len() > MAX_PROTOCOL_BYTES {
                return Err(ComptrolError {
                    code: "browser_message_too_large".to_owned(),
                    message: format!(
                        "Browser protocol messages are limited to {MAX_PROTOCOL_BYTES} bytes"
                    ),
                    recovery: Some(
                        "Inspect the target and retry with a bounded response".to_owned(),
                    ),
                });
            }
            let value: Value = serde_json::from_str(&text).map_err(|error| ComptrolError {
                code: "browser_protocol_invalid".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect the browser protocol version".to_owned()),
            })?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(ComptrolError {
                    code: "browser_command_failed".to_owned(),
                    message: error.to_string(),
                    recovery: Some("Refresh the target and retry once".to_owned()),
                });
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    };
    let document = call("DOM.getDocument", json!({ "depth": -1 }))?;
    let root_id = document
        .get("root")
        .and_then(|root| root.get("nodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The browser did not return a document root".to_owned(),
            recovery: Some("Refresh the browser target".to_owned()),
        })?;
    let node = call(
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
    call(
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
    let verification = call(
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
    Ok(json!({ "path": path, "file_name": file_name_text, "verified": true }))
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
    let targets = discover(endpoint)?;
    let target = crate::bind_browser_target(&targets, target_id, browser_context_id, revision)?;
    let Some(web_socket_url) = target.web_socket_url else {
        return Err(ComptrolError {
            code: "browser_protocol_invalid".to_owned(),
            message: "The target did not provide a websocket debugger URL".to_owned(),
            recovery: Some("Inspect browser targets again".to_owned()),
        });
    };
    if !web_socket_url.starts_with("ws://") {
        return Err(ComptrolError {
            code: "browser_transport_unsupported".to_owned(),
            message: "Only local unencrypted DevTools websocket endpoints are enabled".to_owned(),
            recovery: Some(
                "Use a local browser endpoint or configure a trusted transport".to_owned(),
            ),
        });
    }
    let (mut socket, _) = connect(web_socket_url).map_err(browser_connect_error)?;
    let mut command_id = 0_u64;
    let mut call = |method: &str, params: Value| -> Result<Value, ComptrolError> {
        command_id += 1;
        let id = command_id;
        socket
            .send(Message::Text(
                json!({ "id": id, "method": method, "params": params })
                    .to_string()
                    .into(),
            ))
            .map_err(browser_dispatch_error)?;
        loop {
            let message = socket.read().map_err(browser_response_error)?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).map_err(|error| ComptrolError {
                code: "browser_protocol_invalid".to_owned(),
                message: error.to_string(),
                recovery: Some("Inspect the browser protocol version".to_owned()),
            })?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(ComptrolError {
                    code: "browser_command_failed".to_owned(),
                    message: error.to_string(),
                    recovery: Some("Refresh the target and retry once".to_owned()),
                });
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    };
    call(
        "Page.setDownloadBehavior",
        json!({ "behavior": "allow", "downloadPath": download_dir }),
    )?;
    let selector = serde_json::to_string(selector).map_err(|error| ComptrolError {
        code: "invalid_input".to_owned(),
        message: error.to_string(),
        recovery: None,
    })?;
    call(
        "Runtime.evaluate",
        json!({
            "expression": format!("(() => {{ const link = document.querySelector({selector}); if (!link) throw new Error('download target missing'); link.click(); return true; }})()"),
            "returnByValue": true,
            "awaitPromise": true
        }),
    )?;
    let expected_path = download_dir.join(expected_name);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if expected_path.is_file()
            && !download_dir
                .join(format!("{expected_name}.crdownload"))
                .exists()
        {
            let bytes = std::fs::metadata(&expected_path)
                .map_err(browser_file_error)?
                .len();
            return Ok(
                json!({ "path": expected_path, "file_name": expected_name, "bytes": bytes, "verified": true }),
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(ComptrolError {
        code: "verification_failed".to_owned(),
        message: "The browser did not produce the expected download".to_owned(),
        recovery: Some("Inspect the download target and reconcile before retrying".to_owned()),
    })
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

fn browser_connect_error(error: tungstenite::Error) -> ComptrolError {
    ComptrolError {
        code: "browser_unavailable".to_owned(),
        message: error.to_string(),
        recovery: Some("Start the browser target and retry".to_owned()),
    }
}

fn browser_dispatch_error(error: tungstenite::Error) -> ComptrolError {
    ComptrolError {
        code: "browser_dispatch_failed".to_owned(),
        message: error.to_string(),
        recovery: Some("Reconnect to the exact browser target".to_owned()),
    }
}

fn browser_response_error(error: tungstenite::Error) -> ComptrolError {
    ComptrolError {
        code: "browser_response_failed".to_owned(),
        message: error.to_string(),
        recovery: Some("Inspect the browser target before retrying".to_owned()),
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
}
