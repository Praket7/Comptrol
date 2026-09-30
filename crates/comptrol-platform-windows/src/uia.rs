#![allow(non_upper_case_globals, non_camel_case_types, clippy::collapsible_if)]
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationExpandCollapsePattern,
    IUIAutomationInvokePattern, IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern,
    IUIAutomationValuePattern, ToggleState_Indeterminate, ToggleState_Off, ToggleState_On,
    TreeScope_Children, TreeScope_Descendants, UIA_AutomationIdPropertyId, UIA_ButtonControlTypeId,
    UIA_CONTROLTYPE_ID, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
    UIA_ControlTypePropertyId, UIA_EditControlTypeId, UIA_ExpandCollapsePatternId,
    UIA_HyperlinkControlTypeId, UIA_InvokePatternId, UIA_ListItemControlTypeId, UIA_NamePropertyId,
    UIA_ScrollItemPatternId, UIA_SelectionItemPatternId, UIA_TextControlTypeId, UIA_TextPatternId,
    UIA_TogglePatternId, UIA_ValuePatternId, UIA_WindowControlTypeId,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, IsChild,
    IsWindowVisible, SetCursorPos, WindowFromPoint,
};

#[allow(non_upper_case_globals)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Inspect,
    Press,
    SetValue,
}

#[derive(Clone, Debug)]
pub struct Request<'a> {
    pub process_id: u32,
    pub window_handle: Option<u64>,
    pub name: Option<&'a str>,
    pub automation_id: Option<&'a str>,
    pub role: Option<&'a str>,
    pub action: Action,
    pub value: Option<&'a str>,
    pub expected_attribute: Option<&'a str>,
    pub expected_value: Option<&'a str>,
    /// Select an ordinal among identical controls only when the observed
    /// candidate count is supplied and unchanged.
    pub match_index: Option<usize>,
    pub expected_match_count: Option<usize>,
    pub max_nodes: usize,
    /// Permit a real foreground click only after UIA exposes no semantic action pattern.
    pub allow_physical_click: bool,
}

const DEFAULT_UIA_WORKER_DEADLINE: Duration = Duration::from_secs(8);

/// D2 foreground input lease: a process-wide mutex that orders every UIA
/// action (semantic or physical). Semantic pattern actions do not need focus,
/// but they do mutate the same UI the user is interacting with, so serializing
/// them prevents interleaved mutations under concurrent agents. A poisoned
/// lock is adopted rather than wedging automation forever.
fn input_lease() -> std::sync::MutexGuard<'static, ()> {
    static INPUT_LEASE: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    INPUT_LEASE
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// Subprocess worker API is used by the Comptrol server's hidden worker command.

struct UiaProcess {
    child: Child,
    stdin: ChildStdin,
    responses: mpsc::Receiver<String>,
}

impl UiaProcess {
    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static WORKER: OnceLock<Mutex<Option<UiaProcess>>> = OnceLock::new();

fn worker_slot() -> &'static Mutex<Option<UiaProcess>> {
    WORKER.get_or_init(|| Mutex::new(None))
}

fn start_worker() -> Result<UiaProcess, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("UIA worker executable path unavailable: {error}"))?;
    let mut child = Command::new(executable)
        .arg("__windows-uia-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("UIA worker start failed: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "UIA worker stdin unavailable".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "UIA worker stdout unavailable".to_owned())?;
    let (sender, responses) = mpsc::channel();
    std::thread::Builder::new()
        .name("comptrol-windows-uia-supervisor".to_owned())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .map_err(|error| format!("UIA worker response reader start failed: {error}"))?;
    Ok(UiaProcess {
        child,
        stdin,
        responses,
    })
}

fn worker_deadline() -> Duration {
    std::env::var("COMPTROL_UIA_WORKER_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| Duration::from_millis(value.clamp(100, 30_000)))
        .unwrap_or(DEFAULT_UIA_WORKER_DEADLINE)
}

pub fn execute(request: Request<'_>) -> Result<Value, String> {
    // D2 foreground input lease: physical input (SendInput click/keys) and
    // semantic pattern actions are serialized process-wide so two agents (or
    // an agent plus the user's own automation) cannot interleave hardware-level
    // input into one window. The lease is held for the duration of one UIA
    // request, including its worker round trip.
    let _input_lease = input_lease();
    let payload = request_payload(&request);
    let encoded = serde_json::to_vec(&payload)
        .map_err(|error| format!("UIA worker request encoding failed: {error}"))?;
    let deadline = worker_deadline();
    let mut slot = worker_slot()
        .lock()
        .map_err(|_| "UIA worker supervisor lock poisoned".to_owned())?;
    if slot
        .as_mut()
        .is_some_and(|worker| worker.child.try_wait().ok().flatten().is_some())
    {
        if let Some(worker) = slot.take() {
            worker.stop();
        }
    }
    if slot.is_none() {
        *slot = Some(start_worker()?);
    }
    let worker = slot.as_mut().expect("worker was initialized");
    if worker.stdin.write_all(&encoded).is_err()
        || worker.stdin.write_all(b"\n").is_err()
        || worker.stdin.flush().is_err()
    {
        if let Some(worker) = slot.take() {
            worker.stop();
        }
        return Err("UIA worker pipe failed; no result was received".to_owned());
    }
    match worker.responses.recv_timeout(deadline) {
        Ok(line) => {
            let response: Value = serde_json::from_str(&line)
                .map_err(|error| format!("UIA worker returned invalid response: {error}"))?;
            if response["ok"] == true {
                Ok(response["result"].clone())
            } else {
                Err(response["error"]
                    .as_str()
                    .unwrap_or("UIA worker execution failed")
                    .to_owned())
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if let Some(worker) = slot.take() {
                worker.stop();
            }
            Err(format!(
                "uia_worker_timeout: provider exceeded {} ms; delivery is unknown and the failed worker was terminated",
                deadline.as_millis()
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            if let Some(worker) = slot.take() {
                worker.stop();
            }
            Err("uia_worker_stopped: provider worker exited before responding".to_owned())
        }
    }
}

fn request_payload(request: &Request<'_>) -> Value {
    json!({
        "process_id": request.process_id,
        "window_handle": request.window_handle,
        "name": request.name,
        "automation_id": request.automation_id,
        "role": request.role,
        "action": match request.action { Action::Inspect => "inspect", Action::Press => "press", Action::SetValue => "set_value" },
        "value": request.value,
        "expected_attribute": request.expected_attribute,
        "expected_value": request.expected_value,
        "match_index": request.match_index,
        "expected_match_count": request.expected_match_count,
        "max_nodes": request.max_nodes,
        "allow_physical_click": request.allow_physical_click
    })
}

#[derive(serde::Deserialize)]
struct WorkerRequest {
    process_id: u32,
    window_handle: Option<u64>,
    name: Option<String>,
    automation_id: Option<String>,
    role: Option<String>,
    action: String,
    value: Option<String>,
    expected_attribute: Option<String>,
    expected_value: Option<String>,
    match_index: Option<usize>,
    expected_match_count: Option<usize>,
    max_nodes: usize,
    allow_physical_click: bool,
}

pub fn run_worker_stdio() -> i32 {
    let _dpi_awareness = ThreadDpiAwareness::per_monitor_v2();
    let _com = match ComGuard::initialize() {
        Ok(com) => com,
        Err(_) => return 1,
    };
    let automation = match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) } {
        Ok(automation) => automation,
        Err(_) => return 1,
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let response = match line {
            Ok(line) => run_worker_line(&line, Some(&automation)),
            Err(error) => json!({"ok": false, "error": format!("worker input failed: {error}")}),
        };
        if serde_json::to_writer(&mut stdout, &response).is_err()
            || stdout.write_all(b"\n").is_err()
            || stdout.flush().is_err()
        {
            return 1;
        }
    }
    0
}

fn run_worker_line(line: &str, automation: Option<&IUIAutomation>) -> Value {
    let parsed = serde_json::from_str::<WorkerRequest>(line);
    let result = parsed.and_then(|request| {
        let action = match request.action.as_str() {
            "inspect" => Action::Inspect,
            "press" => Action::Press,
            "set_value" => Action::SetValue,
            _ => {
                return Err(serde_json::Error::io(std::io::Error::other(
                    "unsupported UIA action",
                )));
            }
        };
        let automation = automation.ok_or_else(|| {
            serde_json::Error::io(std::io::Error::other(
                "UI Automation worker is not initialized",
            ))
        })?;
        execute_once(
            automation,
            Request {
                process_id: request.process_id,
                window_handle: request.window_handle,
                name: request.name.as_deref(),
                automation_id: request.automation_id.as_deref(),
                role: request.role.as_deref(),
                action,
                value: request.value.as_deref(),
                expected_attribute: request.expected_attribute.as_deref(),
                expected_value: request.expected_value.as_deref(),
                match_index: request.match_index,
                expected_match_count: request.expected_match_count,
                max_nodes: request.max_nodes,
                allow_physical_click: request.allow_physical_click,
            },
        )
        .map_err(|error| serde_json::Error::io(std::io::Error::other(error)))
    });
    match result {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    }
}

struct ThreadDpiAwareness(Option<DPI_AWARENESS_CONTEXT>);

impl ThreadDpiAwareness {
    fn per_monitor_v2() -> Self {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self((!previous.0.is_null()).then_some(previous))
    }
}

impl Drop for ThreadDpiAwareness {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            let _ = unsafe { SetThreadDpiAwarenessContext(previous) };
        }
    }
}

/// Deliver a closed set of keyboard actions only to the exact foreground
/// window. This supports controls (such as Calculator and browser scroll
/// containers) that expose no usable UIA descendants while preventing
/// background or cross-window keystrokes.
pub fn send_key_sequence(
    process_id: u32,
    window_handle: u64,
    keys: &[String],
) -> Result<Value, String> {
    // Physical keystrokes take the same process-wide input lease.
    let _input_lease = input_lease();
    if window_handle == 0 || keys.is_empty() || keys.len() > 64 {
        return Err("key_sequence requires an exact window_handle and 1 to 64 keys".to_owned());
    }
    let hwnd = HWND(window_handle as *mut _);
    if unsafe { GetForegroundWindow() } != hwnd {
        return Err("key_sequence target window is not foreground".to_owned());
    }
    let mut actual_process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut actual_process_id)) };
    if actual_process_id != process_id {
        return Err("key_sequence window process does not match process_id".to_owned());
    }
    let mut inputs = Vec::new();
    for key in keys {
        let events = key_events(key)?;
        for (vk, key_up) in events {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: 0,
                        dwFlags: if key_up {
                            KEYEVENTF_KEYUP
                        } else {
                            Default::default()
                        },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
    }
    if unsafe { GetForegroundWindow() } != hwnd || !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Err("key_sequence target surface changed before input dispatch".to_owned());
    }
    let mut final_process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut final_process_id)) };
    if final_process_id != process_id {
        return Err("key_sequence target process changed before input dispatch".to_owned());
    }
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(format!(
            "Windows accepted {sent} of {} keyboard events",
            inputs.len()
        ));
    }
    Ok(json!({
        "verified": false,
        "route": "windows_uia_key_sequence",
        "process_id": process_id,
        "window_handle": window_handle,
        "keys": keys,
        "delivered_events": sent,
        "mouse": "untouched",
        "clipboard": "untouched",
    }))
}

fn key_events(key: &str) -> Result<Vec<(u16, bool)>, String> {
    let normalized = key.to_ascii_lowercase();
    let chord = match normalized.as_str() {
        "ctrl+l" => Some(0x4c),
        "ctrl+f" => Some(0x46),
        _ => None,
    };
    if let Some(key_code) = chord {
        return Ok(vec![
            (0x11, false),
            (key_code, false),
            (key_code, true),
            (0x11, true),
        ]);
    }

    let virtual_key = normalized
        .strip_prefix("digit")
        .and_then(|digit| digit.parse::<u16>().ok())
        .filter(|digit| *digit <= 9)
        .map(|digit| 0x30 + digit)
        .or(match normalized.as_str() {
            "multiply" => Some(0x6a),
            "equals" | "enter" => Some(0x0d),
            "end" => Some(0x23),
            "home" => Some(0x24),
            "pagedown" => Some(0x22),
            "pageup" => Some(0x21),
            "arrowdown" => Some(0x28),
            "arrowup" => Some(0x26),
            "arrowleft" => Some(0x25),
            "arrowright" => Some(0x27),
            "tab" => Some(0x09),
            "escape" => Some(0x1b),
            "backspace" => Some(0x08),
            "space" => Some(0x20),
            _ => None,
        });
    let Some(virtual_key) = virtual_key else {
        return Err(format!("unsupported key token: {key}"));
    };
    Ok(vec![(virtual_key, false), (virtual_key, true)])
}

#[cfg(test)]
mod key_tests {
    use super::key_events;

    #[test]
    fn enter_is_case_insensitive() {
        assert_eq!(
            key_events("ENTER").unwrap(),
            vec![(0x0d, false), (0x0d, true)]
        );
        assert_eq!(
            key_events("Enter").unwrap(),
            vec![(0x0d, false), (0x0d, true)]
        );
    }

    #[test]
    fn shortcuts_and_digits_keep_their_expected_events() {
        assert_eq!(
            key_events("ctrl+l").unwrap(),
            vec![(0x11, false), (0x4c, false), (0x4c, true), (0x11, true)]
        );
        assert_eq!(
            key_events("Digit9").unwrap(),
            vec![(0x39, false), (0x39, true)]
        );
    }

    #[test]
    fn unsupported_keys_remain_rejected() {
        assert_eq!(
            key_events("Alt+F4"),
            Err("unsupported key token: Alt+F4".to_owned())
        );
    }
}

fn execute_once(automation: &IUIAutomation, request: Request<'_>) -> Result<Value, String> {
    let started = Instant::now();
    let has_selector =
        request.name.is_some() || request.automation_id.is_some() || request.role.is_some();
    let condition = if request.action == Action::Inspect && !has_selector {
        unsafe { automation.CreateTrueCondition() }
            .map_err(|error| format!("UI Automation condition unavailable: {error}"))?
    } else {
        target_condition(automation, &request)?
    };
    let cache_request = if request.action == Action::Inspect {
        let cache = unsafe { automation.CreateCacheRequest() }
            .map_err(|error| format!("UIA cache request unavailable: {error}"))?;
        for property in [
            UIA_NamePropertyId,
            UIA_AutomationIdPropertyId,
            UIA_ControlTypePropertyId,
            windows::Win32::UI::Accessibility::UIA_IsEnabledPropertyId,
            windows::Win32::UI::Accessibility::UIA_IsOffscreenPropertyId,
            windows::Win32::UI::Accessibility::UIA_BoundingRectanglePropertyId,
        ] {
            unsafe { cache.AddProperty(property) }
                .map_err(|error| format!("UIA property cache unavailable: {error}"))?;
        }
        Some(cache)
    } else {
        None
    };
    let windows = target_windows(automation, &request)?;
    let mut matches = Vec::new();
    let mut inspected = Vec::new();
    let mut inspected_roots = Vec::new();
    let mut seen_elements = Vec::<IUIAutomationElement>::new();
    let mut matched_count = 0usize;
    let mut actionable_count = 0usize;
    let mut rejected = std::collections::BTreeMap::<String, usize>::new();
    let mut bounded_nodes = 0;
    for window in windows {
        let provider_process_id = unsafe { window.CurrentProcessId() }
            .map_err(|error| format!("desktop window process identity failed: {error}"))?;
        let native_window_handle = unsafe { window.CurrentNativeWindowHandle() }
            .map_err(|error| format!("window handle read failed: {error}"))?
            .0 as u64;
        let root_window_handle = effective_window_handle(
            native_window_handle,
            request.window_handle.unwrap_or_default(),
        );
        inspected_roots.push(json!({
            "provider_process_id":provider_process_id,
            "provider_native_window_handle":native_window_handle,
            "effective_window_handle":root_window_handle,
            "requested_window_handle":request.window_handle
        }));
        if !is_requested_window(&request, provider_process_id, root_window_handle) {
            continue;
        }
        // Multiple provider roots may belong to one HWND; element identity
        // deduplication below handles actual duplicate runtime elements.
        let candidates = if let Some(cache) = cache_request.as_ref() {
            unsafe { window.FindAllBuildCache(TreeScope_Descendants, &condition, cache) }
        } else {
            unsafe { window.FindAll(TreeScope_Descendants, &condition) }
        }
        .map_err(|error| format!("UI Automation tree query failed: {error}"))?;
        let count = unsafe { candidates.Length() }
            .map_err(|error| format!("UI Automation result count failed: {error}"))?;
        let mut candidate_elements = (0..count.min(2048))
            .filter_map(|index| unsafe { candidates.GetElement(index).ok() })
            .collect::<Vec<_>>();
        let mut used_raw_fallback = false;
        if candidate_elements.is_empty() {
            candidate_elements =
                raw_view_descendants(automation, &window, cache_request.as_ref(), 2048)?;
            used_raw_fallback = !candidate_elements.is_empty();
        }
        bounded_nodes += candidate_elements.len();
        for element in candidate_elements.into_iter().take(2048) {
            let element = if let Some(cache) = cache_request.as_ref() {
                unsafe { element.BuildUpdatedCache(cache) }.unwrap_or(element)
            } else {
                element
            };
            if used_raw_fallback && !matches_element(&element, &request, root_window_handle)? {
                continue;
            }
            if seen_elements.iter().any(|seen| unsafe {
                automation
                    .CompareElements(seen, &element)
                    .map(|same| same.as_bool())
                    .unwrap_or(false)
            }) {
                continue;
            }
            seen_elements.push(element.clone());
            if request.action == Action::Inspect {
                let name = unsafe { element.CachedName() }
                    .map_err(|error| format!("UI Automation name read failed: {error}"))?;
                let automation_id = unsafe { element.CachedAutomationId() }
                    .map_err(|error| format!("UI Automation id read failed: {error}"))?;
                let control_type = unsafe { element.CachedControlType() }
                    .map_err(|error| format!("UI Automation control type failed: {error}"))?;
                let enabled = unsafe { element.CachedIsEnabled() }
                    .map_err(|error| format!("UI Automation enabled state failed: {error}"))?;
                let offscreen = unsafe { element.CachedIsOffscreen() }
                    .map_err(|error| format!("UI Automation visibility read failed: {error}"))?;
                let bounds = unsafe { element.CachedBoundingRectangle() }
                    .map_err(|error| format!("UI Automation bounds read failed: {error}"))?;
                let semantic_patterns = available_patterns(&element);
                let actionable =
                    enabled.as_bool() && has_actionable_semantic_pattern(&semantic_patterns);
                if actionable {
                    actionable_count += 1;
                } else {
                    let reason = if enabled.as_bool() {
                        "no_supported_semantic_pattern"
                    } else {
                        "disabled"
                    };
                    *rejected.entry(reason.to_owned()).or_default() += 1;
                }
                inspected.push(json!({
                    "name": name.to_string(),
                    "automation_id": automation_id.to_string(),
                    "role": control_type_name(control_type),
                    "enabled": enabled.as_bool(),
                    "offscreen": offscreen.as_bool(),
                    "bounds": [bounds.left, bounds.top, bounds.right, bounds.bottom],
                    "window_handle": root_window_handle,
                    "supported_patterns": semantic_patterns,
                    "actionable": actionable,
                    "actionability_reason": if actionable { "semantic_pattern" } else if !enabled.as_bool() { "disabled" } else { "physical_fallback_requires_foreground_permission" },
                    "is_keyboard_focusable": unsafe { element.CurrentIsKeyboardFocusable() }.map(|value| value.as_bool()).unwrap_or(false),
                }));
                matched_count += 1;
                if inspected.len() >= request.max_nodes {
                    break;
                }
                continue;
            }
            if !matches_element(&element, &request, root_window_handle)? {
                continue;
            }
            matched_count += 1;
            if request.action == Action::Press && has_semantic_press_pattern(&element, &request) {
                // Semantic UIA operations do not need on-screen geometry.
                // Restrict geometry checks to the pointer fallback below.
                matches.push((element, root_window_handle));
                actionable_count += 1;
                if matches.len() > 1 && request.match_index.is_none() {
                    return Err("target_ambiguous".to_owned());
                }
                continue;
            }
            if request.action == Action::Press {
                if !request.allow_physical_click {
                    *rejected
                        .entry(
                            "semantic_patterns_unavailable_and_foreground_click_not_allowed"
                                .to_owned(),
                        )
                        .or_default() += 1;
                    continue;
                }
                let bounds = unsafe { element.CurrentBoundingRectangle() }
                    .map_err(|error| format!("UI Automation bounds read failed: {error}"))?;
                if bounds.left >= bounds.right
                    || bounds.top >= bounds.bottom
                    || unsafe { element.CurrentIsOffscreen() }
                        .map_err(|error| format!("UI Automation visibility read failed: {error}"))?
                        .as_bool()
                {
                    *rejected
                        .entry("offscreen_or_empty_bounds".to_owned())
                        .or_default() += 1;
                    continue;
                }
                let mut point = POINT::default();
                let clickable = unsafe { element.GetClickablePoint(&mut point) }
                    .map_err(|error| format!("UIA clickable point query failed: {error}"))?;
                if !clickable.as_bool() {
                    *rejected.entry("no_clickable_point".to_owned()).or_default() += 1;
                    continue;
                }
            }
            matches.push((element, root_window_handle));
            actionable_count += 1;
            if matches.len() > 1 && request.match_index.is_none() {
                return Err("target_ambiguous".to_owned());
            }
        }
        if request.action == Action::Inspect && inspected.len() >= request.max_nodes {
            break;
        }
    }
    if request.action == Action::Inspect {
        let observation_complete = !inspected.is_empty();
        return Ok(json!({
            "verified": observation_complete,
            "observation_complete": observation_complete,
            "observation_status": if observation_complete { "controls_found" } else { "no_controls_found" },
            "route": "windows_uia_inspect",
            "process_id": request.process_id,
            "controls": inspected,
            "window_roots": inspected_roots,
            "control_count": inspected.len(),
            "matched_count": matched_count,
            "actionable_count": actionable_count,
            "rejected_counts": rejected,
            "candidate_count": bounded_nodes,
            "bounded_nodes": bounded_nodes,
            "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
            "mouse": "untouched",
            "clipboard": "untouched",
        }));
    }
    if let Some(index) = request.match_index {
        checked_match_index(matches.len(), index, request.expected_match_count)
            .map_err(str::to_owned)?;
        let element = matches.swap_remove(index);
        matches.clear();
        matches.push(element);
    } else if request.expected_match_count.is_some() {
        return Err("expected_match_count requires match_index".to_owned());
    }
    let Some((element, root_window_handle)) = matches.pop() else {
        if matched_count > 0 {
            return Err(format!(
                "target_not_actionable: matched_count={matched_count}; actionable_count={actionable_count}; rejected_counts={}",
                serde_json::to_string(&rejected).unwrap_or_default()
            ));
        }
        return Err("target_missing".to_owned());
    };
    let enabled = unsafe { element.CurrentIsEnabled() }
        .map_err(|error| format!("UI Automation enabled state failed: {error}"))?;
    if !enabled.as_bool() {
        return Err("target_disabled".to_owned());
    }
    let element_window_handle = unsafe { element.CurrentNativeWindowHandle() }
        .map_err(|error| format!("window handle read failed: {error}"))?
        .0 as u64;
    let root_window_handle = if root_window_handle == 0 {
        request.window_handle.unwrap_or(0)
    } else {
        root_window_handle
    };
    let window_handle = effective_window_handle(element_window_handle, root_window_handle);
    let mouse_used = match request.action {
        Action::Inspect => unreachable!("inspect returned before action dispatch"),
        Action::Press => {
            if request
                .role
                .is_some_and(|role| role.eq_ignore_ascii_case("checkbox"))
            {
                if let Ok(pattern) = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                } {
                    unsafe { pattern.Toggle() }
                        .map_err(|error| format!("Toggle failed: {error}"))?;
                    false
                } else {
                    click_if_permitted(&element, window_handle, &request)?
                }
            } else if request
                .role
                .is_some_and(|role| role.eq_ignore_ascii_case("combobox"))
            {
                if let Ok(pattern) = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )
                } {
                    unsafe { pattern.Expand() }
                        .map_err(|error| format!("Expand failed: {error}"))?;
                    false
                } else {
                    if let Ok(pattern) = unsafe {
                        element
                            .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                    } {
                        unsafe { pattern.Invoke() }
                            .map_err(|error| format!("Invoke failed: {error}"))?;
                        false
                    } else {
                        click_if_permitted(&element, window_handle, &request)?
                    }
                }
            } else if request
                .role
                .is_some_and(|role| role.eq_ignore_ascii_case("listitem"))
            {
                if let Ok(pattern) = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                } {
                    unsafe { pattern.Invoke() }
                        .map_err(|error| format!("Invoke failed: {error}"))?;
                    false
                } else if let Ok(pattern) = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                } {
                    unsafe { pattern.Select() }
                        .map_err(|error| format!("Selection failed: {error}"))?;
                    false
                } else {
                    click_if_permitted(&element, window_handle, &request)?
                }
            } else {
                // Many native controls expose selection rather than Invoke
                // (Chrome's tabs are a common example). Prefer Invoke for
                // buttons and menus, then fall back to the semantic
                // SelectionItem pattern instead of refusing the action.
                if let Ok(pattern) = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                } {
                    unsafe { pattern.Invoke() }
                        .map_err(|error| format!("Invoke failed: {error}"))?;
                    false
                } else if let Ok(pattern) = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                } {
                    unsafe { pattern.Select() }
                        .map_err(|error| format!("Selection failed: {error}"))?;
                    false
                } else {
                    click_if_permitted(&element, window_handle, &request)?
                }
            }
        }
        Action::SetValue => {
            let value = request.value.ok_or("value_required")?;
            let pattern: IUIAutomationValuePattern =
                unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId) }
                    .map_err(|error| format!("Value pattern unavailable: {error}"))?;
            let value = windows::core::BSTR::from(value);
            unsafe { pattern.SetValue(&value) }
                .map_err(|error| format!("SetValue failed: {error}"))?;
            false
        }
    };
    let verified = verify_after_action(automation, &element, &request)?;
    Ok(json!({
        "verified": verified,
        "route": if mouse_used { "windows_uia_foreground_click" } else { "windows_uia_direct" },
        "process_id": request.process_id,
        "candidate_count": 1,
        "bounded_nodes": bounded_nodes,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "mouse": if mouse_used { "used" } else { "untouched" },
        "clipboard": "untouched",
        "window_handle": window_handle,
    }))
}

/// Some XAML/WinUI surfaces expose their controls only through the raw UIA
/// tree. Try it once when the ordinary control-view query returns no matches.
fn raw_view_descendants(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
    cache_request: Option<&windows::Win32::UI::Accessibility::IUIAutomationCacheRequest>,
    limit: usize,
) -> Result<Vec<IUIAutomationElement>, String> {
    let walker = unsafe { automation.RawViewWalker() }
        .map_err(|error| format!("UIA raw view walker unavailable: {error}"))?;
    let first = if let Some(cache) = cache_request {
        unsafe { walker.GetFirstChildElementBuildCache(root, cache) }
    } else {
        unsafe { walker.GetFirstChildElement(root) }
    };
    let Ok(first) = first else {
        return Ok(Vec::new());
    };
    let mut stack = vec![first];
    let mut descendants = Vec::new();
    while let Some(element) = stack.pop() {
        if descendants.len() >= limit {
            break;
        }
        let sibling = if let Some(cache) = cache_request {
            unsafe { walker.GetNextSiblingElementBuildCache(&element, cache) }
        } else {
            unsafe { walker.GetNextSiblingElement(&element) }
        };
        if let Ok(sibling) = sibling {
            stack.push(sibling);
        }
        let child = if let Some(cache) = cache_request {
            unsafe { walker.GetFirstChildElementBuildCache(&element, cache) }
        } else {
            unsafe { walker.GetFirstChildElement(&element) }
        };
        if let Ok(child) = child {
            stack.push(child);
        }
        descendants.push(element);
    }
    Ok(descendants)
}

fn checked_match_index(
    actual_count: usize,
    index: usize,
    expected_count: Option<usize>,
) -> Result<usize, &'static str> {
    let Some(expected_count) = expected_count else {
        return Err("indexed UIA targeting needs expected_match_count");
    };
    if actual_count != expected_count || index >= actual_count {
        return Err("target_set_changed");
    }
    Ok(index)
}

fn effective_window_handle(element_handle: u64, root_handle: u64) -> u64 {
    if element_handle == 0 {
        root_handle
    } else {
        element_handle
    }
}

fn click_if_permitted(
    element: &IUIAutomationElement,
    window_handle: u64,
    request: &Request<'_>,
) -> Result<bool, String> {
    if !request.allow_physical_click {
        return Err("UIA action patterns unavailable; a foreground click needs foreground_allowed or foreground_required".to_owned());
    }
    if unsafe { element.CurrentIsOffscreen() }
        .map_err(|error| format!("UI Automation visibility read failed: {error}"))?
        .as_bool()
    {
        return Err("a physical click cannot target an offscreen element".to_owned());
    }
    let window_handle = request.window_handle.unwrap_or(window_handle);
    if window_handle == 0 {
        return Err("a foreground click requires an exact window_handle".to_owned());
    }
    let target_hwnd = HWND(window_handle as *mut _);
    if unsafe { GetForegroundWindow() } != target_hwnd {
        return Err(
            "UIA action patterns unavailable; target window is not the foreground window"
                .to_owned(),
        );
    }
    let mut point = POINT::default();
    let clickable = unsafe { element.GetClickablePoint(&mut point) }
        .map_err(|error| format!("UIA clickable point unavailable: {error}"))?;
    if !clickable.as_bool() {
        return Err("UIA target has no visible clickable point".to_owned());
    }
    let hit = unsafe { WindowFromPoint(point) };
    let hit_root = if hit.0.is_null() {
        HWND(std::ptr::null_mut())
    } else {
        unsafe { GetAncestor(hit, GA_ROOT) }
    };
    if hit != target_hwnd
        && !unsafe { IsChild(target_hwnd, hit) }.as_bool()
        && hit_root != target_hwnd
    {
        return Err("clickable point resolves outside the exact target window".to_owned());
    }
    let mut previous = POINT::default();
    let restore_cursor = unsafe { GetCursorPos(&mut previous) }.is_ok();
    unsafe { SetCursorPos(point.x, point.y) }
        .map_err(|error| format!("Could not move pointer to the UIA target: {error}"))?;
    if unsafe { GetForegroundWindow() } != target_hwnd {
        if restore_cursor {
            let _ = unsafe { SetCursorPos(previous.x, previous.y) };
        }
        return Err("foreground window changed before click dispatch".to_owned());
    }
    let mut actual_process_id = 0u32;
    unsafe { GetWindowThreadProcessId(target_hwnd, Some(&mut actual_process_id)) };
    if actual_process_id == 0
        || (actual_process_id != request.process_id && request.window_handle != Some(window_handle))
    {
        if restore_cursor {
            let _ = unsafe { SetCursorPos(previous.x, previous.y) };
        }
        return Err("target window identity changed before click dispatch".to_owned());
    }
    let input = |flags| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe {
        SendInput(
            &[input(MOUSEEVENTF_LEFTDOWN), input(MOUSEEVENTF_LEFTUP)],
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if restore_cursor {
        let _ = unsafe { SetCursorPos(previous.x, previous.y) };
    }
    if sent != 2 {
        return Err(format!("Windows accepted {sent} of 2 click events"));
    }
    Ok(true)
}

/// Ask UIA to filter the tree in-process instead of reading every descendant
/// property across the COM boundary. On large Chromium trees this changes an
/// exact action from O(all descendants) property reads to the small candidate
/// set that actually matches the caller's fresh selector.
fn target_condition(
    automation: &IUIAutomation,
    request: &Request<'_>,
) -> Result<windows::Win32::UI::Accessibility::IUIAutomationCondition, String> {
    let mut conditions = Vec::new();
    if let Some(name) = request.name {
        let value = VARIANT::from(name);
        conditions.push(
            unsafe { automation.CreatePropertyCondition(UIA_NamePropertyId, &value) }
                .map_err(|error| format!("UIA name condition unavailable: {error}"))?,
        );
    }
    if let Some(automation_id) = request.automation_id {
        let value = VARIANT::from(automation_id);
        conditions.push(
            unsafe { automation.CreatePropertyCondition(UIA_AutomationIdPropertyId, &value) }
                .map_err(|error| format!("UIA automation ID condition unavailable: {error}"))?,
        );
    }
    if let Some(role) = request.role {
        let control_type = match role.to_ascii_lowercase().as_str() {
            "button" => UIA_ButtonControlTypeId,
            "checkbox" => UIA_CheckBoxControlTypeId,
            "combobox" => UIA_ComboBoxControlTypeId,
            "listitem" | "option" => UIA_ListItemControlTypeId,
            "edit" | "textfield" => UIA_EditControlTypeId,
            "hyperlink" => UIA_HyperlinkControlTypeId,
            "text" => UIA_TextControlTypeId,
            "window" => UIA_WindowControlTypeId,
            _ => return Err("unsupported_role".to_owned()),
        };
        let value = VARIANT::from(control_type.0);
        conditions.push(
            unsafe { automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &value) }
                .map_err(|error| format!("UIA role condition unavailable: {error}"))?,
        );
    }
    if conditions.is_empty() {
        return unsafe { automation.CreateTrueCondition() }
            .map_err(|error| format!("UI Automation condition unavailable: {error}"));
    }
    let mut combined = conditions.remove(0);
    for condition in conditions {
        combined = unsafe { automation.CreateAndCondition(&combined, &condition) }
            .map_err(|error| format!("UIA selector combination failed: {error}"))?;
    }
    Ok(combined)
}

fn control_type_name(control_type: UIA_CONTROLTYPE_ID) -> &'static str {
    match control_type {
        UIA_ButtonControlTypeId => "button",
        UIA_CheckBoxControlTypeId => "checkbox",
        UIA_ComboBoxControlTypeId => "combobox",
        UIA_EditControlTypeId => "edit",
        UIA_ListItemControlTypeId => "listitem",
        UIA_TextControlTypeId => "text",
        UIA_WindowControlTypeId => "window",
        UIA_HyperlinkControlTypeId => "hyperlink",
        _ => "other",
    }
}

fn matches_element(
    element: &IUIAutomationElement,
    request: &Request<'_>,
    root_window_handle: u64,
) -> Result<bool, String> {
    matches_element_identity(element, request, None, root_window_handle)
}

fn has_semantic_press_pattern(element: &IUIAutomationElement, request: &Request<'_>) -> bool {
    if request
        .role
        .is_some_and(|role| role.eq_ignore_ascii_case("checkbox"))
    {
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) }
            .is_ok()
            || unsafe {
                element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
            }
            .is_ok()
    } else if request
        .role
        .is_some_and(|role| role.eq_ignore_ascii_case("combobox"))
    {
        unsafe {
            element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                UIA_ExpandCollapsePatternId,
            )
        }
        .is_ok()
            || unsafe {
                element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
            }
            .is_ok()
    } else {
        unsafe { element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) }
            .is_ok()
            || unsafe {
                element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
            }
            .is_ok()
    }
}

fn available_patterns(element: &IUIAutomationElement) -> Vec<&'static str> {
    let mut patterns = Vec::new();
    if unsafe { element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) }
        .is_ok()
    {
        patterns.push("invoke");
    }
    if unsafe { element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) }
        .is_ok()
    {
        patterns.push("toggle");
    }
    if unsafe {
        element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
    }
    .is_ok()
    {
        patterns.push("selection_item");
    }
    if unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId)
    }
    .is_ok()
    {
        patterns.push("expand_collapse");
    }
    if unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
        .is_ok()
    {
        patterns.push("value");
    }
    if unsafe { element.GetCurrentPattern(UIA_TextPatternId) }.is_ok() {
        patterns.push("text");
    }
    if unsafe { element.GetCurrentPattern(UIA_ScrollItemPatternId) }.is_ok() {
        patterns.push("scroll_item");
    }
    patterns
}

fn has_actionable_semantic_pattern(patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| {
        matches!(
            *pattern,
            "invoke" | "toggle" | "selection_item" | "expand_collapse" | "value"
        )
    })
}

#[cfg(test)]
mod semantic_pattern_tests {
    use super::has_actionable_semantic_pattern;

    #[test]
    fn read_only_and_unimplemented_patterns_are_not_actionable() {
        assert!(!has_actionable_semantic_pattern(&["text"]));
        assert!(!has_actionable_semantic_pattern(&["scroll_item"]));
    }

    #[test]
    fn implemented_semantic_patterns_are_actionable() {
        for pattern in [
            "invoke",
            "toggle",
            "selection_item",
            "expand_collapse",
            "value",
        ] {
            assert!(has_actionable_semantic_pattern(&[pattern]), "{pattern}");
        }
    }
}

fn matches_element_identity(
    element: &IUIAutomationElement,
    request: &Request<'_>,
    expected_name: Option<&str>,
    root_window_handle: u64,
) -> Result<bool, String> {
    let process_id = unsafe { element.CurrentProcessId() }
        .map_err(|error| format!("UI Automation process identity failed: {error}"))?;
    // Packaged Windows apps are often hosted by ApplicationFrameHost. Their
    // top-level UIA window belongs to the host process while descendants can
    // report the packaged app process. Permit that split only when the caller
    // pinned the exact HWND; without it, retain strict PID matching.
    let exact_hosted_window =
        root_window_handle != 0 && request.window_handle == Some(root_window_handle);
    if process_id != request.process_id as i32 && !exact_hosted_window {
        return Ok(false);
    }
    if let Some(name) = expected_name.or(request.name) {
        let current = unsafe { element.CurrentName() }
            .map_err(|error| format!("UI Automation name read failed: {error}"))?;
        if current != name {
            return Ok(false);
        }
    }
    if let Some(automation_id) = request.automation_id {
        let current = unsafe { element.CurrentAutomationId() }
            .map_err(|error| format!("UI Automation id read failed: {error}"))?;
        if current != automation_id {
            return Ok(false);
        }
    }
    if let Some(role) = request.role {
        let control_type = unsafe { element.CurrentControlType() }
            .map_err(|error| format!("UI Automation control type failed: {error}"))?;
        let expected = match role.to_ascii_lowercase().as_str() {
            "button" => UIA_ButtonControlTypeId,
            "checkbox" => UIA_CheckBoxControlTypeId,
            "combobox" => UIA_ComboBoxControlTypeId,
            "listitem" | "option" => UIA_ListItemControlTypeId,
            "edit" | "textfield" => UIA_EditControlTypeId,
            "hyperlink" => UIA_HyperlinkControlTypeId,
            "text" => UIA_TextControlTypeId,
            "window" => UIA_WindowControlTypeId,
            _ => return Err("unsupported_role".to_owned()),
        };
        if control_type != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

fn find_postcondition_element(
    automation: &IUIAutomation,
    request: &Request<'_>,
) -> Result<Option<IUIAutomationElement>, String> {
    let expected_name = request.expected_value;
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
    let windows = target_windows(automation, request)?;
    let mut matched = None;
    for window in windows {
        let provider_process_id = unsafe { window.CurrentProcessId() }
            .map_err(|error| format!("desktop window process identity failed: {error}"))?;
        let root_window_handle = unsafe { window.CurrentNativeWindowHandle() }
            .map_err(|error| format!("window handle read failed: {error}"))?
            .0 as u64;
        if !is_requested_window(request, provider_process_id, root_window_handle) {
            continue;
        }
        let candidates = unsafe { window.FindAll(TreeScope_Descendants, &condition) }
            .map_err(|error| format!("UI Automation tree query failed: {error}"))?;
        let count = unsafe { candidates.Length() }
            .map_err(|error| format!("UI Automation result count failed: {error}"))?;
        for index in 0..count.min(2048) {
            let element = unsafe { candidates.GetElement(index) }
                .map_err(|error| format!("UI Automation element read failed: {error}"))?;
            if matches_element_identity(&element, request, expected_name, root_window_handle)? {
                if matched.is_some() {
                    return Ok(None);
                }
                matched = Some(element);
            }
        }
    }
    Ok(matched)
}

fn target_windows(
    automation: &IUIAutomation,
    request: &Request<'_>,
) -> Result<Vec<IUIAutomationElement>, String> {
    if let Some(handle) = request.window_handle {
        let target_hwnd = HWND(handle as *mut _);
        let window = unsafe { automation.ElementFromHandle(target_hwnd) }
            .map_err(|error| format!("UI Automation target window unavailable: {error}"))?;
        let process_id = unsafe { window.CurrentProcessId() }
            .map_err(|error| format!("UI Automation process identity failed: {error}"))?;
        let native_handle = unsafe { window.CurrentNativeWindowHandle() }
            .map_err(|error| format!("UI Automation target handle unavailable: {error}"))?
            .0 as u64;
        // ElementFromHandle can return a hosted child provider with native
        // HWND 0. Only substitute the caller-pinned HWND, then revalidate that
        // exact HWND against the Win32 owner PID before accepting the root.
        let actual_handle = effective_window_handle(native_handle, handle);
        if !is_requested_window(request, process_id, actual_handle) {
            return Ok(Vec::new());
        }
        return Ok(vec![window]);
    }
    let root = unsafe { automation.GetRootElement() }
        .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
    let windows = unsafe { root.FindAll(TreeScope_Children, &condition) }
        .map_err(|error| format!("desktop window query failed: {error}"))?;
    let window_count = unsafe { windows.Length() }
        .map_err(|error| format!("desktop window count failed: {error}"))?;
    (0..window_count.min(256))
        .map(|index| {
            unsafe { windows.GetElement(index) }
                .map_err(|error| format!("desktop window read failed: {error}"))
        })
        .collect()
}

/// Accept a hosted UIA root only when the caller pinned its exact HWND and the
/// native window still belongs to the requested process. Provider PID alone is
/// insufficient for hosted windows, but it remains strict for unscoped scans.
fn is_requested_window(
    request: &Request<'_>,
    provider_process_id: i32,
    window_handle: u64,
) -> bool {
    let mut owner_process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(HWND(window_handle as *mut _), Some(&mut owner_process_id));
    }
    matches_window_identity(
        request.process_id,
        request.window_handle,
        window_handle,
        provider_process_id,
        owner_process_id,
    )
}

fn matches_window_identity(
    requested_process_id: u32,
    expected_handle: Option<u64>,
    actual_handle: u64,
    provider_process_id: i32,
    owner_process_id: u32,
) -> bool {
    if actual_handle == 0
        || expected_handle.is_some_and(|expected| expected != actual_handle)
        || owner_process_id != requested_process_id
    {
        return false;
    }
    provider_process_id == requested_process_id as i32 || expected_handle == Some(actual_handle)
}

fn verify_after_action(
    automation: &IUIAutomation,
    element: &IUIAutomationElement,
    request: &Request<'_>,
) -> Result<bool, String> {
    const POST_INVOKE_VERIFY_TIMEOUT: Duration = Duration::from_millis(750);
    const POST_INVOKE_VERIFY_INTERVAL: Duration = Duration::from_millis(25);

    if request.action == Action::Press && request.expected_attribute == Some("window_title") {
        let Some(handle) = request.window_handle else {
            return Err("window_title verification requires an exact window_handle".to_owned());
        };
        let deadline = Instant::now() + POST_INVOKE_VERIFY_TIMEOUT;
        loop {
            let window = unsafe { automation.ElementFromHandle(HWND(handle as *mut _)) }
                .map_err(|error| format!("UIA window verification failed: {error}"))?;
            if unsafe { window.CurrentName() }
                .map_err(|error| format!("UIA window title read failed: {error}"))?
                == request.expected_value.unwrap_or_default()
            {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(POST_INVOKE_VERIFY_INTERVAL);
        }
    }
    if request.action != Action::Press || request.expected_attribute != Some("name") {
        return verify(element, request);
    }

    let deadline = Instant::now() + POST_INVOKE_VERIFY_TIMEOUT;
    loop {
        // Reacquire by process, AutomationId, and role. The accessible name is
        // the postcondition and may legitimately change as a result of Invoke.
        let current = if request.automation_id.is_some() {
            find_postcondition_element(automation, request)
                .ok()
                .flatten()
                .unwrap_or_else(|| element.clone())
        } else {
            element.clone()
        };
        if verify(&current, request)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POST_INVOKE_VERIFY_INTERVAL);
    }
}

fn verify(element: &IUIAutomationElement, request: &Request<'_>) -> Result<bool, String> {
    match request.expected_attribute {
        None => Ok(false),
        Some("name") => Ok(unsafe { element.CurrentName() }
            .map_err(|error| format!("UI Automation verification failed: {error}"))?
            == request.expected_value.unwrap_or_default()),
        Some("value") => {
            let pattern: IUIAutomationValuePattern =
                unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId) }
                    .map_err(|error| format!("Value verification pattern unavailable: {error}"))?;
            Ok(unsafe { pattern.CurrentValue() }
                .map_err(|error| format!("Value verification failed: {error}"))?
                == request.expected_value.unwrap_or_default())
        }
        Some("enabled") => Ok(unsafe { element.CurrentIsEnabled() }
            .map_err(|error| format!("Enabled verification failed: {error}"))?
            .as_bool()
            == (request.expected_value == Some("true"))),
        Some("selected") => {
            let pattern: IUIAutomationSelectionItemPattern =
                unsafe { element.GetCurrentPatternAs(UIA_SelectionItemPatternId) }.map_err(
                    |error| format!("Selection verification pattern unavailable: {error}"),
                )?;
            Ok(unsafe { pattern.CurrentIsSelected() }
                .map_err(|error| format!("Selection verification failed: {error}"))?
                .as_bool()
                == (request.expected_value == Some("true")))
        }
        Some("toggle_state") => {
            let pattern: IUIAutomationTogglePattern =
                unsafe { element.GetCurrentPatternAs(UIA_TogglePatternId) }
                    .map_err(|error| format!("Toggle verification pattern unavailable: {error}"))?;
            let expected = match request.expected_value {
                Some("on") => ToggleState_On,
                Some("off") => ToggleState_Off,
                Some("indeterminate") => ToggleState_Indeterminate,
                _ => return Err("toggle_state expects on, off, or indeterminate".to_owned()),
            };
            Ok(unsafe { pattern.CurrentToggleState() }
                .map_err(|error| format!("Toggle state verification failed: {error}"))?
                == expected)
        }
        Some(_) => Err("unsupported_verification_attribute".to_owned()),
    }
}

struct ComGuard;

impl ComGuard {
    fn initialize() -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BufRead, BufReader, ChildStdin, Command, Duration, Instant, Stdio, UiaProcess,
        checked_match_index, effective_window_handle, matches_window_identity, mpsc,
        run_worker_line,
    };

    #[test]
    fn indexed_target_requires_the_same_observed_candidate_set() {
        assert_eq!(checked_match_index(3, 1, Some(3)), Ok(1));
        assert_eq!(
            checked_match_index(2, 1, Some(3)),
            Err("target_set_changed")
        );
        assert_eq!(
            checked_match_index(3, 3, Some(3)),
            Err("target_set_changed")
        );
        assert_eq!(
            checked_match_index(3, 0, None),
            Err("indexed UIA targeting needs expected_match_count")
        );
    }

    #[test]
    fn child_without_native_handle_uses_its_verified_root_window() {
        assert_eq!(effective_window_handle(0, 41), 41);
        assert_eq!(effective_window_handle(42, 41), 42);
    }

    #[test]
    fn hosted_uia_pid_is_allowed_only_for_exact_owned_window() {
        assert!(matches_window_identity(7, Some(99), 99, 8, 7));
        assert!(!matches_window_identity(7, None, 99, 8, 7));
        assert!(!matches_window_identity(7, Some(99), 100, 8, 7));
        assert!(!matches_window_identity(7, Some(99), 99, 8, 9));
        assert!(matches_window_identity(7, None, 99, 7, 7));
        assert!(!matches_window_identity(7, None, 99, 8, 7));
    }

    #[test]
    fn worker_protocol_rejects_malformed_and_unknown_requests_without_ui_calls() {
        let malformed = run_worker_line("{}", None);
        assert_eq!(malformed["ok"], false);
        let unsupported = run_worker_line(
            r#"{"process_id":1,"action":"screenshot","max_nodes":1,"allow_physical_click":false}"#,
            None,
        );
        assert_eq!(unsupported["ok"], false);
        assert!(
            unsupported["error"]
                .as_str()
                .is_some_and(|error| error.contains("unsupported UIA action"))
        );
    }

    #[test]
    fn nonresponsive_worker_is_bounded_and_terminated() {
        let mut child = Command::new("cmd.exe")
            .args(["/D", "/Q", "/C", "set /p comptrol_wait_forever="])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start a local worker fixture that waits forever for input");
        let stdin: ChildStdin = child.stdin.take().expect("worker stdin");
        let stdout = child.stdout.take().expect("worker stdout");
        let (sender, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if let Ok(line) = line {
                    if sender.send(line).is_err() {
                        break;
                    }
                } else {
                    break;
                }
            }
        });
        let worker = UiaProcess {
            child,
            stdin,
            responses,
        };
        let started = Instant::now();
        match worker.responses.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Timeout) => worker.stop(),
            Ok(_) => panic!("the deliberately nonresponsive worker unexpectedly replied"),
            Err(error) => panic!("worker fixture stopped before its deadline: {error}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stopping the stuck worker exceeded the supervisor bound"
        );
    }
}
