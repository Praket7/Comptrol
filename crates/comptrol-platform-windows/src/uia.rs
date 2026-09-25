#![allow(non_upper_case_globals, non_camel_case_types, clippy::collapsible_if)]
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationValuePattern, TreeScope_Children, TreeScope_Descendants, UIA_ButtonControlTypeId,
    UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId, UIA_EditControlTypeId,
    UIA_InvokePatternId, UIA_ValuePatternId,
};

#[allow(non_upper_case_globals)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Press,
    SetValue,
}

#[derive(Clone, Debug)]
pub struct Request<'a> {
    pub process_id: u32,
    pub name: Option<&'a str>,
    pub automation_id: Option<&'a str>,
    pub role: Option<&'a str>,
    pub action: Action,
    pub value: Option<&'a str>,
    pub expected_attribute: Option<&'a str>,
    pub expected_value: Option<&'a str>,
}

#[derive(Clone, Debug)]
struct OwnedRequest {
    process_id: u32,
    name: Option<String>,
    automation_id: Option<String>,
    role: Option<String>,
    action: Action,
    value: Option<String>,
    expected_attribute: Option<String>,
    expected_value: Option<String>,
}

impl<'a> From<Request<'a>> for OwnedRequest {
    fn from(request: Request<'a>) -> Self {
        Self {
            process_id: request.process_id,
            name: request.name.map(str::to_owned),
            automation_id: request.automation_id.map(str::to_owned),
            role: request.role.map(str::to_owned),
            action: request.action,
            value: request.value.map(str::to_owned),
            expected_attribute: request.expected_attribute.map(str::to_owned),
            expected_value: request.expected_value.map(str::to_owned),
        }
    }
}

impl OwnedRequest {
    fn as_request(&self) -> Request<'_> {
        Request {
            process_id: self.process_id,
            name: self.name.as_deref(),
            automation_id: self.automation_id.as_deref(),
            role: self.role.as_deref(),
            action: self.action,
            value: self.value.as_deref(),
            expected_attribute: self.expected_attribute.as_deref(),
            expected_value: self.expected_value.as_deref(),
        }
    }
}

struct UiaWorker {
    requests: WorkerSender,
}

type WorkItem = (OwnedRequest, std::sync::mpsc::Sender<Result<Value, String>>);
type WorkerSender = std::sync::mpsc::Sender<WorkItem>;

static WORKER: OnceLock<UiaWorker> = OnceLock::new();

fn worker() -> &'static UiaWorker {
    WORKER.get_or_init(|| {
        let (requests, _receiver) = std::sync::mpsc::channel::<WorkItem>();
        std::thread::Builder::new()
            .name("comptrol-windows-uia".to_owned())
            .spawn(move || {
                let com = ComGuard::initialize()
                    .map_err(|error| format!("COM initialization failed in UIA worker: {error}"));
                let automation = match &com {
                    Ok(_) => {
                        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                            .map_err(|error| format!("UI Automation activation failed: {error}"))
                            .ok()
                    }
                    Err(_) => None,
                };
                while let Ok((request, response)) = _receiver.recv() {
                    let result = match (&com, &automation) {
                        (Ok(_), Some(automation)) => execute_once(automation, request.as_request()),
                        (Err(error), _) => Err(error.clone()),
                        (Ok(_), None) => Err("UI Automation activation unavailable".to_owned()),
                    };
                    let _ = response.send(result);
                }
            })
            .expect("failed to start persistent Windows UIA worker");
        UiaWorker { requests }
    })
}

pub fn execute(request: Request<'_>) -> Result<Value, String> {
    let (response, receiver) = std::sync::mpsc::channel();
    worker()
        .requests
        .send((request.into(), response))
        .map_err(|_| "Windows UIA worker stopped".to_owned())?;
    receiver
        .recv()
        .map_err(|_| "Windows UIA worker stopped before responding".to_owned())?
}

fn execute_once(automation: &IUIAutomation, request: Request<'_>) -> Result<Value, String> {
    let started = Instant::now();
    let root = unsafe { automation.GetRootElement() }
        .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
    let windows = unsafe { root.FindAll(TreeScope_Children, &condition) }
        .map_err(|error| format!("desktop window query failed: {error}"))?;
    let window_count = unsafe { windows.Length() }
        .map_err(|error| format!("desktop window count failed: {error}"))?;
    let mut matches = Vec::new();
    let mut bounded_nodes = 0;
    for index in 0..window_count.min(256) {
        let window = unsafe { windows.GetElement(index) }
            .map_err(|error| format!("desktop window read failed: {error}"))?;
        if unsafe { window.CurrentProcessId() }
            .map_err(|error| format!("desktop window process identity failed: {error}"))?
            != request.process_id as i32
        {
            continue;
        }
        let candidates = unsafe { window.FindAll(TreeScope_Descendants, &condition) }
            .map_err(|error| format!("UI Automation tree query failed: {error}"))?;
        let count = unsafe { candidates.Length() }
            .map_err(|error| format!("UI Automation result count failed: {error}"))?;
        bounded_nodes += count.min(2048);
        for candidate_index in 0..count.min(2048) {
            let element = unsafe { candidates.GetElement(candidate_index) }
                .map_err(|error| format!("UI Automation element read failed: {error}"))?;
            if !matches_element(&element, &request)? {
                continue;
            }
            matches.push(element);
            if matches.len() > 1 {
                return Err("target_ambiguous".to_owned());
            }
        }
    }
    let Some(element) = matches.pop() else {
        return Err("target_missing".to_owned());
    };
    let enabled = unsafe { element.CurrentIsEnabled() }
        .map_err(|error| format!("UI Automation enabled state failed: {error}"))?;
    if !enabled.as_bool() {
        return Err("target_disabled".to_owned());
    }
    let window_handle = unsafe { element.CurrentNativeWindowHandle() }
        .map_err(|error| format!("window handle read failed: {error}"))?
        .0 as u64;
    match request.action {
        Action::Press => {
            let pattern: IUIAutomationInvokePattern =
                unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) }
                    .map_err(|error| format!("Invoke pattern unavailable: {error}"))?;
            unsafe { pattern.Invoke() }.map_err(|error| format!("Invoke failed: {error}"))?;
        }
        Action::SetValue => {
            let value = request.value.ok_or("value_required")?;
            let pattern: IUIAutomationValuePattern =
                unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId) }
                    .map_err(|error| format!("Value pattern unavailable: {error}"))?;
            let value = windows::core::BSTR::from(value);
            unsafe { pattern.SetValue(&value) }
                .map_err(|error| format!("SetValue failed: {error}"))?;
        }
    }
    let verified = verify_after_action(automation, &element, &request)?;
    Ok(json!({
        "verified": verified,
        "route": "windows_uia_direct",
        "process_id": request.process_id,
        "candidate_count": 1,
        "bounded_nodes": bounded_nodes,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "mouse": "untouched",
        "clipboard": "untouched",
        "window_handle": window_handle,
    }))
}

fn matches_element(element: &IUIAutomationElement, request: &Request<'_>) -> Result<bool, String> {
    matches_element_identity(element, request, None)
}

fn matches_element_identity(
    element: &IUIAutomationElement,
    request: &Request<'_>,
    expected_name: Option<&str>,
) -> Result<bool, String> {
    let process_id = unsafe { element.CurrentProcessId() }
        .map_err(|error| format!("UI Automation process identity failed: {error}"))?;
    if process_id != request.process_id as i32 {
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
            "edit" | "textfield" => UIA_EditControlTypeId,
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
    let root = unsafe { automation.GetRootElement() }
        .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
    let windows = unsafe { root.FindAll(TreeScope_Children, &condition) }
        .map_err(|error| format!("desktop window query failed: {error}"))?;
    let window_count = unsafe { windows.Length() }
        .map_err(|error| format!("desktop window count failed: {error}"))?;
    let mut matched = None;
    for window_index in 0..window_count.min(256) {
        let window = unsafe { windows.GetElement(window_index) }
            .map_err(|error| format!("desktop window read failed: {error}"))?;
        if unsafe { window.CurrentProcessId() }
            .map_err(|error| format!("desktop window process identity failed: {error}"))?
            != request.process_id as i32
        {
            continue;
        }
        let candidates = unsafe { window.FindAll(TreeScope_Descendants, &condition) }
            .map_err(|error| format!("UI Automation tree query failed: {error}"))?;
        let count = unsafe { candidates.Length() }
            .map_err(|error| format!("UI Automation result count failed: {error}"))?;
        for index in 0..count.min(2048) {
            let element = unsafe { candidates.GetElement(index) }
                .map_err(|error| format!("UI Automation element read failed: {error}"))?;
            if matches_element_identity(&element, request, expected_name)? {
                if matched.is_some() {
                    return Ok(None);
                }
                matched = Some(element);
            }
        }
    }
    Ok(matched)
}

fn verify_after_action(
    automation: &IUIAutomation,
    element: &IUIAutomationElement,
    request: &Request<'_>,
) -> Result<bool, String> {
    const POST_INVOKE_VERIFY_TIMEOUT: Duration = Duration::from_millis(750);
    const POST_INVOKE_VERIFY_INTERVAL: Duration = Duration::from_millis(25);

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
