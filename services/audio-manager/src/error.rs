//! Error types for audio-manager.

/// Canonical error types for the audio-manager daemon.
#[derive(Debug, thiserror::Error)]
pub enum AmpError {
    /// ALSA hardware error (open, control read/write, etc.).
    #[error("ALSA error: {0}")]
    Alsa(#[from] alsa::Error),

    /// I2C bus error (serial mute register access).
    #[error("I2C error: {0}")]
    I2c(#[from] rppal::i2c::Error),

    /// GPIO error (rppal gpiochip access), `source` preserved.
    #[error("GPIO error: {0}")]
    Gpio(#[from] rppal::gpio::Error),

    /// Hardware crate (MA12070P driver) error, `source` preserved.
    #[error("Amp hardware error: {0}")]
    Hardware(#[from] speaker_hardware::AmpError),

    /// I/O error (state file, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// D-Bus error.
    #[error("D-Bus error: {0}")]
    DBus(#[from] zbus::Error),

    /// anyhow error (for context chains).
    #[error("{0}")]
    Anyhow(#[from] anyhow::Error),

    /// D-Bus name already taken by another process.
    #[error("D-Bus name org.speaker.AudioManager already taken")]
    DBusNameTaken,

    /// Hardware validation failed at startup.
    #[error("Hardware validation failed: {reason}")]
    HardwareValidation { reason: String },

    /// Amp card not found.
    #[error("Amp card not found: {card}")]
    CardMissing { card: String },

    /// Mixer control not found or invalid range.
    #[error("Mixer control error: {0}")]
    ControlError(String),

    /// State file corrupted or missing.
    #[error("State file error: {0}")]
    StateFile(String),

    /// Generic internal error.
    #[error("Internal error: {0}")]
    Internal(String),
}

impl AmpError {
    /// Create a hardware validation error.
    pub fn hardware_validation(reason: impl Into<String>) -> Self {
        Self::HardwareValidation {
            reason: reason.into(),
        }
    }

    /// Create a card missing error.
    pub fn card_missing(card: impl Into<String>) -> Self {
        Self::CardMissing { card: card.into() }
    }

    /// Create a control error.
    pub fn control_error(msg: impl Into<String>) -> Self {
        Self::ControlError(msg.into())
    }

    /// Create a state file error.
    pub fn state_file(msg: impl Into<String>) -> Self {
        Self::StateFile(msg.into())
    }

    /// Create an internal error.
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

/// Type alias for results with AmpError.
pub type AmpResult<T> = Result<T, AmpError>;
