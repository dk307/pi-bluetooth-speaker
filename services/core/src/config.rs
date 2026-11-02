//! JSON configuration file loading for the core appliance service.
//!
//! Provides [`CoreConfig`], the strongly typed on-disk configuration storing the
//! startup [`ThemePreset`] override and the marquee scroll rate, loaded from
//! [`DEFAULT_CONFIG_PATH`] (overridable via the `-c/--config` CLI flag).

use std::backtrace::Backtrace;
use std::fmt::{self, Formatter};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hardware::impl_canonical_error_display;
use crate::ui::ThemePreset;

/// Default filesystem path of the appliance JSON configuration file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/speaker-core/config.json";

/// Production default marquee scroll rate (fps).
pub const DEFAULT_MARQUEE_FPS: u32 = 60;

/// Allowed marquee scroll rates: 60 is the production default and 30 halves the
/// render load by scrolling 2 px per 33.3 ms tick at identical on-screen speed.
const MARQUEE_FPS_ALLOWED: [u32; 2] = [30, 60];

/// Strongly typed appliance configuration persisted as JSON on disk.
///
/// Missing fields deserialize to their defaults, so minimal config files
/// (e.g. `{}` or `{"theme": "peppy"}`) remain valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    /// Theme preset override applied on startup; `None` keeps the built-in default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemePreset>,
    /// Marquee scroll rate in fps (`30` or `60`).
    pub marquee_fps: u32,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            theme: None,
            marquee_fps: DEFAULT_MARQUEE_FPS,
        }
    }
}

impl CoreConfig {
    /// Loads the configuration from `path`.
    ///
    /// A missing file is not an error: defaults are returned with a warning so a
    /// fresh appliance boots unconfigured. Any other read failure, malformed
    /// JSON, or out-of-range value is a hard [`ConfigError`].
    ///
    /// # Errors
    /// Returns [`ConfigError`] for non-missing I/O failures, malformed JSON, or
    /// an invalid `marquee_fps` value.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
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
        };
        let config = serde_json::from_str::<Self>(&raw)
            .map_err(|e| ConfigError::parse(format!("invalid config {}", path.display()), e))?;
        config.validate()?;
        Ok(config)
    }

    /// Persists this configuration as pretty JSON to `path`.
    ///
    /// The bytes are staged in a sibling temp file and atomically renamed over
    /// the target, so a power loss mid-write can never leave a truncated
    /// config that bricks the appliance at boot.
    ///
    /// # Errors
    /// Returns [`ConfigError`] if serialization, the temp-file write, or the
    /// rename fails.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let mut raw = serde_json::to_string_pretty(self)
            .map_err(|e| ConfigError::parse(format!("config {}", path.display()), e))?;
        raw.push('\n');
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, raw)
            .map_err(|e| ConfigError::io(format!("failed to write config {}", tmp.display()), e))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| ConfigError::io(format!("failed to install config {}", path.display()), e))
    }

    /// Resolves the effective CLI-independent configuration path.
    #[must_use]
    pub fn resolve_path(override_path: Option<&Path>) -> PathBuf {
        override_path.map_or_else(
            || PathBuf::from(DEFAULT_CONFIG_PATH),
            std::convert::Into::into,
        )
    }

    /// Rejects configuration values outside the domain supported by the appliance.
    ///
    /// # Errors
    /// Returns [`ConfigError`] with [`ConfigError::is_invalid_value`] when
    /// `marquee_fps` is neither 30 nor 60.
    fn validate(&self) -> Result<(), ConfigError> {
        if MARQUEE_FPS_ALLOWED.contains(&self.marquee_fps) {
            Ok(())
        } else {
            Err(ConfigError::invalid(format!(
                "invalid marquee fps '{}': expected 30 or 60",
                self.marquee_fps
            )))
        }
    }
}

#[derive(Debug)]
pub(crate) enum ConfigErrorKind {
    Io {
        context: Box<str>,
        source: io::Error,
    },
    Parse {
        context: Box<str>,
        source: serde_json::Error,
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
    pub(crate) fn io(context: impl Into<Box<str>>, source: io::Error) -> Self {
        Self::new(ConfigErrorKind::Io {
            context: context.into(),
            source,
        })
    }

    /// Builds a JSON (de)serialization failure for a named config operation.
    pub(crate) fn parse(context: impl Into<Box<str>>, source: serde_json::Error) -> Self {
        Self::new(ConfigErrorKind::Parse {
            context: context.into(),
            source,
        })
    }

    /// Builds a domain validation failure for an out-of-range config value.
    pub(crate) fn invalid(message: impl Into<Box<str>>) -> Self {
        Self::new(ConfigErrorKind::InvalidValue {
            message: message.into(),
        })
    }

    /// Returns `true` when the failure was an I/O error reading/writing the file.
    #[must_use]
    pub fn is_io(&self) -> bool {
        matches!(self.kind, ConfigErrorKind::Io { .. })
    }

    /// Returns `true` when the config file contained malformed JSON.
    #[must_use]
    pub fn is_parse(&self) -> bool {
        matches!(self.kind, ConfigErrorKind::Parse { .. })
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
            ConfigErrorKind::Parse { context, source } => {
                write!(f, "config parse error: {context}: {source}")
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
            ConfigErrorKind::Parse { source, .. } => Some(source),
            ConfigErrorKind::InvalidValue { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;
    use crate::diag::ScopedTempDir;

    #[test]
    fn defaults_are_sensible() {
        let config = CoreConfig::default();
        assert_eq!(config.theme, None);
        assert_eq!(config.marquee_fps, 60);
        assert_eq!(DEFAULT_MARQUEE_FPS, 60);
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let dir = ScopedTempDir::new("cfg_missing");
        let path = dir.path().join("does-not-exist.json");
        let config = CoreConfig::load(&path).expect("missing file must yield defaults");
        assert_eq!(config, CoreConfig::default());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = ScopedTempDir::new("cfg_roundtrip");
        let path = dir.path().join("config.json");
        let config = CoreConfig {
            theme: Some(ThemePreset::Peppy),
            marquee_fps: 30,
        };
        config.save(&path).expect("save");
        let loaded = CoreConfig::load(&path).expect("load");
        assert_eq!(loaded, config);
        // Raw on-disk form is the slug-based JSON contract.
        let raw = std::fs::read_to_string(&path).expect("raw read");
        assert!(raw.contains("\"theme\": \"peppy\""), "{raw}");
        assert!(raw.contains("\"marquee_fps\": 30"), "{raw}");
    }

    #[test]
    fn minimal_config_deserializes_with_defaults() {
        let config: CoreConfig = serde_json::from_str("{}").expect("empty object");
        assert_eq!(config, CoreConfig::default());
        let config: CoreConfig =
            serde_json::from_str(r#"{"theme": "studio-dark"}"#).expect("theme only");
        assert_eq!(config.theme, Some(ThemePreset::StudioDark));
        assert_eq!(config.marquee_fps, 60);
    }

    #[test]
    fn load_malformed_json_is_hard_parse_error() {
        let dir = ScopedTempDir::new("cfg_malformed");
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{not json").expect("write");
        let err = CoreConfig::load(&path).expect_err("malformed JSON must fail");
        assert!(err.is_parse());
        assert!(err.to_string().contains("invalid config"));
        // Full Display appends the backtrace only when captured (RUST_BACKTRACE=1);
        // `summary()` never contains one regardless of the capture status.
        assert!(err.summary().contains("config parse error"));
        assert!(!err.summary().contains("Backtrace"));
        if err.backtrace().status() == std::backtrace::BacktraceStatus::Captured {
            assert!(err.to_string().contains("Backtrace:\n"));
        } else {
            assert!(!err.to_string().contains("Backtrace:\n"));
        }
        assert!(err.source().is_some());
    }

    #[test]
    fn load_rejects_unknown_theme_slug() {
        let dir = ScopedTempDir::new("cfg_bad_theme");
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"theme": "bogus"}"#).expect("write");
        let err = CoreConfig::load(&path).expect_err("unknown slug must fail");
        assert!(err.is_parse());
        assert!(err.to_string().contains("unknown theme preset 'bogus'"));
    }

    #[test]
    fn load_rejects_invalid_marquee_fps() {
        for bad in [0u32, 15, 45, 120, u32::MAX] {
            let dir = ScopedTempDir::new("cfg_bad_fps");
            let path = dir.path().join("config.json");
            std::fs::write(&path, format!(r#"{{"marquee_fps": {bad}}}"#)).expect("write");
            let err = CoreConfig::load(&path).expect_err("bad fps must fail");
            assert!(err.is_invalid_value());
            assert!(!err.is_parse() && !err.is_io());
            let rendered = err.to_string();
            assert!(
                rendered.contains(&format!("invalid marquee fps '{bad}': expected 30 or 60")),
                "{rendered}"
            );
            assert!(err.source().is_none(), "domain validation has no source");
        }
    }

    #[test]
    fn load_accepts_allowed_marquee_fps_values() {
        for good in [30u32, 60] {
            let dir = ScopedTempDir::new("cfg_good_fps");
            let path = dir.path().join("config.json");
            std::fs::write(&path, format!(r#"{{"marquee_fps": {good}}}"#)).expect("write");
            let config = CoreConfig::load(&path).expect("valid fps must load");
            assert_eq!(config.marquee_fps, good);
        }
    }

    #[test]
    fn load_io_failure_on_directory_path() {
        let dir = ScopedTempDir::new("cfg_dir_path");
        let err = CoreConfig::load(dir.path()).expect_err("directory read must fail");
        assert!(err.is_io());
        assert!(err.to_string().contains("failed to read config"));
        assert!(err.summary().contains("config I/O error"));
        assert!(!err.summary().contains("Backtrace"));
        assert!(err.source().is_some());
    }

    #[test]
    fn save_io_failure_on_directory_target() {
        let dir = ScopedTempDir::new("cfg_save_dir");
        let config = CoreConfig::default();
        let err = config
            .save(&dir.path().join("missing-dir/config.json"))
            .expect_err("write into missing directory must fail");
        assert!(err.is_io());
        assert!(err.to_string().contains("failed to write config"));
    }

    #[test]
    fn resolve_path_uses_override_and_default() {
        assert_eq!(
            CoreConfig::resolve_path(None),
            PathBuf::from(DEFAULT_CONFIG_PATH)
        );
        assert_eq!(
            CoreConfig::resolve_path(Some(Path::new("/tmp/custom.json"))),
            PathBuf::from("/tmp/custom.json")
        );
    }

    #[test]
    fn error_predicates_and_backtrace_accessor() {
        let err = ConfigError::invalid("synthetic");
        assert!(err.is_invalid_value());
        assert!(!err.is_io() && !err.is_parse());
        assert_eq!(err.summary(), "synthetic");
        assert!(matches!(
            err.backtrace().status(),
            std::backtrace::BacktraceStatus::Captured | std::backtrace::BacktraceStatus::Disabled
        ));
        let io_err = ConfigError::io("ctx", io::Error::other("boom"));
        assert!(io_err.is_io());
        let parse_err = ConfigError::parse(
            "ctx",
            serde_json::from_str::<CoreConfig>("bad").unwrap_err(),
        );
        assert!(parse_err.is_parse());
    }
}
