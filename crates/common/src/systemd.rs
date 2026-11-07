//! Systemd service unit states and status helpers.

use serde::{Deserialize, Serialize};
use std::fmt::{self, Display, Formatter};

/// Operational state of a systemd background service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    /// Service unit is running and operational.
    Active,
    /// Service unit is loaded but currently inactive or stopped.
    Inactive,
    /// Service unit entered a failed or crashed state.
    Failed,
    /// Service unit file was not found on the host system.
    NotFound,
}

impl ServiceState {
    /// Returns `true` if the service is actively running.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Returns a human-readable state description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Active => "Running",
            Self::Inactive => "INACTIVE / STOPPED",
            Self::Failed => "FAILED",
            Self::NotFound => "NOT INSTALLED / NOT FOUND",
        }
    }
}

impl Display for ServiceState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.description())
    }
}

/// Parses active and load state strings into a [`ServiceState`].
#[must_use]
pub fn parse_systemd_state(active: &str, load: &str) -> ServiceState {
    if active == "active" {
        ServiceState::Active
    } else if load == "not-found" || active == "not-found" {
        ServiceState::NotFound
    } else if active == "failed" {
        ServiceState::Failed
    } else {
        ServiceState::Inactive
    }
}
