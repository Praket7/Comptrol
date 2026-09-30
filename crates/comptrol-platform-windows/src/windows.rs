use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, RECT};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_OBJECT_CREATE, EVENT_OBJECT_NAMECHANGE, EnumWindows, GA_ROOT,
    GA_ROOTOWNER, GW_OWNER, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindow,
    GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, OBJID_WINDOW,
    PM_REMOVE, PeekMessageW, QS_ALLINPUT, SW_RESTORE, SetForegroundWindow, ShowWindow,
    TranslateMessage, WINEVENT_OUTOFCONTEXT,
};

static WINDOW_EVENT_SEEN: AtomicBool = AtomicBool::new(false);

/// Wait for an out-of-context WinEvent on a message-pumping thread. The caller
/// always performs a fresh inventory after the wake; this is only a wake hint.
/// A finite timer ensures providers and window events cannot hang readiness.
pub fn wait_for_window_event(timeout_ms: u32) -> bool {
    WINDOW_EVENT_SEEN.store(false, Ordering::SeqCst);
    let timeout_ms = timeout_ms.clamp(1, 30_000);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("comptrol-window-event-wait".to_owned())
        .spawn(move || {
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_CREATE,
                    EVENT_OBJECT_NAMECHANGE,
                    None,
                    Some(on_window_event),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if hook.0.is_null() {
                let _ = sender.send(false);
                return;
            }
            let started = Instant::now();
            let timeout = std::time::Duration::from_millis(timeout_ms as u64);
            let mut message = MSG::default();
            let event_seen = loop {
                if WINDOW_EVENT_SEEN.swap(false, Ordering::SeqCst) {
                    break true;
                }
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    break false;
                }
                let slice_ms = remaining.as_millis().clamp(1, 50) as u32;
                let _ = unsafe {
                    MsgWaitForMultipleObjectsEx(None, slice_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
                };
                while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                if WINDOW_EVENT_SEEN.swap(false, Ordering::SeqCst) {
                    break true;
                }
            };
            let _ = unsafe { UnhookWinEvent(hook) };
            let _ = sender.send(event_seen);
        });
    if spawned.is_err() {
        return false;
    }
    receiver
        .recv_timeout(std::time::Duration::from_millis(timeout_ms as u64 + 250))
        .unwrap_or(false)
}

unsafe extern "system" fn on_window_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    object_id: i32,
    child_id: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if hwnd.0.is_null() || object_id != OBJID_WINDOW.0 || child_id != 0 {
        return;
    }
    WINDOW_EVENT_SEEN.store(true, Ordering::SeqCst);
}

const MAX_WINDOWS: usize = 256;
const MAX_TITLE_CHARS: usize = 2048;
const MAX_CLASS_CHARS: usize = 256;

struct Enumeration {
    limit: usize,
    windows: Vec<Value>,
    truncated: bool,
    process_cache: HashMap<u32, ProcessIdentity>,
}

#[derive(Clone)]
struct ProcessIdentity {
    executable: Option<String>,
    created_at_100ns: Option<u64>,
    app_user_model_id: Option<String>,
}

pub fn enumerate_top_level_windows() -> Result<Value, String> {
    enumerate_top_level_windows_bounded(MAX_WINDOWS)
}

/// C4 compact-output variant: bound the window inventory (default 256, max
/// caller-requested) so one desktop.observe call cannot return hundreds of
/// window records. Invisible/ownerless tool windows still count toward the
/// bound, so callers that need everything should pass a higher limit.
pub fn enumerate_top_level_windows_bounded(max_windows: usize) -> Result<Value, String> {
    let max_windows = max_windows.clamp(1, MAX_WINDOWS);
    let mut state = Enumeration {
        limit: max_windows,
        windows: Vec::with_capacity(max_windows),
        truncated: false,
        process_cache: HashMap::new(),
    };
    let context = LPARAM((&mut state as *mut Enumeration) as isize);
    let result = unsafe { EnumWindows(Some(collect_window), context) };
    if let Err(error) = result
        && !state.truncated
    {
        return Err(format!(
            "EnumWindows failed after {} callbacks: {error}",
            state.windows.len()
        ));
    }
    Ok(json!({
        "windows": state.windows,
        "window_count": state.windows.len(),
        "truncated": state.truncated,
        "enumeration": "win32_enum_windows"
    }))
}

/// Return the exact current foreground HWND. This is used only as a correlation
/// signal after an explicit packaged-app activation; callers still verify the
/// title, host executable, class, and app identity before using it.
pub fn foreground_window_handle() -> Option<u64> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.0.is_null()).then_some(hwnd.0 as usize as u64)
}

/// Focus one already-running application only when its exact executable maps
/// to a single visible top-level window. This route never launches an app.
pub fn focus_existing_executable(executable: &Path) -> Result<Value, String> {
    focus_existing_window(executable, None)
}

/// Focus a verified process window. When `window_handle` is present, other
/// windows owned by the same executable are ignored, which makes duplicate
/// windows safe to target explicitly.
pub fn focus_existing_window(
    executable: &Path,
    window_handle: Option<u64>,
) -> Result<Value, String> {
    let total_started = Instant::now();
    let expected = std::fs::canonicalize(executable)
        .map_err(|error| format!("cannot resolve registered executable: {error}"))?;
    let resolution_started = Instant::now();
    let mut state = FocusEnumeration {
        expected_path: expected.clone(),
        expected_window_handle: window_handle,
        windows: Vec::new(),
    };
    let context = LPARAM((&mut state as *mut FocusEnumeration) as isize);
    if let Err(error) = unsafe { EnumWindows(Some(collect_matching_window), context) }
        && state.windows.len() < 2
    {
        return Err(format!("EnumWindows failed: {error}"));
    }
    let window_resolution_ms = resolution_started.elapsed().as_secs_f64() * 1000.0;
    match state.windows.as_slice() {
        [] => Err("no visible top-level window belongs to the registered executable".to_owned()),
        [target] => {
            if !focus_target_still_matches(target, &expected) {
                return Err("the selected window identity changed before activation".to_owned());
            }
            let hwnd = target.hwnd;
            let activation_started = Instant::now();
            let was_minimized = unsafe { IsIconic(hwnd) }.as_bool();
            if was_minimized {
                let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
            }
            let requested = unsafe { SetForegroundWindow(hwnd) }.as_bool();
            let is_foreground = wait_for_foreground(hwnd, 500);
            if !is_foreground {
                return Err(
                    "Windows did not grant foreground focus to the exact application window"
                        .to_owned(),
                );
            }
            Ok(json!({
                "window_handle": hwnd.0 as usize as u64,
                "executable": expected,
                "verification": "foreground_window_identity",
                "foreground_request_accepted": requested,
                "was_minimized": was_minimized,
                "timings_ms": {
                    "window_resolution_ms": window_resolution_ms,
                    "foreground_activation_and_verification_ms": activation_started.elapsed().as_secs_f64() * 1000.0,
                    "total_ms": total_started.elapsed().as_secs_f64() * 1000.0
                }
            }))
        }
        windows => Err(format!(
            "application has {} visible top-level windows; focus requires an unambiguous window",
            windows.len()
        )),
    }
}

/// Activate an exact previously observed HWND only when its process identity
/// still matches the inventory snapshot.
pub fn focus_window_handle(
    window_handle: u64,
    process_id: u32,
    process_created_at_100ns: Option<u64>,
) -> Result<Value, String> {
    let started = Instant::now();
    if window_handle == 0 {
        return Err("window_handle must be nonzero".to_owned());
    }
    let hwnd = HWND(window_handle as *mut _);
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Err("the exact target window is no longer visible".to_owned());
    }
    let mut current_pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut current_pid)) };
    if current_pid != process_id {
        return Err("the exact target window process identity changed".to_owned());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, current_pid) }
        .map_err(|error| format!("target process is unavailable: {error}"))?;
    let creation = process_creation_time(process);
    unsafe { windows::Win32::Foundation::CloseHandle(process) }.ok();
    if process_created_at_100ns.is_some_and(|expected| creation != Some(expected)) {
        return Err("the exact target window process generation changed".to_owned());
    }
    let was_minimized = unsafe { IsIconic(hwnd) }.as_bool();
    if was_minimized {
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    }
    let requested = unsafe { SetForegroundWindow(hwnd) }.as_bool();
    let verified = wait_for_foreground(hwnd, 500);
    if !verified {
        return Err("Windows did not grant foreground focus to the exact target window".to_owned());
    }
    Ok(json!({
        "window_handle": window_handle,
        "process_id": process_id,
        "process_created_at_100ns": creation,
        "verification": "foreground_window_identity",
        "foreground_request_accepted": requested,
        "was_minimized": was_minimized,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0
    }))
}

fn wait_for_foreground(hwnd: HWND, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if unsafe { GetForegroundWindow() } == hwnd {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

struct FocusEnumeration {
    expected_path: std::path::PathBuf,
    expected_window_handle: Option<u64>,
    windows: Vec<FocusTarget>,
}

#[derive(Clone, Copy)]
struct FocusTarget {
    hwnd: HWND,
    process_id: u32,
    process_created_at_100ns: u64,
}

unsafe extern "system" fn collect_matching_window(
    hwnd: HWND,
    context: LPARAM,
) -> windows::core::BOOL {
    let state = unsafe { &mut *(context.0 as *mut FocusEnumeration) };
    if state
        .expected_window_handle
        .is_some_and(|expected| expected != hwnd.0 as usize as u64)
    {
        return windows::core::BOOL(1);
    }
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return windows::core::BOOL(1);
    }
    let mut process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id == 0 {
        return windows::core::BOOL(1);
    }
    let Ok(process) =
        (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) })
    else {
        return windows::core::BOOL(1);
    };
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    if result.is_err() {
        unsafe { windows::Win32::Foundation::CloseHandle(process) }.ok();
        return windows::core::BOOL(1);
    }
    let path = std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
    let process_created_at_100ns = process_creation_time(process);
    unsafe { windows::Win32::Foundation::CloseHandle(process) }.ok();
    if same_windows_path(&path, &state.expected_path)
        && let Some(process_created_at_100ns) = process_created_at_100ns
    {
        state.windows.push(FocusTarget {
            hwnd,
            process_id,
            process_created_at_100ns,
        });
        if state.windows.len() >= 2 {
            return windows::core::BOOL(0);
        }
    }
    windows::core::BOOL(1)
}

fn focus_target_still_matches(target: &FocusTarget, expected_executable: &Path) -> bool {
    if !unsafe { IsWindowVisible(target.hwnd) }.as_bool() {
        return false;
    }
    let mut current_pid = 0u32;
    unsafe { GetWindowThreadProcessId(target.hwnd, Some(&mut current_pid)) };
    if current_pid != target.process_id {
        return false;
    }
    let Ok(process) =
        (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, current_pid) })
    else {
        return false;
    };
    let same_generation = process_creation_time(process) == Some(target.process_created_at_100ns);
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    let same_executable = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()
    .is_some_and(|_| {
        same_windows_path(
            Path::new(&String::from_utf16_lossy(&buffer[..length as usize])),
            expected_executable,
        )
    });
    unsafe { windows::Win32::Foundation::CloseHandle(process) }.ok();
    same_generation && same_executable
}

fn process_creation_time(process: windows::Win32::Foundation::HANDLE) -> Option<u64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    Some(((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
}

fn same_windows_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

unsafe extern "system" fn collect_window(hwnd: HWND, context: LPARAM) -> windows::core::BOOL {
    let state = unsafe { &mut *(context.0 as *mut Enumeration) };
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    if state.windows.len() >= state.limit {
        state.truncated = true;
        return windows::core::BOOL(0);
    }

    let title = window_text(hwnd);
    let class_name = window_class(hwnd);
    let mut process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    let process = state
        .process_cache
        .entry(process_id)
        .or_insert_with(|| process_identity(process_id))
        .clone();
    let mut rect = RECT::default();
    let rect_available = unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok();
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) }.unwrap_or_default();
    let owner_handle = (!owner.0.is_null()).then_some(owner.0 as usize as u64);
    state.windows.push(json!({
        "window_handle": hwnd.0 as usize as u64,
        "process_id": process_id,
        "process_created_at_100ns": process.created_at_100ns,
        "executable": process.executable,
        "app_user_model_id": process.app_user_model_id,
        "title": title,
        "class_name": class_name,
        "visible": visible,
        "owner_window_handle": owner_handle,
        "root_window_handle": unsafe { GetAncestor(hwnd, GA_ROOT) }.0 as usize as u64,
        "root_owner_window_handle": unsafe { GetAncestor(hwnd, GA_ROOTOWNER) }.0 as usize as u64,
        "rect": if rect_available {
            json!({"left":rect.left,"top":rect.top,"right":rect.right,"bottom":rect.bottom})
        } else {
            Value::Null
        }
    }));
    windows::core::BOOL(1)
}

fn process_identity(process_id: u32) -> ProcessIdentity {
    let Ok(process) =
        (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) })
    else {
        return ProcessIdentity {
            executable: None,
            created_at_100ns: None,
            app_user_model_id: None,
        };
    };
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    let executable = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()
    .map(|_| String::from_utf16_lossy(&buffer[..length as usize]));
    let created = process_creation_time(process);
    let mut aumid_length = 0u32;
    let _ = unsafe { GetApplicationUserModelId(process, &mut aumid_length, None) };
    let app_user_model_id = if aumid_length > 0 {
        let mut aumid = vec![0u16; aumid_length as usize];
        let status = unsafe {
            GetApplicationUserModelId(
                process,
                &mut aumid_length,
                Some(windows::core::PWSTR(aumid.as_mut_ptr())),
            )
        };
        (status.0 == 0)
            .then(|| String::from_utf16_lossy(&aumid[..aumid_length.saturating_sub(1) as usize]))
    } else {
        None
    };
    unsafe { windows::Win32::Foundation::CloseHandle(process) }.ok();
    ProcessIdentity {
        executable,
        created_at_100ns: created,
        app_user_model_id,
    }
}

fn window_text(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let mut buffer = vec![0u16; length.min(MAX_TITLE_CHARS) + 1];
    let written = unsafe { GetWindowTextW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..written.min(buffer.len())])
}

fn window_class(hwnd: HWND) -> String {
    let mut buffer = vec![0u16; MAX_CLASS_CHARS];
    let written = unsafe { GetClassNameW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..written.min(buffer.len())])
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CLASS_CHARS, MAX_TITLE_CHARS, MAX_WINDOWS, Path, same_windows_path,
        wait_for_window_event,
    };

    #[test]
    fn window_inventory_bounds_are_finite() {
        assert_eq!(MAX_WINDOWS, 256);
        assert_eq!(MAX_TITLE_CHARS, 2048);
        assert_eq!(MAX_CLASS_CHARS, 256);
    }

    #[test]
    fn exact_executable_comparison_ignores_windows_path_case_only() {
        assert!(same_windows_path(
            Path::new(r"C:\Apps\Example.exe"),
            Path::new(r"c:\apps\EXAMPLE.EXE")
        ));
        assert!(!same_windows_path(
            Path::new(r"C:\Apps\Example.exe"),
            Path::new(r"C:\Apps\Example2.exe")
        ));
    }

    #[test]
    fn readiness_event_hook_wakes_for_a_native_window_event() {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Accessibility::NotifyWinEvent;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, EVENT_OBJECT_SHOW, SW_SHOWNOACTIVATE, ShowWindow,
            WINDOW_EX_STYLE, WINDOW_STYLE,
        };
        use windows::core::w;

        let fixture = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Comptrol hidden event fixture"),
                WINDOW_STYLE(0),
                -32000,
                -32000,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create hidden native event fixture");
        let event_window = fixture.0 as usize;
        let started = std::time::Instant::now();
        let waiter = std::thread::spawn(|| wait_for_window_event(1_000));
        std::thread::sleep(std::time::Duration::from_millis(150));
        unsafe {
            let _ = ShowWindow(HWND(event_window as *mut _), SW_SHOWNOACTIVATE);
            NotifyWinEvent(EVENT_OBJECT_SHOW, HWND(event_window as *mut _), 0, 0);
        }
        let woke = waiter.join().expect("event waiter");
        let elapsed = started.elapsed();
        unsafe { DestroyWindow(fixture) }.expect("destroy hidden event fixture");
        assert!(woke, "event hook should wake for a native window event");
        assert!(elapsed < std::time::Duration::from_millis(800));
    }
}
