#![deny(unsafe_op_in_unsafe_fn)]

use serde_json::Value;
#[cfg(target_os = "macos")]
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Press,
    SetValue,
}

#[derive(Clone, Debug)]
pub struct Request<'a> {
    pub process_id: u32,
    pub name: &'a str,
    pub role: Option<&'a str>,
    pub action: Action,
    pub value: Option<&'a str>,
    pub expected_attribute: Option<&'a str>,
    pub expected_value: Option<&'a str>,
    pub timeout: Duration,
}

#[cfg(not(target_os = "macos"))]
pub fn execute(_request: Request<'_>) -> Result<Value, String> {
    Err("macOS AX adapter is only available on macOS".to_owned())
}

/// Whether this process is currently trusted for Accessibility (TCC).
/// Read-only: it never prompts and never changes authorization state.
#[cfg(not(target_os = "macos"))]
pub fn accessibility_trusted() -> bool {
    false
}

#[cfg(target_os = "macos")]
pub fn accessibility_trusted() -> bool {
    native::is_process_trusted()
}

#[cfg(target_os = "macos")]
mod native {
    use super::{Action, Request};
    use serde_json::{Value, json};
    use std::ffi::c_void;
    use std::ptr;
    use std::time::Instant;

    type AXUIElementRef = *const c_void;
    type CFArrayRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFIndex = isize;
    type CFStringEncoding = u32;
    type CFStringRef = *const c_void;
    type CFTypeRef = *const c_void;
    type CFTypeID = usize;
    type AXError = i32;
    type Boolean = u8;

    const K_CF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;
    const K_AX_ERROR_SUCCESS: AXError = 0;
    const K_AX_ERROR_CANNOT_COMPLETE: AXError = -25204;
    const K_AX_ERROR_NOT_IMPLEMENTED: AXError = -25205;
    const K_AX_ERROR_INVALID_UI_ELEMENT: AXError = -25206;
    const K_AX_ERROR_ILLEGAL_ARGUMENT: AXError = -25207;
    const K_AX_ERROR_ACTION_UNSUPPORTED: AXError = -25208;
    const K_AX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> Boolean;
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AXError;
        fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> AXError;
        fn AXUIElementIsAttributeSettable(
            element: AXUIElementRef,
            attribute: CFStringRef,
            settable: *mut Boolean,
        ) -> AXError;
        fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
        fn AXUIElementSetAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: CFTypeRef,
        ) -> AXError;
        fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout: f32) -> AXError;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
        fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> *const c_void;
        fn CFGetTypeID(value: CFTypeRef) -> CFTypeID;
        fn CFRelease(value: CFTypeRef);
        fn CFStringCreateWithCString(
            allocator: CFAllocatorRef,
            string: *const i8,
            encoding: CFStringEncoding,
        ) -> CFStringRef;
        fn CFStringGetCString(
            string: CFStringRef,
            buffer: *mut i8,
            buffer_size: CFIndex,
            encoding: CFStringEncoding,
        ) -> bool;
        fn CFStringGetTypeID() -> CFTypeID;
    }

    pub(super) fn is_process_trusted() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    pub(super) fn execute(request: Request<'_>) -> Result<Value, String> {
        if !is_process_trusted() {
            return Err("accessibility_permission_required".to_owned());
        }
        let started = Instant::now();
        let application = unsafe { AXUIElementCreateApplication(request.process_id as i32) };
        if application.is_null() {
            return Err("application_unavailable".to_owned());
        }
        let _application_guard = Release(application);
        let timeout_seconds = request.timeout.as_secs_f32().clamp(0.05, 10.0);
        check_ax(
            unsafe { AXUIElementSetMessagingTimeout(application, timeout_seconds) },
            "set messaging timeout",
        )?;
        let mut actual_pid = 0;
        check_ax(
            unsafe { AXUIElementGetPid(application, &mut actual_pid) },
            "read process id",
        )?;
        if actual_pid != request.process_id as i32 {
            return Err("process_identity_mismatch".to_owned());
        }
        let target = find_target(application, &request, 2048)?;
        match request.action {
            Action::Press => {
                let action = CfString::new("AXPress")?;
                check_ax(
                    unsafe { AXUIElementPerformAction(target, action.as_ref()) },
                    "perform AX press",
                )?;
            }
            Action::SetValue => {
                let value = CfString::new(request.value.ok_or("value_required")?)?;
                let attribute = CfString::new("AXValue")?;
                let mut settable = 0;
                check_ax(
                    unsafe {
                        AXUIElementIsAttributeSettable(target, attribute.as_ref(), &mut settable)
                    },
                    "check AX value settable",
                )?;
                if settable == 0 {
                    return Err("value_not_settable".to_owned());
                }
                check_ax(
                    unsafe {
                        AXUIElementSetAttributeValue(target, attribute.as_ref(), value.as_ref())
                    },
                    "set AX value",
                )?;
            }
        }
        let verified = verify(target, &request)?;
        Ok(json!({
            "verified": verified,
            "route": "macos_ax_direct",
            "process_id": request.process_id,
            "bounded_nodes": 2048,
            "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
            "mouse": "untouched",
            "clipboard": "untouched"
        }))
    }

    fn find_target(
        application: AXUIElementRef,
        request: &Request<'_>,
        limit: usize,
    ) -> Result<AXUIElementRef, String> {
        let children_attribute = CfString::new("AXChildren")?;
        let mut queue = vec![application];
        let mut found = None;
        let mut visited = 0;
        while let Some(node) = queue.pop() {
            visited += 1;
            if visited > limit {
                break;
            }
            if matches_node(node, request)? {
                if found.is_some() {
                    return Err("target_ambiguous".to_owned());
                }
                found = Some(node);
            }
            let mut children: CFTypeRef = ptr::null();
            let error = unsafe {
                AXUIElementCopyAttributeValue(node, children_attribute.as_ref(), &mut children)
            };
            if error == K_AX_ERROR_ATTRIBUTE_UNSUPPORTED || children.is_null() {
                continue;
            }
            check_ax(error, "read AX children")?;
            let _children_guard = Release(children);
            let array = children as CFArrayRef;
            let count = unsafe { CFArrayGetCount(array) }.max(0) as usize;
            for index in (0..count.min(limit - visited)).rev() {
                let child = unsafe { CFArrayGetValueAtIndex(array, index as CFIndex) };
                if !child.is_null() {
                    queue.push(child);
                }
            }
        }
        found.ok_or_else(|| "target_missing".to_owned())
    }

    fn matches_node(node: AXUIElementRef, request: &Request<'_>) -> Result<bool, String> {
        let title = read_string_attribute(node, "AXTitle")?
            .or(read_string_attribute(node, "AXDescription")?);
        if title.as_deref() != Some(request.name) {
            return Ok(false);
        }
        if let Some(role) = request.role
            && read_string_attribute(node, "AXRole")?.as_deref() != Some(role)
        {
            return Ok(false);
        }
        Ok(true)
    }

    fn verify(node: AXUIElementRef, request: &Request<'_>) -> Result<bool, String> {
        match request.expected_attribute {
            None => Ok(false),
            Some("name") => {
                Ok(read_string_attribute(node, "AXTitle")?.as_deref() == request.expected_value)
            }
            Some("value") => {
                Ok(read_string_attribute(node, "AXValue")?.as_deref() == request.expected_value)
            }
            Some(_) => Err("unsupported_verification_attribute".to_owned()),
        }
    }

    fn read_string_attribute(node: AXUIElementRef, name: &str) -> Result<Option<String>, String> {
        let attribute = CfString::new(name)?;
        let mut value: CFTypeRef = ptr::null();
        let error = unsafe { AXUIElementCopyAttributeValue(node, attribute.as_ref(), &mut value) };
        if error == K_AX_ERROR_ATTRIBUTE_UNSUPPORTED || value.is_null() {
            return Ok(None);
        }
        check_ax(error, "read AX attribute")?;
        let _guard = Release(value);
        if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            return Ok(None);
        }
        let mut buffer = vec![0_i8; 4096];
        if !unsafe {
            CFStringGetCString(
                value,
                buffer.as_mut_ptr(),
                buffer.len() as CFIndex,
                K_CF_STRING_ENCODING_UTF8,
            )
        } {
            return Err("AX string value exceeded bound".to_owned());
        }
        let bytes = buffer
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect::<Vec<_>>();
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|error| format!("AX string value was not UTF-8: {error}"))
    }

    fn check_ax(error: AXError, operation: &str) -> Result<(), String> {
        if error == K_AX_ERROR_SUCCESS {
            return Ok(());
        }
        let reason = match error {
            K_AX_ERROR_CANNOT_COMPLETE => "cannot_complete",
            K_AX_ERROR_NOT_IMPLEMENTED => "not_implemented",
            K_AX_ERROR_INVALID_UI_ELEMENT => "invalid_element",
            K_AX_ERROR_ILLEGAL_ARGUMENT => "illegal_argument",
            K_AX_ERROR_ACTION_UNSUPPORTED => "action_unsupported",
            _ => "ax_error",
        };
        Err(format!("{operation}: {reason} ({error})"))
    }

    struct CfString(CFStringRef);

    impl CfString {
        fn new(value: &str) -> Result<Self, String> {
            let bytes = std::ffi::CString::new(value)
                .map_err(|_| "AX string contained a NUL byte".to_owned())?;
            let value = unsafe {
                CFStringCreateWithCString(ptr::null(), bytes.as_ptr(), K_CF_STRING_ENCODING_UTF8)
            };
            if value.is_null() {
                Err("CoreFoundation string allocation failed".to_owned())
            } else {
                Ok(Self(value))
            }
        }

        fn as_ref(&self) -> CFStringRef {
            self.0
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }

    struct Release(CFTypeRef);

    impl Drop for Release {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug)]
struct OwnedRequest {
    process_id: u32,
    name: String,
    role: Option<String>,
    action: Action,
    value: Option<String>,
    expected_attribute: Option<String>,
    expected_value: Option<String>,
    timeout: Duration,
}

#[cfg(target_os = "macos")]
impl<'a> From<Request<'a>> for OwnedRequest {
    fn from(request: Request<'a>) -> Self {
        Self {
            process_id: request.process_id,
            name: request.name.to_owned(),
            role: request.role.map(str::to_owned),
            action: request.action,
            value: request.value.map(str::to_owned),
            expected_attribute: request.expected_attribute.map(str::to_owned),
            expected_value: request.expected_value.map(str::to_owned),
            timeout: request.timeout,
        }
    }
}

#[cfg(target_os = "macos")]
impl OwnedRequest {
    fn as_request(&self) -> Request<'_> {
        Request {
            process_id: self.process_id,
            name: &self.name,
            role: self.role.as_deref(),
            action: self.action,
            value: self.value.as_deref(),
            expected_attribute: self.expected_attribute.as_deref(),
            expected_value: self.expected_value.as_deref(),
            timeout: self.timeout,
        }
    }
}

#[cfg(target_os = "macos")]
#[cfg(target_os = "macos")]
type WorkItem = (OwnedRequest, mpsc::Sender<Result<Value, String>>);
#[cfg(target_os = "macos")]
type WorkerSender = mpsc::Sender<WorkItem>;

#[cfg(target_os = "macos")]
static WORKER: OnceLock<WorkerSender> = OnceLock::new();

#[cfg(target_os = "macos")]
pub fn execute(request: Request<'_>) -> Result<Value, String> {
    let sender = WORKER.get_or_init(|| {
        let (requests, receiver) =
            mpsc::channel::<(OwnedRequest, mpsc::Sender<Result<Value, String>>)>();
        std::thread::Builder::new()
            .name("comptrol-macos-ax".to_owned())
            .spawn(move || {
                while let Ok((request, response)) = receiver.recv() {
                    let result = native::execute(request.as_request());
                    let _ = response.send(result);
                }
            })
            .expect("failed to start persistent macOS AX worker");
        requests
    });
    let (response, receiver) = mpsc::channel();
    sender
        .send((request.into(), response))
        .map_err(|_| "macOS AX worker stopped".to_owned())?;
    receiver
        .recv()
        .map_err(|_| "macOS AX worker stopped before responding".to_owned())?
}
