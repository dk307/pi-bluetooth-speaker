//! Atomic state file persistence for amplifier state.

use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use tempfile::NamedTempFile;
use tracing::debug;

use crate::amp_state::AmpState;
use crate::error::AmpResult;

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

    /// Load state from file. Returns error if file missing or corrupt.
    pub fn load(&self) -> AmpResult<AmpState> {
        let file = File::open(&self.path).context("open state file")?;
        let reader = BufReader::new(file);
        let state: AmpState = serde_json::from_reader(reader).context("deserialize state")?;
        debug!(path = %self.path.display(), ?state, "Loaded amp state");
        Ok(state)
    }

    /// Save state to file atomically (temp file + rename).
    pub async fn save(&self, state: &AmpState) -> AmpResult<()> {
        // Serialize to JSON first (fail fast before any I/O)
        let json = serde_json::to_vec_pretty(state).context("serialize state")?;

        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).context("create state directory")?;
        }

        // Write to temp file in same directory (for atomic rename)
        let temp = NamedTempFile::new_in(self.path.parent().unwrap_or_else(|| Path::new(".")))
            .context("create temp file")?;

        // Write and fsync
        temp.as_file().write_all(&json).context("write temp file")?;
        temp.as_file().sync_all().context("fsync temp file")?;

        // Atomic rename
        temp.persist(&self.path)
            .context("rename temp to state file")?;

        debug!(path = %self.path.display(), ?state, "Saved amp state");
        Ok(())
    }
}
