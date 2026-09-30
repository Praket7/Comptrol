//! Direct Windows UI Automation execution.
//!
//! The public entry point is safe and returns structured JSON. COM and UIA
//! pointers remain inside this crate and are never exposed to the runtime.

#[cfg(windows)]
mod uia;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use uia::{Action, Request, execute, run_worker_stdio, send_key_sequence};
#[cfg(windows)]
pub use windows::{
    enumerate_top_level_windows, enumerate_top_level_windows_bounded, foreground_window_handle,
    wait_for_window_event,
};
#[cfg(windows)]
pub use windows::{focus_existing_executable, focus_existing_window, focus_window_handle};

#[cfg(not(windows))]
pub mod unsupported {
    use serde_json::Value;

    pub fn execute(_: Value) -> Result<Value, String> {
        Err("Windows UI Automation is only available on Windows".to_owned())
    }

    pub fn enumerate_top_level_windows() -> Result<Value, String> {
        Err("Top-level window enumeration is only available on Windows".to_owned())
    }

    pub fn enumerate_top_level_windows_bounded(_: usize) -> Result<Value, String> {
        Err("Top-level window enumeration is only available on Windows".to_owned())
    }

    pub fn foreground_window_handle() -> Option<u64> {
        None
    }

    pub fn wait_for_window_event(_: u32) -> bool {
        false
    }

    pub fn focus_existing_executable(_: &std::path::Path) -> Result<Value, String> {
        Err("Focusing an existing application is only available on Windows".to_owned())
    }

    pub fn focus_existing_window(_: &std::path::Path, _: Option<u64>) -> Result<Value, String> {
        Err("Focusing an existing application is only available on Windows".to_owned())
    }

    pub fn focus_window_handle(_: u64, _: u32, _: Option<u64>) -> Result<Value, String> {
        Err("Focusing an exact window is only available on Windows".to_owned())
    }
}
