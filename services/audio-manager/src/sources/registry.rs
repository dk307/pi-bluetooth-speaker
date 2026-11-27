//! Source registry and strongly-typed audio source configuration models.

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize};
use speaker_mpd::{MpdHost, MpdPort};

/// Number of subdevices provided by `snd-aloop` (`0..=7`).
pub const MAX_LOOPBACK_SUBDEVICES: u32 = 8;

/// Fallback sample rate used when forcing a `CamillaDSP` capture target before a rate is reported.
pub const DEFAULT_SOURCE_RATE_HZ: u32 = 48_000;

/// Maximum length of a validated [`SourceId`].
const MAX_SOURCE_ID_LEN: usize = 32;

/// Validated audio source identifier (`[a-z0-9_-]`, starting with ASCII alphanumeric, 1..=32 bytes).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SourceId(Box<str>);

impl SourceId {
    /// Returns `true` when `raw` satisfies [`SourceId`] syntax rules.
    #[must_use]
    pub fn is_valid(raw: &str) -> bool {
        let bytes = raw.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= MAX_SOURCE_ID_LEN
            && bytes[0].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
            && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
    }

    /// Validates and constructs a [`SourceId`].
    ///
    /// # Errors
    /// Returns a descriptive error string when `raw` is empty, too long, or contains invalid characters.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if !Self::is_valid(trimmed) {
            return Err(format!(
                "invalid source id '{trimmed}': expected 1..={MAX_SOURCE_ID_LEN} chars of [a-z0-9_-] starting with [a-z0-9]"
            ));
        }
        Ok(Self(Box::from(trimmed)))
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for SourceId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for SourceId {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for SourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Auto-switching arbitration policy across multiple audio sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutoSwitchPolicy {
    /// Whichever source transitions from idle to streaming takes over immediately.
    #[default]
    #[serde(rename = "last-play")]
    LastPlayWins,
    /// The currently streaming source retains output until it stops; new starts are paused.
    #[serde(rename = "first-play")]
    FirstPlayWins,
    /// Higher `priority` sources preempt lower-priority ones; equal/lower priority starts are paused.
    #[serde(rename = "priority")]
    Priority,
}

impl AutoSwitchPolicy {
    /// Parses a policy string (`"last-play"`, `"first-play"`, or `"priority"`).
    ///
    /// # Errors
    /// Returns a descriptive error string for unknown policy names.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "last-play" => Ok(Self::LastPlayWins),
            "first-play" => Ok(Self::FirstPlayWins),
            "priority" => Ok(Self::Priority),
            other => Err(format!(
                "invalid auto_switch_policy '{other}': expected 'last-play', 'first-play', or 'priority'"
            )),
        }
    }

    /// Returns the canonical wire string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LastPlayWins => "last-play",
            Self::FirstPlayWins => "first-play",
            Self::Priority => "priority",
        }
    }
}

impl Display for AutoSwitchPolicy {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Capture device topology for a registered audio source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureSpec {
    /// Dedicated `snd-aloop` subdevice (`hw:Loopback,0,{N}` -> `hw:Loopback,1,{N}`).
    Loopback {
        /// Loopback subdevice index (`0..=7`).
        subdevice: u32,
    },
    /// Direct ALSA capture PCM (for future hardware line-in or USB audio inputs).
    AlsaDirect {
        /// ALSA capture device identifier (e.g., `hw:1,0`).
        device: Box<str>,
        /// Fallback sample rate when not dynamically probed.
        #[serde(default = "default_rate_hz")]
        default_rate_hz: u32,
    },
}

const fn default_rate_hz() -> u32 {
    DEFAULT_SOURCE_RATE_HZ
}

impl CaptureSpec {
    /// Returns the ALSA capture device string passed to `CamillaDSP`.
    #[must_use]
    pub fn camilladsp_device(&self) -> Box<str> {
        match self {
            Self::Loopback { subdevice } => Box::from(format!("hw:Loopback,1,{subdevice}")),
            Self::AlsaDirect { device, .. } => device.clone(),
        }
    }

    /// Returns the `snd-aloop` subdevice index if this is a loopback source.
    #[must_use]
    pub const fn loopback_subdevice(&self) -> Option<u32> {
        match self {
            Self::Loopback { subdevice } => Some(*subdevice),
            Self::AlsaDirect { .. } => None,
        }
    }

    /// Returns the fallback sample rate for this capture path.
    #[must_use]
    pub const fn default_rate_hz(&self) -> u32 {
        match self {
            Self::Loopback { .. } => DEFAULT_SOURCE_RATE_HZ,
            Self::AlsaDirect {
                default_rate_hz, ..
            } => *default_rate_hz,
        }
    }
}

/// Pause/yield control transport for a registered audio source.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlSpec {
    /// Pause via `bt-manager` system D-Bus (`org.speaker.Bluetooth1.PauseActive`).
    BluetoothDbus,
    /// Pause via MPD TCP protocol (`pause 1`) using `speaker-mpd`.
    MpdTcp {
        /// MPD daemon host.
        #[serde(default)]
        host: MpdHost,
        /// MPD daemon TCP port.
        #[serde(default)]
        port: MpdPort,
    },
    /// Passive source with no remote pause endpoint (e.g., analog line-in).
    #[default]
    None,
}

/// Static configuration entry for one registered audio source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDefinition {
    /// Unique identifier (`"bluetooth"`, `"rmpd"`, ...).
    pub id: SourceId,
    /// Human-readable label shown on UI / D-Bus.
    pub display_name: Box<str>,
    /// Arbitration priority (`0..=255`, higher wins under `Priority` policy).
    pub priority: u8,
    /// ALSA capture path feeding `CamillaDSP`.
    pub capture: CaptureSpec,
    /// Optional pause/yield control adapter.
    #[serde(default)]
    pub control: ControlSpec,
}

/// Validated immutable registry of configured audio sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRegistry {
    definitions: Vec<SourceDefinition>,
    by_id: HashMap<SourceId, usize>,
    by_subdevice: HashMap<u32, SourceId>,
}

impl SourceRegistry {
    /// Default source list for the appliance (`bluetooth` + `rmpd`).
    #[must_use]
    pub fn default_definitions() -> Vec<SourceDefinition> {
        vec![
            SourceDefinition {
                id: SourceId(Box::from("bluetooth")),
                display_name: Box::from("Bluetooth"),
                priority: 50,
                capture: CaptureSpec::Loopback { subdevice: 0 },
                control: ControlSpec::BluetoothDbus,
            },
            SourceDefinition {
                id: SourceId(Box::from("rmpd")),
                display_name: Box::from("Music Player"),
                priority: 40,
                capture: CaptureSpec::Loopback { subdevice: 1 },
                control: ControlSpec::MpdTcp {
                    host: MpdHost::default(),
                    port: MpdPort::default(),
                },
            },
        ]
    }

    /// Validates and indexes a list of source definitions.
    ///
    /// # Errors
    /// Returns a descriptive error string if the list is empty or contains duplicate IDs,
    /// duplicate loopback subdevices, out-of-range subdevices, or empty device names.
    pub fn from_definitions(definitions: Vec<SourceDefinition>) -> Result<Self, String> {
        if definitions.is_empty() {
            return Err("sources list cannot be empty".to_owned());
        }

        let mut by_id = HashMap::with_capacity(definitions.len());
        let mut by_subdevice = HashMap::new();
        let mut seen_ids = HashSet::with_capacity(definitions.len());

        for (idx, def) in definitions.iter().enumerate() {
            if def.display_name.trim().is_empty() {
                return Err(format!("source '{}' has empty display_name", def.id));
            }
            if !seen_ids.insert(def.id.clone()) {
                return Err(format!("duplicate source id '{}'", def.id));
            }
            match &def.capture {
                CaptureSpec::Loopback { subdevice } => {
                    if *subdevice >= MAX_LOOPBACK_SUBDEVICES {
                        return Err(format!(
                            "source '{}' loopback subdevice {} out of range 0..{}",
                            def.id, subdevice, MAX_LOOPBACK_SUBDEVICES
                        ));
                    }
                    if let Some(prev) = by_subdevice.insert(*subdevice, def.id.clone()) {
                        return Err(format!(
                            "loopback subdevice {subdevice} assigned to both '{prev}' and '{}'",
                            def.id
                        ));
                    }
                }
                CaptureSpec::AlsaDirect {
                    device,
                    default_rate_hz,
                } => {
                    if device.trim().is_empty() {
                        return Err(format!(
                            "source '{}' AlsaDirect capture device cannot be empty",
                            def.id
                        ));
                    }
                    if *default_rate_hz == 0 {
                        return Err(format!(
                            "source '{}' AlsaDirect default_rate_hz must be non-zero",
                            def.id
                        ));
                    }
                }
            }
            by_id.insert(def.id.clone(), idx);
        }

        Ok(Self {
            definitions,
            by_id,
            by_subdevice,
        })
    }

    /// Builds the default appliance registry.
    #[must_use]
    pub fn default_appliance() -> Self {
        Self::from_definitions(Self::default_definitions())
            .expect("default appliance source definitions are statically valid")
    }

    /// Returns all registered definitions in configured order.
    #[must_use]
    pub fn definitions(&self) -> &[SourceDefinition] {
        &self.definitions
    }

    /// Looks up a source definition by ID.
    #[must_use]
    pub fn get(&self, id: &SourceId) -> Option<&SourceDefinition> {
        self.by_id.get(id).map(|&idx| &self.definitions[idx])
    }

    /// Returns `true` if `id` is registered.
    #[must_use]
    pub fn contains(&self, id: &SourceId) -> bool {
        self.by_id.contains_key(id)
    }

    /// Looks up the source ID bound to `snd-aloop` subdevice `subdevice`.
    #[must_use]
    pub fn by_loopback_subdevice(&self, subdevice: u32) -> Option<&SourceId> {
        self.by_subdevice.get(&subdevice)
    }

    /// Returns sorted `(subdevice, SourceId)` pairs for all loopback-backed sources.
    #[must_use]
    pub fn loopback_subdevices(&self) -> Vec<(u32, SourceId)> {
        let mut pairs: Vec<(u32, SourceId)> = self
            .by_subdevice
            .iter()
            .map(|(&sub, id)| (sub, id.clone()))
            .collect();
        pairs.sort_unstable_by_key(|(sub, _)| *sub);
        pairs
    }
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::default_appliance()
    }
}
