//! Direct Windows UI Automation execution.
//!
//! The public entry point is safe and returns structured JSON. COM and UIA
//! pointers remain inside this crate and are never exposed to the runtime.

#[cfg(windows)]
mod uia;

#[cfg(windows)]
pub use uia::{Action, Request, SemanticCacheEntry, UiaScopedCache, execute};

#[cfg(not(windows))]
pub mod unsupported {
    use serde_json::Value;

    pub fn execute(_: Value) -> Result<Value, String> {
        Err("Windows UI Automation is only available on Windows".to_owned())
    }
}
