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

/// Production default volume step per rotary encoder detent (`2%`).
pub const DEFAULT_VOLUME_STEP_PERCENT: u8 = 2;

/// Minimum allowed volume step per rotary encoder detent (`1%`).
pub const MIN_VOLUME_STEP_PERCENT: u8 = 1;

/// Maximum allowed volume step per rotary encoder detent (`25%`).
pub const MAX_VOLUME_STEP_PERCENT: u8 = 25;

/// Production default Volume HUD auto-dismiss timeout in milliseconds (`4000` ms).
pub const DEFAULT_VOLUME_HUD_TIMEOUT_MS: u32 = 4_000;

/// Minimum allowed Volume HUD auto-dismiss timeout in milliseconds (`250` ms).
pub const MIN_VOLUME_HUD_TIMEOUT_MS: u32 = 250;

/// Maximum allowed Volume HUD auto-dismiss timeout in milliseconds (`30_000` ms).
pub const MAX_VOLUME_HUD_TIMEOUT_MS: u32 = 30_000;

/// Production default Volume HUD fade-out duration in milliseconds (`200` ms).
pub const DEFAULT_VOLUME_HUD_FADE_MS: u32 = 200;

/// Maximum allowed Volume HUD fade-out duration in milliseconds (`2_000` ms).
pub const MAX_VOLUME_HUD_FADE_MS: u32 = 2_000;

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
    assert!(
        (MIN_VOLUME_STEP_PERCENT <= DEFAULT_VOLUME_STEP_PERCENT)
            && (DEFAULT_VOLUME_STEP_PERCENT <= MAX_VOLUME_STEP_PERCENT)
    );
    assert!(
        (MIN_VOLUME_HUD_TIMEOUT_MS <= DEFAULT_VOLUME_HUD_TIMEOUT_MS)
            && (DEFAULT_VOLUME_HUD_TIMEOUT_MS <= MAX_VOLUME_HUD_TIMEOUT_MS)
    );
    assert!(DEFAULT_VOLUME_HUD_FADE_MS <= MAX_VOLUME_HUD_FADE_MS);
};

/// Strongly typed rotary encoder volume step percentage (`1..=25`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct VolumeStepPercent(u8);

impl VolumeStepPercent {
    /// Production default step (`2%`).
    pub const DEFAULT: Self = Self::new(DEFAULT_VOLUME_STEP_PERCENT);

    /// Const constructor validated against `1..=25`.
    ///
    /// # Panics
    /// Panics if `step` is not in the range `1..=25`.
    #[must_use]
    pub const fn new(step: u8) -> Self {
        assert!(
            step >= MIN_VOLUME_STEP_PERCENT && step <= MAX_VOLUME_STEP_PERCENT,
            "volume_step_percent out of range 1..=25"
        );
        Self(step)
    }

    /// Returns the raw step percentage (`1..=25`).
    #[must_use]
    pub const fn value(self) -> u8 {
        self.0
    }
}

impl Default for VolumeStepPercent {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u8> for VolumeStepPercent {
    type Error = ConfigError;

    fn try_from(step: u8) -> Result<Self, Self::Error> {
        if (MIN_VOLUME_STEP_PERCENT..=MAX_VOLUME_STEP_PERCENT).contains(&step) {
            Ok(Self::new(step))
        } else {
            Err(ConfigError::invalid(format!(
                "invalid volume_step_percent '{step}': expected {MIN_VOLUME_STEP_PERCENT}..={MAX_VOLUME_STEP_PERCENT}"
            )))
        }
    }
}

impl From<VolumeStepPercent> for u8 {
    fn from(step: VolumeStepPercent) -> Self {
        step.0
    }
}

/// Strongly typed Volume HUD auto-dismiss timeout in milliseconds (`250..=30_000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct VolumeHudTimeoutMs(u32);

impl VolumeHudTimeoutMs {
    /// Production default timeout (`4000` ms).
    pub const DEFAULT: Self = Self::new(DEFAULT_VOLUME_HUD_TIMEOUT_MS);

    /// Const constructor validated against `250..=30_000`.
    ///
    /// # Panics
    /// Panics if `ms` is not in the range `250..=30_000`.
    #[must_use]
    pub const fn new(ms: u32) -> Self {
        assert!(
            ms >= MIN_VOLUME_HUD_TIMEOUT_MS && ms <= MAX_VOLUME_HUD_TIMEOUT_MS,
            "volume_hud_timeout_ms out of range 250..=30000"
        );
        Self(ms)
    }

    /// Returns the raw timeout in milliseconds.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl Default for VolumeHudTimeoutMs {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u32> for VolumeHudTimeoutMs {
    type Error = ConfigError;

    fn try_from(ms: u32) -> Result<Self, Self::Error> {
        if (MIN_VOLUME_HUD_TIMEOUT_MS..=MAX_VOLUME_HUD_TIMEOUT_MS).contains(&ms) {
            Ok(Self::new(ms))
        } else {
            Err(ConfigError::invalid(format!(
                "invalid volume_hud_timeout_ms '{ms}': expected {MIN_VOLUME_HUD_TIMEOUT_MS}..={MAX_VOLUME_HUD_TIMEOUT_MS}"
            )))
        }
    }
}

impl From<VolumeHudTimeoutMs> for u32 {
    fn from(ms: VolumeHudTimeoutMs) -> Self {
        ms.0
    }
}

/// Strongly typed Volume HUD fade-out duration in milliseconds (`0..=2_000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct VolumeHudFadeMs(u32);

impl VolumeHudFadeMs {
    /// Production default fade duration (`200` ms).
    pub const DEFAULT: Self = Self::new(DEFAULT_VOLUME_HUD_FADE_MS);

    /// Const constructor validated against `0..=2_000`.
    ///
    /// # Panics
    /// Panics if `ms` exceeds `2_000`.
    #[must_use]
    pub const fn new(ms: u32) -> Self {
        assert!(
            ms <= MAX_VOLUME_HUD_FADE_MS,
            "volume_hud_fade_ms out of range 0..=2000"
        );
        Self(ms)
    }

    /// Returns the raw fade duration in milliseconds.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl Default for VolumeHudFadeMs {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u32> for VolumeHudFadeMs {
    type Error = ConfigError;

    fn try_from(ms: u32) -> Result<Self, Self::Error> {
        if ms <= MAX_VOLUME_HUD_FADE_MS {
            Ok(Self::new(ms))
        } else {
            Err(ConfigError::invalid(format!(
                "invalid volume_hud_fade_ms '{ms}': expected 0..={MAX_VOLUME_HUD_FADE_MS}"
            )))
        }
    }
}

impl From<VolumeHudFadeMs> for u32 {
    fn from(ms: VolumeHudFadeMs) -> Self {
        ms.0
    }
}

/// Strongly typed appliance configuration persisted as JSON on disk.
///
/// Missing fields deserialize to their defaults, so minimal config files
/// (e.g. `{}` or `{"theme": "peppy"}`) remain valid. Domain newtypes enforce
/// allowed bounds at the deserialization boundary, so an invalid value cannot
/// inhabit this struct.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    /// Theme preset override applied on startup; `None` keeps the built-in default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemePreset>,
    /// Marquee scroll rate in fps (`30` or `60`).
    pub marquee_fps: MarqueeFps,
    /// Rotary encoder volume step percentage per detent (`1..=25`, default `2`).
    pub volume_step_percent: VolumeStepPercent,
    /// Volume HUD auto-dismiss timeout in milliseconds (`250..=30000`, default `4000`).
    pub volume_hud_timeout_ms: VolumeHudTimeoutMs,
    /// Volume HUD fade-out duration in milliseconds (`0..=2000`, default `200`).
    pub volume_hud_fade_ms: VolumeHudFadeMs,
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
        Self::resolve_path_with(override_path, Path::exists)
    }

    /// Resolves the configuration file path with an injectable existence predicate
    /// for hermetic testing across host environments.
    #[must_use]
    pub fn resolve_path_with<F>(override_path: Option<&Path>, exists: F) -> PathBuf
    where
        F: Fn(&Path) -> bool,
    {
        if let Some(path) = override_path {
            path.to_path_buf()
        } else {
            let primary = Path::new(DEFAULT_CONFIG_PATH);
            if exists(primary) {
                primary.to_path_buf()
            } else {
                let fallback = Path::new(FALLBACK_CONFIG_PATH);
                if exists(fallback) {
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
