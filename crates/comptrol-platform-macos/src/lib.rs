//! Direct macOS Accessibility (AX) semantic adapter.
//!
//! This is the macOS leg of the three-OS semantic-first guarantee documented
//! in `comptrol-platform-windows::uia`: controls are addressed by accessible
//! identity (name / AXIdentifier / role) inside one exact process, actions run
//! through the element's own advertised AX actions (never synthesized pointer
//! input), and every result distinguishes dispatch from independent
//! verification. The parity contract with the Windows UIA route is exactly:
//!
//! * `Action::Inspect` - bounded control observation;
//! * `Action::Press` - role-aware semantic action order (checkbox toggles via
//!   press, combo boxes expand, list items select, everything else invokes),
//!   performed only through actions the element advertises;
//! * `Action::SetValue` ("fill") - the AX value setter;
//! * [`send_key_sequence`] ("dispatch") - a closed set of keyboard tokens to
//!   the frontmost target application only;
//! * `match_index` + `expected_match_count` ordinal disambiguation with the
//!   same `target_set_changed` discipline as Windows;
//! * postcondition verification (`name`, `value`, `enabled`, `selected`,
//!   `window_title`) read back from the accessibility tree after the action.
//!
//! There is deliberately no physical-click fallback on this route: the direct
//! AX adapter is semantic-only, so `mouse` is always `"untouched"` here.

#![deny(unsafe_op_in_unsafe_fn)]
// The semantic helpers are shared with the macOS implementation but also
// compiled on other hosts so their unit tests and API stay portable.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use serde_json::Value;
#[cfg(target_os = "macos")]
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Inspect,
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
    /// Select an ordinal among identical controls only when the observed
    /// candidate count is supplied and unchanged.
    pub match_index: Option<usize>,
    pub expected_match_count: Option<usize>,
    pub max_nodes: usize,
    /// API parity with the Windows route; refused honestly on macOS because
    /// this adapter never synthesizes pointer input.
    pub allow_physical_click: bool,
    pub timeout: Duration,
}

#[cfg(not(target_os = "macos"))]
pub fn execute(_request: Request<'_>) -> Result<Value, String> {
    Err("macOS AX adapter is only available on macOS".to_owned())
}

#[cfg(not(target_os = "macos"))]
pub fn send_key_sequence(_process_id: u32, _keys: &[String]) -> Result<Value, String> {
    Err("macOS key dispatch is only available on macOS".to_owned())
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

/// Platform-neutral targeting semantics shared with (and unit-tested like)
/// the Windows UIA and Linux AT-SPI routes. Everything here is pure logic, so
/// it is compiled and tested on every build host even though the AX call
/// surface itself is macOS-only.
pub(crate) mod semantics {
    /// AX action names, most preferred first for the control's role. The
    /// order mirrors the Windows pattern order (Toggle -> ExpandCollapse ->
    /// Invoke -> SelectionItem) expressed as macOS action names.
    pub fn press_preference(role: Option<&str>) -> &'static [&'static str] {
        match role.map(str::to_ascii_lowercase).as_deref() {
            Some("checkbox") => &["AXPress"],
            Some("combobox") => &["AXShowMenu", "AXPress"],
            Some("listitem") | Some("option") => &["AXPick", "AXPress", "AXConfirm"],
            _ => &["AXPress", "AXConfirm", "AXPick", "AXShowMenu"],
        }
    }

    /// Role matching accepts the platform-neutral names used by intent
    /// schemas ("button") and the raw AX names ("AXButton") interchangeably.
    pub fn role_matches(candidate: &str, expected: &str) -> bool {
        let expected = expected.to_ascii_lowercase();
        let candidate = candidate.to_ascii_lowercase();
        if expected.starts_with("ax") {
            return candidate == expected;
        }
        let accepted: &[&str] = match expected.as_str() {
            "button" => &["axbutton"],
            "checkbox" => &["axcheckbox"],
            "combobox" => &["axcombobox"],
            "listitem" | "option" => &["axrow", "axcell", "axlist"],
            "edit" | "textfield" => &["axtextfield", "axtextarea"],
            "hyperlink" => &["axlink"],
            "text" => &["axstatictext"],
            "window" => &["axwindow"],
            _ => return false,
        };
        accepted.contains(&candidate.as_str())
    }

    /// Ordinal targeting: an ordinal is only meaningful against an observed
    /// candidate count that has not changed since it was supplied.
    pub fn checked_match_index(
        actual_count: usize,
        index: usize,
        expected_count: Option<usize>,
    ) -> Result<usize, &'static str> {
        let Some(expected_count) = expected_count else {
            return Err("indexed targeting needs expected_match_count");
        };
        if actual_count != expected_count || index >= actual_count {
            return Err("target_set_changed");
        }
        Ok(index)
    }

    /// One closed-set keyboard token as macOS virtual key events
    /// `(key_code, key_up)`. The token set is identical to the Windows route;
    /// "equals" is treated as evaluate (Return), matching Windows Calculator
    /// semantics, so recipes behave the same on every OS.
    pub fn key_events(key: &str) -> Result<KeyPlan, String> {
        let normalized = key.to_ascii_lowercase();
        if let Some(chord) = normalized.strip_prefix("ctrl+").map(str::to_owned) {
            let code = match chord.as_str() {
                "l" => 37_u16,
                "f" => 3,
                _ => return Err(format!("unsupported key token: {key}")),
            };
            return Ok(KeyPlan {
                events: vec![(59, false), (code, false), (code, true), (59, true)],
                control_chord: true,
            });
        }
        let code = match normalized.as_str() {
            "multiply" => 67,
            "equals" | "enter" => 36,
            "end" => 119,
            "home" => 115,
            "pagedown" => 121,
            "pageup" => 116,
            "arrowdown" => 125,
            "arrowup" => 126,
            "arrowleft" => 123,
            "arrowright" => 124,
            "tab" => 48,
            "escape" => 53,
            "backspace" => 51,
            "space" => 49,
            _ => {
                if let Some(digit) = normalized
                    .strip_prefix("digit")
                    .and_then(|digit| digit.parse::<u16>().ok())
                    .filter(|digit| *digit <= 9)
                {
                    match digit {
                        0 => 29,
                        1 => 18,
                        2 => 19,
                        3 => 20,
                        4 => 21,
                        5 => 23,
                        6 => 22,
                        7 => 26,
                        8 => 28,
                        _ => 25,
                    }
                } else {
                    return Err(format!("unsupported key token: {key}"));
                }
            }
        };
        Ok(KeyPlan {
            events: vec![(code, false), (code, true)],
            control_chord: false,
        })
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct KeyPlan {
        /// `(macOS virtual key code, key_up)` in dispatch order.
        pub events: Vec<(u16, bool)>,
        /// Whether the events carry the Control modifier (posted with the
        /// control flag set on every event of the chord).
        pub control_chord: bool,
    }

    #[cfg(test)]
    mod tests {
        use super::{checked_match_index, key_events, press_preference, role_matches};

        #[test]
        fn index_targeting_requires_an_observed_count() {
            assert!(checked_match_index(3, 1, None).is_err());
            assert!(checked_match_index(3, 1, Some(3)).is_ok());
            assert!(checked_match_index(3, 1, Some(2)).is_err());
            assert!(checked_match_index(3, 3, Some(3)).is_err());
        }

        #[test]
        fn roles_match_platform_and_generic_names() {
            assert!(role_matches("AXButton", "button"));
            assert!(role_matches("AXButton", "AXButton"));
            assert!(role_matches("axbutton", "AXButton"));
            assert!(role_matches("AXCheckBox", "checkbox"));
            assert!(!role_matches("AXButton", "checkbox"));
            assert!(!role_matches("AXButton", "not-a-role"));
        }

        #[test]
        fn press_preference_is_role_aware_and_ordered() {
            assert_eq!(press_preference(Some("checkbox")), &["AXPress"]);
            assert_eq!(
                press_preference(Some("ComboBox")),
                &["AXShowMenu", "AXPress"]
            );
            assert_eq!(
                press_preference(None),
                &["AXPress", "AXConfirm", "AXPick", "AXShowMenu"]
            );
        }

        #[test]
        fn key_tokens_map_to_macos_virtual_keys() {
            assert_eq!(
                key_events("ENTER").unwrap().events,
                vec![(36, false), (36, true)]
            );
            assert_eq!(
                key_events("escape").unwrap().events,
                vec![(53, false), (53, true)]
            );
            assert_eq!(
                key_events("digit0").unwrap().events,
                vec![(29, false), (29, true)]
            );
            assert_eq!(
                key_events("digit9").unwrap().events,
                vec![(25, false), (25, true)]
            );
        }

        #[test]
        fn control_chords_expand_to_the_full_event_plan() {
            let plan = key_events("ctrl+l").unwrap();
            assert!(plan.control_chord);
            assert_eq!(
                plan.events,
                vec![(59, false), (37, false), (37, true), (59, true)]
            );
            assert!(key_events("ctrl+x").is_err());
            assert!(key_events("hyper").is_err());
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::{Action, Request, semantics};
    use serde_json::{Value, json};
    use std::ffi::c_void;
    use std::ptr;
    use std::time::{Duration, Instant};

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
    type CGEventRef = *mut c_void;

    const K_CF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;
    const K_AX_ERROR_SUCCESS: AXError = 0;
    const K_AX_ERROR_CANNOT_COMPLETE: AXError = -25204;
    const K_AX_ERROR_NOT_IMPLEMENTED: AXError = -25205;
    const K_AX_ERROR_INVALID_UI_ELEMENT: AXError = -25206;
    const K_AX_ERROR_ILLEGAL_ARGUMENT: AXError = -25207;
    const K_AX_ERROR_ACTION_UNSUPPORTED: AXError = -25208;
    const K_AX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
    const K_CG_HID_EVENT_TAP: u32 = 0;
    const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
    const POST_ACTION_VERIFY_TIMEOUT: Duration = Duration::from_millis(750);
    const POST_ACTION_VERIFY_INTERVAL: Duration = Duration::from_millis(25);

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> Boolean;
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AXError;
        fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFArrayRef) -> AXError;
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
        fn CFBooleanGetTypeID() -> CFTypeID;
        fn CFBooleanGetValue(value: CFTypeRef) -> Boolean;
        fn CFGetTypeID(value: CFTypeRef) -> CFTypeID;
        fn CFRelease(value: CFTypeRef);
        fn CFRetain(value: CFTypeRef) -> CFTypeRef;
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

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreateKeyboardEvent(
            source: *const c_void,
            virtual_key: u16,
            key_down: bool,
        ) -> CGEventRef;
        fn CGEventSetFlags(event: CGEventRef, flags: u64);
        fn CGEventPost(tap: u32, event: CGEventRef);
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
        if request.action == Action::Inspect {
            return inspect(application, &request, started);
        }
        let mut matches = find_matches(application, &request, 2048)?;
        if let Some(index) = request.match_index {
            semantics::checked_match_index(matches.len(), index, request.expected_match_count)
                .map_err(str::to_owned)?;
            let element = matches.swap_remove(index);
            matches.clear();
            matches.push(element);
        } else if request.expected_match_count.is_some() {
            return Err("expected_match_count requires match_index".to_owned());
        }
        let Some(target) = matches.pop() else {
            return Err("target_missing".to_owned());
        };
        if !matches.is_empty() {
            return Err("target_ambiguous".to_owned());
        }
        if !read_bool_attribute(target.0, "AXEnabled")?.unwrap_or(true) {
            return Err("target_disabled".to_owned());
        }
        let performed = match request.action {
            Action::Inspect => unreachable!("inspect returned before action dispatch"),
            Action::Press => press(target.0, &request)?,
            Action::SetValue => {
                let value = request.value.ok_or("value_required")?;
                set_value(target.0, value)?
            }
        };
        let verified = verify_after_action(target.0, &request)?;
        Ok(json!({
            "verified": verified,
            "route": "macos_ax_direct",
            "process_id": request.process_id,
            "candidate_count": 1,
            "bounded_nodes": 2048,
            "action": performed,
            "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
            "mouse": "untouched",
            "clipboard": "untouched"
        }))
    }

    /// Deliver a closed set of keyboard tokens to the frontmost target
    /// application only. Re-checked immediately before dispatch so a target
    /// that lost focus never receives input.
    pub(super) fn send_key_sequence(process_id: u32, keys: &[String]) -> Result<Value, String> {
        if !is_process_trusted() {
            return Err("accessibility_permission_required".to_owned());
        }
        if keys.is_empty() || keys.len() > 64 {
            return Err("key_sequence requires 1 to 64 keys".to_owned());
        }
        let application = unsafe { AXUIElementCreateApplication(process_id as i32) };
        if application.is_null() {
            return Err("application_unavailable".to_owned());
        }
        let _application_guard = Release(application);
        if !frontmost(application)? {
            return Err("key_sequence target application is not foreground".to_owned());
        }
        let mut plans = Vec::new();
        for key in keys {
            plans.push(semantics::key_events(key)?);
        }
        if !frontmost(application)? {
            return Err("key_sequence target surface changed before input dispatch".to_owned());
        }
        let mut delivered = 0usize;
        for plan in &plans {
            for (code, key_up) in &plan.events {
                let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), *code, !key_up) };
                if event.is_null() {
                    return Err("keyboard event allocation failed".to_owned());
                }
                if plan.control_chord {
                    unsafe { CGEventSetFlags(event, K_CG_EVENT_FLAG_MASK_CONTROL) };
                }
                unsafe { CGEventPost(K_CG_HID_EVENT_TAP, event) };
                unsafe { CFRelease(event) };
                delivered += 1;
            }
        }
        Ok(json!({
            "verified": false,
            "route": "macos_ax_key_sequence",
            "process_id": process_id,
            "keys": keys,
            "delivered_events": delivered,
            "mouse": "untouched",
            "clipboard": "untouched"
        }))
    }

    fn frontmost(application: AXUIElementRef) -> Result<bool, String> {
        let frontmost = read_bool_attribute(application, "AXFrontmost")?.unwrap_or(false);
        let focused = read_raw_attribute(application, "AXFocusedWindow")?
            .map(|value| {
                unsafe { CFRelease(value) };
                true
            })
            .unwrap_or(false);
        Ok(frontmost && focused)
    }

    fn inspect(
        application: AXUIElementRef,
        request: &Request<'_>,
        started: Instant,
    ) -> Result<Value, String> {
        let children_attribute = CfString::new("AXChildren")?;
        let mut queue = vec![Retain(application)];
        let mut controls = Vec::new();
        let mut candidate_count = 0usize;
        while let Some(node) = queue.pop() {
            candidate_count += 1;
            let actions = action_names(node.0)?;
            let enabled = read_bool_attribute(node.0, "AXEnabled")?.unwrap_or(true);
            let actionable = enabled && !actions.is_empty();
            controls.push(json!({
                "name": read_string_attribute(node.0, "AXTitle")?
                    .or(read_string_attribute(node.0, "AXDescription")?),
                "automation_id": read_string_attribute(node.0, "AXIdentifier")?,
                "role": read_string_attribute(node.0, "AXRole")?,
                "enabled": enabled,
                "supported_actions": actions,
                "actionable": actionable,
                "actionability_reason": if actionable { "semantic_action" }
                    else if !enabled { "disabled" } else { "no_supported_semantic_action" },
            }));
            if controls.len() >= request.max_nodes || candidate_count >= 2048 {
                break;
            }
            let mut children: CFTypeRef = ptr::null();
            let error = unsafe {
                AXUIElementCopyAttributeValue(node.0, children_attribute.as_ref(), &mut children)
            };
            if error == K_AX_ERROR_ATTRIBUTE_UNSUPPORTED || children.is_null() {
                continue;
            }
            check_ax(error, "read AX children")?;
            let _children_guard = Release(children);
            let array = children as CFArrayRef;
            let count = unsafe { CFArrayGetCount(array) }.max(0) as usize;
            for index in (0..count.min(2048 - candidate_count)).rev() {
                let child = unsafe { CFArrayGetValueAtIndex(array, index as CFIndex) };
                if !child.is_null() {
                    queue.push(Retain::new(child));
                }
            }
        }
        let observation_complete = !controls.is_empty();
        let control_count = controls.len();
        Ok(json!({
            "verified": observation_complete,
            "observation_complete": observation_complete,
            "observation_status": if observation_complete { "controls_found" } else { "no_controls_found" },
            "route": "macos_ax_inspect",
            "process_id": request.process_id,
            "controls": controls,
            "control_count": control_count,
            "candidate_count": candidate_count,
            "bounded_nodes": candidate_count,
            "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
            "mouse": "untouched",
            "clipboard": "untouched"
        }))
    }

    /// Collect every node matching the supplied selectors (each selector
    /// constrains; unsupplied selectors are unconstrained), bounded like the
    /// Windows route. Ambiguity is decided by the caller so ordinal
    /// disambiguation can apply first.
    fn find_matches(
        application: AXUIElementRef,
        request: &Request<'_>,
        limit: usize,
    ) -> Result<Vec<Retain>, String> {
        if request.name.is_none() && request.automation_id.is_none() && request.role.is_none() {
            return Err("target_unspecified: needs name, automation_id, or role".to_owned());
        }
        let children_attribute = CfString::new("AXChildren")?;
        let mut queue = vec![Retain::new(application)];
        let mut matches = Vec::new();
        let mut visited = 0usize;
        while let Some(node) = queue.pop() {
            visited += 1;
            if visited > limit {
                break;
            }
            if matches_node(node.0, request)? {
                matches.push(node);
                continue;
            }
            let mut children: CFTypeRef = ptr::null();
            let error = unsafe {
                AXUIElementCopyAttributeValue(node.0, children_attribute.as_ref(), &mut children)
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
                    queue.push(Retain::new(child));
                }
            }
        }
        Ok(matches)
    }

    fn matches_node(node: AXUIElementRef, request: &Request<'_>) -> Result<bool, String> {
        if let Some(name) = request.name {
            let title = read_string_attribute(node, "AXTitle")?
                .or(read_string_attribute(node, "AXDescription")?);
            if title.as_deref() != Some(name) {
                return Ok(false);
            }
        }
        if let Some(automation_id) = request.automation_id
            && read_string_attribute(node, "AXIdentifier")?.as_deref() != Some(automation_id)
        {
            return Ok(false);
        }
        if let Some(role) = request.role {
            let Some(actual) = read_string_attribute(node, "AXRole")? else {
                return Ok(false);
            };
            if !semantics::role_matches(&actual, role) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Semantic-first press: perform the role's most preferred action among
    /// those the element advertises. A sole advertised action counts as the
    /// element's own semantic action; anything else is honestly unactionable
    /// rather than reaching for the pointer.
    fn press(target: AXUIElementRef, request: &Request<'_>) -> Result<String, String> {
        let advertised = action_names(target)?;
        for preferred in semantics::press_preference(request.role) {
            if advertised.iter().any(|action| action == preferred) {
                perform_action(target, preferred)?;
                return Ok((*preferred).to_owned());
            }
        }
        if advertised.len() == 1 {
            perform_action(target, &advertised[0])?;
            return Ok(advertised[0].clone());
        }
        Err(format!(
            "target_not_actionable: no preferred semantic action advertised (advertised: {advertised:?})"
        ))
    }

    fn set_value(target: AXUIElementRef, value: &str) -> Result<String, String> {
        let cf_value = CfString::new(value)?;
        let attribute = CfString::new("AXValue")?;
        let mut settable = 0;
        check_ax(
            unsafe { AXUIElementIsAttributeSettable(target, attribute.as_ref(), &mut settable) },
            "check AX value settable",
        )?;
        if settable == 0 {
            return Err("value_not_settable".to_owned());
        }
        check_ax(
            unsafe { AXUIElementSetAttributeValue(target, attribute.as_ref(), cf_value.as_ref()) },
            "set AX value",
        )?;
        Ok("AXValue".to_owned())
    }

    fn perform_action(target: AXUIElementRef, action: &str) -> Result<(), String> {
        let cf_action = CfString::new(action)?;
        check_ax(
            unsafe { AXUIElementPerformAction(target, cf_action.as_ref()) },
            &format!("perform {action}"),
        )
    }

    fn action_names(node: AXUIElementRef) -> Result<Vec<String>, String> {
        let mut names: CFArrayRef = ptr::null();
        let error = unsafe { AXUIElementCopyActionNames(node, &mut names) };
        if error == K_AX_ERROR_ATTRIBUTE_UNSUPPORTED || names.is_null() {
            return Ok(Vec::new());
        }
        check_ax(error, "read AX action names")?;
        let _names_guard = Release(names);
        let count = unsafe { CFArrayGetCount(names) }.max(0) as usize;
        let mut out = Vec::new();
        for index in 0..count {
            let value = unsafe { CFArrayGetValueAtIndex(names, index as CFIndex) };
            if value.is_null() {
                continue;
            }
            if let Some(text) = cf_string_to_rust(value as CFStringRef) {
                out.push(text);
            }
        }
        Ok(out)
    }

    fn verify_after_action(target: AXUIElementRef, request: &Request<'_>) -> Result<bool, String> {
        let poll = request.action == Action::Press
            && matches!(
                request.expected_attribute,
                Some("name") | Some("window_title")
            );
        if !poll {
            return verify(target, request);
        }
        let deadline = Instant::now() + POST_ACTION_VERIFY_TIMEOUT;
        loop {
            if verify(target, request)? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(POST_ACTION_VERIFY_INTERVAL);
        }
    }

    fn verify(node: AXUIElementRef, request: &Request<'_>) -> Result<bool, String> {
        match request.expected_attribute {
            None => Ok(false),
            Some("name") => Ok(read_string_attribute(node, "AXTitle")?
                .or(read_string_attribute(node, "AXDescription")?)
                .as_deref()
                == request.expected_value),
            Some("value") => {
                Ok(read_string_attribute(node, "AXValue")?.as_deref() == request.expected_value)
            }
            Some("enabled") => Ok(read_bool_attribute(node, "AXEnabled")?.unwrap_or(false)
                == (request.expected_value == Some("true"))),
            Some("selected") => Ok(read_bool_attribute(node, "AXSelected")?.unwrap_or(false)
                == (request.expected_value == Some("true"))),
            Some("window_title") => {
                let Some(window) = read_raw_attribute(node, "AXWindow")? else {
                    return Ok(false);
                };
                let _window_guard = Release(window);
                Ok(
                    read_string_attribute(window as AXUIElementRef, "AXTitle")?.as_deref()
                        == request.expected_value,
                )
            }
            Some(_) => Err("unsupported_verification_attribute".to_owned()),
        }
    }

    fn read_raw_attribute(node: AXUIElementRef, name: &str) -> Result<Option<CFTypeRef>, String> {
        let attribute = CfString::new(name)?;
        let mut value: CFTypeRef = ptr::null();
        let error = unsafe { AXUIElementCopyAttributeValue(node, attribute.as_ref(), &mut value) };
        if error == K_AX_ERROR_ATTRIBUTE_UNSUPPORTED || value.is_null() {
            return Ok(None);
        }
        check_ax(error, "read AX attribute")?;
        Ok(Some(value))
    }

    fn read_bool_attribute(node: AXUIElementRef, name: &str) -> Result<Option<bool>, String> {
        let Some(value) = read_raw_attribute(node, name)? else {
            return Ok(None);
        };
        let _guard = Release(value);
        Ok(Some(
            unsafe { CFGetTypeID(value) } == unsafe { CFBooleanGetTypeID() }
                && unsafe { CFBooleanGetValue(value) } != 0,
        ))
    }

    fn read_string_attribute(node: AXUIElementRef, name: &str) -> Result<Option<String>, String> {
        let Some(value) = read_raw_attribute(node, name)? else {
            return Ok(None);
        };
        let _guard = Release(value);
        Ok(cf_string_to_rust(value as CFStringRef))
    }

    fn cf_string_to_rust(value: CFStringRef) -> Option<String> {
        if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            return None;
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
            return None;
        }
        let bytes = buffer
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect::<Vec<_>>();
        String::from_utf8(bytes).ok()
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

    /// A retained element reference. `CFArrayGetValueAtIndex` hands out
    /// borrowed pointers; queued nodes outlive the children array they came
    /// from, so every queued element is retained and released here.
    struct Retain(CFTypeRef);

    impl Retain {
        fn new(value: CFTypeRef) -> Self {
            Self(unsafe { CFRetain(value) })
        }
    }

    impl Drop for Retain {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }
}

#[cfg(target_os = "macos")]
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
    match_index: Option<usize>,
    expected_match_count: Option<usize>,
    max_nodes: usize,
    allow_physical_click: bool,
    timeout: Duration,
}

#[cfg(target_os = "macos")]
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
            match_index: request.match_index,
            expected_match_count: request.expected_match_count,
            max_nodes: request.max_nodes,
            allow_physical_click: request.allow_physical_click,
            timeout: request.timeout,
        }
    }
}

#[cfg(target_os = "macos")]
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
            match_index: self.match_index,
            expected_match_count: self.expected_match_count,
            max_nodes: self.max_nodes,
            allow_physical_click: self.allow_physical_click,
            timeout: self.timeout,
        }
    }
}

#[cfg(target_os = "macos")]
enum Job {
    Action(OwnedRequest),
    Keys { process_id: u32, keys: Vec<String> },
}

#[cfg(target_os = "macos")]
type WorkItem = (Job, mpsc::Sender<Result<Value, String>>);
#[cfg(target_os = "macos")]
type WorkerSender = mpsc::Sender<WorkItem>;

#[cfg(target_os = "macos")]
static WORKER: OnceLock<WorkerSender> = OnceLock::new();

#[cfg(target_os = "macos")]
pub fn execute(request: Request<'_>) -> Result<Value, String> {
    let sender = WORKER.get_or_init(|| {
        let (requests, receiver) = mpsc::channel::<WorkItem>();
        std::thread::Builder::new()
            .name("comptrol-macos-ax".to_owned())
            .spawn(move || {
                while let Ok((job, response)) = receiver.recv() {
                    let result = match job {
                        Job::Action(request) => native::execute(request.as_request()),
                        Job::Keys { process_id, keys } => {
                            native::send_key_sequence(process_id, &keys)
                        }
                    };
                    let _ = response.send(result);
                }
            })
            .expect("failed to start persistent macOS AX worker");
        requests
    });
    let (response, receiver) = mpsc::channel();
    sender
        .send((Job::Action(request.into()), response))
        .map_err(|_| "macOS AX worker stopped".to_owned())?;
    receiver
        .recv()
        .map_err(|_| "macOS AX worker stopped before responding".to_owned())?
}

#[cfg(target_os = "macos")]
pub fn send_key_sequence(process_id: u32, keys: &[String]) -> Result<Value, String> {
    let sender = WORKER.get_or_init(|| {
        let (requests, receiver) = mpsc::channel::<WorkItem>();
        std::thread::Builder::new()
            .name("comptrol-macos-ax".to_owned())
            .spawn(move || {
                while let Ok((job, response)) = receiver.recv() {
                    let result = match job {
                        Job::Action(request) => native::execute(request.as_request()),
                        Job::Keys { process_id, keys } => {
                            native::send_key_sequence(process_id, &keys)
                        }
                    };
                    let _ = response.send(result);
                }
            })
            .expect("failed to start persistent macOS AX worker");
        requests
    });
    // Dispatch shares the AX worker's serialization so a key stream can never
    // interleave with a semantic action under concurrent agents.
    let (response, receiver) = mpsc::channel();
    sender
        .send((
            Job::Keys {
                process_id,
                keys: keys.to_vec(),
            },
            response,
        ))
        .map_err(|_| "macOS AX worker stopped".to_owned())?;
    receiver
        .recv()
        .map_err(|_| "macOS AX worker stopped before responding".to_owned())?
}

#[cfg(test)]
mod request_shape {
    //! Locks the `Request` construction shape used by the `#[cfg]`-gated core
    //! call sites (which are not compiled on every host). If the struct fields
    //! change, this test fails to compile here instead of failing on a target
    //! machine only.
    use super::{Action, Request};
    use std::time::Duration;

    #[test]
    fn request_accepts_the_full_core_field_set() {
        let name = String::from("Save");
        let request = Request {
            process_id: 4242,
            name: Some(name.as_str()),
            automation_id: None,
            role: Some("button"),
            action: Action::Press,
            value: None,
            expected_attribute: Some("name"),
            expected_value: Some("Saved"),
            match_index: Some(0),
            expected_match_count: Some(1),
            max_nodes: 128,
            allow_physical_click: false,
            timeout: Duration::from_millis(1500),
        };
        assert_eq!(request.process_id, 4242);
        assert_eq!(request.name, Some("Save"));
        assert_eq!(request.match_index, Some(0));
        assert_eq!(request.expected_match_count, Some(1));
        assert_eq!(request.max_nodes, 128);
        assert!(!request.allow_physical_click);
    }
}
