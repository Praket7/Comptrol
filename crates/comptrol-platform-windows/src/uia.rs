use serde_json::{Value, json};
use std::time::Instant;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationValuePattern, TreeScope_Descendants, UIA_ButtonControlTypeId,
    UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId, UIA_EditControlTypeId,
};

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

pub fn execute(request: Request<'_>) -> Result<Value, String> {
    let started = Instant::now();
    let com =
        ComGuard::initialize().map_err(|error| format!("COM initialization failed: {error}"))?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|error| format!("UI Automation activation failed: {error}"))?;
    let root = unsafe { automation.GetRootElement() }
        .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
    let candidates = unsafe { root.FindAll(TreeScope_Descendants, &condition) }
        .map_err(|error| format!("UI Automation tree query failed: {error}"))?;
    let count = unsafe { candidates.Length() }
        .map_err(|error| format!("UI Automation result count failed: {error}"))?;
    let mut matches = Vec::new();
    for index in 0..count.min(2048) {
        let element = unsafe { candidates.GetElement(index) }
            .map_err(|error| format!("UI Automation element read failed: {error}"))?;
        if !matches_element(&element, &request)? {
            continue;
        }
        matches.push(element);
        if matches.len() > 1 {
            return Err("target_ambiguous".to_owned());
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
    match request.action {
        Action::Press => {
            let pattern: IUIAutomationInvokePattern = unsafe {
                element.GetCurrentPatternAs(windows::Win32::UI::Accessibility::UIA_InvokePatternId)
            }
            .map_err(|error| format!("Invoke pattern unavailable: {error}"))?;
            unsafe { pattern.Invoke() }.map_err(|error| format!("Invoke failed: {error}"))?;
        }
        Action::SetValue => {
            let value = request.value.ok_or("value_required")?;
            let pattern: IUIAutomationValuePattern = unsafe {
                element.GetCurrentPatternAs(windows::Win32::UI::Accessibility::UIA_ValuePatternId)
            }
            .map_err(|error| format!("Value pattern unavailable: {error}"))?;
            let value = windows::core::BSTR::from(value);
            unsafe { pattern.SetValue(&value) }
                .map_err(|error| format!("SetValue failed: {error}"))?;
        }
    }
    let verified = verify(&element, &request)?;
    drop(com);
    Ok(json!({
        "verified": verified,
        "route": "windows_uia_direct",
        "process_id": request.process_id,
        "candidate_count": 1,
        "bounded_nodes": count.min(2048),
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "mouse": "untouched",
        "clipboard": "untouched"
    }))
}

fn matches_element(element: &IUIAutomationElement, request: &Request<'_>) -> Result<bool, String> {
    let process_id = unsafe { element.CurrentProcessId() }
        .map_err(|error| format!("UI Automation process identity failed: {error}"))?;
    if process_id != request.process_id as i32 {
        return Ok(false);
    }
    if let Some(name) = request.name {
        let current = unsafe { element.CurrentName() }
            .map_err(|error| format!("UI Automation name read failed: {error}"))?;
        if current.to_string() != name {
            return Ok(false);
        }
    }
    if let Some(automation_id) = request.automation_id {
        let current = unsafe { element.CurrentAutomationId() }
            .map_err(|error| format!("UI Automation id read failed: {error}"))?;
        if current.to_string() != automation_id {
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

fn verify(element: &IUIAutomationElement, request: &Request<'_>) -> Result<bool, String> {
    match request.expected_attribute {
        None => Ok(false),
        Some("name") => Ok(unsafe { element.CurrentName() }
            .map_err(|error| format!("UI Automation verification failed: {error}"))?
            .to_string()
            == request.expected_value.unwrap_or_default()),
        Some("value") => {
            let pattern: IUIAutomationValuePattern = unsafe {
                element.GetCurrentPatternAs(windows::Win32::UI::Accessibility::UIA_ValuePatternId)
            }
            .map_err(|error| format!("Value verification pattern unavailable: {error}"))?;
            Ok(unsafe { pattern.CurrentValue() }
                .map_err(|error| format!("Value verification failed: {error}"))?
                .to_string()
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
