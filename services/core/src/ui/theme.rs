//! Unified Global Theme System & Palettes.
//!
//! Provides strongly typed theme presets and palette models conforming to
//! Microsoft Pragmatic Rust Guidelines (`M-STRONG-TYPES`, `M-STRONG-TYPES-GUARD`).
//! Coordinates with Slint ahead-of-time declarative bindings (`ThemePalette`, `Theme`).

use std::fmt::{self, Display, Formatter};

use slint::ComponentHandle;

use super::generated::{MainWindow, Theme, ThemePalette};

/// Strongly typed theme preset identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum ThemePreset {
    /// Vintage `McIntosh` Electric Cyan & Deep Obsidian (`#00C8FF`).
    #[default]
    McIntoshBlue = 0,
    /// Warm Marantz / Accuphase Incandescent Amber (`#E5A93C`).
    MarantzAmber = 1,
    /// Retro Synthwave Neon Cyan & Magenta (`#00F0FF`).
    CyberpunkNeon = 2,
    /// Minimalist Studio Carbon & Precision Monochrome (`#F0F0F0`).
    StudioDark = 3,
}

impl ThemePreset {
    /// Number of theme presets available.
    pub const COUNT: usize = 4;

    /// Returns the integer index corresponding to this preset.
    #[must_use]
    pub const fn index(self) -> i32 {
        self as i32
    }

    /// Resolves a theme preset from an integer index (wraps modulo [`COUNT`](Self::COUNT)).
    #[must_use]
    pub const fn from_index(index: i32) -> Self {
        #[expect(
            clippy::cast_possible_wrap,
            clippy::cast_possible_truncation,
            reason = "COUNT is small (4)"
        )]
        match index.rem_euclid(Self::COUNT as i32) {
            0 => Self::McIntoshBlue,
            1 => Self::MarantzAmber,
            2 => Self::CyberpunkNeon,
            _ => Self::StudioDark,
        }
    }

    /// Returns the next theme preset in cyclic order.
    #[must_use]
    pub const fn next(self) -> Self {
        Self::from_index(self.index() + 1)
    }

    /// Returns the human-readable display name for this theme preset.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::McIntoshBlue => "Vintage McIntosh Blue",
            Self::MarantzAmber => "Marantz Amber",
            Self::CyberpunkNeon => "Retro Cyberpunk Neon",
            Self::StudioDark => "Minimalist Studio Dark",
        }
    }

    /// Returns the URL- and config-friendly slug for this theme preset.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::McIntoshBlue => "mcintosh",
            Self::MarantzAmber => "marantz",
            Self::CyberpunkNeon => "cyberpunk",
            Self::StudioDark => "studio-dark",
        }
    }

    /// Returns the associated default `PeppyMeter` skin identifier.
    #[must_use]
    pub const fn meter_skin_id(self) -> &'static str {
        match self {
            Self::McIntoshBlue => "mcintosh_800x480",
            Self::MarantzAmber => "marantz_800x480",
            Self::CyberpunkNeon => "cyberpunk_800x480",
            Self::StudioDark => "studio_dark_800x480",
        }
    }

    /// Parses a theme preset from a slug or name string.
    ///
    /// # Errors
    /// Returns a descriptive error message if `s` is not recognized.
    pub fn parse_slug(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mcintosh" | "mcintosh-blue" | "vintage-mcintosh" | "blue" | "0" => {
                Ok(Self::McIntoshBlue)
            }
            "marantz" | "marantz-amber" | "accuphase" | "amber" | "1" => Ok(Self::MarantzAmber),
            "cyberpunk" | "cyberpunk-neon" | "synthwave" | "neon" | "2" => Ok(Self::CyberpunkNeon),
            "studio-dark" | "studio" | "dark" | "minimalist" | "mono" | "3" => Ok(Self::StudioDark),
            other => Err(format!(
                "unknown theme preset '{other}': expected 'mcintosh', 'marantz', 'cyberpunk', or 'studio-dark'"
            )),
        }
    }

    /// Builds the corresponding Slint [`ThemePalette`] model for this preset.
    #[must_use]
    pub fn palette(self) -> ThemePalette {
        match self {
            Self::McIntoshBlue => ThemePalette {
                chassis: slint::Color::from_rgb_u8(0x09, 0x0D, 0x12),
                card_surface: slint::Color::from_rgb_u8(0x11, 0x19, 0x23),
                card_border: slint::Color::from_rgb_u8(0x30, 0x36, 0x3D),
                primary_accent: slint::Color::from_rgb_u8(0x00, 0xC8, 0xFF),
                secondary_accent: slint::Color::from_rgb_u8(0x70, 0xE1, 0xFF),
                needle_peak: slint::Color::from_rgb_u8(0xFF, 0x33, 0x44),
                dock_active_text: slint::Color::from_rgb_u8(0x09, 0x0D, 0x12),
                text_primary: slint::Color::from_rgb_u8(0xF0, 0xF6, 0xFC),
                text_muted: slint::Color::from_rgb_u8(0x8B, 0x94, 0x9E),
                meter_skin_id: self.meter_skin_id().into(),
            },
            Self::MarantzAmber => ThemePalette {
                chassis: slint::Color::from_rgb_u8(0x12, 0x10, 0x0D),
                card_surface: slint::Color::from_rgb_u8(0x1C, 0x17, 0x12),
                card_border: slint::Color::from_rgb_u8(0x3A, 0x2E, 0x22),
                primary_accent: slint::Color::from_rgb_u8(0xE5, 0xA9, 0x3C),
                secondary_accent: slint::Color::from_rgb_u8(0xFF, 0xD1, 0x66),
                needle_peak: slint::Color::from_rgb_u8(0xD9, 0x38, 0x29),
                dock_active_text: slint::Color::from_rgb_u8(0x12, 0x10, 0x0D),
                text_primary: slint::Color::from_rgb_u8(0xFD, 0xFB, 0xF7),
                text_muted: slint::Color::from_rgb_u8(0x9C, 0x92, 0x83),
                meter_skin_id: self.meter_skin_id().into(),
            },
            Self::CyberpunkNeon => ThemePalette {
                chassis: slint::Color::from_rgb_u8(0x0D, 0x0B, 0x18),
                card_surface: slint::Color::from_rgb_u8(0x1B, 0x14, 0x2E),
                card_border: slint::Color::from_rgb_u8(0x3C, 0x23, 0x5A),
                primary_accent: slint::Color::from_rgb_u8(0x00, 0xF0, 0xFF),
                secondary_accent: slint::Color::from_rgb_u8(0xFF, 0x00, 0x7F),
                needle_peak: slint::Color::from_rgb_u8(0xFF, 0x14, 0x93),
                dock_active_text: slint::Color::from_rgb_u8(0x0D, 0x0B, 0x18),
                text_primary: slint::Color::from_rgb_u8(0xF5, 0xEE, 0xFD),
                text_muted: slint::Color::from_rgb_u8(0xA3, 0x90, 0xC5),
                meter_skin_id: self.meter_skin_id().into(),
            },
            Self::StudioDark => ThemePalette {
                chassis: slint::Color::from_rgb_u8(0x12, 0x12, 0x12),
                card_surface: slint::Color::from_rgb_u8(0x1E, 0x1E, 0x1E),
                card_border: slint::Color::from_rgb_u8(0x2D, 0x2D, 0x2D),
                primary_accent: slint::Color::from_rgb_u8(0xF0, 0xF0, 0xF0),
                secondary_accent: slint::Color::from_rgb_u8(0xA0, 0xA0, 0xA0),
                needle_peak: slint::Color::from_rgb_u8(0x00, 0xE6, 0x76),
                dock_active_text: slint::Color::from_rgb_u8(0x00, 0x00, 0x00),
                text_primary: slint::Color::from_rgb_u8(0xFF, 0xFF, 0xFF),
                text_muted: slint::Color::from_rgb_u8(0x88, 0x88, 0x88),
                meter_skin_id: self.meter_skin_id().into(),
            },
        }
    }

    /// Applies this theme preset to a [`MainWindow`] component.
    pub fn apply(self, window: &MainWindow) {
        let theme = window.global::<Theme>();
        theme.set_active_preset(self.index());
        theme.set_active(self.palette());
    }
}

impl Display for ThemePreset {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_preset_indexing_and_cyclic_next() {
        assert_eq!(ThemePreset::from_index(0), ThemePreset::McIntoshBlue);
        assert_eq!(ThemePreset::from_index(1), ThemePreset::MarantzAmber);
        assert_eq!(ThemePreset::from_index(2), ThemePreset::CyberpunkNeon);
        assert_eq!(ThemePreset::from_index(3), ThemePreset::StudioDark);
        assert_eq!(ThemePreset::from_index(4), ThemePreset::McIntoshBlue);
        assert_eq!(ThemePreset::from_index(-1), ThemePreset::StudioDark);

        assert_eq!(ThemePreset::McIntoshBlue.next(), ThemePreset::MarantzAmber);
        assert_eq!(ThemePreset::MarantzAmber.next(), ThemePreset::CyberpunkNeon);
        assert_eq!(ThemePreset::CyberpunkNeon.next(), ThemePreset::StudioDark);
        assert_eq!(ThemePreset::StudioDark.next(), ThemePreset::McIntoshBlue);
    }

    #[test]
    fn slug_parsing_valid_and_invalid() {
        assert_eq!(
            ThemePreset::parse_slug("mcintosh").unwrap(),
            ThemePreset::McIntoshBlue
        );
        assert_eq!(
            ThemePreset::parse_slug("Marantz").unwrap(),
            ThemePreset::MarantzAmber
        );
        assert_eq!(
            ThemePreset::parse_slug("cyberpunk-neon").unwrap(),
            ThemePreset::CyberpunkNeon
        );
        assert_eq!(
            ThemePreset::parse_slug("studio-dark").unwrap(),
            ThemePreset::StudioDark
        );
        assert_eq!(
            ThemePreset::parse_slug("0").unwrap(),
            ThemePreset::McIntoshBlue
        );
        assert_eq!(
            ThemePreset::parse_slug("3").unwrap(),
            ThemePreset::StudioDark
        );

        ThemePreset::parse_slug("unknown-theme").unwrap_err();
        ThemePreset::parse_slug("").unwrap_err();
    }

    #[test]
    fn display_and_meter_skin_id_consistency() {
        for preset in [
            ThemePreset::McIntoshBlue,
            ThemePreset::MarantzAmber,
            ThemePreset::CyberpunkNeon,
            ThemePreset::StudioDark,
        ] {
            assert_ne!(preset.display_name(), "");
            assert_ne!(preset.slug(), "");
            assert_ne!(preset.meter_skin_id(), "");
            assert_eq!(preset.to_string(), preset.display_name());

            let palette = preset.palette();
            assert_eq!(palette.meter_skin_id.as_str(), preset.meter_skin_id());
        }
    }
}
