//! Configuration for audio-manager.

use std::path::PathBuf;

use config::{Config as LayeredConfig, Environment, File, FileFormat};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::rate_watch::DEFAULT_CAMILLADSP_CONFIG_PATH;
use crate::sources::registry::{AutoSwitchPolicy, SourceDefinition, SourceRegistry};

/// Configuration loaded from `/etc/audio-manager/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// ALSA card name for the amplifier (e.g., "sndrpihifiberry").
    #[serde(default = "default_alsa_card")]
    pub alsa_card: String,

    /// Amp control mode: `"i2c"` (default, Phase 2 hifiberry-dac mode) or
    /// `"alsa"` (legacy kernel-driver mode, retained until cutover is
    /// verified across reboots).
    #[serde(default = "default_control_mode")]
    pub control_mode: String,

    /// `/ENABLE` BCM GPIO pin (Phase 0 pinout: 14, active-LOW).
    #[serde(default = "default_enable_pin")]
    pub enable_gpio_pin: u8,

    /// `/MUTE` BCM GPIO pin (Phase 0 pinout: 15, active-LOW, boot-time only).
    #[serde(default = "default_mute_pin")]
    pub mute_gpio_pin: u8,

    /// `error` BCM GPIO pin (Phase 0 pinout: 23, fault indicator).
    #[serde(default = "default_error_pin")]
    pub error_gpio_pin: u8,

    /// Booster-converter enable BCM GPIO pin (kernel `booster_gp`: 17).
    #[serde(default = "default_booster_pin")]
    pub booster_gpio_pin: u8,

    /// Enable the low-frequency safety tick (telemetry diagnostics; the
    /// event paths are always the primary mechanism).
    #[serde(default = "default_safety_tick")]
    pub safety_tick: bool,

    /// Enable the DSP stream-lifecycle watcher (plan §11.3): polls the
    /// `CamillaDSP` engine state and emits `StreamStarting`/`StreamStopping`
    /// edges to the controller.
    #[serde(default = "default_dsp_watch")]
    pub dsp_watch_enabled: bool,

    /// Idle poll interval for the DSP watcher (ms).
    #[serde(default = "default_dsp_poll_interval_ms")]
    pub dsp_poll_interval_ms: u64,

    /// Path to the runtime amp state file.
    #[serde(default = "default_state_path")]
    pub state_path: PathBuf,

    /// Path to the runtime multi-source routing state file.
    #[serde(default = "default_source_state_path")]
    pub source_state_path: PathBuf,

    /// Path to the `CamillaDSP` base YAML configuration template.
    #[serde(default = "default_camilladsp_config_path")]
    pub camilladsp_config_path: PathBuf,

    /// Default multi-source auto-switch policy (`"last-play"`, `"first-play"`, or `"priority"`).
    #[serde(default)]
    pub auto_switch_policy: AutoSwitchPolicy,

    /// Registered audio sources (`bluetooth`, `rmpd`, and future inputs).
    #[serde(default = "SourceRegistry::default_definitions")]
    pub sources: Vec<SourceDefinition>,

    /// Log level override (optional).
    #[serde(default)]
    pub log_level: Option<String>,
}

fn default_alsa_card() -> String {
    "sndrpihifiberry".to_string()
}

fn default_control_mode() -> String {
    "i2c".to_string()
}

const fn default_enable_pin() -> u8 {
    14
}

const fn default_mute_pin() -> u8 {
    15
}

const fn default_error_pin() -> u8 {
    23
}

const fn default_booster_pin() -> u8 {
    17
}

const fn default_safety_tick() -> bool {
    true
}

const fn default_dsp_watch() -> bool {
    true
}

const fn default_dsp_poll_interval_ms() -> u64 {
    500
}

fn default_state_path() -> PathBuf {
    PathBuf::from("/var/lib/speaker/amp-state.json")
}

fn default_source_state_path() -> PathBuf {
    PathBuf::from("/var/lib/speaker/source-state.json")
}

fn default_camilladsp_config_path() -> PathBuf {
    PathBuf::from(DEFAULT_CAMILLADSP_CONFIG_PATH)
}

impl Config {
    /// Load configuration from file, with environment variable overrides.
    ///
    /// # Errors
    /// Returns an error if config deserialization or field validation fails.
    pub fn load() -> anyhow::Result<Self> {
        let config_path = "/etc/audio-manager/config.json";
        let mut builder = LayeredConfig::builder();

        if std::path::Path::new(config_path).exists() {
            builder = builder.add_source(File::new(config_path, FileFormat::Json));
            info!(path = config_path, "Loaded config file");
        } else {
            info!("No config file at {}, using defaults", config_path);
        }

        // Environment variables with prefix AMP_MGR_ (flat keys, no separator).
        builder = builder.add_source(Environment::with_prefix("AMP_MGR"));

        let settings = builder.build()?;
        let mut config: Config = settings.try_deserialize()?;

        if config.alsa_card.is_empty() {
            config.alsa_card = default_alsa_card();
        }
        if config.control_mode != "i2c" && config.control_mode != "alsa" {
            anyhow::bail!(
                "Invalid control_mode '{}': expected \"i2c\" or \"alsa\"",
                config.control_mode
            );
        }
        if config.state_path.as_os_str().is_empty() {
            config.state_path = default_state_path();
        }
        if config.source_state_path.as_os_str().is_empty() {
            config.source_state_path = default_source_state_path();
        }
        if config.camilladsp_config_path.as_os_str().is_empty() {
            config.camilladsp_config_path = default_camilladsp_config_path();
        }
        if config.source_state_path == config.state_path {
            anyhow::bail!(
                "source_state_path and state_path must be distinct ('{}')",
                config.state_path.display()
            );
        }

        SourceRegistry::from_definitions(config.sources.clone())
            .map_err(|e| anyhow::anyhow!("Invalid sources configuration: {e}"))?;

        info!(?config, "Resolved configuration");
        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            alsa_card: default_alsa_card(),
            control_mode: default_control_mode(),
            enable_gpio_pin: default_enable_pin(),
            mute_gpio_pin: default_mute_pin(),
            error_gpio_pin: default_error_pin(),
            booster_gpio_pin: default_booster_pin(),
            safety_tick: default_safety_tick(),
            dsp_watch_enabled: default_dsp_watch(),
            dsp_poll_interval_ms: default_dsp_poll_interval_ms(),
            state_path: default_state_path(),
            source_state_path: default_source_state_path(),
            camilladsp_config_path: default_camilladsp_config_path(),
            auto_switch_policy: AutoSwitchPolicy::default(),
            sources: SourceRegistry::default_definitions(),
            log_level: None,
        }
    }
}
