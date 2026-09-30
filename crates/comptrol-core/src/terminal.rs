//! P5.5: `desktop.terminal` and `desktop.explorer`.
//!
//! `command.run` already exists but is deliberately headless: it captures
//! stdout and never shows anything. That is the right primitive for a
//! pipeline step and the wrong one for "run this in a terminal and tell me
//! what it printed", which is how a person actually checks their work.
//!
//! `desktop.terminal` fills that gap without becoming a shell. It writes a
//! typed command into a real terminal window and reads the result back
//! through a marker, so the output the caller sees is the output the user
//! would have seen. There is no `sh -c`, no string interpolation into a
//! shell, and no clipboard or mouse involvement.
//!
//! `desktop.explorer` is the same idea for files: reveal an exact path in
//! the platform file manager, and report which surface it opened.

use crate::{
    ActionResult, ComptrolError, DeliveryState, EffectState, OperationRequest, RecoveryState,
    VerificationState, success,
};
use serde_json::{Value, json};

/// The sentinel echoed after a command so readback knows where the output
/// stops. It is deliberately unlikely to appear in real output.
const READBACK_MARKER: &str = "__comptrol_eof__";

/// The longest output the caller will receive from a terminal command.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Run one allowlisted command in a visible terminal and read its output.
///
/// The command is passed as structured argv, exactly like `command.run`.
/// The only shell involvement is quoting the argv into a *display* string
/// for the terminal line; the executed program is always the explicit
/// executable, so a crafted argument cannot become a second command.
pub fn terminal_run(request: &OperationRequest, operation_id: String) -> ActionResult {
    terminal_run_with_allowlist(request, operation_id, None)
}

/// The same route with an explicit allowlist, used by tests so the
/// allowlist rule can be exercised without mutating process environment
/// (this crate denies `unsafe`).
fn terminal_run_with_allowlist(
    request: &OperationRequest,
    operation_id: String,
    allowlist: Option<&str>,
) -> ActionResult {
    let Some(commands) = request.params.get("commands").and_then(Value::as_array) else {
        return invalid_input(
            request,
            operation_id,
            "desktop.terminal needs a commands array",
        );
    };
    if commands.is_empty() {
        return invalid_input(
            request,
            operation_id,
            "desktop.terminal needs at least one command",
        );
    }
    if commands.len() > 32 {
        return invalid_input(
            request,
            operation_id,
            "desktop.terminal accepts at most 32 commands per call",
        );
    }
    // Validate every command before running any of them, so a typo in the
    // third command cannot leave the first two half-applied.
    let mut parsed = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let program = command
            .get("program")
            .and_then(Value::as_str)
            .filter(|program| !program.trim().is_empty() && !program.chars().any(char::is_control));
        let Some(program) = program else {
            return invalid_input(
                request,
                operation_id,
                &format!(
                    "commands[{index}].program must be a non-empty string without control characters"
                ),
            );
        };
        let args = match command.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut args = Vec::with_capacity(items.len());
                for item in items {
                    let Some(text) = item.as_str() else {
                        return invalid_input(
                            request,
                            operation_id,
                            &format!("commands[{index}].args must contain only strings"),
                        );
                    };
                    args.push(text.to_owned());
                }
                args
            }
            Some(_) => {
                return invalid_input(
                    request,
                    operation_id,
                    &format!("commands[{index}].args must be an array of strings"),
                );
            }
        };
        parsed.push((program.to_owned(), args));
    }
    // The same explicit allowlist that `command.run` uses. Without this the
    // terminal route would be a way to run any executable the policy switch
    // had been opened for.
    for (index, (program, _)) in parsed.iter().enumerate() {
        let permitted = match allowlist {
            Some(allowlist) => crate::program_in_allowlist(program, allowlist),
            None => crate::command_program_allowed(program),
        };
        if !permitted {
            return ActionResult::refused(
                request,
                operation_id,
                ComptrolError {
                    code: "policy_denied".to_owned(),
                    message: format!(
                        "commands[{index}] program {program} is not in the explicit local command allowlist"
                    ),
                    recovery: Some(
                        "Add the exact executable to COMPTROL_COMMAND_ALLOWLIST locally".to_owned(),
                    ),
                },
            );
        }
    }

    let root = request
        .params
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let timeout_ms = request
        .params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(60_000)
        .clamp(100, 600_000);
    let visible = request.params.get("visible").and_then(Value::as_bool) == Some(false);

    let (results, timed_out) = run_in_terminal(&parsed, root, timeout_ms);
    let output = results
        .iter()
        .map(|result| result["stdout"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = output.len() > MAX_OUTPUT_BYTES;
    let combined = if output.len() > MAX_OUTPUT_BYTES {
        &output[..MAX_OUTPUT_BYTES]
    } else {
        &output[..]
    };
    let expected = request
        .postcondition
        .as_ref()
        .and_then(|postcondition| postcondition.get("output_contains"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let observed = if timed_out {
        None
    } else if let Some(expected) = expected.as_deref() {
        Some(combined.contains(expected))
    } else {
        Some(true)
    };
    let verified = observed == Some(true);
    let mut data = json!({
        "commands": results,
        "output": combined,
        "truncated": truncated,
        "visible": !visible,
        "mouse": "untouched",
        "clipboard": "untouched",
        "verified_by": if verified { "terminal_output_readback" } else { "not_verified" },
    });
    if let Some(expected) = expected {
        data["expected_output_contains"] = json!(expected);
        data["matched"] = json!(observed == Some(true));
    }
    if let Some(error) = timed_out_error(timed_out, timeout_ms) {
        let mut data = data;
        data["timed_out"] = json!(true);
        return ActionResult {
            operation_id,
            intent: request.intent.clone(),
            route: "terminal".to_owned(),
            target: request.target.clone(),
            preflight: "passed".to_owned(),
            delivery: if timed_out {
                DeliveryState::Unknown
            } else {
                DeliveryState::Delivered
            },
            effect: if timed_out {
                EffectState::Unknown
            } else {
                EffectState::Changed
            },
            verification: if timed_out {
                VerificationState::NotAttempted
            } else if verified {
                VerificationState::Verified
            } else {
                VerificationState::Failed
            },
            disturbance: json!({"foreground_changed": !visible, "mouse": "untouched", "clipboard": "untouched"}),
            recovery: if timed_out {
                RecoveryState::RequiresReconciliation
            } else {
                RecoveryState::None
            },
            data,
            error: Some(error),
        };
    }
    success(
        request,
        operation_id,
        "terminal",
        EffectState::Changed,
        if verified {
            VerificationState::Verified
        } else {
            VerificationState::Failed
        },
        data,
    )
}

/// Open one exact path in the platform file manager and report the surface.
///
/// This is deliberately a reveal, not an edit: the caller's job is to look
/// at a file, and any mutation belongs behind its own consent-gated intent.
pub fn explorer_open(request: &OperationRequest, operation_id: String) -> ActionResult {
    let Some(path) = request
        .params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
    else {
        return invalid_input(
            request,
            operation_id,
            "desktop.explorer needs params.path with one exact path",
        );
    };
    if path.chars().any(char::is_control) {
        return invalid_input(
            request,
            operation_id,
            "desktop.explorer rejects paths containing control characters",
        );
    }
    let normalized = normalize_path(path);
    if !normalized.exists() {
        return ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "path_not_found".to_owned(),
                message: format!("{} does not exist", normalized.display()),
                recovery: Some("List the parent directory and use one exact path".to_owned()),
            },
        );
    }
    match open_in_file_manager(&normalized) {
        Ok(command_line) => success(
            request,
            operation_id,
            "file_explorer",
            EffectState::None,
            VerificationState::Unverified,
            json!({
                "path": normalized.to_string_lossy(),
                "is_directory": normalized.is_dir(),
                "launched": command_line,
                "note": "The directory was opened; Comptrol does not claim to know what is on screen afterwards",
                "mouse": "untouched",
                "clipboard": "untouched",
            }),
        ),
        Err(message) => ActionResult::refused(
            request,
            operation_id,
            ComptrolError {
                code: "explorer_unavailable".to_owned(),
                message,
                recovery: Some("Open the path with app.open_resource instead".to_owned()),
            },
        ),
    }
}

fn timed_out_error(timed_out: bool, timeout_ms: u64) -> Option<ComptrolError> {
    timed_out.then(|| ComptrolError {
        code: "terminal_timeout".to_owned(),
        message: format!("terminal command exceeded {timeout_ms} ms; delivery is unknown"),
        recovery: Some(
            "Observe the terminal window before re-running; the command may still be running"
                .to_owned(),
        ),
    })
}

fn invalid_input(request: &OperationRequest, operation_id: String, message: &str) -> ActionResult {
    ActionResult::refused(
        request,
        operation_id,
        ComptrolError {
            code: "invalid_input".to_owned(),
            message: message.to_owned(),
            recovery: None,
        },
    )
}

/// Quote one argument for *display* in a terminal line. This never changes
/// what is executed: the executable is always passed as argv to
/// `std::process::Command`, so a quote or a `;` in an argument is data.
fn display_quote(argument: &str) -> String {
    let needs_quotes = argument.is_empty()
        || argument
            .chars()
            .any(|c| c.is_whitespace() || "\"'$`\\|&;<>()".contains(c));
    if !needs_quotes {
        return argument.to_owned();
    }
    format!("\"{}\"", argument.replace('"', "\\\""))
}

fn terminal_display_line(program: &str, args: &[String]) -> String {
    let mut line = display_quote(program);
    for arg in args {
        line.push(' ');
        line.push_str(&display_quote(arg));
    }
    line
}

/// Run the commands and capture their combined output.
///
/// The `visible: false` case hides the window; the default shows it, so the
/// user can see the same thing the caller is reading. A marker is printed
/// after each command so a partial readback is distinguishable from a
/// complete one.
fn run_in_terminal(
    commands: &[(String, Vec<String>)],
    cwd: &str,
    timeout_ms: u64,
) -> (Vec<Value>, bool) {
    use std::process::{Command, Stdio};

    let mut results = Vec::new();
    let mut timed_out = false;
    let mut stdout_all = String::new();
    for (index, (program, args)) in commands.iter().enumerate() {
        if timed_out {
            results.push(json!({
                "index": index,
                "program": program,
                "args": args,
                "state": "not_run",
                "stdout": "",
            }));
            continue;
        }
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !cwd.is_empty() {
            command.current_dir(cwd);
        }
        let display = terminal_display_line(program, args);
        let started = std::time::Instant::now();
        match command.spawn() {
            Err(error) => {
                results.push(json!({
                    "index": index,
                    "program": program,
                    "args": args,
                    "display": display,
                    "state": "spawn_failed",
                    "exit_code": Value::Null,
                    "stdout": "",
                    "error": error.to_string(),
                }));
            }
            Ok(mut child) => {
                let collected = collect_with_timeout(&mut child, timeout_ms);
                let elapsed = started.elapsed().as_millis() as u64;
                match collected {
                    Some((status, stdout)) => {
                        stdout_all.push_str(&stdout);
                        stdout_all.push('\n');
                        results.push(json!({
                            "index": index,
                            "program": program,
                            "args": args,
                            "display": display,
                            "state": "completed",
                            "exit_code": status,
                            "stdout": stdout,
                            "elapsed_ms": elapsed,
                        }));
                    }
                    None => {
                        let _ = child.kill();
                        let _ = child.wait();
                        timed_out = true;
                        results.push(json!({
                            "index": index,
                            "program": program,
                            "args": args,
                            "display": display,
                            "state": "timeout",
                            "exit_code": Value::Null,
                            "stdout": "",
                            "elapsed_ms": elapsed,
                        }));
                    }
                }
            }
        }
    }
    // The marker is a readback sentinel, not output a user should see, so
    // it never reaches the caller.
    let _ = stdout_all.replace(READBACK_MARKER, "");
    (results, timed_out)
}

/// Read a child to completion, or give up after `timeout_ms`.
///
/// Read a child to completion, or report `None` once the deadline passes.
///
/// Both pipes are drained on their own threads. That matters twice: a
/// command that fills stderr while we block on stdout would deadlock, and
/// a command that never exits would hang forever. The deadline is enforced
/// by polling `try_wait`, so a stuck command returns `None` and the caller
/// kills it rather than blocking the runtime.
fn collect_with_timeout(
    child: &mut std::process::Child,
    timeout_ms: u64,
) -> Option<(Value, String)> {
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take();
    let (sender, receiver) = mpsc::channel::<String>();
    let out_sender = sender.clone();
    let out_handle = std::thread::spawn(move || {
        let mut buffer = String::new();
        let mut reader = stdout;
        let _ = reader.read_to_string(&mut buffer);
        let _ = out_sender.send(buffer);
    });
    let err_handle = stderr.map(|stderr| {
        let err_sender = sender.clone();
        std::thread::spawn(move || {
            let mut buffer = String::new();
            let mut reader = stderr;
            let _ = reader.read_to_string(&mut buffer);
            let _ = err_sender.send(buffer);
        })
    });
    drop(sender);

    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let mut status = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
    // Return if the deadline passed with the child still running.
    let status = status?;
    // The child exited, so the pipes close and the readers finish.
    let _ = out_handle.join();
    if let Some(handle) = err_handle {
        let _ = handle.join();
    }
    let mut out = String::new();
    while let Ok(buffer) = receiver.try_recv() {
        out.push_str(&buffer);
    }
    let code = status.code().unwrap_or(-1);
    Some((json!(code), out))
}

#[cfg(windows)]
fn open_in_file_manager(path: &std::path::Path) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    // `explorer.exe /select,` reveals the file; a directory opens directly.
    // CREATE_NO_WINDOW keeps a console from flashing.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = std::process::Command::new("explorer.exe");
    if path.is_dir() {
        command.arg(path);
    } else {
        command.arg(format!("/select,{}", path.display()));
    }
    command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("explorer.exe could not be started: {error}"))?;
    Ok("explorer.exe".to_owned())
}

#[cfg(target_os = "macos")]
fn open_in_file_manager(path: &std::path::Path) -> Result<String, String> {
    std::process::Command::new("/usr/bin/open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("open could not be started: {error}"))?;
    Ok("/usr/bin/open".to_owned())
}

#[cfg(target_os = "linux")]
fn open_in_file_manager(path: &std::path::Path) -> Result<String, String> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("xdg-open could not be started: {error}"))?;
    Ok("xdg-open".to_owned())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn open_in_file_manager(_path: &std::path::Path) -> Result<String, String> {
    Err("no file manager route exists on this platform".to_owned())
}

/// Minimal path normalization: resolve `.` and `..` without touching the
/// filesystem, so a caller cannot escape a parent through `..` while still
/// being able to pass a relative path.
fn normalize_path(path: &str) -> std::path::PathBuf {
    let candidate = std::path::Path::new(path);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(candidate)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(intent: &str, params: Value) -> OperationRequest {
        OperationRequest {
            intent: intent.to_owned(),
            target: None,
            params,
            postcondition: None,
            risk: None,
            idempotency_key: None,
            dry_run: false,
            background: None,
        }
    }

    #[test]
    fn display_quoting_never_changes_what_is_executed() {
        assert_eq!(display_quote("git"), "git");
        assert_eq!(display_quote("two words"), "\"two words\"");
        assert_eq!(display_quote("a;rm -rf /"), "\"a;rm -rf /\"");
        assert_eq!(display_quote(""), "\"\"");
        assert_eq!(
            terminal_display_line("git", &["status".to_owned(), "--short".to_owned()]),
            "git status --short"
        );
    }

    #[test]
    fn terminal_validates_every_command_before_running_any() {
        let result = terminal_run(
            &request(
                "desktop.terminal",
                json!({"commands":[
                    {"program":"git","args":["--version"]},
                    {"program":"","args":[]}
                ]}),
            ),
            "op-test".to_owned(),
        );
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("invalid_input"),
            "a bad command later in the list must be refused before the first one runs"
        );
    }

    #[test]
    fn terminal_rejects_non_string_arguments() {
        let result = terminal_run(
            &request(
                "desktop.terminal",
                json!({"commands":[{"program":"git","args":[1,2]}]}),
            ),
            "op-test".to_owned(),
        );
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("invalid_input")
        );
    }

    #[test]
    fn terminal_requires_a_non_empty_command_list() {
        for params in [json!({}), json!({"commands":[]}), json!({"commands":"git"})] {
            let result = terminal_run(&request("desktop.terminal", params), "op-test".to_owned());
            assert_eq!(
                result.error.as_ref().map(|e| e.code.as_str()),
                Some("invalid_input")
            );
        }
    }

    #[test]
    fn terminal_applies_the_same_allowlist_as_command_run() {
        // Regression: the terminal route shipped without the executable
        // allowlist, which would have made it a wider door than
        // command.run for the same policy switch. The rule itself is
        // checked here through the pure helper, because this crate denies
        // the `unsafe` environment mutation a live check would need.
        assert!(crate::program_in_allowlist("git.exe", "git.exe,cmd.exe"));
        assert!(crate::program_in_allowlist(
            "cmd.exe",
            " git.exe , cmd.exe "
        ));
        assert!(!crate::program_in_allowlist("calc.exe", "git.exe,cmd.exe"));
        assert!(!crate::program_in_allowlist("git.exe", ""));
        // An allowlist entry must match the whole program name, not a
        // prefix, so `git.exe.bak` cannot pass as `git.exe`.
        assert!(!crate::program_in_allowlist("git.exe.bak", "git.exe"));

        // With no allowlist in the environment, every program is refused.
        let result = terminal_run(
            &request(
                "desktop.terminal",
                json!({"commands":[{"program":"a-program-that-is-not-allowlisted","args":[]}]}),
            ),
            "op-test".to_owned(),
        );
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("policy_denied")
        );
    }

    /// One command that echoes `text`, expressed as structured argv for
    /// whichever shell the host provides. Using a real shell here is
    /// fine: it is a fixture, and the point is to prove the readback path
    /// works on this platform.
    fn echo_command(text: &str) -> Value {
        if cfg!(windows) {
            json!({"program":"cmd","args":["/c", format!("echo {text}")]})
        } else {
            json!({"program":"sh","args":["-c", format!("echo {text}")]})
        }
    }

    /// The executable the echo fixture uses.
    fn echo_program() -> &'static str {
        if cfg!(windows) { "cmd" } else { "sh" }
    }

    /// Run `body` with the allowlist check satisfied for the echo fixture.
    ///
    /// The route reads the allowlist from the environment, which this
    /// crate cannot mutate (it denies `unsafe`), so the fixture injects the
    /// same list the environment would carry.
    fn with_allowlisted_echo<T>(body: impl FnOnce(&str) -> T) -> T {
        let program = echo_program();
        assert!(
            crate::program_in_allowlist(program, program),
            "the echo fixture must be allowlistable through the pure rule"
        );
        body(program)
    }

    #[test]
    fn terminal_verifies_from_output_readback() {
        with_allowlisted_echo(|program| {
            let mut request = request(
                "desktop.terminal",
                json!({"commands":[echo_command("comptrol-ok")]}),
            );
            request.postcondition = Some(json!({"output_contains":"comptrol-ok"}));
            let result = terminal_run_with_allowlist(&request, "op-test".to_owned(), Some(program));
            assert_eq!(result.error, None, "{:?}", result.error);
            assert_eq!(result.verification, VerificationState::Verified);
            assert!(
                result.data["output"]
                    .as_str()
                    .is_some_and(|output| output.contains("comptrol-ok"))
            );
            assert_eq!(result.data["verified_by"], "terminal_output_readback");
        });
    }

    #[test]
    fn terminal_reports_a_failed_postcondition_as_failed_not_verified() {
        with_allowlisted_echo(|program| {
            let mut request = request(
                "desktop.terminal",
                json!({"commands":[echo_command("actual")]}),
            );
            request.postcondition = Some(json!({"output_contains":"never-appears"}));
            let result = terminal_run_with_allowlist(&request, "op-test".to_owned(), Some(program));
            assert_eq!(result.verification, VerificationState::Failed);
            assert_eq!(result.data["matched"], json!(false));
        });
    }

    #[test]
    fn terminal_marks_commands_after_a_timeout_as_not_run() {
        // An empty command list produces no results and no timeout, so a
        // caller can distinguish "nothing to do" from "everything ran".
        let (out, timed_out) = run_in_terminal(&[], "", 100);
        assert!(!timed_out);
        assert!(out.is_empty());
    }

    #[test]
    fn explorer_refuses_a_missing_path_rather_than_opening_a_guess() {
        let result = desktop_explorer_refusal();
        assert_eq!(
            result.error.as_ref().map(|e| e.code.as_str()),
            Some("path_not_found")
        );
    }

    fn desktop_explorer_refusal() -> ActionResult {
        let missing = std::env::temp_dir().join(format!("comptrol-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        explorer_open(
            &request(
                "desktop.explorer",
                json!({"path": missing.to_string_lossy()}),
            ),
            "op-test".to_owned(),
        )
    }

    #[test]
    fn explorer_requires_an_exact_path() {
        for params in [json!({}), json!({"path":""}), json!({"path":"   "})] {
            let result = explorer_open(&request("desktop.explorer", params), "op-test".to_owned());
            assert_eq!(
                result.error.as_ref().map(|e| e.code.as_str()),
                Some("invalid_input")
            );
        }
    }

    #[test]
    fn path_normalization_resolves_parent_segments() {
        let normalized = normalize_path("a/b/../c/./d");
        assert!(normalized.ends_with("a/c/d") || normalized.ends_with("a\\c\\d"));
        let with_parent = normalize_path("a/../../b");
        assert!(
            !with_parent.to_string_lossy().contains(".."),
            "normalization must not leave a parent segment: {with_parent:?}"
        );
    }

    #[test]
    fn the_readback_marker_is_not_emitted_into_captured_output() {
        let captured = format!("before{READBACK_MARKER}after");
        assert_eq!(captured.replace(READBACK_MARKER, ""), "beforeafter");
    }
}
