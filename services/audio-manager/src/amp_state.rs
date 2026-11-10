//! Amplifier state machine and domain logic.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::AmpResult;

/// Type alias for the percent-to-raw curve table.
type PercentToRaw = [u16; 101];

/// Internal amplifier state.
/// - `level_raw`: raw register value 0..144 (0 = -144 dB mute, 144 = 0 dB)
/// - `muted`: whether mute is active (when true, level_raw is remembered but 0 is written)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmpState {
    pub level_raw: u16,
    pub muted: bool,
}

impl Default for AmpState {
    /// Default: 50% volume (≈ -28 dB), unmuted.
    fn default() -> Self {
        // 50% from PERCENT_TO_RAW table: index 50 = 116 raw
        Self {
            level_raw: 116,
            muted: false,
        }
    }
}

impl AmpState {
    /// Maximum raw register value (0 dB).
    pub const MAX_RAW: u16 = 144;
    /// Minimum raw register value (mute, -144 dB).
    pub const MIN_RAW: u16 = 0;

    /// Create a new state with validation.
    pub fn new(level_raw: u16, muted: bool) -> AmpResult<Self> {
        if level_raw > Self::MAX_RAW {
            return Err(crate::error::AmpError::internal(format!(
                "level_raw {} exceeds MAX_RAW {}",
                level_raw,
                Self::MAX_RAW
            )));
        }
        Ok(Self { level_raw, muted })
    }

    /// Get the level that should be written to hardware.
    /// If muted, returns 0 (hardware mute). Otherwise returns level_raw.
    pub fn hardware_level(&self) -> u16 {
        if self.muted { 0 } else { self.level_raw }
    }

    /// Apply a volume percent change (0..100), returning new state.
    /// If currently muted, stores the new level but does not change mute.
    pub fn with_percent(self, percent: u8, curve: &PercentToRaw) -> Self {
        let new_raw = curve[percent as usize];
        Self {
            level_raw: new_raw,
            muted: self.muted,
        }
    }

    /// Apply mute on: remember current level, set muted=true.
    pub fn mute(self) -> Self {
        if self.muted {
            self
        } else {
            Self {
                level_raw: self.level_raw, // remember
                muted: true,
            }
        }
    }

    /// Apply mute off: restore remembered level, set muted=false.
    pub fn unmute(self) -> Self {
        if !self.muted {
            self
        } else {
            Self {
                level_raw: self.level_raw, // already remembered
                muted: false,
            }
        }
    }

    /// Get the current level as percent (0..100) for D-Bus reporting.
    /// Canonical inverse of PERCENT_TO_RAW: returns the midpoint percent of
    /// the raw value's flat segment (the curve's top segment is compressed —
    /// several percents share one raw step), bounding the roundtrip error to
    /// half the segment width. Falls back to the nearest raw when the level
    /// is outside the curve's range.
    pub fn level_percent(&self, curve: &PercentToRaw) -> u8 {
        Self::raw_to_percent(self.level_raw, curve)
            .unwrap_or_else(|| Self::nearest_percent(self.level_raw, curve))
    }

    /// Midpoint percent of the flat segment for `raw`; `None` when no percent
    /// maps to it.
    fn raw_to_percent(raw: u16, curve: &PercentToRaw) -> Option<u8> {
        let mut first: Option<u8> = None;
        let mut last = 0u8;
        for (p, &r) in curve.iter().enumerate() {
            if r == raw {
                if first.is_none() {
                    first = Some(u8::try_from(p).ok()?);
                }
                last = u8::try_from(p).ok()?;
            }
        }
        first.map(|f| (f + last).div_ceil(2))
    }

    /// Nearest percent when `raw` is not on the curve.
    fn nearest_percent(raw: u16, curve: &PercentToRaw) -> u8 {
        let mut best = 0u8;
        let mut best_diff = u16::MAX;
        for (p, &r) in curve.iter().enumerate() {
            let diff = r.abs_diff(raw);
            if diff < best_diff {
                best_diff = diff;
                best = u8::try_from(p).unwrap_or(best);
            }
        }
        best
    }

    /// Get the hardware level as percent (what's actually on the amp).
    /// Canonical midpoint inverse, matching [`Self::level_percent`].
    pub fn hardware_level_percent(&self, curve: &PercentToRaw) -> u8 {
        Self::raw_to_percent(self.hardware_level(), curve)
            .unwrap_or_else(|| Self::nearest_percent(self.hardware_level(), curve))
    }
}

impl fmt::Display for AmpState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hw = self.hardware_level();
        write!(
            f,
            "AmpState(level_raw={}, muted={}, hw_level={})",
            self.level_raw, self.muted, hw
        )
    }
}
