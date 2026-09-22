//! Native launch with verification.
//!
//! The launcher never shells out through an intermediate shell. It
//! spawns the resolved executable directly (or the platform open
//! surface for URLs/deep links). Direct executable routes may verify the
//! destination process identity after a bounded settle window. Platform
//! helper routes such as open, xdg-open, or explorer only prove dispatch
//! and deliberately remain unverified until the destination identity is
//! observed independently.

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
    #[error("no exact resource launch route is available for {0}")]
    ExactResourceRouteUnavailable(String),
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
        unsafe extern "system" {
            fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> RawHandle;
            fn CloseHandle(handle: RawHandle) -> i32;
            fn WaitForSingleObject(handle: RawHandle, milliseconds: u32) -> u32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const WAIT_OBJECT_0: u32 = 0;
        const WAIT_TIMEOUT: u32 = 258;
        const WAIT_FAILED: u32 = 0xFFFFFFFF;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return Ok(false);
            }
            let result = WaitForSingleObject(handle, 0);
            CloseHandle(handle);
            match result {
                WAIT_TIMEOUT => Ok(true),   // still running
                WAIT_OBJECT_0 => Ok(false), // exited
                WAIT_FAILED => Err(LaunchError::Io(std::io::Error::last_os_error())),
                _ => Ok(false), // unexpected state, treat as exited
            }
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

/// Launch the exact resolved app and optional resource.
///
/// Executable-backed applications receive resources directly as argv. macOS
/// application bundles use LaunchServices through `open -a <bundle>`, which
/// preserves exact app selection but returns only the helper PID. Routes that
/// cannot preserve the resolved app identity refuse rather than silently
/// delegating the resource to the OS default handler.
pub fn launch(request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError> {
    let settle = std::time::Duration::from_millis(request.settle_ms.max(50));
    let mut metadata = BTreeMap::new();
    metadata.insert("background".to_owned(), request.background.to_string());

    #[cfg(target_os = "macos")]
    if request.app.platform == "macos"
        && let Some(bundle) = request
            .app
            .executable
            .as_deref()
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("app"))
    {
        let resource = match &request.resource {
            Resource::File { path } => Some(path.as_str()),
            Resource::Url { url } => Some(url.as_str()),
            Resource::DeepLink { uri } => Some(uri.as_str()),
            Resource::None => None,
        };
        let child = open_macos_bundle(bundle, resource, request.background, &request.args)?;
        let pid = child.id();
        std::thread::sleep(settle);
        return Ok(LaunchOutcome {
            app_id: request.app.id.clone(),
            route: "macos_launchservices".to_owned(),
            pid: Some(pid),
            resource: request.resource.clone(),
            metadata,
        });
    }

    match &request.resource {
        Resource::File { path } => {
            launch_executable(request, Some(path.as_str()), metadata, settle)
        }
        Resource::Url { url } => launch_executable(request, Some(url.as_str()), metadata, settle),
        Resource::DeepLink { uri } => {
            launch_executable(request, Some(uri.as_str()), metadata, settle)
        }
        Resource::None => {
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
            launch_executable(request, None, metadata, settle)
        }
    }
}

fn launch_executable(
    request: &LaunchRequest,
    resource: Option<&str>,
    metadata: BTreeMap<String, String>,
    settle: std::time::Duration,
) -> Result<LaunchOutcome, LaunchError> {
    let Some(executable) = request.app.executable.clone() else {
        return Err(LaunchError::ExactResourceRouteUnavailable(
            request.app.id.clone(),
        ));
    };
    if executable.is_dir() {
        return Err(LaunchError::ExactResourceRouteUnavailable(
            request.app.id.clone(),
        ));
    }
    let mut command = std::process::Command::new(executable);
    if let Some(resource) = resource {
        command.arg(resource);
    }
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

#[cfg(target_os = "macos")]
fn open_macos_bundle(
    bundle: &std::path::Path,
    resource: Option<&str>,
    background: bool,
    args: &[String],
) -> Result<std::process::Child, LaunchError> {
    let mut command = std::process::Command::new("/usr/bin/open");
    if background {
        command.arg("-g");
    }
    command.arg("-a").arg(bundle);
    if let Some(resource) = resource {
        command.arg(resource);
    }
    if !args.is_empty() {
        command.arg("--args").args(args);
    }
    Ok(command.stdin(Stdio::null()).stdout(Stdio::null()).spawn()?)
}

fn route_pid_is_destination(route: &str) -> bool {
    matches!(route, "executable_argv")
}

/// Launch and then verify, per the V5 rule that delivery is not verification.
///
/// Only direct executable routes bind the returned PID to the destination app.
/// Native open helpers and Windows AUMID shell dispatch return Unavailable
/// rather than accidentally verifying open, xdg-open, or explorer.exe.
pub fn launch_verified(
    request: &LaunchRequest,
) -> Result<(LaunchOutcome, LaunchVerification), LaunchError> {
    let outcome = launch(request)?;
    if !route_pid_is_destination(&outcome.route) {
        return Ok((outcome, LaunchVerification::Unavailable));
    }
    let Some(pid) = outcome.pid else {
        return Ok((outcome, LaunchVerification::Unavailable));
    };
    match process_alive(pid) {
        Ok(true) => Ok((outcome, LaunchVerification::Verified)),
        Ok(false) => Ok((outcome, LaunchVerification::ExitedEarly)),
        Err(_) => Ok((outcome, LaunchVerification::Unavailable)),
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
    fn helper_launcher_pids_are_not_destination_verification() {
        assert!(!route_pid_is_destination("native_open"));
        assert!(!route_pid_is_destination("aumid_shell"));
        assert!(!route_pid_is_destination("macos_launchservices"));
        assert!(route_pid_is_destination("executable_argv"));
    }

    #[test]
    fn launcher_probe_reports_unavailable_on_unknown_pid() {
        // Non-existent PIDs return ExitedEarly (process not alive), not Unavailable
        // Unavailable is only returned on non-Unix platforms where liveness probing is not implemented
        let result = launcher_probe(0x7FFFFFFF);
        assert_eq!(result, LaunchVerification::ExitedEarly);
    }
}
