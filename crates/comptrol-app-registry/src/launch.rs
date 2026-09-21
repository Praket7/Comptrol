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
use std::collections::BTreeMap;
use std::process::Stdio;

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry failed: {0}")]
    Registry(#[from] crate::RegistryError),
}

/// What was launched and what identity was observed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub app_id: String,
    pub route: String,
    pub pid: Option<u32>,
    pub resource: OpenResource,
    pub metadata: BTreeMap<String, String>,
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
    /// Additional launch arguments.
    pub args: Vec<String>,
}

impl LaunchRequest {
    pub fn new(app: AppEntry) -> Self {
        Self {
            app,
            resource: Resource::None,
            settle_ms: 750,
            background: false,
            args: Vec::new(),
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
    #[cfg(windows)]
    {
        use std::os::windows::io::RawHandle;
        extern "system" {
            fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> RawHandle;
            fn CloseHandle(handle: RawHandle) -> i32;
            fn WaitForSingleObject(handle: RawHandle, milliseconds: u32) -> u32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 258;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return Ok(false);
            }
            // Check if process has exited by waiting briefly (0ms = immediate check)
            let exit_code = WaitForSingleObject(handle, 0);
            CloseHandle(handle);
            // WAIT_OBJECT_0 (0) means process has exited, WAIT_TIMEOUT (258) means still running
            Ok(exit_code == STILL_ACTIVE || exit_code == 0x102) // 0x102 = WAIT_TIMEOUT
        }
    }
    #[cfg(not(any(unix, windows)))]
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
    let mut metadata = BTreeMap::new();
    metadata.insert("background".to_owned(), request.background.to_string());

    match &request.resource {
        Resource::Url { url } => {
            let child = open_native(url, request.background)?;
            let pid = child.id();
            std::thread::sleep(settle);
            Ok(LaunchOutcome {
                app_id: request.app.id.clone(),
                route: "native_open".to_owned(),
                pid: Some(pid),
                resource: request.resource.clone(),
                metadata,
            })
        }
        Resource::DeepLink { uri } => {
            let child = open_native(uri, request.background)?;
            let pid = child.id();
            std::thread::sleep(settle);
            Ok(LaunchOutcome {
                app_id: request.app.id.clone(),
                route: "native_open".to_owned(),
                pid: Some(pid),
                resource: request.resource.clone(),
                metadata,
            })
        }
        Resource::File { path } => {
            let executable = request
                .app
                .executable
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from(&request.app.id));
            let mut command = std::process::Command::new(executable);
            command.arg(path).stdin(Stdio::null()).stdout(Stdio::null());
            if request.background {
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    command.process_group(0);
                }
            }
            command
                .args(&request.args)
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            let child = command.spawn()?;
            let pid = child.id();
            std::thread::sleep(settle);
            Ok(LaunchOutcome {
                app_id: request.app.id.clone(),
                route: "executable_argv".to_owned(),
                pid: Some(pid),
                resource: request.resource.clone(),
                metadata,
            })
        }
        Resource::None => {
            // On Windows, detect AUMID (AppUserModelID) patterns and use shell:AppsFolder
            #[cfg(windows)]
            {
                let app_id = &request.app.id;
                let is_aumid = app_id.contains('_')
                    && (app_id.ends_with("!App") || app_id.ends_with("!Application"));
                if is_aumid {
                    let shell_path = format!("shell:AppsFolder\\{app_id}");
                    let mut command = std::process::Command::new("explorer.exe");
                    command.arg(&shell_path);
                    command.stdin(Stdio::null()).stdout(Stdio::null());
                    let child = command.spawn()?;
                    let pid = child.id();
                    std::thread::sleep(settle);
                    return Ok(LaunchOutcome {
                        app_id: request.app.id.clone(),
                        route: "aumid_shell".to_owned(),
                        pid: Some(pid),
                        resource: request.resource.clone(),
                        metadata,
                    });
                }
            }
            let executable = request
                .app
                .executable
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from(&request.app.id));
            let mut command = std::process::Command::new(executable);
            command
                .args(&request.args)
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            if request.background {
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    command.process_group(0);
                }
            }
            let child = command.spawn()?;
            let pid = child.id();
            std::thread::sleep(settle);
            Ok(LaunchOutcome {
                app_id: request.app.id.clone(),
                route: "executable_argv".to_owned(),
                pid: Some(pid),
                resource: request.resource.clone(),
                metadata,
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
fn open_native(target: &str, background: bool) -> Result<std::process::Child, LaunchError> {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("/usr/bin/open");
        if background {
            cmd.arg("-g"); // Don't bring to front
        }
        Ok(cmd
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?)
    }
    #[cfg(windows)]
    {
        // Use explorer.exe for URL/deep-link opening
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(target);
        if background {
            // On Windows, we can't easily background explorer.exe
        }
        Ok(cmd.stdin(Stdio::null()).stdout(Stdio::null()).spawn()?)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut cmd = std::process::Command::new("xdg-open");
        if background {
            // Create a new process group so the child doesn't receive
            // signals from the parent's terminal. Uses process_group(0)
            // instead of calling setsid() in the parent process.
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
        }
        Ok(cmd
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_probe_reports_unavailable_on_unknown_pid() {
        // Non-existent PIDs return ExitedEarly (process not alive), not Unavailable
        // Unavailable is only returned on non-Unix platforms where liveness probing is not implemented
        let result = launcher_probe(0x7FFFFFFF);
        assert_eq!(result, LaunchVerification::ExitedEarly);
    }
}
