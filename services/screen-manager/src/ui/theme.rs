//! Unified Global Theme System & Palettes.
//!
//! Provides strongly typed theme presets and palette models conforming to
//! Microsoft Pragmatic Rust Guidelines (`M-STRONG-TYPES`, `M-STRONG-TYPES-GUARD`).
//! Coordinates with Slint ahead-of-time declarative bindings (`ThemePalette`, `Theme`).

use std::fmt::{self, Display, Formatter};

use slint::ComponentHandle;

use super::generated::{
    AppTheme, MainWindow, MaterialPalette, MaterialScheme, MaterialSchemes, SplashScreen, Theme,
    ThemePalette,
};

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
    /// Classic `PeppyMeter` Vintage Gold, Incandescent Cream & Deep Walnut (`#F5B83D`).
    Peppy = 4,
}

impl ThemePreset {
    /// Number of theme presets available.
    pub const COUNT: usize = 5;

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
            reason = "COUNT is small (5)"
        )]
        match index.rem_euclid(Self::COUNT as i32) {
            0 => Self::McIntoshBlue,
            1 => Self::MarantzAmber,
            2 => Self::CyberpunkNeon,
            3 => Self::StudioDark,
            _ => Self::Peppy,
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
            Self::Peppy => "PeppyMeter Vintage Gold",
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
            Self::Peppy => "peppy",
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
            Self::Peppy => "peppy_800x480",
        }
    }

    /// Returns the primary font family name for this theme preset.
    #[must_use]
    #[expect(
        clippy::match_same_arms,
        reason = "Explicit per-theme arm structure allows each theme preset to independently customize its typography"
    )]
    pub const fn font_family(self) -> &'static str {
        match self {
            Self::McIntoshBlue => "Inter",
            Self::MarantzAmber => "Inter",
            Self::CyberpunkNeon => "Inter",
            Self::StudioDark => "Inter",
            Self::Peppy => "Inter",
        }
    }

    /// `(primary, secondary)` swatch colors per preset, mirroring the literal
    /// hexes in `settings_view.slint` (`ThemePresetCard` `primary_color` /
    /// `secondary_color`). The unit test below pins the two sides together.
    #[expect(
        clippy::type_complexity,
        reason = "flat lookup table of 5 (primary, secondary) RGB pairs"
    )]
    pub const PRESET_SWATCHES: [((u8, u8, u8), (u8, u8, u8)); 5] = [
        ((0x00, 0xC8, 0xFF), (0x70, 0xE1, 0xFF)), // McIntosh
        ((0xE5, 0xA9, 0x3C), (0xFF, 0xD1, 0x66)), // Marantz
        ((0x00, 0xF0, 0xFF), (0xFF, 0x00, 0x7F)), // Cyberpunk
        ((0xF0, 0xF0, 0xF0), (0xA0, 0xA0, 0xA0)), // Studio Dark
        ((0xF5, 0xB8, 0x3D), (0xFF, 0xDF, 0x85)), // Peppy
    ];

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
            "peppy" | "peepy" | "peppymeter" | "peppy-gold" | "peppy-vintage" | "gold" | "4" => {
                Ok(Self::Peppy)
            }
            other => Err(format!(
                "unknown theme preset '{other}': expected 'mcintosh', 'marantz', 'cyberpunk', 'studio-dark', or 'peppy'"
            )),
        }
    }

    /// Builds the corresponding Material [`MaterialSchemes`] model for this preset (dark-only, light==dark).
    /// Returns a `MaterialSchemes` with both `light` and `dark` set to the same dark scheme.
    #[must_use]
    pub fn material_schemes(self) -> MaterialSchemes {
        let dark_scheme = self.material_scheme_dark();
        MaterialSchemes {
            light: dark_scheme.clone(),
            dark: dark_scheme,
        }
    }

    /// Weighted average of two colors (for container derivation).
    fn avg_color(a: slint::Color, b: slint::Color, wa: u16, wb: u16) -> slint::Color {
        let blend = |x: u8, y: u8| -> u8 {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "weighted average of two u8 values fits in u8"
            )]
            {
                ((u16::from(x) * wa + u16::from(y) * wb) / (wa + wb)) as u8
            }
        };
        slint::Color::from_rgb_u8(
            blend(a.red(), b.red()),
            blend(a.green(), b.green()),
            blend(a.blue(), b.blue()),
        )
    }

    /// Per-preset base colors (same values as the legacy `ThemePalette`).
    #[must_use]
    fn material_base_colors(
        self,
    ) -> (
        slint::Color,
        slint::Color,
        slint::Color,
        slint::Color,
        slint::Color,
        slint::Color,
        slint::Color,
        slint::Color,
        slint::Color,
    ) {
        let c = |r: u8, g: u8, b: u8| slint::Color::from_rgb_u8(r, g, b);
        match self {
            Self::McIntoshBlue => (
                c(0x00, 0xC8, 0xFF),
                c(0x70, 0xE1, 0xFF),
                c(0x09, 0x0D, 0x12),
                c(0x11, 0x19, 0x23),
                c(0x30, 0x36, 0x3D),
                c(0xF0, 0xF6, 0xFC),
                c(0x8B, 0x94, 0x9E),
                c(0xFF, 0x33, 0x44),
                c(0x09, 0x0D, 0x12),
            ),
            Self::MarantzAmber => (
                c(0xE5, 0xA9, 0x3C),
                c(0xFF, 0xD1, 0x66),
                c(0x12, 0x10, 0x0D),
                c(0x1C, 0x17, 0x12),
                c(0x3A, 0x2E, 0x22),
                c(0xFD, 0xFB, 0xF7),
                c(0x9C, 0x92, 0x83),
                c(0xD9, 0x38, 0x29),
                c(0x12, 0x10, 0x0D),
            ),
            Self::CyberpunkNeon => (
                c(0x00, 0xF0, 0xFF),
                c(0xFF, 0x00, 0x7F),
                c(0x0D, 0x0B, 0x18),
                c(0x1B, 0x14, 0x2E),
                c(0x3C, 0x23, 0x5A),
                c(0xF5, 0xEE, 0xFD),
                c(0xA3, 0x90, 0xC5),
                c(0xFF, 0x14, 0x93),
                c(0x0D, 0x0B, 0x18),
            ),
            Self::StudioDark => (
                c(0xF0, 0xF0, 0xF0),
                c(0xA0, 0xA0, 0xA0),
                c(0x12, 0x12, 0x12),
                c(0x1E, 0x1E, 0x1E),
                c(0x2D, 0x2D, 0x2D),
                c(0xFF, 0xFF, 0xFF),
                c(0x88, 0x88, 0x88),
                c(0x00, 0xE6, 0x76),
                c(0x00, 0x00, 0x00),
            ),
            Self::Peppy => (
                c(0xF5, 0xB8, 0x3D),
                c(0xFF, 0xDF, 0x85),
                c(0x14, 0x11, 0x0D),
                c(0x20, 0x1A, 0x14),
                c(0x48, 0x38, 0x24),
                c(0xFF, 0xFD, 0xF5),
                c(0xA6, 0x9B, 0x8D),
                c(0xE6, 0x39, 0x46),
                c(0x14, 0x11, 0x0D),
            ),
        }
    }

    /// Builds the dark [`MaterialScheme`] for this preset.
    #[must_use]
    fn material_scheme_dark(self) -> MaterialScheme {
        let avg = Self::avg_color;
        let (
            primary,
            secondary,
            chassis,
            card_surface,
            card_border,
            text_primary,
            text_muted,
            needle_peak,
            dock_active_text,
        ) = self.material_base_colors();

        // Container derivations (darken/lighten from base surface) using weighted average helper
        let surface_container_high = avg(card_surface, chassis, 1, 2);
        let surface_container_highest = avg(card_surface, chassis, 2, 1);
        let surface_container_low = avg(card_surface, chassis, 1, 1);
        let primary_container = avg(primary, chassis, 1, 3);
        let secondary_container = avg(secondary, chassis, 1, 3);
        let surface_dim = avg(chassis, card_surface, 3, 1);
        let surface_bright = avg(card_surface, text_primary, 3, 1);
        let surface_container_lowest = chassis;
        let primary_fixed = primary;
        let primary_fixed_dim = primary;
        let secondary_fixed = secondary;
        let secondary_fixed_dim = secondary;

        MaterialScheme {
            primary,
            surfaceTint: primary,
            onPrimary: dock_active_text,
            primaryContainer: primary_container,
            onPrimaryContainer: text_primary,
            secondary,
            onSecondary: dock_active_text,
            secondaryContainer: secondary_container,
            onSecondaryContainer: text_primary,
            tertiary: needle_peak,
            onTertiary: text_primary,
            tertiaryContainer: surface_container_high,
            onTertiaryContainer: text_primary,
            error: needle_peak,
            onError: text_primary,
            errorContainer: surface_container_high,
            onErrorContainer: text_primary,
            background: chassis,
            onBackground: text_primary,
            surface: card_surface,
            onSurface: text_primary,
            surfaceVariant: card_border,
            onSurfaceVariant: text_muted,
            outline: card_border,
            outlineVariant: card_border,
            shadow: slint::Color::from_rgb_u8(0, 0, 0),
            scrim: slint::Color::from_rgb_u8(0, 0, 0),
            inverseSurface: text_primary,
            inverseOnSurface: chassis,
            inversePrimary: primary_container,
            primaryFixed: primary_fixed,
            onPrimaryFixed: dock_active_text,
            primaryFixedDim: primary_fixed_dim,
            onPrimaryFixedVariant: primary_container,
            secondaryFixed: secondary_fixed,
            onSecondaryFixed: dock_active_text,
            secondaryFixedDim: secondary_fixed_dim,
            onSecondaryFixedVariant: secondary_container,
            tertiaryFixed: needle_peak,
            onTertiaryFixed: text_primary,
            tertiaryFixedDim: needle_peak,
            onTertiaryFixedVariant: surface_container_high,
            surfaceDim: surface_dim,
            surfaceBright: surface_bright,
            surfaceContainerLowest: surface_container_lowest,
            surfaceContainerLow: surface_container_low,
            surfaceContainer: card_surface,
            surfaceContainerHigh: surface_container_high,
            surfaceContainerHighest: surface_container_highest,
        }
    }

    /// Applies this theme preset to a [`MainWindow`] component.
    pub fn apply(self, window: &MainWindow) {
        // Push Material scheme
        let material_palette = window.global::<MaterialPalette>();
        // Force dark mode: all presets are dark-only (light == dark scheme)
        material_palette
            .set_color_scheme(slint::private_unstable_api::re_exports::ColorScheme::Dark);
        let schemes = self.material_schemes();
        material_palette.set_schemes(schemes);

        // Single source of truth for the preset index (skin selection reads this).
        window.global::<AppTheme>().set_active_preset(self.index());

        // Also update legacy Theme for now (P2-P5 will migrate off)
        let theme = window.global::<Theme>();
        theme.set_active_preset(self.index());
        // Inline ThemePalette creation (palette() method removed, replaced by material_schemes())
        let palette = match self {
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
                font_family: self.font_family().into(),
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
                font_family: self.font_family().into(),
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
                font_family: self.font_family().into(),
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
                font_family: self.font_family().into(),
            },
            Self::Peppy => ThemePalette {
                chassis: slint::Color::from_rgb_u8(0x14, 0x11, 0x0D),
                card_surface: slint::Color::from_rgb_u8(0x20, 0x1A, 0x14),
                card_border: slint::Color::from_rgb_u8(0x48, 0x38, 0x24),
                primary_accent: slint::Color::from_rgb_u8(0xF5, 0xB8, 0x3D),
                secondary_accent: slint::Color::from_rgb_u8(0xFF, 0xDF, 0x85),
                needle_peak: slint::Color::from_rgb_u8(0xE6, 0x39, 0x46),
                dock_active_text: slint::Color::from_rgb_u8(0x14, 0x11, 0x0D),
                text_primary: slint::Color::from_rgb_u8(0xFF, 0xFD, 0xF5),
                text_muted: slint::Color::from_rgb_u8(0xA6, 0x9B, 0x8D),
                meter_skin_id: self.meter_skin_id().into(),
                font_family: self.font_family().into(),
            },
        };
        theme.set_active(palette);
    }

    /// Applies this theme preset to a [`SplashScreen`] component.
    pub fn apply_splash(self, splash: &SplashScreen) {
        // Splash has its own component tree: push Material schemes into its
        // MaterialPalette global (colors) and legacy Theme (font_family only).
        let material_palette = splash.global::<MaterialPalette>();
        material_palette
            .set_color_scheme(slint::private_unstable_api::re_exports::ColorScheme::Dark);
        material_palette.set_schemes(self.material_schemes());

        let theme = splash.global::<Theme>();
        theme.set_active_preset(self.index());
        // Keep legacy ThemePalette for splash during transition (P5 removes Theme)
        let palette = match self {
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
                font_family: self.font_family().into(),
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
                font_family: self.font_family().into(),
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
                font_family: self.font_family().into(),
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
                font_family: self.font_family().into(),
            },
            Self::Peppy => ThemePalette {
                chassis: slint::Color::from_rgb_u8(0x14, 0x11, 0x0D),
                card_surface: slint::Color::from_rgb_u8(0x20, 0x1A, 0x14),
                card_border: slint::Color::from_rgb_u8(0x48, 0x38, 0x24),
                primary_accent: slint::Color::from_rgb_u8(0xF5, 0xB8, 0x3D),
                secondary_accent: slint::Color::from_rgb_u8(0xFF, 0xDF, 0x85),
                needle_peak: slint::Color::from_rgb_u8(0xE6, 0x39, 0x46),
                dock_active_text: slint::Color::from_rgb_u8(0x14, 0x11, 0x0D),
                text_primary: slint::Color::from_rgb_u8(0xFF, 0xFD, 0xF5),
                text_muted: slint::Color::from_rgb_u8(0xA6, 0x9B, 0x8D),
                meter_skin_id: self.meter_skin_id().into(),
                font_family: self.font_family().into(),
            },
        };
        theme.set_active(palette);
    }
}

impl Display for ThemePreset {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

impl serde::Serialize for ThemePreset {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.slug())
    }
}

impl<'de> serde::Deserialize<'de> for ThemePreset {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Own the slug string: the layered `config` reader holds owned values
        // and cannot guarantee a `&'de str` borrow.
        let slug = String::deserialize(deserializer)?;
        Self::parse_slug(&slug).map_err(serde::de::Error::custom)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const ALL_PRESETS: [ThemePreset; 5] = [
        ThemePreset::McIntoshBlue,
        ThemePreset::MarantzAmber,
        ThemePreset::CyberpunkNeon,
        ThemePreset::StudioDark,
        ThemePreset::Peppy,
    ];

    #[test]
    fn preset_swatches_match_base_colors() {
        // Pins settings_view.slint literal swatches to the Rust scheme source.
        // Expected hexes are duplicated from settings_view.slint.
        let expected = [
            ((0x00, 0xC8, 0xFF), (0x70, 0xE1, 0xFF)),
            ((0xE5, 0xA9, 0x3C), (0xFF, 0xD1, 0x66)),
            ((0x00, 0xF0, 0xFF), (0xFF, 0x00, 0x7F)),
            ((0xF0, 0xF0, 0xF0), (0xA0, 0xA0, 0xA0)),
            ((0xF5, 0xB8, 0x3D), (0xFF, 0xDF, 0x85)),
        ];
        assert_eq!(ThemePreset::PRESET_SWATCHES, expected);
        for (preset, ((pr, pg, pb), (sr, sg, sb))) in
            ALL_PRESETS.iter().zip(ThemePreset::PRESET_SWATCHES)
        {
            let (primary, secondary, ..) = preset.material_base_colors();
            assert_eq!(primary, slint::Color::from_rgb_u8(pr, pg, pb));
            assert_eq!(secondary, slint::Color::from_rgb_u8(sr, sg, sb));
        }
    }

    #[test]
    fn material_schemes_dark_equals_light() {
        for preset in ALL_PRESETS {
            let schemes = preset.material_schemes();
            assert_eq!(schemes.dark, schemes.light, "{preset:?} dark != light");
        }
    }

    #[test]
    fn material_schemes_presets_are_distinct() {
        let primaries: Vec<_> = ALL_PRESETS
            .iter()
            .map(|p| p.material_schemes().dark.primary)
            .collect();
        for (i, a) in primaries.iter().enumerate() {
            for b in &primaries[i + 1..] {
                assert_ne!(a, b, "preset primaries must be distinct");
            }
        }
        // Spot-check each preset's primary against its legacy accent color.
        let expected = [
            slint::Color::from_rgb_u8(0x00, 0xC8, 0xFF),
            slint::Color::from_rgb_u8(0xE5, 0xA9, 0x3C),
            slint::Color::from_rgb_u8(0x00, 0xF0, 0xFF),
            slint::Color::from_rgb_u8(0xF0, 0xF0, 0xF0),
            slint::Color::from_rgb_u8(0xF5, 0xB8, 0x3D),
        ];
        for (preset, want) in ALL_PRESETS.iter().zip(expected) {
            assert_eq!(preset.material_schemes().dark.primary, want);
        }
    }

    #[test]
    fn material_schemes_core_tokens_match_legacy_palette() {
        for preset in ALL_PRESETS {
            let dark = preset.material_schemes().dark;
            let (
                primary,
                _,
                chassis,
                card_surface,
                card_border,
                text_primary,
                text_muted,
                needle_peak,
                dock_active_text,
            ) = preset.material_base_colors();
            // Legacy -> Material derivation contract (see THEME_MIGRATION_PLAN.md P1).
            assert_eq!(dark.primary, primary);
            assert_eq!(dark.background, chassis);
            assert_eq!(dark.surface, card_surface);
            assert_eq!(dark.outline, card_border);
            assert_eq!(dark.outlineVariant, card_border);
            assert_eq!(dark.onBackground, text_primary);
            assert_eq!(dark.onSurface, text_primary);
            assert_eq!(dark.onSurfaceVariant, text_muted);
            assert_eq!(dark.error, needle_peak);
            assert_eq!(dark.onPrimary, dock_active_text);
            assert_eq!(dark.onSecondary, dock_active_text);
        }
    }

    #[test]
    fn serializes_to_slug() {
        for preset in [
            ThemePreset::McIntoshBlue,
            ThemePreset::MarantzAmber,
            ThemePreset::CyberpunkNeon,
            ThemePreset::StudioDark,
            ThemePreset::Peppy,
        ] {
            let json = serde_json::to_string(&preset).unwrap();
            assert_eq!(json, format!("\"{}\"", preset.slug()));
        }
    }

    #[test]
    fn deserializes_slug_and_aliases() {
        for (slug, expected) in [
            ("mcintosh", ThemePreset::McIntoshBlue),
            ("marantz", ThemePreset::MarantzAmber),
            ("cyberpunk", ThemePreset::CyberpunkNeon),
            ("studio-dark", ThemePreset::StudioDark),
            ("peppy", ThemePreset::Peppy),
            ("blue", ThemePreset::McIntoshBlue),
            ("gold", ThemePreset::Peppy),
        ] {
            assert_eq!(
                serde_json::from_str::<ThemePreset>(&format!("\"{slug}\"")).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn deserializes_from_value_and_reader() {
        let value = serde_json::Value::String("marantz".to_owned());
        assert_eq!(
            serde_json::from_value::<ThemePreset>(value).unwrap(),
            ThemePreset::MarantzAmber
        );
        let reader = std::io::Cursor::new(b"\"peppy\"".to_vec());
        assert_eq!(
            serde_json::from_reader::<_, ThemePreset>(reader).unwrap(),
            ThemePreset::Peppy
        );
    }

    #[test]
    fn rejects_unknown_slug() {
        let err = serde_json::from_str::<ThemePreset>("\"bogus\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown theme preset 'bogus'"), "{err}");
    }
}
