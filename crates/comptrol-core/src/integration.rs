use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn list() -> Value {
    let home = home_dir();
    let entries = [
        ("codex", ".codex/config.toml", "mcp_servers", "toml"),
        ("claude-code", ".claude.json", "mcpServers", "json"),
        ("cursor", ".cursor/mcp.json", "mcpServers", "json"),
    ];
    Value::Array(
        entries
            .into_iter()
            .map(|(client, relative, key, format)| {
                let path = home.join(relative);
                json!({
                    "client": client,
                    "config": path,
                    "exists": path.is_file(),
                    "format": format,
                    "supported": true,
                    "server_key": key
                })
            })
            .collect(),
    )
}

pub fn proposal(client: &str, path: &Path) -> Result<Value, String> {
    refuse_symlink(path)?;
    if client == "codex" {
        return toml_proposal(path);
    }
    let key = server_key(client)?;
    let current = read_config(path)?;
    let proposed = merge_config(current.clone(), key, command())?;
    Ok(json!({
        "client": client,
        "config": path,
        "server_key": key,
        "current": current,
        "proposed": proposed,
        "apply_required": true
    }))
}

pub fn apply(client: &str, path: &Path) -> Result<Value, String> {
    if client == "codex" {
        return apply_toml(path);
    }
    let plan = proposal(client, path)?;
    let proposed = plan
        .get("proposed")
        .ok_or_else(|| "integration proposal has no proposed config".to_owned())?;
    let bytes = serde_json::to_vec_pretty(proposed).map_err(|error| error.to_string())?;
    let backup = backup(path)?;
    atomic_write(path, &bytes).map_err(|error| error.to_string())?;
    let _: Value = serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
        .map_err(|error| format!("written config failed validation: {error}"))?;
    Ok(json!({
        "client": client,
        "config": path,
        "applied": true,
        "backup": backup,
        "smoke_test": "json_valid"
    }))
}

pub fn undo(path: &Path) -> Result<Value, String> {
    refuse_symlink(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = format!(
        "{}.comptrol.bak.",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    );
    let mut backups = fs::read_dir(parent)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    backups.sort();
    let Some(backup) = backups.pop() else {
        return Err("no Comptrol integration backup exists".to_owned());
    };
    if path.is_file() {
        let current = PathBuf::from(format!("{}.comptrol.undo.{}", path.display(), timestamp()));
        fs::copy(path, current).map_err(|error| error.to_string())?;
    }
    fs::copy(&backup, path).map_err(|error| error.to_string())?;
    validate_written(path)?;
    Ok(json!({ "config": path, "restored": true, "backup": backup }))
}

fn merge_config(mut current: Value, key: &str, command: PathBuf) -> Result<Value, String> {
    let root = current
        .as_object_mut()
        .ok_or_else(|| "client config must contain a JSON object".to_owned())?;
    let servers = root
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| format!("client config field {key} must be an object"))?;
    servers.insert(
        "comptrol".to_owned(),
        json!({ "command": command, "args": ["mcp"] }),
    );
    Ok(current)
}

fn read_config(path: &Path) -> Result<Value, String> {
    refuse_symlink(path)?;
    if !path.exists() {
        return Ok(json!({}));
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "unsupported or invalid JSON config at {}: {error}",
            path.display()
        )
    })
}

fn toml_proposal(path: &Path) -> Result<Value, String> {
    let current = if path.exists() {
        fs::read_to_string(path).map_err(|error| error.to_string())?
    } else {
        String::new()
    };
    if !current.is_empty() {
        toml::from_str::<toml::Value>(&current)
            .map_err(|error| format!("invalid Codex TOML config: {error}"))?;
    }
    let proposed = merge_toml(&current, &command())?;
    toml::from_str::<toml::Value>(&proposed)
        .map_err(|error| format!("generated Codex TOML failed validation: {error}"))?;
    Ok(json!({
        "client": "codex",
        "config": path,
        "server_key": "mcp_servers",
        "format": "toml",
        "current_text": current,
        "proposed_text": proposed,
        "apply_required": true
    }))
}

fn apply_toml(path: &Path) -> Result<Value, String> {
    let plan = toml_proposal(path)?;
    let proposed = plan
        .get("proposed_text")
        .and_then(Value::as_str)
        .ok_or_else(|| "Codex proposal has no proposed TOML".to_owned())?;
    let backup = backup(path)?;
    atomic_write(path, proposed.as_bytes()).map_err(|error| error.to_string())?;
    validate_written(path)?;
    Ok(json!({
        "client": "codex",
        "config": path,
        "applied": true,
        "backup": backup,
        "smoke_test": "toml_valid"
    }))
}

fn merge_toml(current: &str, command: &Path) -> Result<String, String> {
    let command = command
        .to_str()
        .ok_or_else(|| "Comptrol binary path is not valid UTF8".to_owned())?;
    let command = toml::Value::String(command.to_owned()).to_string();
    let block = format!("[mcp_servers.comptrol]\ncommand = {command}\nargs = [\"mcp\"]\n");
    let header = "[mcp_servers.comptrol]";
    let mut offset = 0;
    let mut start = None;
    let mut end = current.len();
    for line in current.split_inclusive('\n') {
        let trimmed = line.trim();
        if start.is_none() && trimmed == header {
            start = Some(offset);
        } else if start.is_some() && trimmed.starts_with('[') && trimmed.ends_with(']') {
            end = offset;
            break;
        }
        offset += line.len();
    }
    if let Some(start) = start {
        let mut merged = current.to_owned();
        merged.replace_range(start..end, &block);
        Ok(merged)
    } else {
        let mut merged = current.to_owned();
        if !merged.is_empty() && !merged.ends_with('\n') {
            merged.push('\n');
        }
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(&block);
        Ok(merged)
    }
}

fn backup(path: &Path) -> Result<Option<PathBuf>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let backup = PathBuf::from(format!("{}.comptrol.bak.{}", path.display(), timestamp()));
    fs::copy(path, &backup).map_err(|error| error.to_string())?;
    Ok(Some(backup))
}

fn validate_written(path: &Path) -> Result<(), String> {
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    if path.extension().and_then(|extension| extension.to_str()) == Some("toml") {
        toml::from_str::<toml::Value>(&contents)
            .map(|_| ())
            .map_err(|error| format!("restored config failed TOML validation: {error}"))
    } else {
        serde_json::from_str::<Value>(&contents)
            .map(|_| ())
            .map_err(|error| format!("restored config failed JSON validation: {error}"))
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.comptrol.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config"),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, path)
}

fn refuse_symlink(path: &Path) -> Result<(), String> {
    if path.is_symlink() {
        return Err(format!(
            "refusing symlinked client config {}",
            path.display()
        ));
    }
    Ok(())
}

fn server_key(client: &str) -> Result<&'static str, String> {
    match client {
        "codex" => Ok("mcp_servers"),
        "claude-code" | "cursor" => Ok("mcpServers"),
        _ => Err(format!("unsupported client {client}")),
    }
}

fn command() -> PathBuf {
    std::env::var_os("COMPTROL_BIN")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("comptrol"))
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_preserves_unknown_json_fields() {
        let config = merge_config(
            json!({ "other": { "keep": true } }),
            "mcpServers",
            PathBuf::from("comptrol"),
        )
        .expect("proposal");
        assert_eq!(config["other"]["keep"], true);
        assert_eq!(config["mcpServers"]["comptrol"]["args"][0], "mcp");
    }

    #[test]
    fn toml_client_uses_mcp_servers() {
        assert_eq!(server_key("codex"), Ok("mcp_servers"));
    }

    #[test]
    fn toml_merge_preserves_other_tables() {
        let merged = merge_toml(
            "model = \"x\"\n\n[mcp_servers.other]\ncommand = \"other\"\n",
            Path::new("comptrol"),
        )
        .expect("toml");
        assert!(merged.contains("model = \"x\""));
        assert!(merged.contains("[mcp_servers.other]"));
        assert!(merged.contains("[mcp_servers.comptrol]"));
    }

    #[test]
    fn apply_and_undo_round_trip_with_backup() {
        let directory = std::env::temp_dir().join(format!("comptrol-integrate-{}", timestamp()));
        fs::create_dir_all(&directory).expect("directory");
        let path = directory.join("mcp.json");
        fs::write(&path, br#"{"unknown":{"keep":true}}"#).expect("config");
        let applied = apply("cursor", &path).expect("apply");
        assert_eq!(applied["applied"], true);
        let current: Value = serde_json::from_slice(&fs::read(&path).expect("read applied"))
            .expect("valid applied config");
        assert_eq!(current["unknown"]["keep"], true);
        assert!(current["mcpServers"]["comptrol"]["args"][0] == "mcp");
        undo(&path).expect("undo");
        let restored: Value = serde_json::from_slice(&fs::read(&path).expect("read restored"))
            .expect("valid restored config");
        assert_eq!(restored["unknown"]["keep"], true);
        assert!(restored.get("mcpServers").is_none());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn toml_apply_and_undo_round_trip() {
        let directory =
            std::env::temp_dir().join(format!("comptrol-integrate-toml-{}", timestamp()));
        fs::create_dir_all(&directory).expect("directory");
        let path = directory.join("config.toml");
        fs::write(&path, "model = \"x\"\n").expect("config");
        apply("codex", &path).expect("apply");
        let applied = fs::read_to_string(&path).expect("read applied");
        assert!(applied.contains("[mcp_servers.comptrol]"));
        undo(&path).expect("undo");
        assert_eq!(
            fs::read_to_string(&path).expect("read restored"),
            "model = \"x\"\n"
        );
        let _ = fs::remove_dir_all(directory);
    }
}
