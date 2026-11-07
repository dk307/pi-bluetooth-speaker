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

    /// Unchecked const constructor for fixed constants.
    #[must_use]
    pub const fn new(secs: u64) -> Self {
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
            Err(ConfigError::invalid(
                "invalid interval seconds '0': interval must be at least 1 second",
            ))
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
}

impl Default for BtManagerConfig {
    fn default() -> Self {
        Self {
            interval_secs: PollIntervalSecs::DEFAULT,
            cover_art: true,
            auto_trust: true,
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
        settings
            .try_deserialize::<Self>()
            .map_err(|e| ConfigError::config(format!("invalid config {}", path.display()), e))
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
        Ok(())
    }

    /// Resolves the effective config path from an optional CLI override.
    ///
    /// Checks the override path first, falling back to [`DEFAULT_BT_CONFIG_PATH`].
    #[must_use]
    pub fn resolve_path(override_path: Option<&Path>) -> PathBuf {
        Self::resolve_path_with(override_path, Path::exists)
    }

    /// Resolves the effective config path using an injectable filesystem probe.
    #[must_use]
    pub fn resolve_path_with<F>(override_path: Option<&Path>, _exists_fn: F) -> PathBuf
    where
        F: Fn(&Path) -> bool,
    {
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
    use speaker_common::diag::ScopedTempDir;
    use std::error::Error as _;

    #[test]
    fn defaults_are_sensible() {
        let config = BtManagerConfig::default();
        assert_eq!(config.interval_secs, PollIntervalSecs::DEFAULT);
        assert_eq!(config.interval_secs.value(), 5);
        assert_eq!(config.interval_secs.as_duration(), Duration::from_secs(5));
        assert!(config.cover_art);
        assert!(config.auto_trust);
    }

    #[test]
    fn poll_interval_secs_conversions() {
        let interval = PollIntervalSecs::new(10);
        assert_eq!(interval.value(), 10);
        assert_eq!(interval.as_duration(), Duration::from_secs(10));
        assert_eq!(u64::from(interval), 10);
        assert_eq!(PollIntervalSecs::try_from(10).unwrap(), interval);
        let _ = PollIntervalSecs::try_from(0).unwrap_err();
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let dir = ScopedTempDir::new("bt_cfg_missing");
        let path = dir.path().join("does-not-exist.json");
        let config = BtManagerConfig::load(&path).expect("missing file must yield defaults");
        assert_eq!(config, BtManagerConfig::default());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = ScopedTempDir::new("bt_cfg_roundtrip");
        let path = dir.path().join("config.json");
        let config = BtManagerConfig {
            interval_secs: PollIntervalSecs::new(10),
            cover_art: false,
            auto_trust: false,
        };
        config.save(&path).expect("save");
        let loaded = BtManagerConfig::load(&path).expect("load");
        assert_eq!(loaded, config);
        let raw = std::fs::read_to_string(&path).expect("raw read");
        assert!(raw.contains("\"interval_secs\": 10"), "{raw}");
        assert!(raw.contains("\"cover_art\": false"), "{raw}");
        assert!(raw.contains("\"auto_trust\": false"), "{raw}");
    }

    #[test]
    fn minimal_config_deserializes_with_defaults() {
        let config: BtManagerConfig = serde_json::from_str("{}").expect("empty object");
        assert_eq!(config, BtManagerConfig::default());
        let config: BtManagerConfig =
            serde_json::from_str(r#"{"interval_secs": 15}"#).expect("interval only");
        assert_eq!(config.interval_secs, PollIntervalSecs::new(15));
        assert!(config.cover_art);
        assert!(config.auto_trust);
    }

    #[test]
    fn load_malformed_json_is_hard_parse_error() {
        let dir = ScopedTempDir::new("bt_cfg_malformed");
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{not json").expect("write");
        let err = BtManagerConfig::load(&path).expect_err("malformed JSON must fail");
        assert!(err.is_config());
        assert!(err.to_string().contains("failed to read config"));
        assert!(err.summary().contains("config error"));
        assert!(!err.summary().contains("Backtrace"));
        assert!(err.source().is_some());
    }

    #[test]
    fn load_rejects_zero_interval() {
        let dir = ScopedTempDir::new("bt_cfg_zero_interval");
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"interval_secs": 0}"#).expect("write");
        let err = BtManagerConfig::load(&path).expect_err("zero interval must fail");
        assert!(err.is_config());
        let rendered = err.to_string();
        assert!(
            rendered.contains("interval must be at least 1 second"),
            "{rendered}"
        );
    }

    #[test]
    fn resolve_path_uses_override_and_default() {
        assert_eq!(
            BtManagerConfig::resolve_path(None),
            PathBuf::from(DEFAULT_BT_CONFIG_PATH)
        );
        assert_eq!(
            BtManagerConfig::resolve_path(Some(Path::new("/tmp/custom.json"))),
            PathBuf::from("/tmp/custom.json")
        );
        assert_eq!(
            BtManagerConfig::resolve_path_with(None, |_| true),
            PathBuf::from(DEFAULT_BT_CONFIG_PATH)
        );
    }

    #[test]
    fn error_predicates_and_backtrace_accessor() {
        let err = ConfigError::invalid("synthetic");
        assert!(err.is_invalid_value());
        assert!(!err.is_io() && !err.is_config());
        assert_eq!(err.summary(), "synthetic");
        assert!(matches!(
            err.backtrace().status(),
            std::backtrace::BacktraceStatus::Captured | std::backtrace::BacktraceStatus::Disabled
        ));
        let io_err = ConfigError::io("ctx", io::Error::other("boom"));
        assert!(io_err.is_io());
        let layered_err = ConfigError::config("ctx", config::ConfigError::Message("boom".into()));
        assert!(layered_err.is_config());
    }

    #[test]
    fn save_io_failure_on_directory_target() {
        let dir = ScopedTempDir::new("bt_cfg_save_dir");
        let config = BtManagerConfig::default();
        let target = dir.path().join("blocked");
        std::fs::create_dir(&target).expect("create blocking directory");
        let err = config
            .save(&target)
            .expect_err("rename over directory must fail");
        assert!(err.is_io());
        assert!(err.to_string().contains("failed to install config"));
    }

    #[test]
    fn save_installs_world_readable_mode() {
        let dir = ScopedTempDir::new("bt_cfg_mode");
        let path = dir.path().join("config.json");
        BtManagerConfig::default().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
    }
}
