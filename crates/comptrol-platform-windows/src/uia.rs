#![allow(non_upper_case_globals, non_camel_case_types, clippy::collapsible_if)]
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;
use windows::Win32::Foundation::RECT;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationElement,
    IUIAutomationInvokePattern, IUIAutomationValuePattern, TreeScope_Children,
    TreeScope_Descendants, UIA_AutomationIdPropertyId, UIA_BoundingRectanglePropertyId,
    UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
    UIA_ControlTypePropertyId, UIA_EditControlTypeId, UIA_InvokePatternId, UIA_IsEnabledPropertyId,
    UIA_IsOffscreenPropertyId, UIA_NamePropertyId, UIA_ProcessIdPropertyId, UIA_ValuePatternId,
    UIA_WindowControlTypeId, UIA_WindowPatternId,
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

#[derive(Clone, Debug)]
pub struct SemanticCacheEntry {
    pub process_id: u32,
    pub automation_id: Option<String>,
    pub name: Option<String>,
    pub role: Option<String>,
    pub bounding_rect: Option<(f64, f64, f64, f64)>,
    pub is_enabled: bool,
    pub is_offscreen: bool,
    pub window_handle: u64,
}

#[derive(Debug)]
pub struct UiaScopedCache {
    cache_request: IUIAutomationCacheRequest,
    per_window_index: HashMap<u64, Vec<SemanticCacheEntry>>,
}

impl UiaScopedCache {
    pub fn new(automation: &IUIAutomation) -> Result<Self, String> {
        let cache_request = unsafe { automation.CreateCacheRequest() }
            .map_err(|error| format!("cache request creation failed: {error}"))?;
        unsafe {
            cache_request
                .AddProperty(UIA_ProcessIdPropertyId)
                .map_err(|error| format!("failed to add ProcessId to cache: {error}"))?;
            cache_request
                .AddProperty(UIA_NamePropertyId)
                .map_err(|error| format!("failed to add Name to cache: {error}"))?;
            cache_request
                .AddProperty(UIA_AutomationIdPropertyId)
                .map_err(|error| format!("failed to add AutomationId to cache: {error}"))?;
            cache_request
                .AddProperty(UIA_ControlTypePropertyId)
                .map_err(|error| format!("failed to add ControlType to cache: {error}"))?;
            cache_request
                .AddProperty(UIA_BoundingRectanglePropertyId)
                .map_err(|error| format!("failed to add BoundingRectangle to cache: {error}"))?;
            cache_request
                .AddProperty(UIA_IsEnabledPropertyId)
                .map_err(|error| format!("failed to add IsEnabled to cache: {error}"))?;
            cache_request
                .AddProperty(UIA_IsOffscreenPropertyId)
                .map_err(|error| format!("failed to add IsOffscreen to cache: {error}"))?;
            cache_request
                .AddPattern(UIA_InvokePatternId)
                .map_err(|error| format!("failed to add InvokePattern to cache: {error}"))?;
            cache_request
                .AddPattern(UIA_ValuePatternId)
                .map_err(|error| format!("failed to add ValuePattern to cache: {error}"))?;
            cache_request
                .AddPattern(UIA_WindowPatternId)
                .map_err(|error| format!("failed to add WindowPattern to cache: {error}"))?;
        }
        Ok(Self {
            cache_request,
            per_window_index: HashMap::new(),
        })
    }

    pub fn index_process_windows(
        &mut self,
        automation: &IUIAutomation,
        process_id: u32,
    ) -> Result<Vec<SemanticCacheEntry>, String> {
        let root = unsafe { automation.GetRootElement() }
            .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
        let condition = unsafe { automation.CreateTrueCondition() }
            .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
        let cached = unsafe {
            root.FindAllBuildCache(TreeScope_Descendants, &condition, &self.cache_request)
        }
        .map_err(|error| format!("scoped tree cache query failed: {error}"))?;
        let count = unsafe { cached.Length() }
            .map_err(|error| format!("cache result count failed: {error}"))?;
        let mut entries = Vec::new();
        for index in 0..count.min(4096) {
            let element = unsafe { cached.GetElement(index) }
                .map_err(|error| format!("cache element read failed: {error}"))?;
            let entry_process_id = unsafe { element.CachedProcessId() }
                .map_err(|error| format!("cached process id failed: {error}"))?
                as u32;
            if entry_process_id != process_id {
                continue;
            }
            let rect: RECT = unsafe { element.CachedBoundingRectangle() }
                .map_err(|error| format!("cached bounding rect failed: {error}"))?;
            let automation_id = unsafe { element.CachedAutomationId() }
                .ok()
                .and_then(|bstr| {
                    let s = bstr.to_string();
                    if s.is_empty() { None } else { Some(s) }
                });
            let name = unsafe { element.CachedName() }.ok().and_then(|bstr| {
                let s = bstr.to_string();
                if s.is_empty() { None } else { Some(s) }
            });
            let control_type = unsafe { element.CachedControlType() }
                .map_err(|error| format!("cached control type failed: {error}"))?;
            let is_enabled = unsafe { element.CachedIsEnabled() }
                .map_err(|error| format!("cached enabled failed: {error}"))?
                .as_bool();
            let is_offscreen = unsafe { element.CachedIsOffscreen() }
                .map_err(|error| format!("cached offscreen failed: {error}"))?
                .as_bool();
            let window_handle = unsafe { element.CurrentNativeWindowHandle() }
                .map_err(|error| format!("window handle failed: {error}"))?
                .0 as u64;
            let role = match control_type {
                UIA_ButtonControlTypeId => Some("button"),
                UIA_CheckBoxControlTypeId => Some("checkbox"),
                UIA_ComboBoxControlTypeId => Some("combobox"),
                UIA_EditControlTypeId => Some("edit"),
                UIA_WindowControlTypeId => Some("window"),
                _ => None,
            }
            .map(str::to_owned);
            let _ = control_type; // suppress unused warning for match arm constants
            entries.push(SemanticCacheEntry {
                process_id: entry_process_id,
                automation_id,
                name,
                role,
                bounding_rect: Some((
                    rect.left as f64,
                    rect.top as f64,
                    (rect.right - rect.left) as f64,
                    (rect.bottom - rect.top) as f64,
                )),
                is_enabled,
                is_offscreen,
                window_handle,
            });
        }
        self.per_window_index
            .insert(process_id as u64, entries.clone());
        Ok(entries)
    }

    pub fn get_indexed(&self, process_id: u32) -> Vec<&SemanticCacheEntry> {
        self.per_window_index
            .get(&(process_id as u64))
            .map(|entries| entries.iter().collect())
            .unwrap_or_default()
    }

    pub fn invalidate_process(&mut self, process_id: u32) {
        self.per_window_index.remove(&(process_id as u64));
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
                let mut cache = automation
                    .as_ref()
                    .and_then(|a| UiaScopedCache::new(a).ok());
                while let Ok((request, response)) = _receiver.recv() {
                    let result = match (&com, &automation, &mut cache) {
                        (Ok(_), Some(automation), Some(cache)) => {
                            execute_once(automation, cache, request.as_request())
                        }
                        (Ok(_), Some(_), None) => Err("UIA cache not initialized".to_owned()),
                        (_, None, _) => Err("UI Automation activation unavailable".to_owned()),
                        (Err(error), _, _) => Err(error.clone()),
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

fn execute_once(
    automation: &IUIAutomation,
    cache: &mut UiaScopedCache,
    request: Request<'_>,
) -> Result<Value, String> {
    let started = Instant::now();
    let entries = cache.get_indexed(request.process_id);
    if !entries.is_empty() {
        for entry in entries {
            if matches_cached_entry(entry, &request) {
                let window_handle = entry.window_handle;
                let element = find_element_by_window_handle(automation, window_handle)?;
                let enabled = unsafe { element.CurrentIsEnabled() }
                    .map_err(|error| format!("UI Automation enabled state failed: {error}"))?;
                if !enabled.as_bool() {
                    return Err("target_disabled".to_owned());
                }
                match request.action {
                    Action::Press => {
                        let pattern: IUIAutomationInvokePattern =
                            unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) }
                                .map_err(|error| format!("Invoke pattern unavailable: {error}"))?;
                        unsafe { pattern.Invoke() }
                            .map_err(|error| format!("Invoke failed: {error}"))?;
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
                let verified = verify(&element, &request)?;
                return Ok(json!({
                    "verified": verified,
                    "route": "windows_uia_cached",
                    "process_id": request.process_id,
                    "cache_hit": true,
                    "window_handle": window_handle,
                    "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
                    "mouse": "untouched",
                    "clipboard": "untouched"
                }));
            }
        }
    }
    let root = unsafe { automation.GetRootElement() }
        .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
    let windows =
        unsafe { root.FindAllBuildCache(TreeScope_Children, &condition, &cache.cache_request) }
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
    let verified = verify(&element, &request)?;
    let _ = cache.index_process_windows(automation, request.process_id);
    Ok(json!({
        "verified": verified,
        "route": "windows_uia_scoped_cache",
        "process_id": request.process_id,
        "candidate_count": 1,
        "bounded_nodes": bounded_nodes,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "mouse": "untouched",
        "clipboard": "untouched",
        "cache_hit": false,
        "window_handle": window_handle,
        "indexed_entries": cache.get_indexed(request.process_id).len()
    }))
}

fn matches_cached_entry(entry: &SemanticCacheEntry, request: &Request<'_>) -> bool {
    if let Some(name) = request.name {
        if entry.name.as_deref() != Some(name) {
            return false;
        }
    }
    if let Some(automation_id) = request.automation_id {
        if entry.automation_id.as_deref() != Some(automation_id) {
            return false;
        }
    }
    if let Some(role) = request.role {
        if entry.role.as_deref() != Some(role) {
            return false;
        }
    }
    true
}

#[allow(dead_code)]
fn verify_cached_entry(entry: &SemanticCacheEntry, request: &Request<'_>) -> Result<bool, String> {
    match request.expected_attribute {
        None => Ok(false),
        Some("name") => Ok(entry.name.as_deref() == request.expected_value),
        Some("enabled") => Ok(entry.is_enabled == (request.expected_value == Some("true"))),
        Some(_) => Err("unsupported_verification_attribute".to_owned()),
    }
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

fn find_element_by_window_handle(
    automation: &IUIAutomation,
    window_handle: u64,
) -> Result<IUIAutomationElement, String> {
    let root = unsafe { automation.GetRootElement() }
        .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
    let candidates = unsafe { root.FindAll(TreeScope_Descendants, &condition) }
        .map_err(|error| format!("UI Automation tree query failed: {error}"))?;
    let count = unsafe { candidates.Length() }
        .map_err(|error| format!("UI Automation result count failed: {error}"))?;
    for index in 0..count.min(4096) {
        let element = unsafe { candidates.GetElement(index) }
            .map_err(|error| format!("UI Automation element read failed: {error}"))?;
        let handle = unsafe { element.CurrentNativeWindowHandle() }
            .map_err(|error| format!("window handle read failed: {error}"))?;
        if handle.0 as u64 == window_handle {
            return Ok(element);
        }
    }
    Err("window handle not found in automation tree".to_owned())
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
