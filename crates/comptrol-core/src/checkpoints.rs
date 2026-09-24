use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::io;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Checkpoint {
    pub id: String,
    pub source: PathBuf,
    pub backup: Option<PathBuf>,
    pub existed: bool,
}

#[derive(Debug)]
pub struct CheckpointStore {
    root: PathBuf,
    checkpoint_dir: Dir,
    sandbox_path: PathBuf,
    sandbox: Dir,
}

impl CheckpointStore {
    pub fn new(state_dir: &Path) -> io::Result<Self> {
        let root = state_dir.join("checkpoints");
        let sandbox_path = state_dir.join("sandbox");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&sandbox_path)?;
        let state = Dir::open_ambient_dir(state_dir, ambient_authority())?;
        Ok(Self {
            checkpoint_dir: state.open_dir("checkpoints")?,
            sandbox: state.open_dir("sandbox")?,
            root,
            sandbox_path,
        })
    }

    pub fn create(&self, id: &str, source: &Path) -> io::Result<Checkpoint> {
        validate_id(id)?;
        let source = self.sandbox_relative(source)?;
        self.create_parent_dirs(&source)?;

        let backup_name = format!("{id}.bak");
        let mut source_file = match self.sandbox.open(&source) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if let Some(source_file) = source_file.as_mut() {
            if !source_file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "checkpoint source must be a regular file",
                ));
            }
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            let mut backup = self.checkpoint_dir.open_with(&backup_name, &options)?;
            io::copy(source_file, &mut backup)?;
            backup.flush()?;
        }
        let existed = source_file.is_some();
        let checkpoint = Checkpoint {
            id: id.to_owned(),
            source: source.clone(),
            backup: existed.then(|| PathBuf::from(backup_name)),
            existed,
        };
        let manifest = format!("{id}.json");
        self.checkpoint_dir.write(
            manifest,
            serde_json::to_vec(&checkpoint).map_err(io::Error::other)?,
        )?;
        Ok(checkpoint)
    }

    pub fn write_sandbox_file(&self, id: &str, path: &Path, bytes: &[u8]) -> io::Result<()> {
        validate_id(id)?;
        validate_relative(path)?;
        self.create_parent_dirs(path)?;
        self.create(id, path)?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file = self.sandbox.open_with(path, &options)?;
        file.write_all(bytes)?;
        file.flush()
    }

    pub fn read_sandbox_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        validate_relative(path)?;
        let mut file = self.sandbox.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sandbox source must be a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub fn restore(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        validate_id(&checkpoint.id)?;
        let source = self.sandbox_relative(&checkpoint.source)?;
        self.create_parent_dirs(&source)?;
        if let Some(backup) = &checkpoint.backup {
            let backup = self.checkpoint_relative(backup)?;
            let mut backup_file = self.checkpoint_dir.open(backup)?;
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            let mut destination = self.sandbox.open_with(&source, &options)?;
            io::copy(&mut backup_file, &mut destination)?;
            destination.flush()?;
        } else {
            match self.sandbox.remove_file(&source) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn sandbox_path(&self) -> &Path {
        &self.sandbox_path
    }

    pub fn restore_id(&self, id: &str) -> io::Result<Checkpoint> {
        validate_id(id)?;
        let manifest = format!("{id}.json");
        let mut file = self.checkpoint_dir.open(manifest)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let checkpoint: Checkpoint = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        self.restore(&checkpoint)?;
        Ok(checkpoint)
    }

    fn create_parent_dirs(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            self.sandbox.create_dir_all(parent)?;
        }
        Ok(())
    }

    fn sandbox_relative(&self, path: &Path) -> io::Result<PathBuf> {
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.sandbox_path).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "checkpoint source is outside the sandbox",
                )
            })?
        } else {
            path
        };
        validate_relative(relative)?;
        Ok(relative.to_path_buf())
    }

    fn checkpoint_relative(&self, path: &Path) -> io::Result<PathBuf> {
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.root).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "checkpoint backup is outside the checkpoint store",
                )
            })?
        } else {
            path
        };
        validate_relative(relative)?;
        Ok(relative.to_path_buf())
    }
}

fn validate_id(id: &str) -> io::Result<()> {
    if !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid checkpoint id",
        ))
    }
}

fn validate_relative(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || path.to_string_lossy().contains('\\')
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sandbox path must be relative and contain no traversal",
        ));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn sandbox_paths_cannot_escape_through_symlinks() {
    use std::os::unix::fs::symlink;

    let root =
        std::env::temp_dir().join(format!("comptrol-checkpoint-link-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).expect("outside directory");
    let canary = outside.join("canary.txt");
    std::fs::write(&canary, b"untouched").expect("canary");
    let store = CheckpointStore::new(&root).expect("store");
    symlink(&canary, store.sandbox_path().join("file-link")).expect("file symlink");
    symlink(&outside, store.sandbox_path().join("directory-link")).expect("directory symlink");

    assert!(
        store
            .write_sandbox_file("file-link-write", Path::new("file-link"), b"changed")
            .is_err()
    );
    assert!(
        store
            .write_sandbox_file(
                "directory-link-write",
                Path::new("directory-link/other.txt"),
                b"changed"
            )
            .is_err()
    );
    assert!(store.read_sandbox_file(Path::new("file-link")).is_err());
    assert_eq!(std::fs::read(canary).expect("canary remains"), b"untouched");
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(windows)]
#[test]
fn sandbox_paths_cannot_escape_through_directory_junctions() {
    use std::process::Command;

    let root = std::env::temp_dir().join(format!(
        "comptrol-checkpoint-junction-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).expect("outside directory");
    let canary = outside.join("canary.txt");
    std::fs::write(&canary, b"untouched").expect("canary");
    let store = CheckpointStore::new(&root).expect("store");
    let junction = store.sandbox_path().join("outside-link");
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .status()
        .expect("create junction");
    if !status.success() {
        let _ = std::fs::remove_dir_all(root);
        return;
    }
    assert!(
        store
            .write_sandbox_file(
                "junction-write",
                Path::new("outside-link/canary.txt"),
                b"changed"
            )
            .is_err()
    );
    assert_eq!(std::fs::read(canary).expect("canary remains"), b"untouched");
    let _ = std::fs::remove_dir_all(root);
}
