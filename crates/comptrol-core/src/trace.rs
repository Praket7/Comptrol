use crate::{ActionResult, OperationRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceMode {
    PrivacyMinimal,
    Developer,
    FixtureFull,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TraceEntry {
    pub request: OperationRequest,
    pub result: ActionResult,
    pub mode: TraceMode,
}

#[derive(Clone, Debug)]
pub struct TraceRecorder {
    path: PathBuf,
    mode: TraceMode,
}

impl TraceRecorder {
    pub fn open(path: PathBuf, mode: TraceMode) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { path, mode })
    }

    pub fn append(&self, request: &OperationRequest, result: &ActionResult) -> io::Result<()> {
        let entry = TraceEntry {
            request: sanitize_request(request, self.mode),
            result: result.clone(),
            mode: self.mode,
        };
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        serde_json::to_writer(&mut file, &entry)?;
        file.write_all(b"\n")?;
        file.flush()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn read_trace(path: &Path) -> io::Result<Vec<TraceEntry>> {
    let file = std::fs::File::open(path)?;
    Ok(BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect())
}

fn sanitize_request(request: &OperationRequest, mode: TraceMode) -> OperationRequest {
    if matches!(mode, TraceMode::FixtureFull) {
        return request.clone();
    }
    let mut sanitized = request.clone();
    if let Value::Object(params) = &mut sanitized.params {
        for key in ["content", "value", "text", "body"] {
            if params.contains_key(key) {
                params.insert(key.to_owned(), json!({"redacted": true}));
            }
        }
    }
    if matches!(mode, TraceMode::PrivacyMinimal) {
        sanitized.postcondition = None;
    }
    sanitized
}
