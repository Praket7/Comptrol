//! `comptrol setup` — one command from a fresh machine to a working install.
//!
//! The mission's seamlessness requirement: stage the browser extension with its
//! pinned ID → register the native messaging host (three OS paths) → doctor
//! ping → write MCP configs for Freebuff / Claude Code / Codex / OpenCode
//! (daemon mode) → verify. Fresh machine to working = one command plus one
//! extension click.
//!
//! The config generator is pure and unit-tested ([`ClientConfig`],
//! [`client_configs`], [`render_config`]); the orchestrator
//! ([`run_setup`]) writes those configs to disk and shells out to the existing
//! `extensions/comptrol-browser-bridge/install.py` for native-host registration
//! (that script is the single, tested source of truth for the three OS registry
//! / directory paths). Nothing here runs with more privilege than the invoking
//! user; the Windows native-host registration is per-user (HKCU).

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The exact Chrome extension ID the bridge authorizes. This is pinned so the
/// native-messaging host only ever accepts this one origin — a different
/// extension cannot talk to the host even if a user installs it.
pub const EXTENSION_ID: &str = "bpnakihocoimajcddkohnpgkepdmdkna";

/// One MCP client config to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    /// Human name shown in `setup` output (e.g. "Claude Code").
    pub client: &'static str,
    /// Path relative to the user's home (or config root), e.g.
    /// `Library/Application Support/Claude/claude_desktop_config.json`.
    pub relative_path: String,
    /// Serialized config body (JSON or TOML).
    pub body: String,
}

/// Direct stdio command. Resident sharing is provided separately by the npm
/// daemon launcher; setting its environment flag on this binary cannot enable it.
fn mcp_server_entry(binary: &str) -> Value {
    json!({
        "command": binary,
        "args": ["mcp"],
        "env": {}
    })
}

/// JSON `mcpServers` object shared by Freebuff, Claude Code, and OpenCode.
fn mcp_servers_object(binary: &str) -> Value {
    json!({ "comptrol": mcp_server_entry(binary) })
}

/// The four client configs for one platform. `binary` is the absolute path to
/// the `comptrol` executable; `os` is `"windows" | "macos" | "linux"`. This is
/// pure so tests can assert exact bodies without touching the filesystem.
pub fn client_configs(binary: &str, _os: &str) -> Vec<ClientConfig> {
    let servers = mcp_servers_object(binary);
    let mut configs = Vec::new();

    // Freebuff: JSON mcpServers under .freebuff/mcp.json (or per-platform
    // equivalent). Freebuff reads a project/user MCP file in mcpServers format.
    configs.push(ClientConfig {
        client: "Freebuff",
        relative_path: ".freebuff/mcp.json".to_owned(),
        body: serde_json::to_string_pretty(&json!({ "mcpServers": servers })).unwrap_or_default()
            + "\n",
    });

    // Claude Code: ~/.claude.json carries an mcpServers block; we emit a
    // standalone fragment the user (or `claude mcp add`) merges. We write the
    // full mcpServers file under .claude/ for a copy-paste-free install.
    configs.push(ClientConfig {
        client: "Claude Code",
        relative_path: ".claude.json".to_owned(),
        body: serde_json::to_string_pretty(&json!({ "mcpServers": servers })).unwrap_or_default()
            + "\n",
    });

    // Codex: TOML under .codex/config.toml using mcp_servers.<name> tables.
    let codex_body =
        toml::to_string(&json!({"mcp_servers": {"comptrol": mcp_server_entry(binary)}}))
            .expect("client config contains TOML-compatible values");
    configs.push(ClientConfig {
        client: "Codex",
        relative_path: ".codex/config.toml".to_owned(),
        body: codex_body,
    });

    // OpenCode v2 uses an argv array and mcp.servers, not mcpServers.
    configs.push(ClientConfig {
        client: "OpenCode",
        relative_path: ".config/opencode/opencode.json".to_owned(),
        body: serde_json::to_string_pretty(&json!({
            "$schema": "https://opencode.ai/config.json",
            "mcp": {"servers": {"comptrol": {
                "type": "local", "command": [binary, "mcp"],
                "environment": {}
            }}}
        }))
        .expect("valid config")
            + "\n",
    });

    configs
}

/// Render a config body for one named client. Useful for `comptrol setup
/// --print <client>` and for tests.
pub fn render_config(binary: &str, os: &str, client: &str) -> Option<String> {
    client_configs(binary, os)
        .into_iter()
        .find(|config| config.client.eq_ignore_ascii_case(client))
        .map(|config| config.body)
}

/// Write the four client configs under `home`. Existing files are preserved
/// (never silently overwritten) — `setup` reports them so the user can merge.
/// Returns one line of human-readable status per client.
pub fn write_client_configs(binary: &str, os: &str, home: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    for config in client_configs(binary, os) {
        let path = home.join(&config.relative_path);
        if path.exists() {
            lines.push(format!(
                "  {} config exists: {} (left unchanged)",
                config.client,
                path.display()
            ));
            continue;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, &config.body) {
            Ok(()) => lines.push(format!(
                "  {} config written: {}",
                config.client,
                path.display()
            )),
            Err(error) => lines.push(format!(
                "  {} config FAILED: {} ({error})",
                config.client,
                path.display()
            )),
        }
    }
    lines
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn current_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

fn default_python_command(os: &str) -> &'static str {
    if os == "windows" { "python" } else { "python3" }
}

/// The staged-extension + native-host directory the bridge installer expects.
fn extension_source_dir() -> PathBuf {
    // The installer lives in the repo next to the staged package; resolve it
    // relative to the running binary's checkout when possible.
    let candidates = [
        PathBuf::from("extensions/comptrol-browser-bridge/install.py"),
        PathBuf::from("../extensions/comptrol-browser-bridge/install.py"),
        PathBuf::from("../../extensions/comptrol-browser-bridge/install.py"),
    ];
    for candidate in candidates {
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("extensions/comptrol-browser-bridge/install.py")
}

/// `comptrol setup` orchestrator. Steps:
///   1. write the four client MCP configs (pure, preserved-if-exists),
///   2. register the native messaging host via `install.py` (three OS paths),
///   3. print the extension-ID pin and the one remaining manual action
///      (click Reload on the extension in `chrome://extensions`).
///      Returns a process exit code (0 = success).
pub fn run_setup(args: &[String]) -> i32 {
    let home = home_dir();
    let os = current_os();
    let binary = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "comptrol".to_owned());

    let print_only = args
        .iter()
        .position(|arg| arg == "--print")
        .and_then(|index| args.get(index + 1))
        .cloned();

    if let Some(client) = print_only {
        match render_config(&binary, os, &client) {
            Some(body) => {
                print!("{body}");
                return 0;
            }
            None => {
                eprintln!(
                    "unknown client {client}; expected one of: Freebuff, Claude Code, Codex, OpenCode"
                );
                return 2;
            }
        }
    }

    println!("comptrol setup (os={os})");
    println!("binary: {binary}");
    println!();
    println!("== MCP client configs ==");
    let mut failed = false;
    for line in write_client_configs(&binary, os, &home) {
        failed |= line.contains("FAILED:");
        println!("{line}");
    }

    println!();
    println!("== native messaging host ==");
    let installer = extension_source_dir();
    if installer.exists() {
        // macOS and most Linux distributions expose Python 3 as `python3`;
        // Windows installations commonly expose it as `python`. Allow an
        // explicit override for managed environments and keep failures visible.
        let python = std::env::var("COMPTROL_PYTHON")
            .unwrap_or_else(|_| default_python_command(os).to_owned());
        let status = std::process::Command::new(&python)
            .arg(&installer)
            .arg("--extension-id")
            .arg(EXTENSION_ID)
            .status();
        match status {
            Ok(code) if code.success() => {
                println!("  native host registered for extension {EXTENSION_ID}")
            }
            Ok(code) => {
                failed = true;
                println!("  installer exited with {code} (see output above)");
            }
            Err(error) => {
                failed = true;
                println!(
                    "  could not run {} ({error}); run it manually to register the native host",
                    installer.display()
                );
            }
        }
    } else {
        failed = true;
        println!(
            "  installer not found at {}; native host not registered",
            installer.display()
        );
    }

    println!();
    println!("== one manual step ==");
    println!("  Open chrome://extensions, find Comptrol, and click Reload (or Load unpacked)");
    println!("  pointing at the staged extension so the pinned ID {EXTENSION_ID} is live.");
    println!();
    if failed {
        eprintln!("setup incomplete: fix the errors above before connecting clients");
        1
    } else {
        println!(
            "Files prepared. Existing configs may need merging; extension loading and live verification are still required."
        );
        0
    }
}

#[cfg(test)]
mod tests {
    use super::{EXTENSION_ID, client_configs, default_python_command, render_config};
    use serde_json::Value;
    use std::path::Path;

    #[test]
    fn four_clients_are_generated() {
        let configs = client_configs("/usr/bin/comptrol", "linux");
        let names: Vec<&str> = configs.iter().map(|c| c.client).collect();
        assert_eq!(names, ["Freebuff", "Claude Code", "Codex", "OpenCode"]);
    }

    #[test]
    fn json_clients_share_one_comptrol_mcp_entry() {
        for config in client_configs("/usr/bin/comptrol", "linux") {
            if config.client == "Codex" || config.client == "OpenCode" {
                continue;
            }
            let parsed: Value = serde_json::from_str(&config.body).expect("valid JSON");
            let entry = &parsed["mcpServers"]["comptrol"];
            assert_eq!(entry["command"], "/usr/bin/comptrol");
            assert_eq!(entry["args"][0], "mcp");
            assert_eq!(entry["env"], serde_json::json!({}));
        }
    }

    #[test]
    fn codex_config_is_toml_with_an_mcp_servers_table() {
        let body = render_config("/usr/bin/comptrol", "linux", "codex").expect("codex config");
        assert!(body.contains("[mcp_servers.comptrol]"));
        assert!(body.contains("command = \"/usr/bin/comptrol\""));
        assert!(body.contains("args = [\"mcp\"]"));
    }

    #[test]
    fn windows_binary_path_survives_in_configs() {
        let configs = client_configs(r"C:\bin\comptrol.exe", "windows");
        for config in configs {
            if config.client == "Codex" {
                let parsed: toml::Value = toml::from_str(&config.body).expect("valid Windows TOML");
                assert_eq!(
                    parsed["mcp_servers"]["comptrol"]["command"].as_str(),
                    Some(r"C:\bin\comptrol.exe")
                );
                assert!(
                    parsed["mcp_servers"]["comptrol"]
                        .get("environment")
                        .is_none()
                );
                assert!(
                    parsed["mcp_servers"]["comptrol"]["env"]
                        .as_table()
                        .unwrap()
                        .is_empty()
                );
            } else {
                let parsed: Value = serde_json::from_str(&config.body).unwrap();
                let command = if config.client == "OpenCode" {
                    &parsed["mcp"]["servers"]["comptrol"]["command"][0]
                } else {
                    &parsed["mcpServers"]["comptrol"]["command"]
                };
                assert_eq!(command, r"C:\bin\comptrol.exe");
            }
        }
    }

    #[test]
    fn extension_id_is_the_pinned_32_char_lowercase_id() {
        assert_eq!(EXTENSION_ID.len(), 32);
        assert!(EXTENSION_ID.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn setup_uses_the_platform_python_name() {
        assert_eq!(default_python_command("windows"), "python");
        assert_eq!(default_python_command("macos"), "python3");
        assert_eq!(default_python_command("linux"), "python3");
    }

    #[test]
    fn write_preserves_existing_configs() {
        let temp = std::env::temp_dir().join(format!("comptrol-setup-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp);
        // Pre-create one client's config to prove it is not overwritten.
        let existing = temp.join(".freebuff/mcp.json");
        let _ = std::fs::create_dir_all(existing.parent().unwrap());
        std::fs::write(&existing, "sentinel").unwrap();
        super::write_client_configs("/usr/bin/comptrol", "linux", Path::new(&temp));
        let body = std::fs::read_to_string(&existing).unwrap();
        assert_eq!(body, "sentinel");
        let _ = std::fs::remove_dir_all(&temp);
    }
}
