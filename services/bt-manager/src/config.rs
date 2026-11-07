//! JSON configuration file loading for the Bluetooth manager service via the
//! layered `config` reader.
//!
//! Provides [`BtManagerConfig`], the strongly typed on-disk configuration storing the
//! status poll interval, cover-art provider toggle, and auto-trust pairing agent toggle,
//! loaded from `speaker_common::diag::DEFAULT_BT_CONFIG_PATH` (overridable via the `-c/--config` CLI flag).

use std::backtrace::Backtrace;
use std::fmt::{self, Formatter};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use config::Config as LayeredConfig;
use serde::{Deserialize, Serialize};

use crate::agent::AgentCapabilityMode;
use speaker_common::diag::DEFAULT_BT_CONFIG_PATH;
use speaker_common::impl_canonical_error_display;

/// Production default status poll interval in seconds (5 seconds).
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// Strongly typed poll interval in seconds.
///
/// The Bluetooth manager requires an interval of at least 1 second; a zero
/// value cannot inhabit the type, so [`BtManagerConfig`] never carries one and no
/// post-deserialization validation pass is required. Serializes transparently
/// as its raw `u64` rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct PollIntervalSecs(u64);

impl PollIntervalSecs {
    /// Production default poll interval constant (5 seconds).
    pub const DEFAULT: Self = Self::new(DEFAULT_POLL_INTERVAL_SECS);

    /// Const constructor enforcing the `>= 1` invariant at compile-time and runtime.
    ///
    /// # Panics
    /// Panics if `secs == 0`.
    #[must_use]
    pub const fn new(secs: u64) -> Self {
        assert!(secs >= 1, "poll interval must be at least 1 second");
        Self(secs)
    }

    /// Returns the raw interval in seconds.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Converts this interval into a [`Duration`].
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        Duration::from_secs(self.0)
    }
}

impl Default for PollIntervalSecs {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u64> for PollIntervalSecs {
    type Error = ConfigError;

    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        if secs >= 1 {
            Ok(Self::new(secs))
        } else {
            Err(ConfigError::invalid(format!(
                "invalid interval seconds '{secs}': interval must be at least 1 second"
            )))
        }
    }
}

impl From<PollIntervalSecs> for u64 {
    fn from(interval: PollIntervalSecs) -> Self {
        interval.0
    }
}

const _: () = {
    assert!(DEFAULT_POLL_INTERVAL_SECS >= 1);
};

/// Strongly typed Bluetooth manager configuration persisted as JSON on disk.
///
/// Missing fields deserialize to their defaults, so minimal config files
/// (e.g. `{}` or `{"interval_secs": 10}`) remain valid. [`PollIntervalSecs`]
/// enforces the minimum interval at the deserialization boundary, so an invalid
/// zero interval cannot inhabit this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BtManagerConfig {
    /// Status poll interval between monitor ticks in seconds (at least 1).
    pub interval_secs: PollIntervalSecs,
    /// Hosts the D-Bus cover-art provider alongside the monitor loop.
    pub cover_art: bool,
    /// Runs the Bluetooth auto-trust agent for incoming pairing requests.
    pub auto_trust: bool,
    /// `BlueZ` pairing agent capability negotiation mode.
    pub capability: AgentCapabilityMode,
}

impl Default for BtManagerConfig {
    fn default() -> Self {
        Self {
            interval_secs: PollIntervalSecs::DEFAULT,
            cover_art: true,
            auto_trust: true,
            capability: AgentCapabilityMode::DisplayYesNo,
        }
    }
}

impl BtManagerConfig {
    /// Loads the configuration from `path` via the layered `config` reader.
    ///
    /// A missing file is not an error: defaults are returned with a warning so a
    /// fresh appliance boots unconfigured. Any other read failure, malformed
    /// JSON, or a zero `interval_secs` is a hard [`ConfigError`].
    ///
    /// # Errors
    /// Returns [`ConfigError`] for non-missing I/O failures, malformed or
    /// non-conforming JSON (including invalid domain values), while the file
    /// exists.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::metadata(path) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %path.display(),
                    "config file not found; using built-in defaults"
                );
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(ConfigError::io(
                    format!("failed to read config {}", path.display()),
                    e,
                ));
            }
        }
        let settings = LayeredConfig::builder()
            .add_source(config::File::from(path).format(config::FileFormat::Json))
            .build()
            .map_err(|e| {
                ConfigError::config(format!("failed to read config {}", path.display()), e)
            })?;
        let cfg: Self = settings
            .try_deserialize::<Self>()
            .map_err(|e| ConfigError::config(format!("invalid config {}", path.display()), e))?;
        tracing::info!(
            path = %path.display(),
            interval_secs = cfg.interval_secs.value(),
            cover_art = cfg.cover_art,
            auto_trust = cfg.auto_trust,
            capability = cfg.capability.as_capability_str(),
            "loaded configuration from file"
        );
        Ok(cfg)
    }

    /// Atomically persists this configuration to `path` as pretty-printed JSON.
    ///
    /// Writes to a temporary sibling file first and renames it over `path` so
    /// partial writes cannot corrupt an existing file. Sets the file mode to
    /// world-readable `0644`.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for serialization or file write failures.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        use std::io::Write;

        let mut raw = serde_json::to_string_pretty(self).map_err(|e| {
            ConfigError::config(
                format!("failed to serialize config {}", path.display()),
                config::ConfigError::Foreign(Box::new(e)),
            )
        })?;
        raw.push('\n');

        let parent = path.parent().unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new().tempfile_in(parent).map_err(|e| {
            ConfigError::io(format!("failed to create config {}", path.display()), e)
        })?;
        let mut file = tmp.as_file();
        file.write_all(raw.as_bytes()).map_err(|e| {
            ConfigError::io(format!("failed to write config {}", path.display()), e)
        })?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .map_err(|e| {
                ConfigError::io(format!("failed to chmod config {}", path.display()), e)
            })?;
        file.sync_all().map_err(|e| {
            ConfigError::io(format!("failed to flush config {}", path.display()), e)
        })?;
        tmp.persist(path).map_err(|e| {
            ConfigError::io(
                format!("failed to install config {}", path.display()),
                e.error,
            )
        })?;
        tracing::info!(path = %path.display(), "saved configuration atomically");
        Ok(())
    }

    /// Resolves the effective config path from an optional CLI override.
    ///
    /// Checks the override path first, falling back to [`DEFAULT_BT_CONFIG_PATH`].
    #[must_use]
    pub fn resolve_path(override_path: Option<&Path>) -> PathBuf {
        if let Some(p) = override_path {
            p.to_path_buf()
        } else {
            PathBuf::from(DEFAULT_BT_CONFIG_PATH)
        }
    }
}

#[derive(Debug)]
pub(crate) enum ConfigErrorKind {
    Io {
        context: Box<str>,
        source: io::Error,
    },
    /// Failure surfaced by the layered `config` reader: file access, JSON
    /// parsing, or serde deserialization of the strongly typed [`BtManagerConfig`].
    /// The foreign error is boxed to keep [`ConfigError`] small by value.
    Config {
        context: Box<str>,
        source: Box<config::ConfigError>,
    },
    InvalidValue {
        message: Box<str>,
    },
}

/// Canonical configuration error wrapping its kind and a captured backtrace.
#[derive(Debug)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    backtrace: Backtrace,
}

impl ConfigError {
    pub(crate) fn new(kind: ConfigErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Builds an I/O failure for a named config operation.
    pub fn io(context: impl Into<Box<str>>, source: io::Error) -> Self {
        Self::new(ConfigErrorKind::Io {
            context: context.into(),
            source,
        })
    }

    /// Builds a layered-reader failure for a named config operation.
    pub fn config(context: impl Into<Box<str>>, source: config::ConfigError) -> Self {
        Self::new(ConfigErrorKind::Config {
            context: context.into(),
            source: Box::new(source),
        })
    }

    /// Builds a domain validation failure for an out-of-range config value.
    pub fn invalid(message: impl Into<Box<str>>) -> Self {
        Self::new(ConfigErrorKind::InvalidValue {
            message: message.into(),
        })
    }

    /// Returns `true` when the failure was an I/O error reading/writing the file.
    #[must_use]
    pub fn is_io(&self) -> bool {
        matches!(self.kind, ConfigErrorKind::Io { .. })
    }

    /// Returns `true` when the layered `config` reader (access/parse/deserialize) failed.
    #[must_use]
    pub fn is_config(&self) -> bool {
        matches!(self.kind, ConfigErrorKind::Config { .. })
    }

    /// Returns `true` when a value violated the domain validation rules.
    #[must_use]
    pub fn is_invalid_value(&self) -> bool {
        matches!(self.kind, ConfigErrorKind::InvalidValue { .. })
    }

    /// Returns the captured backtrace at error construction time.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ConfigErrorKind::Io { context, source } => {
                write!(f, "config I/O error: {context}: {source}")
            }
            ConfigErrorKind::Config { context, source } => {
                write!(f, "config error: {context}: {source}")
            }
            ConfigErrorKind::InvalidValue { message } => write!(f, "{message}"),
        }
    }
}

impl_canonical_error_display!(ConfigError);

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ConfigErrorKind::Io { source, .. } => Some(source),
            ConfigErrorKind::Config { source, .. } => Some(source),
            ConfigErrorKind::InvalidValue { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_secs_conversions() {
        let interval = PollIntervalSecs::new(10);
        assert_eq!(interval.value(), 10);
        assert_eq!(interval.as_duration(), Duration::from_secs(10));
        assert_eq!(u64::from(interval), 10);
        assert_eq!(PollIntervalSecs::try_from(10).unwrap(), interval);
        let err = PollIntervalSecs::try_from(0).unwrap_err();
        assert!(err.to_string().contains("invalid interval seconds '0'"));
    }

    #[test]
    #[should_panic(expected = "poll interval must be at least 1 second")]
    fn poll_interval_secs_new_rejects_zero() {
        let _ = PollIntervalSecs::new(0);
    }
}
