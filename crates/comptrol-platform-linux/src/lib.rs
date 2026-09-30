//! Direct Linux AT-SPI semantic adapter.
//!
//! This is the Linux leg of the three-OS semantic-first guarantee documented
//! in `comptrol-platform-windows::uia`: controls are addressed by accessible
//! identity (name / accessible-id / role) inside one exact process, actions run
//! through the element's own advertised ATK actions (never synthesized pointer
//! input), and every result distinguishes dispatch from independent
//! verification. The parity contract with the Windows UIA route is exactly:
//!
//! * `Action::Inspect` - bounded control observation;
//! * `Action::Press` - role-aware semantic action order (checkbox toggles via
//!   toggle, combo boxes expand, list items select, everything else presses),
//!   performed only through actions the element advertises;
//! * `Action::SetValue` ("fill") - the EditableText setter;
//! * [`send_key_sequence`] ("dispatch") - a closed set of keyboard tokens to
//!   the focused target application only, synthesized through the AT-SPI
//!   device event controller;
//! * `match_index` + `expected_match_count` ordinal disambiguation with the
//!   same `target_set_changed` discipline as Windows;
//! * postcondition verification (`name`, `value`, `enabled`, `selected`,
//!   `window_title`) read back from the accessibility tree after the action.
//!
//! There is deliberately no physical-click fallback on this route: the direct
//! AT-SPI adapter is semantic-only, so `mouse` is always `"untouched"` here.
//!
//! The AT-SPI surface compiles on every host (zbus is portable) so the API
//! usage is verified wherever the workspace builds; at runtime a host without
//! a session accessibility bus fails honestly with `AT-SPI connection
//! unavailable` rather than fabricating results.

#![deny(unsafe_code)]

use serde_json::Value;
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
    /// API parity with the Windows route; ignored here because this adapter
    /// never synthesizes pointer input (`mouse` is always `"untouched"`).
    pub allow_physical_click: bool,
    pub timeout: Duration,
}

/// Platform-neutral targeting semantics shared with (and unit-tested like)
/// the Windows UIA and macOS AX routes. Everything here is pure logic, so it
/// is compiled and tested on every build host even though the AT-SPI call
/// surface itself is only meaningful with a session accessibility bus.
pub(crate) mod semantics {
    /// ATK action names, most preferred first for the control's role. The
    /// order mirrors the Windows pattern order (Toggle -> ExpandCollapse ->
    /// Invoke -> SelectionItem) expressed as ATK action names.
    pub fn press_preference(role: Option<&str>) -> &'static [&'static str] {
        match role.map(str::to_ascii_lowercase).as_deref() {
            Some("checkbox") => &["Toggle", "Press", "Click", "Activate"],
            Some("combobox") => &["Expand", "Menu", "Press", "Activate"],
            Some("listitem") | Some("option") => &["Select", "Press", "Click", "Activate"],
            _ => &[
                "Press", "Click", "Activate", "Toggle", "Select", "Expand", "Menu",
            ],
        }
    }

    /// Role matching accepts the platform-neutral names used by intent
    /// schemas ("button") and the raw ATK role names ("push button")
    /// interchangeably; separators are insignificant.
    pub fn role_matches(candidate: &str, expected: &str) -> bool {
        let expected = normalize(expected);
        let candidate = normalize(candidate);
        if expected.is_empty() {
            return false;
        }
        let accepted: &[&str] = match expected.as_str() {
            "button" => &["pushbutton", "togglebutton", "button"],
            "checkbox" => &["checkbox"],
            "combobox" => &["combobox"],
            "listitem" | "option" => &["listitem", "tablecell", "row", "cell"],
            "edit" | "textfield" | "entry" => &["entry", "passwordtext", "text", "textarea"],
            "hyperlink" => &["link", "hyperlink"],
            "text" => &["label", "text", "static"],
            "window" => &["window", "frame", "dialog", "alert"],
            other => {
                return candidate == other;
            }
        };
        accepted.contains(&candidate.as_str())
    }

    fn normalize(value: &str) -> String {
        value
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
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

    /// AT-SPI key mask for the Control modifier (the `KEYMASK_CONTROL`
    /// constant device event controllers use).
    pub const KEYMASK_CONTROL: i32 = 1 << 2;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum KeyOp {
        /// Synthesize a full press-release of an X keysym.
        Sym(u32),
        /// Lock (hold) a modifier mask until unlocked.
        LockModifiers(i32),
        /// Unlock a previously locked modifier mask.
        UnlockModifiers(i32),
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct KeyPlan {
        /// Key synthesis operations in dispatch order.
        pub ops: Vec<KeyOp>,
        /// Whether the plan carries the Control modifier.
        pub control_chord: bool,
    }

    /// One closed-set keyboard token as AT-SPI key synthesis operations in
    /// dispatch order. The token set is identical to the Windows route;
    /// "equals" is treated as evaluate (Return), matching Windows Calculator
    /// semantics, so recipes behave the same on every OS.
    pub fn key_events(key: &str) -> Result<KeyPlan, String> {
        let normalized = key.to_ascii_lowercase();
        if let Some(chord) = normalized.strip_prefix("ctrl+") {
            let symbol = match chord {
                "l" => 0x6c_u32,
                "f" => 0x66,
                _ => return Err(format!("unsupported key token: {key}")),
            };
            return Ok(KeyPlan {
                ops: vec![
                    KeyOp::LockModifiers(KEYMASK_CONTROL),
                    KeyOp::Sym(symbol),
                    KeyOp::UnlockModifiers(KEYMASK_CONTROL),
                ],
                control_chord: true,
            });
        }
        let symbol = match normalized.as_str() {
            "multiply" => 0xffaa,         // KP_Multiply
            "equals" | "enter" => 0xff0d, // Return
            "end" => 0xff57,
            "home" => 0xff50,
            "pagedown" => 0xff56,
            "pageup" => 0xff55,
            "arrowdown" => 0xff54,
            "arrowup" => 0xff52,
            "arrowleft" => 0xff51,
            "arrowright" => 0xff53,
            "tab" => 0xff09,
            "escape" => 0xff1b,
            "backspace" => 0xff08,
            "space" => 0x20,
            _ => {
                if let Some(digit) = normalized
                    .strip_prefix("digit")
                    .and_then(|digit| digit.parse::<u32>().ok())
                    .filter(|digit| *digit <= 9)
                {
                    0x30 + digit
                } else {
                    return Err(format!("unsupported key token: {key}"));
                }
            }
        };
        Ok(KeyPlan {
            ops: vec![KeyOp::Sym(symbol)],
            control_chord: false,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::{KeyOp, checked_match_index, key_events, press_preference, role_matches};

        #[test]
        fn index_targeting_requires_an_observed_count() {
            assert!(checked_match_index(3, 1, None).is_err());
            assert!(checked_match_index(3, 1, Some(3)).is_ok());
            assert!(checked_match_index(3, 1, Some(2)).is_err());
            assert!(checked_match_index(3, 3, Some(3)).is_err());
        }

        #[test]
        fn roles_match_platform_and_generic_names() {
            assert!(role_matches("push button", "button"));
            assert!(role_matches("PushButton", "button"));
            assert!(role_matches("check box", "checkbox"));
            assert!(role_matches("check box", "check box"));
            assert!(!role_matches("push button", "checkbox"));
            assert!(!role_matches("push button", "not-a-role"));
        }

        #[test]
        fn press_preference_is_role_aware_and_ordered() {
            assert_eq!(
                press_preference(Some("checkbox")),
                &["Toggle", "Press", "Click", "Activate"]
            );
            assert_eq!(
                press_preference(Some("ComboBox")),
                &["Expand", "Menu", "Press", "Activate"]
            );
            assert_eq!(
                press_preference(None),
                &[
                    "Press", "Click", "Activate", "Toggle", "Select", "Expand", "Menu"
                ]
            );
        }

        #[test]
        fn key_tokens_map_to_x_keysyms() {
            assert_eq!(key_events("ENTER").unwrap().ops, vec![KeyOp::Sym(0xff0d)]);
            assert_eq!(key_events("escape").unwrap().ops, vec![KeyOp::Sym(0xff1b)]);
            assert_eq!(key_events("digit0").unwrap().ops, vec![KeyOp::Sym(0x30)]);
            assert_eq!(key_events("digit9").unwrap().ops, vec![KeyOp::Sym(0x39)]);
        }

        #[test]
        fn control_chords_expand_to_the_full_operation_plan() {
            let plan = key_events("ctrl+l").unwrap();
            assert!(plan.control_chord);
            assert_eq!(
                plan.ops,
                vec![
                    KeyOp::LockModifiers(super::KEYMASK_CONTROL),
                    KeyOp::Sym(0x6c),
                    KeyOp::UnlockModifiers(super::KEYMASK_CONTROL),
                ]
            );
            assert!(key_events("ctrl+x").is_err());
            assert!(key_events("hyper").is_err());
        }
    }
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
    match_index: Option<usize>,
    expected_match_count: Option<usize>,
    max_nodes: usize,
    allow_physical_click: bool,
    timeout: Duration,
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
            match_index: request.match_index,
            expected_match_count: request.expected_match_count,
            max_nodes: request.max_nodes,
            allow_physical_click: request.allow_physical_click,
            timeout: request.timeout,
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
            match_index: self.match_index,
            expected_match_count: self.expected_match_count,
            max_nodes: self.max_nodes,
            allow_physical_click: self.allow_physical_click,
            timeout: self.timeout,
        }
    }
}

enum Job {
    Request(OwnedRequest),
    Keys { process_id: u32, keys: Vec<String> },
}

type WorkItem = (Job, mpsc::Sender<Result<Value, String>>);
type WorkerSender = mpsc::Sender<WorkItem>;

const KEY_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(2_000);

static WORKER: OnceLock<WorkerSender> = OnceLock::new();

fn worker() -> &'static WorkerSender {
    WORKER.get_or_init(|| {
        let (requests, receiver) = mpsc::channel::<WorkItem>();
        std::thread::Builder::new()
            .name("comptrol-linux-atspi".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let connection = match &runtime {
                    Ok(runtime) => {
                        runtime.block_on(async { atspi::AccessibilityConnection::new().await.ok() })
                    }
                    Err(_) => None,
                };
                while let Ok((job, response)) = receiver.recv() {
                    let result = match (&runtime, &connection) {
                        (Ok(runtime), Some(connection)) => runtime.block_on(async {
                            match job {
                                Job::Request(request) => {
                                    let timeout = request.timeout;
                                    tokio::time::timeout(
                                        timeout,
                                        native::execute_async(connection, request.as_request()),
                                    )
                                    .await
                                    .map_err(|_| "AT-SPI operation timed out".to_owned())?
                                }
                                Job::Keys { process_id, keys } => tokio::time::timeout(
                                    KEY_SEQUENCE_TIMEOUT,
                                    native::send_key_sequence_async(connection, process_id, &keys),
                                )
                                .await
                                .map_err(|_| "AT-SPI operation timed out".to_owned())?,
                            }
                        }),
                        (_, None) => Err("AT-SPI connection unavailable".to_owned()),
                        (Err(error), _) => Err(format!("AT-SPI runtime unavailable: {error}")),
                    };
                    let _ = response.send(result);
                }
            })
            .expect("failed to start persistent Linux AT-SPI worker");
        requests
    })
}

pub fn execute(request: Request<'_>) -> Result<Value, String> {
    let (response, receiver) = mpsc::channel();
    worker()
        .send((Job::Request(request.into()), response))
        .map_err(|_| "Linux AT-SPI worker stopped".to_owned())?;
    receiver
        .recv()
        .map_err(|_| "Linux AT-SPI worker stopped before responding".to_owned())?
}

/// Deliver a closed set of keyboard tokens to the focused target application
/// only. Re-checked immediately before dispatch so a target that lost focus
/// never receives input.
pub fn send_key_sequence(process_id: u32, keys: &[String]) -> Result<Value, String> {
    let (response, receiver) = mpsc::channel();
    worker()
        .send((
            Job::Keys {
                process_id,
                keys: keys.to_vec(),
            },
            response,
        ))
        .map_err(|_| "Linux AT-SPI worker stopped".to_owned())?;
    receiver
        .recv()
        .map_err(|_| "Linux AT-SPI worker stopped before responding".to_owned())?
}

mod native {
    use super::{Action, Request, semantics};
    use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
    use atspi::proxy::device_event_controller::{DeviceEventControllerProxy, KeySynthType};
    use atspi::proxy::proxy_ext::ProxyExt;
    use atspi::{AccessibilityConnection, State, zbus};
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};

    const MAX_TRAVERSAL: usize = 2048;
    const POST_ACTION_VERIFY_TIMEOUT: Duration = Duration::from_millis(750);
    const POST_ACTION_VERIFY_INTERVAL: Duration = Duration::from_millis(25);
    const WINDOW_TITLE_WALK_LIMIT: usize = 16;

    pub(super) async fn execute_async(
        connection: &AccessibilityConnection,
        request: Request<'_>,
    ) -> Result<Value, String> {
        let started = Instant::now();
        let application = find_application(connection, request.process_id).await?;
        if request.action == Action::Inspect {
            return inspect(connection, &application, &request, started).await;
        }
        let mut matches = find_matches(connection, &application, &request, MAX_TRAVERSAL).await?;
        if let Some(index) = request.match_index {
            let index =
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
        if !enabled(&target).await? {
            return Err("target_disabled".to_owned());
        }
        let performed = match request.action {
            Action::Inspect => unreachable!("inspect returned before action dispatch"),
            Action::Press => press(&target, &request).await?,
            Action::SetValue => {
                let value = request.value.ok_or("value_required")?;
                set_value(&target, value).await?
            }
        };
        let verified = verify_after_action(connection, &target, &request).await?;
        Ok(json!({
            "verified": verified,
            "route": "linux_atspi_direct",
            "process_id": request.process_id,
            "candidate_count": 1,
            "bounded_nodes": MAX_TRAVERSAL,
            "action": performed,
            "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
            "mouse": "untouched",
            "clipboard": "untouched"
        }))
    }

    pub(super) async fn send_key_sequence_async(
        connection: &AccessibilityConnection,
        process_id: u32,
        keys: &[String],
    ) -> Result<Value, String> {
        if keys.is_empty() || keys.len() > 64 {
            return Err("key_sequence requires 1 to 64 keys".to_owned());
        }
        let application = find_application(connection, process_id).await?;
        let mut plans = Vec::new();
        for key in keys {
            plans.push(semantics::key_events(key)?);
        }
        if !application_foreground(connection, &application).await? {
            return Err("key_sequence target application is not foreground".to_owned());
        }
        let controller = DeviceEventControllerProxy::new(connection.connection())
            .await
            .map_err(|error| format!("AT-SPI device event controller unavailable: {error}"))?;
        let mut delivered = 0usize;
        for plan in &plans {
            for operation in &plan.ops {
                let (keycode, synth) = match operation {
                    semantics::KeyOp::Sym(symbol) => (*symbol as i32, KeySynthType::Sym),
                    semantics::KeyOp::LockModifiers(mask) => (*mask, KeySynthType::Lockmodifiers),
                    semantics::KeyOp::UnlockModifiers(mask) => {
                        (*mask, KeySynthType::Unlockmodifiers)
                    }
                };
                controller
                    .generate_keyboard_event(keycode, "", synth)
                    .await
                    .map_err(|error| format!("AT-SPI key synthesis failed: {error}"))?;
                delivered += 1;
            }
        }
        Ok(json!({
            "verified": false,
            "route": "linux_atspi_key_sequence",
            "process_id": process_id,
            "keys": keys,
            "delivered_events": delivered,
            "mouse": "untouched",
            "clipboard": "untouched"
        }))
    }

    async fn inspect(
        connection: &AccessibilityConnection,
        application: &AccessibleProxy<'_>,
        request: &Request<'_>,
        started: Instant,
    ) -> Result<Value, String> {
        let mut queue = vec![application.clone()];
        let mut controls = Vec::new();
        let mut candidate_count = 0usize;
        while let Some(node) = queue.pop() {
            candidate_count += 1;
            let advertised = advertised_actions(&node).await;
            let node_enabled = enabled(&node).await.unwrap_or(true);
            let actionable = node_enabled && !advertised.is_empty();
            controls.push(json!({
                "name": node.name().await.ok().filter(|value| !value.is_empty()),
                "automation_id": node.accessible_id().await.ok().filter(|value| !value.is_empty()),
                "role": node.get_role_name().await.ok(),
                "enabled": node_enabled,
                "supported_actions": advertised,
                "actionable": actionable,
                "actionability_reason": if actionable { "semantic_action" }
                    else if !node_enabled { "disabled" } else { "no_supported_semantic_action" },
            }));
            if controls.len() >= request.max_nodes || candidate_count >= MAX_TRAVERSAL {
                break;
            }
            let Ok(children) = node.get_children().await else {
                continue;
            };
            for child in children.into_iter().rev() {
                if child.is_null() {
                    continue;
                }
                let Ok(proxy) = child.into_accessible_proxy(connection.connection()).await else {
                    continue;
                };
                queue.push(proxy);
            }
        }
        let observation_complete = !controls.is_empty();
        let control_count = controls.len();
        Ok(json!({
            "verified": observation_complete,
            "observation_complete": observation_complete,
            "observation_status": if observation_complete { "controls_found" } else { "no_controls_found" },
            "route": "linux_atspi_inspect",
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

    /// Find the application object for one exact process. Every subsequent
    /// traversal is scoped under it, so a control in another process can
    /// never match.
    async fn find_application(
        connection: &AccessibilityConnection,
        process_id: u32,
    ) -> Result<AccessibleProxy<'_>, String> {
        let root = connection
            .root_accessible_on_registry()
            .await
            .map_err(|error| format!("AT-SPI registry unavailable: {error}"))?;
        let dbus = zbus::fdo::DBusProxy::new(connection.connection())
            .await
            .map_err(|error| format!("D-Bus identity proxy unavailable: {error}"))?;
        let children = root
            .get_children()
            .await
            .map_err(|error| format!("AT-SPI application traversal failed: {error}"))?;
        for child in children.into_iter().filter(|child| !child.is_null()) {
            let proxy = child
                .into_accessible_proxy(connection.connection())
                .await
                .map_err(|error| format!("AT-SPI application proxy failed: {error}"))?;
            if node_process_id(&proxy, &dbus).await? == Some(process_id) {
                return Ok(proxy);
            }
        }
        Err("application_unavailable".to_owned())
    }

    async fn node_process_id(
        node: &AccessibleProxy<'_>,
        dbus: &zbus::fdo::DBusProxy<'_>,
    ) -> Result<Option<u32>, String> {
        let application = node
            .get_application()
            .await
            .map_err(|error| format!("AT-SPI application identity failed: {error}"))?;
        let Some(bus_name) = application.name() else {
            return Ok(None);
        };
        let pid = dbus
            .get_connection_unix_process_id(zbus_names::BusName::Unique(bus_name.clone()))
            .await
            .map_err(|error| format!("AT-SPI process identity failed: {error}"))?;
        Ok(Some(pid))
    }

    /// Collect every node matching the supplied selectors (each selector
    /// constrains; unsupplied selectors are unconstrained), bounded like the
    /// Windows route. Ambiguity is decided by the caller so ordinal
    /// disambiguation can apply first.
    async fn find_matches<'c>(
        connection: &'c AccessibilityConnection,
        application: &AccessibleProxy<'c>,
        request: &Request<'_>,
        limit: usize,
    ) -> Result<Vec<AccessibleProxy<'c>>, String> {
        if request.name.is_none() && request.automation_id.is_none() && request.role.is_none() {
            return Err("target_unspecified: needs name, automation_id, or role".to_owned());
        }
        let mut queue = vec![application.clone()];
        let mut matches = Vec::new();
        let mut visited = 0usize;
        while let Some(node) = queue.pop() {
            visited += 1;
            if visited > limit {
                break;
            }
            if node_matches(&node, request).await? {
                matches.push(node.clone());
                if matches.len() > 1 && request.match_index.is_none() {
                    break;
                }
            }
            let Ok(children) = node.get_children().await else {
                continue;
            };
            for child in children.into_iter().rev() {
                if child.is_null() {
                    continue;
                }
                let Ok(proxy) = child.into_accessible_proxy(connection.connection()).await else {
                    continue;
                };
                queue.push(proxy);
            }
        }
        Ok(matches)
    }

    async fn node_matches(
        node: &AccessibleProxy<'_>,
        request: &Request<'_>,
    ) -> Result<bool, String> {
        if let Some(name) = request.name {
            let candidate = node
                .name()
                .await
                .map_err(|error| format!("AT-SPI name read failed: {error}"))?;
            if candidate != name {
                return Ok(false);
            }
        }
        if let Some(automation_id) = request.automation_id {
            let candidate = node
                .accessible_id()
                .await
                .map_err(|error| format!("AT-SPI accessible-id read failed: {error}"))?;
            if candidate != automation_id {
                return Ok(false);
            }
        }
        if let Some(role) = request.role {
            let candidate = node
                .get_role_name()
                .await
                .map_err(|error| format!("AT-SPI role read failed: {error}"))?;
            if !semantics::role_matches(&candidate, role) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Semantic-first press: perform the role's most preferred action among
    /// those the element advertises. A sole advertised action counts as the
    /// element's own semantic action; anything else is honestly unactionable
    /// rather than reaching for the pointer.
    async fn press(target: &AccessibleProxy<'_>, request: &Request<'_>) -> Result<String, String> {
        let proxies = target
            .proxies()
            .await
            .map_err(|error| format!("AT-SPI action interfaces unavailable: {error}"))?;
        let action = proxies
            .action()
            .await
            .map_err(|_| "target_not_actionable: no action interface".to_owned())?;
        let advertised = action
            .get_actions()
            .await
            .map_err(|error| format!("AT-SPI action enumeration failed: {error}"))?;
        for preferred in semantics::press_preference(request.role) {
            if let Some(index) = advertised
                .iter()
                .position(|advertised| advertised.name.eq_ignore_ascii_case(preferred))
            {
                return perform_action(&action, index as i32, preferred).await;
            }
        }
        if advertised.len() == 1 {
            return perform_action(&action, 0, &advertised[0].name).await;
        }
        let advertised: Vec<&str> = advertised
            .iter()
            .map(|advertised| advertised.name.as_str())
            .collect();
        Err(format!(
            "target_not_actionable: no preferred semantic action advertised (advertised: {advertised:?})"
        ))
    }

    async fn perform_action(
        action: &atspi::proxy::action::ActionProxy<'_>,
        index: i32,
        name: &str,
    ) -> Result<String, String> {
        if !action
            .do_action(index)
            .await
            .map_err(|error| format!("AT-SPI action failed: {error}"))?
        {
            return Err("action_rejected".to_owned());
        }
        Ok(name.to_owned())
    }

    async fn set_value(target: &AccessibleProxy<'_>, value: &str) -> Result<String, String> {
        let proxies = target
            .proxies()
            .await
            .map_err(|error| format!("AT-SPI value interfaces unavailable: {error}"))?;
        let editable = proxies
            .editable_text()
            .await
            .map_err(|_| "value_not_settable".to_owned())?;
        if !editable
            .set_text_contents(value)
            .await
            .map_err(|error| format!("AT-SPI set-value failed: {error}"))?
        {
            return Err("value_rejected".to_owned());
        }
        Ok("EditableText".to_owned())
    }

    async fn advertised_actions(node: &AccessibleProxy<'_>) -> Vec<String> {
        let Ok(proxies) = node.proxies().await else {
            return Vec::new();
        };
        let Ok(action) = proxies.action().await else {
            return Vec::new();
        };
        match action.get_actions().await {
            Ok(advertised) => advertised
                .into_iter()
                .map(|advertised| advertised.name)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn enabled(node: &AccessibleProxy<'_>) -> Result<bool, String> {
        let state = node
            .get_state()
            .await
            .map_err(|error| format!("AT-SPI state read failed: {error}"))?;
        Ok(state.contains(State::Enabled) || state.contains(State::Sensitive))
    }

    async fn application_foreground(
        connection: &AccessibilityConnection,
        application: &AccessibleProxy<'_>,
    ) -> Result<bool, String> {
        let state = application
            .get_state()
            .await
            .map_err(|error| format!("AT-SPI state read failed: {error}"))?;
        if state.contains(State::Active) || state.contains(State::Focused) {
            return Ok(true);
        }
        let children = application
            .get_children()
            .await
            .map_err(|error| format!("AT-SPI window traversal failed: {error}"))?;
        for child in children
            .into_iter()
            .filter(|child| !child.is_null())
            .take(32)
        {
            let proxy = child
                .into_accessible_proxy(connection.connection())
                .await
                .map_err(|error| format!("AT-SPI window proxy failed: {error}"))?;
            let state = proxy
                .get_state()
                .await
                .map_err(|error| format!("AT-SPI window state read failed: {error}"))?;
            if state.contains(State::Active) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn verify_after_action<'c>(
        connection: &'c AccessibilityConnection,
        target: &AccessibleProxy<'c>,
        request: &Request<'_>,
    ) -> Result<bool, String> {
        let poll = request.action == Action::Press
            && matches!(
                request.expected_attribute,
                Some("name") | Some("window_title")
            );
        if !poll {
            return verify(connection, target, request).await;
        }
        let deadline = Instant::now() + POST_ACTION_VERIFY_TIMEOUT;
        loop {
            if verify(connection, target, request).await? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(POST_ACTION_VERIFY_INTERVAL);
        }
    }

    async fn verify<'c>(
        connection: &'c AccessibilityConnection,
        node: &AccessibleProxy<'c>,
        request: &Request<'_>,
    ) -> Result<bool, String> {
        match request.expected_attribute {
            None => Ok(false),
            Some("name") => Ok(node
                .name()
                .await
                .map_err(|error| format!("AT-SPI verification failed: {error}"))?
                == request.expected_value.unwrap_or_default()),
            Some("value") => {
                let proxies = node.proxies().await.map_err(|error| {
                    format!("AT-SPI verification interfaces unavailable: {error}")
                })?;
                let text_proxy = proxies.text().await.map_err(|error| {
                    format!("AT-SPI verification text interface unavailable: {error}")
                })?;
                let count = text_proxy
                    .character_count()
                    .await
                    .map_err(|error| format!("AT-SPI value length verification failed: {error}"))?;
                let text = text_proxy
                    .get_text(0, count)
                    .await
                    .map_err(|error| format!("AT-SPI value verification failed: {error}"))?;
                Ok(text == request.expected_value.unwrap_or_default())
            }
            Some("enabled") => Ok(enabled(node).await? == (request.expected_value == Some("true"))),
            Some("selected") => {
                let state = node
                    .get_state()
                    .await
                    .map_err(|error| format!("AT-SPI state read failed: {error}"))?;
                Ok(state.contains(State::Selected) == (request.expected_value == Some("true")))
            }
            Some("window_title") => {
                let Some(window) = window_ancestor(connection, node).await? else {
                    return Ok(false);
                };
                Ok(window
                    .name()
                    .await
                    .map_err(|error| format!("AT-SPI window title read failed: {error}"))?
                    == request.expected_value.unwrap_or_default())
            }
            Some(_) => Err("unsupported_verification_attribute".to_owned()),
        }
    }

    /// Walk up to the containing top-level window (frame / dialog / window)
    /// for `window_title` verification, bounded like every other traversal.
    async fn window_ancestor<'c>(
        connection: &'c AccessibilityConnection,
        node: &AccessibleProxy<'c>,
    ) -> Result<Option<AccessibleProxy<'c>>, String> {
        let mut current = node.clone();
        for _ in 0..WINDOW_TITLE_WALK_LIMIT {
            let parent_ref = current
                .parent()
                .await
                .map_err(|error| format!("AT-SPI parent traversal failed: {error}"))?;
            if parent_ref.is_null() {
                return Ok(None);
            }
            let parent = parent_ref
                .into_accessible_proxy(connection.connection())
                .await
                .map_err(|error| format!("AT-SPI parent proxy failed: {error}"))?;
            let role = parent
                .get_role_name()
                .await
                .map_err(|error| format!("AT-SPI role read failed: {error}"))?;
            if semantics::role_matches(&role, "window") {
                return Ok(Some(parent));
            }
            current = parent;
        }
        Ok(None)
    }
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
