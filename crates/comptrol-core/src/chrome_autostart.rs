//! Lazy local Chrome DevTools bootstrap.
//!
//! Comptrol only needs a local Chrome when a browser operation actually runs.
//! Starting Chrome at process start was both slow and rude: every `comptrol
//! mcp` session -- including desktop-only and adapter-only ones -- launched a
//! visible Chrome window with an `about:blank` tab, so asking for a Blender or
//! terminal operation threw a browser window in the user's face, once per
//! client session.
//!
//! The rules here are therefore:
//!
//! * start only when a browser route needs an endpoint, never at startup;
//! * reuse an already-running Comptrol Chrome instead of starting a second one;
//! * start with no window at all, and let the first tab Comptrol opens create
//!   it.
//!
//! Both `--no-startup-window` and reuse are load-bearing. The first is what
//! keeps `about:blank` out of the user's face (verified on Windows Chrome:
//! with the flag the browser answers DevTools and reports zero page targets).
//! The second keeps repeated client sessions from stacking Chrome instances.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::default_state_dir;

/// The Chrome process this runtime started, if any.
///
/// Only a process this runtime started is ever stopped again; a reused browser
/// belongs to whoever started it.
static STARTED: OnceLock<Mutex<Option<Child>>> = OnceLock::new();

fn started() -> &'static Mutex<Option<Child>> {
    STARTED.get_or_init(|| Mutex::new(None))
}

fn environment(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Auto-start is available where Comptrol can launch the local Chrome, and is
/// off when the operator sets `COMPTROL_AUTO_START_CHROME_CDP=0`.
fn enabled() -> bool {
    cfg!(windows) && std::env::var("COMPTROL_AUTO_START_CHROME_CDP").as_deref() != Ok("0")
}

fn profile_dir() -> PathBuf {
    environment("COMPTROL_CHROME_PROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_state_dir().join("chrome-cdp-profile"))
}

/// Whether this runtime could start a local browser if asked.
///
/// Capability discovery uses this so a client is told the browser route
/// is reachable before anything has started one, instead of being told
/// the browser is unavailable and never trying.
pub fn available() -> bool {
    enabled() && chrome_path().is_some()
}

/// Whether an intent is a browser operation, and so may start a browser.
///
/// Desktop, terminal, filesystem, and adapter intents are deliberately
/// absent: those must never make a browser appear.
pub fn intent_needs_browser(intent: &str) -> bool {
    intent.starts_with("browser.cdp.")
        || intent == "browser.ensure_session"
        || intent == "browser.session.connect"
}

pub fn chrome_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for variable in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(base).join("Google/Chrome/Application/chrome.exe"));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// One bounded `GET /json/version` against a loopback DevTools port.
fn devtools_answers(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    if stream
        .write_all(b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.contains("200 OK") && response.contains("Browser")
}

/// A Chrome started against this profile writes the port it chose into
/// `DevToolsActivePort`; reusing that port avoids stacking instances and
/// windows across sessions.
fn running_port(profile: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let port = text.lines().next()?.trim().parse::<u16>().ok()?;
    devtools_answers(port).then_some(port)
}

/// Point every local browser route at `endpoint`.
///
/// The session endpoint is set in addition to the environment because call
/// sites disagree about which one they read, and because a browser Comptrol
/// started exists only to serve Comptrol's DevTools requests.
#[allow(unsafe_code)]
fn adopt(endpoint: String) {
    // SAFETY: `set_var` is unsafe because a concurrent environment reader
    // may observe a torn value. The only variables written here are the two
    // Comptrol already wrote for exactly this purpose (the old eager path
    // did the same on every session start), and a browser route reading
    // them is precisely the caller waiting on this function.
    unsafe {
        std::env::set_var("COMPTROL_ALLOW_BROWSER_CDP", "1");
        std::env::set_var("COMPTROL_CDP_ENDPOINT", &endpoint);
    }
    crate::browser::set_active_endpoint(endpoint);
}

/// Ensure a local DevTools endpoint exists, starting Chrome on first use.
///
/// Returns true when a local endpoint is reachable afterwards. This is called
/// from the browser route resolution, so an operation that never touches a
/// browser never pays for Chrome.
pub fn ensure() -> bool {
    // Process-lifetime memo: a failed auto-start must not re-run its spawn
    // probe on every browser operation. The probe used to cost 10 s per
    // call (200 x 50 ms port wait) when Chrome could not report a port,
    // which made every bridge-routed operation pay the tax twice.
    static ENSURED: OnceLock<bool> = OnceLock::new();
    if let Some(cached) = ENSURED.get() {
        if *cached && running_port(&profile_dir()).is_none() {
            // A previously adopted browser died; re-probe once below by
            // falling through with the cache cleared.
        } else {
            return *cached;
        }
    }
    let result = ensure_uncached();
    let _ = ENSURED.set(result);
    result
}

fn ensure_uncached() -> bool {
    if environment("COMPTROL_CDP_ENDPOINT").is_some() {
        return true;
    }
    if !enabled() {
        return false;
    }
    let profile = profile_dir();
    if let Some(port) = running_port(&profile) {
        adopt(format!("http://127.0.0.1:{port}"));
        return true;
    }
    let Some(chrome) = chrome_path() else {
        eprintln!("Comptrol Chrome CDP auto-start skipped because Chrome was not found");
        return false;
    };
    if let Err(error) = std::fs::create_dir_all(&profile) {
        eprintln!(
            "Comptrol Chrome CDP auto-start skipped because the profile could not be created: {error}"
        );
        return false;
    }
    // `--remote-debugging-port=0` (not a fixed port) is load-bearing: Chrome
    // records the port it chose in the profile's `DevToolsActivePort` file,
    // which is exactly what `running_port` reads, so the next session can find
    // this browser again instead of stacking another. With an explicit port
    // Chrome does not write that file at all (verified against this machine's
    // Chrome).
    // `--no-startup-window` is what keeps this from throwing an `about:blank`
    // window at the user; the first tab Comptrol opens creates the window. An
    // explicit COMPTROL_CHROME_START_URL means the operator asked for a page,
    // and then a window is exactly what they asked for.
    let start_url = environment("COMPTROL_CHROME_START_URL");
    let mut arguments = vec![
        "--remote-debugging-address=127.0.0.1".to_owned(),
        "--remote-debugging-port=0".to_owned(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
    ];
    match start_url {
        Some(url) => arguments.push(url),
        None => arguments.push("--no-startup-window".to_owned()),
    }
    let mut child = match Command::new(&chrome)
        .args(&arguments)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "Comptrol Chrome CDP auto-start skipped because Chrome could not launch: {error}"
            );
            return false;
        }
    };
    // Wait for Chrome to report its chosen port through `DevToolsActivePort`.
    // A second launch against the same profile hands its command line to the
    // running instance and exits, so this loop adopting the reported port is
    // also what lets such a joined child serve operations instead of failing.
    // Bounded to ~3 s: when Chrome cannot report a port (profile lock held by
    // the user's visible instance, headless environments), waiting longer
    // just taxes every caller; the bridge route remains available. A child
    // that already exited joined an existing instance whose port file will
    // never appear in this profile, so bail instead of waiting the window.
    let mut port = None;
    for _ in 0..60 {
        if let Some(found) = running_port(&profile) {
            port = Some(found);
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let Some(port) = port else {
        eprintln!(
            "Comptrol Chrome CDP auto-start skipped because Chrome did not report a DevTools port"
        );
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    adopt(format!("http://127.0.0.1:{port}"));
    // Only a browser this child really owns is stopped again at shutdown; a
    // child that joined an existing instance has already exited and belongs to
    // whichever session started that instance.
    if child.try_wait().ok().flatten().is_none() {
        if let Ok(mut slot) = started().lock() {
            *slot = Some(child);
        }
        eprintln!(
            "Comptrol started a windowless local Chrome for browser operations (profile {})",
            profile.display()
        );
    }
    true
}

/// Stop the Chrome this runtime started, if it started one.
pub fn shutdown() {
    let Ok(mut slot) = started().lock() else {
        return;
    };
    let Some(mut child) = slot.take() else {
        return;
    };
    drop(slot);
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}
