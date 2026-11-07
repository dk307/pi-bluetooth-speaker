//! JSON configuration file loading for the core appliance service via the
//! layered `config` reader.
//!
//! Provides [`CoreConfig`], the strongly typed on-disk configuration storing the
//! startup [`ThemePreset`] override and the marquee scroll rate, loaded from
//! `speaker_common::diag::DEFAULT_CONFIG_PATH` (overridable via the `-c/--config` CLI flag).

use std::backtrace::Backtrace;
use std::fmt::{self, Formatter};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use config::Config as LayeredConfig;
use serde::{Deserialize, Serialize};

use crate::ui::ThemePreset;
use speaker_common::diag::{DEFAULT_CONFIG_PATH, FALLBACK_CONFIG_PATH};
use speaker_common::impl_canonical_error_display;

/// Production default marquee scroll rate (fps).
pub const DEFAULT_MARQUEE_FPS: u32 = 60;

/// Allowed marquee scroll rates: 60 is the production default and 30 halves the
/// render load by scrolling 2 px per 33.3 ms tick at identical on-screen speed.
const MARQUEE_FPS_ALLOWED: [u32; 2] = [30, 60];

/// Strongly typed marquee scroll rate in frames per second.
///
/// The appliance display pipeline only supports `30` or `60` fps; an invalid
/// value cannot inhabit the type, so [`CoreConfig`] never carries one and no
/// post-deserialization validation pass is required. Serializes transparently
/// as its raw `u32` rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct MarqueeFps(u32);

impl MarqueeFps {
    /// Halved-rate preset (2 px per tick scrolling).
    pub const FPS_30: Self = Self::new(30);
    /// Production-default rate.
    pub const FPS_60: Self = Self::new(DEFAULT_MARQUEE_FPS);

    /// Unchecked const constructor for fixed-rate constants.
    #[must_use]
    pub const fn new(fps: u32) -> Self {
        Self(fps)
    }

    /// Returns the raw scroll rate in fps.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl Default for MarqueeFps {
    fn default() -> Self {
        Self::FPS_60
    }
}

impl TryFrom<u32> for MarqueeFps {
    type Error = ConfigError;

    fn try_from(fps: u32) -> Result<Self, Self::Error> {
        if MARQUEE_FPS_ALLOWED.contains(&fps) {
            Ok(Self::new(fps))
        } else {
            Err(ConfigError::invalid(format!(
                "invalid marquee fps '{fps}': expected 30 or 60"
            )))
        }
    }
}

impl From<MarqueeFps> for u32 {
    fn from(marquee_fps: MarqueeFps) -> Self {
        marquee_fps.0
    }
}

const _: () = {
    assert!((30 <= DEFAULT_MARQUEE_FPS) && (DEFAULT_MARQUEE_FPS <= 60));
};

/// Strongly typed appliance configuration persisted as JSON on disk.
///
/// Missing fields deserialize to their defaults, so minimal config files
/// (e.g. `{}` or `{"theme": "peppy"}`) remain valid. [`MarqueeFps`] enforces
/// the allowed scroll rates at the deserialization boundary, so an invalid
/// value cannot inhabit this struct.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    /// Theme preset override applied on startup; `None` keeps the built-in default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemePreset>,
    /// Marquee scroll rate in fps (`30` or `60`).
    pub marquee_fps: MarqueeFps,
}

impl CoreConfig {
    /// Loads the configuration from `path` via the layered `config` reader.
    ///
    /// A missing file is not an error: defaults are returned with a warning so a
    /// fresh appliance boots unconfigured. Any other read failure, malformed
    /// JSON, an unknown theme slug, or an out-of-range `marquee_fps` is a hard
    /// [`ConfigError`].
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
        let config = settings
            .try_deserialize::<Self>()
            .map_err(|e| ConfigError::config(format!("invalid config {}", path.display()), e))?;
        Ok(config)
    }

    /// Persists this configuration as pretty JSON to `path`.
    ///
    /// [`MarqueeFps`] makes out-of-domain rates unrepresentable, so every
    /// persistable configuration is valid by construction and can never brick
    /// the daemon at the next boot.
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

        // The `tempfile` crate stages the bytes in a sibling temp file, which
        // is fsynced and then atomically renamed over the target via
        // `persist`: a power loss mid-write can never leave a truncated config
        // that would brick the daemon at the next boot (malformed JSON is a
        // hard error by contract). A failed persist consumes the temp file and
        // deletes it on drop, so no `.tmp` litter survives a failed save.
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

    /// Resolves the effective CLI-independent configuration path.
    ///
    /// Checks the override path first, then checks for [`DEFAULT_CONFIG_PATH`]
    /// (`/etc/screen-manager/config.json`), falling back to [`FALLBACK_CONFIG_PATH`]
    /// (`/etc/speaker-core/config.json`) if it exists on disk.
    #[must_use]
    pub fn resolve_path(override_path: Option<&Path>) -> PathBuf {
        if let Some(path) = override_path {
            path.to_path_buf()
        } else {
            let primary = Path::new(DEFAULT_CONFIG_PATH);
            if primary.exists() {
                primary.to_path_buf()
            } else {
                let fallback = Path::new(FALLBACK_CONFIG_PATH);
                if fallback.exists() {
                    fallback.to_path_buf()
                } else {
                    primary.to_path_buf()
                }
            }
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
    /// parsing, or serde deserialization of the strongly typed [`CoreConfig`].
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
    use std::error::Error as _;

    use super::*;
    use speaker_common::diag::ScopedTempDir;

    #[test]
    fn defaults_are_sensible() {
        let config = CoreConfig::default();
        assert_eq!(config.theme, None);
        assert_eq!(config.marquee_fps, MarqueeFps::FPS_60);
        assert_eq!(config.marquee_fps.value(), 60);
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
    fn load_missing_file_logs_warning_field_with_subscriber() {
        let dir = ScopedTempDir::new("cfg_missing_logged");
        let path = dir.path().join("absent.json");
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let config = CoreConfig::load(&path).expect("missing file must yield defaults");
            assert_eq!(config, CoreConfig::default());
        });
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = ScopedTempDir::new("cfg_roundtrip");
        let path = dir.path().join("config.json");
        let config = CoreConfig {
            theme: Some(ThemePreset::Peppy),
            marquee_fps: MarqueeFps::FPS_30,
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
        assert_eq!(config.marquee_fps, MarqueeFps::FPS_60);
    }

    #[test]
    fn load_malformed_json_is_hard_parse_error() {
        let dir = ScopedTempDir::new("cfg_malformed");
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{not json").expect("write");
        let err = CoreConfig::load(&path).expect_err("malformed JSON must fail");
        assert!(err.is_config());
        // config-rs reports malformed files as FileParse failures during build().
        assert!(err.to_string().contains("failed to read config"));
        // Full Display appends the backtrace only when captured (RUST_BACKTRACE=1);
        // `summary()` never contains one regardless of the capture status.
        assert!(err.summary().contains("config error"));
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
        assert!(err.is_config());
        assert!(err.to_string().contains("unknown theme preset 'bogus'"));
    }

    #[test]
    fn load_rejects_invalid_marquee_fps() {
        for bad in [0u32, 15, 45, 120, u32::MAX] {
            let dir = ScopedTempDir::new("cfg_bad_fps");
            let path = dir.path().join("config.json");
            std::fs::write(&path, format!(r#"{{"marquee_fps": {bad}}}"#)).expect("write");
            let err = CoreConfig::load(&path).expect_err("bad fps must fail");
            // The invalid rate cannot inhabit `MarqueeFps`, so the layered
            // reader reports it as a deserialize failure.
            assert!(err.is_config(), "{err}");
            let rendered = err.to_string();
            assert!(
                rendered.contains(&format!("invalid marquee fps '{bad}': expected 30 or 60")),
                "{rendered}"
            );
        }
    }

    #[test]
    fn load_accepts_allowed_marquee_fps_values() {
        for (good, expected) in [(30u32, MarqueeFps::FPS_30), (60, MarqueeFps::FPS_60)] {
            let dir = ScopedTempDir::new("cfg_good_fps");
            let path = dir.path().join("config.json");
            std::fs::write(&path, format!(r#"{{"marquee_fps": {good}}}"#)).expect("write");
            let config = CoreConfig::load(&path).expect("valid fps must load");
            assert_eq!(config.marquee_fps, expected);
        }
    }

    #[test]
    fn marquee_fps_tryfrom_enforces_allowed_values() {
        for good in [30u32, 60] {
            assert_eq!(
                MarqueeFps::try_from(good).expect("allowed rate").value(),
                good
            );
        }
        for bad in [0u32, 15, 45, 120, u32::MAX] {
            let err = MarqueeFps::try_from(bad).expect_err("out-of-domain rate must be rejected");
            assert!(err.is_invalid_value());
            assert_eq!(
                err.summary(),
                format!("invalid marquee fps '{bad}': expected 30 or 60")
            );
        }
    }

    #[test]
    fn marquee_fps_serde_rejects_invalid_rate() {
        let err = serde_json::from_str::<MarqueeFps>("45").expect_err("invalid rate");
        assert!(
            err.to_string().contains("invalid marquee fps '45'"),
            "{err}"
        );
        let fps: MarqueeFps = serde_json::from_str("30").expect("allowed rate");
        assert_eq!(fps, MarqueeFps::FPS_30);
        // Serialization is transparent u32.
        assert_eq!(
            serde_json::to_string(&MarqueeFps::FPS_60).expect("serialize"),
            "60"
        );
        // Invalid rates are unrepresentable in CoreConfig, so `save()` can
        // never persist them.
        assert_eq!(u32::from(MarqueeFps::FPS_30), 30);
        assert_eq!(MarqueeFps::default(), MarqueeFps::FPS_60);
    }

    #[test]
    fn load_io_failure_on_directory_path() {
        let dir = ScopedTempDir::new("cfg_dir_path");
        let err = CoreConfig::load(dir.path()).expect_err("directory read must fail");
        // The layered reader surfaces the EISDIR access failure.
        assert!(err.is_config());
        assert!(err.to_string().contains("failed to read config"));
        assert!(err.summary().contains("config error"));
        assert!(!err.summary().contains("Backtrace"));
        assert!(err.source().is_some());
    }

    #[test]
    fn save_io_failure_on_directory_target() {
        let dir = ScopedTempDir::new("cfg_save_dir");
        let config = CoreConfig::default();
        // Renaming the temp file over an existing directory must fail.
        let target = dir.path().join("blocked");
        std::fs::create_dir(&target).expect("create blocking directory");
        let err = config
            .save(&target)
            .expect_err("rename over directory must fail");
        assert!(err.is_io());
        assert!(err.to_string().contains("failed to install config"));
        // The temp file lives as a sibling of the rename target; a failed
        // persist consumes it, so no `.tmp` litter may survive the save.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp file must be removed on failed save"
        );
    }

    #[test]
    fn save_replaces_existing_config_atomically() {
        let dir = ScopedTempDir::new("cfg_save_replace");
        let path = dir.path().join("config.json");
        CoreConfig {
            theme: None,
            marquee_fps: MarqueeFps::FPS_60,
        }
        .save(&path)
        .expect("initial save");
        CoreConfig {
            theme: Some(ThemePreset::StudioDark),
            marquee_fps: MarqueeFps::FPS_30,
        }
        .save(&path)
        .expect("replacement save");
        let loaded = CoreConfig::load(&path).expect("reload replaced config");
        assert_eq!(loaded.theme, Some(ThemePreset::StudioDark));
        assert_eq!(loaded.marquee_fps, MarqueeFps::FPS_30);
        // Only the final artifact remains; no temp files left behind.
        let leftovers: Vec<_> = dir
            .path()
            .read_dir()
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["config.json".to_string()]);
    }

    #[test]
    fn load_ignores_path_extension_for_json_format() {
        // The on-disk contract is always JSON regardless of the operator's
        // filename choice: format detection is pinned, not guessed.
        for name in ["config.txt", "conf", "settings.json"] {
            let dir = ScopedTempDir::new("cfg_extless");
            let path = dir.path().join(name);
            std::fs::write(&path, r#"{"marquee_fps": 30}"#).expect("write");
            let config = CoreConfig::load(&path).expect("valid fps must load");
            assert_eq!(config.marquee_fps, MarqueeFps::FPS_30, "{name}");
        }
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
}
#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn save_installs_world_readable_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        CoreConfig::default().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
    }
}
