//! Native launch with verification.
//!
//! The launcher never shells out through an intermediate shell. It
//! spawns the resolved executable directly (or the platform open
//! surface for URLs/deep links), records the spawned process identity,
//! and verifies the process is still alive after a bounded settle
//! window. The settle window is an event-free liveness check, not a
//! sleep-based UI wait.

use crate::registry::AppEntry;
use crate::{Resource, Resource as OpenResource};
use serde::{Deserialize, Serialize};
use std::process::Stdio;

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry failed: {0}")]
    Registry(#[from] crate::RegistryError),
}

/// What was launched and what identity was observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaunchOutcome {
    pub app_id: String,
    pub route: String,
    pub pid: Option<u32>,
    pub resource: OpenResource,
}

/// Verification result carried on the outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchVerification {
    /// Process identity observed alive after the settle window.
    Verified,
    /// Process exited before the settle window closed.
    ExitedEarly,
    /// Verification could not run on this platform.
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct LaunchRequest {
    pub app: AppEntry,
    pub resource: Resource,
    /// Milliseconds to wait before the liveness check (default 750ms).
    pub settle_ms: u64,
    /// Open without foregrounding where the platform supports it.
    pub background: bool,
}

impl LaunchRequest {
    pub fn new(app: AppEntry) -> Self {
        Self {
            app,
            resource: Resource::None,
            settle_ms: 750,
            background: false,
        }
    }
}

/// Platform hook for tests: returns the process identity of `pid` if it
/// is alive, `None` if it exited, and `Err` when liveness cannot be
/// checked on this platform.
pub fn process_alive(pid: u32) -> Result<bool, LaunchError> {
    #[cfg(unix)]
    {
        // signal 0 probes existence without side effects.
        let rc = unsafe { probe_liveness(pid as i32) };
        Ok(rc == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(LaunchError::Io(std::io::Error::other(
            "process liveness probing is not implemented on this platform",
        )))
    }
}

#[cfg(unix)]
unsafe fn probe_liveness(pid: i32) -> i32 {
    unsafe extern "C" {
        #[link_name = "kill"]
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, 0) }
}

/// Launch the resolved app. URL/deep-link resources route through the
/// platform open surface (which resolves the user's default handler);
/// file and no-resource launches spawn the resolved executable.
pub fn launch(request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError> {
    let settle = std::time::Duration::from_millis(request.settle_ms.max(50));
    match &request.resource {
        Resource::Url { url } => {
            let child = open_native(url)?;
            let pid = child.id();
            std::thread::sleep(settle);
            Ok(LaunchOutcome {
                app_id: request.app.id.clone(),
                route: "native_open".to_owned(),
                pid: Some(pid),
                resource: request.resource.clone(),
            })
        }
        Resource::DeepLink { uri } => {
            let child = open_native(uri)?;
            let pid = child.id();
            std::thread::sleep(settle);
            Ok(LaunchOutcome {
                app_id: request.app.id.clone(),
                route: "native_open".to_owned(),
                pid: Some(pid),
                resource: request.resource.clone(),
            })
        }
        Resource::File { path } => {
            let mut command = std::process::Command::new(
                request
                    .app
                    .executable
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from(&request.app.id)),
            );
            command.arg(path).stdin(Stdio::null()).stdout(Stdio::null());
            let child = command.spawn()?;
            let pid = child.id();
            std::thread::sleep(settle);
            Ok(LaunchOutcome {
                app_id: request.app.id.clone(),
                route: "executable_argv".to_owned(),
                pid: Some(pid),
                resource: request.resource.clone(),
            })
        }
        Resource::None => {
            let mut command = std::process::Command::new(
                request
                    .app
                    .executable
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from(&request.app.id)),
            );
            command.stdin(Stdio::null()).stdout(Stdio::null());
            let child = command.spawn()?;
            let pid = child.id();
            std::thread::sleep(settle);
            Ok(LaunchOutcome {
                app_id: request.app.id.clone(),
                route: "executable_argv".to_owned(),
                pid: Some(pid),
                resource: request.resource.clone(),
            })
        }
    }
}

/// Launch and then verify, per the V5 rule that a spawned process is
/// delivery, not verification.
pub fn launch_verified(
    request: &LaunchRequest,
) -> Result<(LaunchOutcome, LaunchVerification), LaunchError> {
    let outcome = launch(request)?;
    let Some(pid) = outcome.pid else {
        return Ok((outcome, LaunchVerification::Unavailable));
    };
    match process_alive(pid) {
        Ok(true) => Ok((outcome, LaunchVerification::Verified)),
        Ok(false) => Ok((outcome, LaunchVerification::ExitedEarly)),
        Err(_) => Ok((outcome, LaunchVerification::Unavailable)),
    }
}

/// Open a URL or deep link through the platform's default-handler
/// surface without an intermediate shell.
fn open_native(target: &str) -> Result<std::process::Child, LaunchError> {
    #[cfg(target_os = "macos")]
    {
        Ok(std::process::Command::new("/usr/bin/open")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?)
    }
    #[cfg(windows)]
    {
        // `start` equivalent through cmd would be a shell; use the
        // documented ShellExecute surface via `explorer` instead.
        Ok(std::process::Command::new("explorer")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Ok(std::process::Command::new("xdg-open")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?)
    }
}

/// Test probe exposing the verification decision for a given pid so
/// conformance scripts can exercise the model without launching apps.
pub fn launcher_probe(pid: u32) -> LaunchVerification {
    match process_alive(pid) {
        Ok(true) => LaunchVerification::Verified,
        Ok(false) => LaunchVerification::ExitedEarly,
        Err(_) => LaunchVerification::Unavailable,
    }
}
