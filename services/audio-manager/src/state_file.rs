//! Atomic state file persistence for amplifier and source-routing state.

use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Serialize, de::DeserializeOwned};
use tempfile::NamedTempFile;
use tracing::debug;

use crate::AmpError;
use crate::amp_state::AmpState;
use crate::error::AmpResult;

/// Reads JSON from `path`, returning `T::default()` when the file does not exist.
///
/// # Errors
/// Returns [`AmpError`] on I/O failure or malformed JSON.
pub(crate) fn load_json_or_default<T: DeserializeOwned + Default>(path: &Path) -> AmpResult<T> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "State file not found, returning default");
            return Ok(T::default());
        }
        Err(e) => return Err(AmpError::Io(e)),
    };
    let reader = BufReader::new(file);
    let state: T = serde_json::from_reader(reader).context("deserialize state")?;
    Ok(state)
}

/// Atomically writes `state` as pretty JSON to `path` (`NamedTempFile` + `fsync` + `rename`).
///
/// # Errors
/// Returns [`AmpError`] on serialization or filesystem failure.
pub(crate) fn save_json_atomic<T: Serialize>(path: &Path, state: &T) -> AmpResult<()> {
    let json = serde_json::to_vec_pretty(state).context("serialize state")?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create state directory")?;
    }

    let temp = NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))
        .context("create temp file")?;

    temp.as_file().write_all(&json).context("write temp file")?;
    temp.as_file().sync_all().context("fsync temp file")?;

    temp.persist(path).context("rename temp to state file")?;
    Ok(())
}

/// Manages the runtime state file for amp persistence.
#[derive(Debug, Clone)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    /// Create a new state file manager.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Load state from file. Returns default if missing, or error if corrupt.
    ///
    /// # Errors
    /// Returns [`AmpError`] on I/O or JSON deserialization failure.
    pub fn load(&self) -> AmpResult<AmpState> {
        let state: AmpState = load_json_or_default(&self.path)?;
        debug!(path = %self.path.display(), ?state, "Loaded amp state");
        Ok(state)
    }

    /// Save state to file atomically (temp file + rename) on a blocking thread.
    ///
    /// # Errors
    /// Returns [`AmpError`] on serialization, I/O, or blocking-task failure.
    pub async fn save(&self, state: &AmpState) -> AmpResult<()> {
        let path = self.path.clone();
        let state_copy = *state;
        tokio::task::spawn_blocking(move || save_json_atomic(&path, &state_copy))
            .await
            .context("State file save task panicked")??;
        debug!(path = %self.path.display(), ?state, "Saved amp state");
        Ok(())
    }
}
