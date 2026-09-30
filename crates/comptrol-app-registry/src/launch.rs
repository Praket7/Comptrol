//! Native launch with verification.
//!
//! The launcher never shells out through an intermediate shell. It
//! spawns the resolved executable directly (or the platform open
//! surface for URLs/deep links). Direct executable routes may verify the
//! destination process identity after direct dispatch. Platform
//! helper routes such as open, xdg-open, or explorer only prove dispatch
//! and deliberately remain unverified until the destination identity is
//! observed independently.

use crate::registry::AppEntry;
use crate::{Resource, Resource as OpenResource};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
    /// Monotonic phase durations populated by `launch_verified`.
    #[serde(default)]
    pub timings_ms: BTreeMap<String, u64>,
}

/// Verification result carried on the outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchVerification {
    /// Process identity observed alive after dispatch.
    Verified,
    /// Process exited before post-dispatch verification.
    ExitedEarly,
    /// Verification could not run on this platform.
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct LaunchRequest {
    pub app: AppEntry,
    pub resource: Resource,
    /// Deprecated compatibility field. Readiness is event/poll driven and
    /// never waits this fixed duration.
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
    let mut metadata = BTreeMap::new();
    metadata.insert("background".to_owned(), request.background.to_string());

    // Windows Settings is a packaged AUMID app with no executable path. Its
    // documented ms-settings protocol is the exact app-owned route for
    // opening a named Settings page (for example, ms-settings:colors).
    #[cfg(windows)]
    if request.app.id
        == "windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel"
        && let Resource::DeepLink { uri } = &request.resource
        && uri.starts_with("ms-settings:")
        && !uri.chars().any(char::is_whitespace)
    {
        let mut command = std::process::Command::new("explorer.exe");
        command
            .arg(uri)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn()?;
        let pid = child.id();
        return Ok(LaunchOutcome {
            app_id: request.app.id.clone(),
            route: "windows_settings_protocol".to_owned(),
            pid: Some(pid),
            resource: request.resource.clone(),
            metadata,
            timings_ms: BTreeMap::new(),
        });
    }

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
        return Ok(LaunchOutcome {
            app_id: request.app.id.clone(),
            route: "macos_launchservices".to_owned(),
            pid: Some(pid),
            resource: request.resource.clone(),
            metadata,
            timings_ms: BTreeMap::new(),
        });
    }

    match &request.resource {
        Resource::File { path } => launch_executable(request, Some(path.as_str()), metadata),
        Resource::Url { url } => launch_executable(request, Some(url.as_str()), metadata),
        Resource::DeepLink { uri } => launch_executable(request, Some(uri.as_str()), metadata),
        Resource::None => {
            #[cfg(windows)]
            {
                // Activate packaged apps through Windows' application
                // activation API, which returns the destination PID. The PID
                // is still not treated as window/content readiness.
                if is_aumid(&request.app.id) {
                    let pid = activate_packaged_app(&request.app.id)?;
                    return Ok(LaunchOutcome {
                        app_id: request.app.id.clone(),
                        route: "windows_app_activation".to_owned(),
                        pid: Some(pid),
                        resource: request.resource.clone(),
                        metadata,
                        timings_ms: BTreeMap::new(),
                    });
                }
            }
            launch_executable(request, None, metadata)
        }
    }
}

fn launch_executable(
    request: &LaunchRequest,
    resource: Option<&str>,
    metadata: BTreeMap<String, String>,
) -> Result<LaunchOutcome, LaunchError> {
    let Some(registered_executable) = request.app.executable.clone() else {
        return Err(LaunchError::ExactResourceRouteUnavailable(
            request.app.id.clone(),
        ));
    };
    let executable = resolve_known_launcher(&registered_executable);
    if executable.is_dir() {
        return Err(LaunchError::ExactResourceRouteUnavailable(
            request.app.id.clone(),
        ));
    }
    let mut command = std::process::Command::new(&executable);
    if let Some(resource) = resource {
        command.arg(resource);
    }
    command
        .args(&request.app.launch_args)
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
    Ok(LaunchOutcome {
        app_id: request.app.id.clone(),
        route: "executable_argv".to_owned(),
        pid: Some(pid),
        resource: request.resource.clone(),
        metadata: if executable != registered_executable {
            let mut metadata = metadata;
            metadata.insert(
                "launcher_resolved_from".to_owned(),
                registered_executable.to_string_lossy().into_owned(),
            );
            metadata.insert(
                "launched_executable".to_owned(),
                executable.to_string_lossy().into_owned(),
            );
            metadata
        } else {
            metadata
        },
        timings_ms: BTreeMap::new(),
    })
}

fn resolve_known_launcher(registered: &Path) -> PathBuf {
    #[cfg(windows)]
    if registered
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("blender-launcher.exe"))
        && let Some(parent) = registered.parent()
    {
        let blender = parent.join("blender.exe");
        if blender.is_file() {
            return blender;
        }
    }
    registered.to_path_buf()
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

#[cfg(windows)]
fn activate_packaged_app(aumid: &str) -> Result<u32, LaunchError> {
    use windows::Win32::System::Com::{
        CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        ACTIVATEOPTIONS, ApplicationActivationManager, IApplicationActivationManager,
    };
    use windows::core::PCWSTR;

    struct ComApartment;
    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|error| LaunchError::Io(std::io::Error::other(error.to_string())))?;
    let _apartment = ComApartment;
    let manager: IApplicationActivationManager =
        unsafe { CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER) }
            .map_err(|error| LaunchError::Io(std::io::Error::other(error.to_string())))?;
    let app_id = aumid
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let arguments = [0u16];
    unsafe {
        manager.ActivateApplication(
            PCWSTR(app_id.as_ptr()),
            PCWSTR(arguments.as_ptr()),
            ACTIVATEOPTIONS(0),
        )
    }
    .map_err(|error| LaunchError::Io(std::io::Error::other(error.to_string())))
}

#[cfg(any(windows, test))]
fn is_aumid(app_id: &str) -> bool {
    app_id
        .split_once('!')
        .is_some_and(|(package_family, app)| !package_family.is_empty() && !app.is_empty())
}

/// Launch and then verify, per the V5 rule that delivery is not verification.
///
/// Only direct executable routes bind the returned PID to the destination app.
/// Native open helpers and Windows AUMID shell dispatch return Unavailable
/// rather than accidentally verifying open, xdg-open, or explorer.exe.
pub fn launch_verified(
    request: &LaunchRequest,
) -> Result<(LaunchOutcome, LaunchVerification), LaunchError> {
    let total_started = std::time::Instant::now();
    let launch_started = std::time::Instant::now();
    let mut outcome = launch(request)?;
    let launch_dispatch_ms = launch_started.elapsed().as_millis() as u64;
    let verification_started = std::time::Instant::now();
    let verification = if !route_pid_is_destination(&outcome.route) {
        LaunchVerification::Unavailable
    } else if let Some(pid) = outcome.pid {
        match process_alive(pid) {
            Ok(true) => LaunchVerification::Verified,
            Ok(false) => LaunchVerification::ExitedEarly,
            Err(_) => LaunchVerification::Unavailable,
        }
    } else {
        LaunchVerification::Unavailable
    };
    outcome
        .timings_ms
        .insert("launch_dispatch_ms".to_owned(), launch_dispatch_ms);
    outcome.timings_ms.insert(
        "process_identity_verification_ms".to_owned(),
        verification_started.elapsed().as_millis() as u64,
    );
    outcome.timings_ms.insert(
        "total_ms".to_owned(),
        total_started.elapsed().as_millis() as u64,
    );
    Ok((outcome, verification))
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

    #[test]
    fn aumid_accepts_packaged_apps_with_specific_application_ids() {
        assert!(is_aumid(
            "windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel"
        ));
        assert!(is_aumid("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"));
        assert!(!is_aumid("windows.immersivecontrolpanel_cw5n1h2txyewy!"));
        assert!(!is_aumid("Microsoft.WindowsCalculator"));
    }

    #[cfg(windows)]
    #[test]
    fn blender_launcher_resolves_to_the_actual_application_executable() {
        let root =
            std::env::temp_dir().join(format!("comptrol-blender-launch-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let launcher = root.join("blender-launcher.exe");
        let blender = root.join("blender.exe");
        std::fs::write(&launcher, b"launcher fixture").unwrap();
        std::fs::write(&blender, b"application fixture").unwrap();
        assert_eq!(resolve_known_launcher(&launcher), blender);
        std::fs::remove_dir_all(root).unwrap();
    }
}
