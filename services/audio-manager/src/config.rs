//! Configuration for audio-manager.

use std::path::PathBuf;

use config::{Config as LayeredConfig, Environment, File, FileFormat};
use serde::{Deserialize, Serialize};
use tracing::info;

/// Configuration loaded from `/etc/audio-manager/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// ALSA card name for the Merus amplifier (e.g., "sndrpimerusamp").
    #[serde(default = "default_alsa_card")]
    pub alsa_card: String,

    /// Path to the runtime state file.
    #[serde(default = "default_state_path")]
    pub state_path: PathBuf,

    /// Log level override (optional).
    #[serde(default)]
    pub log_level: Option<String>,
}

fn default_alsa_card() -> String {
    "sndrpimerusamp".to_string()
}

fn default_state_path() -> PathBuf {
    PathBuf::from("/var/lib/speaker/amp-state.json")
}

impl Config {
    /// Load configuration from file, with environment variable overrides.
    pub fn load() -> anyhow::Result<Self> {
        // 2. Config file (optional, missing is OK)
        let config_path = "/etc/audio-manager/config.json";
        let mut builder = LayeredConfig::builder();

        if std::path::Path::new(config_path).exists() {
            builder = builder.add_source(File::new(config_path, FileFormat::Json));
            info!(path = config_path, "Loaded config file");
        } else {
            info!("No config file at {}, using defaults", config_path);
        }

        // 3. Environment variables with prefix AMP_MGR_
        builder = builder.add_source(Environment::with_prefix("AMP_MGR").separator("_"));

        let settings = builder.build()?;
        let mut config: Config = settings.try_deserialize()?;

        // Apply defaults for any missing fields (serde default handles this during deserialize,
        // but we can also manually fill in)
        if config.alsa_card.is_empty() {
            config.alsa_card = default_alsa_card();
        }
        if config.state_path.as_os_str().is_empty() {
            config.state_path = default_state_path();
        }

        info!(?config, "Resolved configuration");
        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            alsa_card: default_alsa_card(),
            state_path: default_state_path(),
            log_level: None,
        }
    }
}
