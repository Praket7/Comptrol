use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Checkpoint {
    pub id: String,
    pub source: PathBuf,
    pub backup: Option<PathBuf>,
    pub existed: bool,
}

#[derive(Clone, Debug)]
pub struct CheckpointStore {
    root: PathBuf,
}

impl CheckpointStore {
    pub fn new(state_dir: &Path) -> io::Result<Self> {
        let root = state_dir.join("checkpoints");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn create(&self, id: &str, source: &Path) -> io::Result<Checkpoint> {
        let backup = if source.exists() {
            let destination = self.root.join(format!("{id}.bak"));
            fs::copy(source, &destination)?;
            Some(destination)
        } else {
            None
        };
        let checkpoint = Checkpoint {
            id: id.to_owned(),
            source: source.to_path_buf(),
            backup,
            existed: source.exists(),
        };
        let manifest = self.root.join(format!("{id}.json"));
        fs::write(
            manifest,
            serde_json::to_vec(&checkpoint).map_err(io::Error::other)?,
        )?;
        Ok(checkpoint)
    }

    pub fn restore(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        if let Some(backup) = &checkpoint.backup {
            if let Some(parent) = checkpoint.source.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(backup, &checkpoint.source)?;
        } else if checkpoint.source.exists() {
            fs::remove_file(&checkpoint.source)?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn restore_id(&self, id: &str) -> io::Result<Checkpoint> {
        let manifest = self.root.join(format!("{id}.json"));
        let checkpoint: Checkpoint =
            serde_json::from_slice(&fs::read(manifest)?).map_err(io::Error::other)?;
        self.restore(&checkpoint)?;
        Ok(checkpoint)
    }
}
