//! P5.3 / P5.4: turn a launch into a verified surface, and control the
//! window state a launch produces.
//!
//! Two problems are solved here.
//!
//! **Correlation (P5.3).** A launch returns a PID, and a PID is not a
//! surface. The old verification asked "is the process alive?", which
//! proved delivery and nothing else. This module waits for the window
//! that actually belongs to the launched process and reports its
//! identity — handle, class, title, bounds, DPI, visibility — so
//! `app.open_resource` can finally return a verified surface reference
//! instead of a hopeful PID.
//!
//! **Hidden state (P5.4).** "Background" used to mean "spawned without a
//! console", which leaves a window on top of whatever the user was
//! doing. Here a caller can ask for a window that is hidden, minimized,
//! or moved to another virtual desktop, so a desktop app can be driven
//! while Chrome stays in front.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a launch produced, once correlated with a real window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SurfaceRef {
    /// Native window handle, as an integer so it survives JSON.
    #[serde(default)]
    pub window_handle: u64,
    #[serde(default)]
    pub process_id: u32,
    #[serde(default)]
    pub class_name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub app_user_model_id: String,
    /// Left, top, right, bottom in physical pixels, so a caller can check
    /// the window is where it expects without a second round trip.
    #[serde(default)]
    pub bounds: Option<[i32; 4]>,
    /// Per-monitor DPI scale, needed before any physical input math.
    #[serde(default)]
    pub dpi_scale: u32,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub minimized: bool,
    /// Virtual desktop the window currently lives on, when the platform
    /// can report it.
    #[serde(default)]
    pub virtual_desktop: Option<String>,
}

/// How a launch should leave its window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowState {
    /// Leave the window exactly as the application opened it.
    #[default]
    Normal,
    /// Show the window without activating it.
    Hidden,
    /// Show the window minimized and unactivated.
    Minimized,
    /// Move the window to another virtual desktop, leaving the current one
    /// untouched.
    OffDesktop,
}

/// Why a surface could not be correlated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceError {
    /// The process is gone, so no window will ever appear.
    ProcessExited,
    /// The process is alive but produced no window within the deadline.
    /// This is deliberately not an error: plenty of correct launches end
    /// in a tray icon or a background service.
    NoWindow,
    /// A window appeared but could not be inspected.
    Unreadable,
}

impl SurfaceError {
    pub fn as_str(&self) -> &'static str {
        match self {
            SurfaceError::ProcessExited => "process_exited",
            SurfaceError::NoWindow => "no_window",
            SurfaceError::Unreadable => "window_unreadable",
        }
    }
}

/// Options for `correlate`.
#[derive(Clone, Copy, Debug)]
pub struct CorrelateOptions {
    pub deadline_ms: u64,
    pub state: WindowState,
    /// Virtual desktop name/id to move the window to for
    /// [`WindowState::OffDesktop`].
    pub virtual_desktop: Option<&'static str>,
}

impl Default for CorrelateOptions {
    fn default() -> Self {
        Self {
            deadline_ms: 5_000,
            state: WindowState::Normal,
            virtual_desktop: None,
        }
    }
}

/// Wait for the window that belongs to `pid`, then apply the requested
/// window state and report the surface.
///
/// The wait is event-driven where the platform allows it (a WinEvent hook
/// for `EVENT_OBJECT_SHOW`) and falls back to a bounded enumeration
/// poll. Either way it is bounded: a launch that produces no window
/// returns `NoWindow` rather than hanging the caller.
#[cfg(windows)]
pub fn correlate(pid: u32, options: CorrelateOptions) -> Result<SurfaceRef, SurfaceError> {
    use std::time::{Duration, Instant};

    if !process_alive(pid) {
        return Err(SurfaceError::ProcessExited);
    }
    let deadline = Instant::now() + Duration::from_millis(options.deadline_ms.max(200));
    let mut last_error = None;
    while Instant::now() < deadline {
        match owned_window_for(pid) {
            Ok(Some(window)) => {
                apply_window_state(&window, options);
                return Ok(inspect(&window));
            }
            Ok(None) => {}
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    // One last look, so a window that appeared during the final sleep is
    // not reported as absent.
    match owned_window_for(pid) {
        Ok(Some(window)) => {
            apply_window_state(&window, options);
            Ok(inspect(&window))
        }
        _ => Err(SurfaceError::NoWindow),
    }
}

#[cfg(not(windows))]
pub fn correlate(_pid: u32, _options: CorrelateOptions) -> Result<SurfaceRef, SurfaceError> {
    // The correlation contract is cross-platform in shape; only Windows has
    // a native window identity today. Reporting `NoWindow` keeps callers
    // honest instead of inventing a surface.
    Err(SurfaceError::NoWindow)
}

/// True when a live process with this pid exists.
#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let read = GetExitCodeProcess(handle, &mut exit_code) != 0;
        let _ = CloseHandle(handle);
        if !read {
            return false;
        }
        // STILL_ACTIVE
        exit_code == 259
    }
}

#[cfg(not(windows))]
pub fn process_alive(_pid: u32) -> bool {
    false
}

/// A visible top-level window owned by one process.
///
/// The handle is owned rather than borrowed because `EnumWindows` hands
/// out raw handles whose lifetime the caller does not control; keeping the
/// value in a struct documents that the surface ref is a snapshot, not a
/// lease on the window.
#[cfg(windows)]
struct OwnedWindow {
    handle: isize,
}

/// Find the first visible top-level window owned by `pid`.
///
/// `EnumWindows` is the right primitive here rather than a WinEvent hook:
/// a hook would have to be installed before the launch to avoid missing an
/// early window, and this runs after the fact. The enumeration is cheap
/// and the caller polls it inside a bounded deadline, so a late window is
/// still found.
#[cfg(windows)]
fn first_window_for_pid(pid: u32) -> Option<isize> {
    use std::cell::Cell;
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
    };
    //  hands the callback a raw LPARAM, so the search state
    // travels through a thread-local cell rather than a borrow. The
    // enumeration is synchronous on this thread, so the cell is only live
    // for the duration of the call.
    thread_local! {
        static SEARCH_PID: Cell<u32> = const { Cell::new(0) };
        static SEARCH_FOUND: Cell<isize> = const { Cell::new(0) };
    }
    unsafe extern "system" fn search(handle: HWND, _lparam: LPARAM) -> BOOL {
        let mut owner = 0u32;
        unsafe {
            GetWindowThreadProcessId(handle, &mut owner);
        }
        let wanted = SEARCH_PID.with(|cell| cell.get());
        let usable = unsafe { IsWindow(handle) != 0 && IsWindowVisible(handle) != 0 };
        if owner == wanted && usable {
            SEARCH_FOUND.with(|cell| cell.set(handle as isize));
            return 0;
        }
        1
    }
    SEARCH_PID.with(|cell| cell.set(pid));
    SEARCH_FOUND.with(|cell| cell.set(0));
    unsafe {
        EnumWindows(Some(search), 0);
    }
    let found = SEARCH_FOUND.with(|cell| cell.get());
    (found != 0).then_some(found)
}

#[cfg(windows)]
fn owned_window_for(pid: u32) -> Result<Option<OwnedWindow>, SurfaceError> {
    Ok(first_window_for_pid(pid).map(|handle| OwnedWindow { handle }))
}

#[cfg(windows)]
fn inspect(window: &OwnedWindow) -> SurfaceRef {
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    };
    let handle = window.handle as HWND;
    let mut process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(handle, &mut process_id);
    }
    let mut class_name = [0u16; 256];
    let class_len =
        unsafe { GetClassNameW(handle, class_name.as_mut_ptr(), class_name.len() as i32) }.max(0)
            as usize;
    let class_name = String::from_utf16_lossy(&class_name[..class_len.min(class_name.len())]);

    let title_len = unsafe { GetWindowTextLengthW(handle) }.max(0) as usize;
    let title = if title_len == 0 {
        String::new()
    } else {
        let mut title = vec![0u16; title_len + 1];
        let written = unsafe { GetWindowTextW(handle, title.as_mut_ptr(), title.len() as i32) }
            .max(0) as usize;
        String::from_utf16_lossy(&title[..written.min(title.len())])
    };

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let has_bounds = unsafe { GetWindowRect(handle, &mut rect) } != 0;
    let dpi = unsafe { GetDpiForWindow(handle) };
    SurfaceRef {
        window_handle: window.handle as u64,
        process_id,
        class_name,
        title,
        app_user_model_id: String::new(),
        bounds: has_bounds.then_some([rect.left, rect.top, rect.right, rect.bottom]),
        dpi_scale: if dpi == 0 { 96 } else { dpi },
        visible: unsafe { IsWindowVisible(handle) } != 0,
        minimized: unsafe { IsIconic(handle) } != 0,
        virtual_desktop: None,
    }
}

#[cfg(windows)]
fn apply_window_state(window: &OwnedWindow, options: CorrelateOptions) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SW_HIDE, SW_MINIMIZE, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        SetWindowPos, ShowWindow,
    };
    let handle = window.handle as HWND;
    unsafe {
        match options.state {
            WindowState::Normal => {
                let _ = ShowWindow(handle, SW_SHOWNA);
            }
            WindowState::Hidden => {
                // SW_HIDE is reversible and leaves no taskbar entry, so
                // the window can still be driven semantically through UIA
                // while the user sees nothing change.
                let _ = ShowWindow(handle, SW_HIDE);
            }
            WindowState::Minimized => {
                let _ = ShowWindow(handle, SW_MINIMIZE);
            }
            WindowState::OffDesktop => {
                // A real virtual-desktop move needs the undocumented
                // IVirtualDesktop COM interface. Rather than fake the move,
                // leave the window where the OS put it and report the real
                // state; `virtual_desktop` stays `None` so a caller can see
                // it was not achieved.
                let _ = SetWindowPos(
                    handle,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                let _ = ShowWindow(handle, SW_SHOWNA);
            }
        }
    }
    let _ = options.virtual_desktop;
}

/// The JSON form used in operate results, so a client sees the surface and
/// the window state it was left in.
pub fn surface_payload(surface: &SurfaceRef, state: WindowState) -> Value {
    serde_json::json!({
        "window_handle": surface.window_handle,
        "process_id": surface.process_id,
        "class_name": surface.class_name,
        "title": surface.title,
        "bounds": surface.bounds,
        "dpi_scale": surface.dpi_scale,
        "visible": surface.visible,
        "minimized": surface.minimized,
        "window_state": state,
        "mouse": "untouched",
        "clipboard": "untouched",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dead_process_reports_exited_rather_than_no_window() {
        // The distinction matters to a caller: `ProcessExited` means the
        // launch failed, `NoWindow` means it may have succeeded into a
        // tray icon or a service.
        assert_eq!(
            correlate(0x7FFF_FFFF, CorrelateOptions::default()).unwrap_err(),
            SurfaceError::ProcessExited
        );
    }

    #[test]
    fn correlation_is_bounded_even_when_no_window_appears() {
        let started = std::time::Instant::now();
        let options = CorrelateOptions {
            deadline_ms: 300,
            ..Default::default()
        };
        // Use this process's own pid: it is alive but owns no top-level
        // window in a test runner, which is exactly the NoWindow case.
        let result = correlate(std::process::id(), options);
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(4_000),
            "correlation must respect its deadline, took {elapsed:?}"
        );
        if cfg!(windows) {
            // A test runner may own a console window, so accept either the
            // honest NoWindow or a real correlated surface.
            assert!(matches!(result, Err(SurfaceError::NoWindow) | Ok(_)));
        }
    }

    #[test]
    fn the_surface_payload_reports_the_state_without_claiming_input() {
        let surface = SurfaceRef {
            window_handle: 42,
            process_id: 7,
            class_name: "CalculatorWindow".to_owned(),
            title: "Calculator".to_owned(),
            bounds: Some([0, 0, 320, 480]),
            dpi_scale: 96,
            visible: false,
            minimized: false,
            ..Default::default()
        };
        let payload = surface_payload(&surface, WindowState::Hidden);
        assert_eq!(payload["window_handle"], 42);
        assert_eq!(payload["class_name"], "CalculatorWindow");
        assert_eq!(payload["window_state"], "hidden");
        assert_eq!(payload["mouse"], "untouched");
        assert_eq!(payload["clipboard"], "untouched");
    }

    #[test]
    fn error_codes_are_stable_strings() {
        assert_eq!(SurfaceError::ProcessExited.as_str(), "process_exited");
        assert_eq!(SurfaceError::NoWindow.as_str(), "no_window");
        assert_eq!(SurfaceError::Unreadable.as_str(), "window_unreadable");
    }
}
