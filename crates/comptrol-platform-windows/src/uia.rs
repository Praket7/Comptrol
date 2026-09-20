use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationElement,
    IUIAutomationInvokePattern, IUIAutomationValuePattern, IUIAutomationWindowPattern,
    TreeScope_Children, TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId, UIA_EditControlTypeId, UIA_WindowControlTypeId,
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
    automation: IUIAutomation,
    cache_request: IUIAutomationCacheRequest,
    per_window_index: HashMap<u64, Vec<SemanticCacheEntry>>,
    subscribed_processes: HashSet<u32>,
}

impl UiaScopedCache {
    pub fn new(automation: IUIAutomation) -> Result<Self, String> {
        let cache_request = unsafe { automation.CreateCacheRequest() }
            .map_err(|error| format!("cache request creation failed: {error}"))?;
        unsafe {
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
            automation,
            cache_request,
            per_window_index: HashMap::new(),
            subscribed_processes: HashSet::new(),
        })
    }

    pub fn find_window_by_process(
        &mut self,
        process_id: u32,
    ) -> Result<Option<IUIAutomationElement>, String> {
        let root = unsafe { self.automation.GetRootElement() }
            .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
        let condition = unsafe { self.automation.CreateTrueCondition() }
            .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
        let cached = unsafe {
            root.FindFirstBuildCache(TreeScope_Children, &condition, &self.cache_request)
        }
        .map_err(|error| format!("scoped window cache query failed: {error}"))?;
        let window_handle = unsafe { cached.CurrentNativeWindowHandle() }
            .map_err(|error| format!("native window handle read failed: {error}"))?;
        if window_handle.0 == 0 {
            return Ok(None);
        }
        let pid = unsafe { cached.CurrentProcessId() }
            .map_err(|error| format!("process id read failed: {error}"))?;
        if pid != process_id as i32 {
            return Ok(None);
        }
        Ok(Some(cached))
    }

    pub fn index_process_windows(
        &mut self,
        process_id: u32,
    ) -> Result<Vec<SemanticCacheEntry>, String> {
        let root = unsafe { self.automation.GetRootElement() }
            .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
        let condition = unsafe { self.automation.CreateTrueCondition() }
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
            let entry_process_id = unsafe { element.CurrentProcessId() }
                .map_err(|error| format!("process id read failed: {error}"))?
                as u32;
            let entry = SemanticCacheEntry {
                process_id: entry_process_id,
                automation_id: unsafe { element.CurrentAutomationId() }.ok().flatten(),
                name: unsafe { element.CurrentName() }.ok().flatten(),
                role: None,
                bounding_rect: unsafe { element.CurrentBoundingRectangle() }
                    .ok()
                    .map(|rect| (rect.left, rect.top, rect.width, rect.height)),
                is_enabled: unsafe { element.CurrentIsEnabled() }
                    .map_err(|error| format!("enabled state failed: {error}"))
                    .map(|v| v.as_bool())
                    .unwrap_or(false),
                is_offscreen: unsafe { element.CurrentIsOffscreen() }
                    .map_err(|error| format!("offscreen state failed: {error}"))
                    .map(|v| v.as_bool())
                    .unwrap_or(false),
                window_handle: unsafe { element.CurrentNativeWindowHandle() }
                    .map_err(|error| format!("window handle read failed: {error}"))
                    .map(|v| v.0)
                    .unwrap_or(0),
            };
            entries.push(entry);
        }
        self.per_window_index.insert(process_id as u64, entries);
        Ok(entries)
    }

    pub fn subscribe_process_events(&mut self, process_id: u32) -> Result<(), String> {
        self.subscribed_processes.insert(process_id);
        Ok(())
    }

    pub fn get_indexed(&self, window_handle: u64) -> Vec<&SemanticCacheEntry> {
        self.per_window_index
            .get(&window_handle)
            .map(|entries| entries.iter().collect())
            .unwrap_or_default()
    }

    pub fn invalidate_window(&mut self, window_handle: u64) {
        self.per_window_index.remove(&window_handle);
    }
}

struct UiaWorker {
    requests: WorkerSender,
}

type WorkItem = (OwnedRequest, mpsc::Sender<Result<Value, String>>);
type WorkerSender = mpsc::Sender<WorkItem>;

static WORKER: OnceLock<UiaWorker> = OnceLock::new();
static CACHE: OnceLock<Arc<Mutex<UiaScopedCache>>> = OnceLock::new();

fn worker() -> &'static UiaWorker {
    WORKER.get_or_init(|| {
        let (requests, receiver) =
            mpsc::channel::<(OwnedRequest, mpsc::Sender<Result<Value, String>>)>();
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
                    .and_then(|a| UiaScopedCache::new(a.clone()).ok());
                while let Ok((request, response)) = receiver.recv() {
                    let result = match (&com, &automation, &mut cache) {
                        (Ok(_), Some(automation), Some(cache)) => {
                            execute_once(automation, cache, request.as_request())
                        }
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
    let (response, receiver) = mpsc::channel();
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
    let root = unsafe { automation.GetRootElement() }
        .map_err(|error| format!("UI Automation root unavailable: {error}"))?;
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("UI Automation condition unavailable: {error}"))?;
    let cached =
        unsafe { root.FindFirstBuildCache(TreeScope_Children, &condition, &cache.cache_request) }
            .map_err(|error| format!("scoped window cache query failed: {error}"))?;
    let candidates = unsafe { cached.FindAll(TreeScope_Descendants, &condition) }
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
    let window_handle = unsafe { element.CurrentNativeWindowHandle() }
        .map_err(|error| format!("window handle read failed: {error}"))?;
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
    let entries = cache.get_indexed(window_handle.0);
    Ok(json!({
        "verified": verified,
        "route": "windows_uia_scoped_cache",
        "process_id": request.process_id,
        "candidate_count": 1,
        "bounded_nodes": count.min(2048),
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "mouse": "untouched",
        "clipboard": "untouched",
        "cache_hit": !entries.is_empty(),
        "window_handle": window_handle.0,
        "indexed_entries": entries.len()
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

fn verify(element: &IUIAutomationElement, request: &Request<'_>) -> Result<bool, String> {
    match request.expected_attribute {
        None => Ok(false),
        Some("name") => Ok(unsafe { element.CurrentName() }
            .map_err(|error| format!("UI Automation verification failed: {error}"))?
            == request.expected_value.unwrap_or_default()),
        Some("value") => {
            let pattern: IUIAutomationValuePattern = unsafe {
                element.GetCurrentPatternAs(windows::Win32::UI::Accessibility::UIA_ValuePatternId)
            }
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
