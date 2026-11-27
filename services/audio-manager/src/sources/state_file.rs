//! Atomic persistence for source routing state (`/var/lib/speaker/source-state.json`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::AmpResult;
use crate::sources::registry::{AutoSwitchPolicy, SourceId};
use crate::state_file::{load_json_or_default, save_json_atomic};

/// Persisted multi-source selection, pin, and arbitration policy.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PersistedSourceState {
    /// Most recently active source ID, restored on boot.
    #[serde(default)]
    pub last_source: Option<SourceId>,
    /// Explicitly pinned source ID, if user-locked.
    #[serde(default)]
    pub pinned_source: Option<SourceId>,
    /// Persisted auto-switch policy.
    #[serde(default)]
    pub auto_switch_policy: AutoSwitchPolicy,
}

/// Manages atomic persistence of [`PersistedSourceState`].
#[derive(Debug, Clone)]
pub struct SourceStateFile {
    path: PathBuf,
}

impl SourceStateFile {
    /// Creates a state file handler at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Loads persisted source state, returning defaults if the file is missing.
    ///
    /// # Errors
    /// Returns [`crate::AmpError`] on I/O or JSON deserialization failure.
    pub fn load(&self) -> AmpResult<PersistedSourceState> {
        let state: PersistedSourceState = load_json_or_default(&self.path)?;
        debug!(path = %self.path.display(), ?state, "Loaded source state");
        Ok(state)
    }

    /// Saves source state atomically (`NamedTempFile` + `fsync` + `rename`).
    ///
    /// # Errors
    /// Returns [`crate::AmpError`] on serialization or filesystem failure.
    pub async fn save(&self, state: &PersistedSourceState) -> AmpResult<()> {
        save_json_atomic(&self.path, state)?;
        debug!(path = %self.path.display(), ?state, "Saved source state");
        Ok(())
    }
}
