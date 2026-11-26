//! Material 3 dynamic cover-art color extraction and theme resolution engine.
//!
//! Pure-Rust color pipeline built on `mcu-material-color` (Google's Material
//! Color Utilities) that extracts a dominant seed color from album artwork via
//! `QuantizerCelebi` + `Score` and resolves a dark [`MaterialScheme`] according
//! to the user's selected [`ThemePreset`] and [`CoverThemeMode`].

use std::collections::{HashMap, VecDeque};
use std::fmt::{self, Display, Formatter};

use mcu_material_color::prelude::*;

use super::generated::MaterialScheme;
use super::theme::ThemePreset;

/// Minimum HCT chroma for an extracted cover-art seed to be treated as chromatic.
///
/// Lower-chroma (black-and-white / grayscale) covers return `None` so the UI
/// keeps the active [`ThemePreset`] instead of flashing a generic fallback hue.
pub const MIN_COVER_CHROMA: f64 = 15.0;

/// Target dimension (px) for adaptive stride sampling (~4,096 pixels total).
const SAMPLE_TARGET_DIM: u32 = 64;

/// Minimum alpha channel value (0–255) for a pixel to contribute to quantization.
const MIN_PIXEL_ALPHA: u8 = 200;

/// Maximum palette clusters passed to `QuantizerCelebi`.
const QUANTIZE_MAX_COLORS: usize = 128;

/// CAM16-UCS blend factor for accents in [`CoverThemeMode::SubtleTint`].
const SUBTLE_TINT_ACCENT_STRENGTH: f64 = 0.28;

/// CAM16-UCS blend factor for dark surfaces in [`CoverThemeMode::SubtleTint`].
const SUBTLE_TINT_SURFACE_STRENGTH: f64 = 0.18;

/// Subtle CAM16-UCS tint used in [`CoverThemeMode::Harmonize`] when the base
/// accent is achromatic (e.g. `StudioDark` `#F0F0F0`), where pure hue rotation
/// would otherwise be a no-op.
const ACHROMATIC_HARMONIZE_STRENGTH: f64 = 0.12;

/// HCT chroma threshold below which a base accent is treated as achromatic.
const ACHROMATIC_CHROMA_THRESHOLD: f64 = 5.0;

/// Default maximum number of cached cover-art seeds in [`BoundedSeedCache`].
pub const DEFAULT_SEED_CACHE_CAPACITY: usize = 64;

/// Cover-art dynamic theming adaptation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum CoverThemeMode {
    /// Ignore cover art; render the selected [`ThemePreset`] verbatim.
    Off = 0,
    /// Keep preset neutral surfaces fixed; subtly harmonize accents toward the cover art.
    Harmonize = 1,
    /// Gently blend preset accents and surfaces toward the cover art in CAM16-UCS space.
    #[default]
    SubtleTint = 2,
    /// Full dark Material 3 scheme generated directly from the cover-art seed.
    AutoCover = 3,
}

impl CoverThemeMode {
    /// Number of cover-theme modes available.
    pub const COUNT: usize = 4;

    /// Returns the integer index corresponding to this mode.
    #[must_use]
    pub const fn index(self) -> i32 {
        self as i32
    }

    /// Resolves a mode from an integer index (wraps modulo [`COUNT`](Self::COUNT)).
    #[must_use]
    pub const fn from_index(index: i32) -> Self {
        #[expect(
            clippy::cast_possible_wrap,
            reason = "COUNT is 4, which always fits in i32"
        )]
        match index.rem_euclid(Self::COUNT as i32) {
            0 => Self::Off,
            1 => Self::Harmonize,
            2 => Self::SubtleTint,
            _ => Self::AutoCover,
        }
    }

    /// Advances or rewinds this mode by `dir` steps (wrapping cyclically).
    #[must_use]
    pub const fn step(self, dir: i32) -> Self {
        Self::from_index(self.index().saturating_add(dir))
    }

    /// Returns the human-readable UI label for this mode.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Harmonize => "Harmonize",
            Self::SubtleTint => "Subtle Tint",
            Self::AutoCover => "Auto Cover",
        }
    }

    /// Returns the config-friendly slug for this mode.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Harmonize => "harmonize",
            Self::SubtleTint => "subtle-tint",
            Self::AutoCover => "auto-cover",
        }
    }

    /// Parses a cover theme mode from a slug or numeric index string.
    ///
    /// # Errors
    /// Returns a descriptive error message when `s` is not recognized.
    pub fn parse_slug(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "predefined" | "none" | "0" => Ok(Self::Off),
            "harmonize" | "harmonized" | "1" => Ok(Self::Harmonize),
            "subtle-tint" | "subtle_tint" | "subtle" | "tint" | "2" => Ok(Self::SubtleTint),
            "auto-cover" | "auto_cover" | "auto" | "full" | "3" => Ok(Self::AutoCover),
            other => Err(format!(
                "unknown cover_theme_mode '{other}': expected 'off', 'harmonize', 'subtle-tint', or 'auto-cover'"
            )),
        }
    }
}

impl Display for CoverThemeMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

impl serde::Serialize for CoverThemeMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.slug())
    }
}

impl<'de> serde::Deserialize<'de> for CoverThemeMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let slug = String::deserialize(deserializer)?;
        Self::parse_slug(&slug).map_err(serde::de::Error::custom)
    }
}

/// Bounded FIFO cache mapping cover-art paths to their extracted ARGB seeds.
///
/// Prevents unbounded memory growth over weeks of appliance uptime while
/// skipping `QuantizerCelebi` on consecutive tracks sharing the same album art.
#[derive(Debug, Clone)]
pub struct BoundedSeedCache {
    capacity: usize,
    entries: HashMap<Box<str>, Option<u32>>,
    order: VecDeque<Box<str>>,
}

impl Default for BoundedSeedCache {
    fn default() -> Self {
        Self::new(DEFAULT_SEED_CACHE_CAPACITY)
    }
}

impl BoundedSeedCache {
    /// Creates a new bounded cache with the given maximum number of entries.
    ///
    /// A `capacity` of `0` is clamped to `1`.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            capacity: cap,
            entries: HashMap::with_capacity(cap),
            order: VecDeque::with_capacity(cap),
        }
    }

    /// Looks up a previously extracted seed for `key`.
    ///
    /// Returns `Some(seed)` on cache hit (where `seed` itself may be `None` for
    /// monochrome artwork) and `None` on cache miss.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Option<u32>> {
        self.entries.get(key).copied()
    }

    /// Inserts or updates a seed for `key`, evicting the oldest entry if full.
    pub fn insert(&mut self, key: Box<str>, seed: Option<u32>) {
        if let Some(existing) = self.entries.get_mut(&key) {
            *existing = seed;
            return;
        }
        if self.entries.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.entries.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, seed);
    }

    /// Returns the number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when the cache contains no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Converts a `0xAARRGGBB` ARGB integer into a [`slint::Color`].
#[must_use]
pub fn argb_to_slint(argb: u32) -> slint::Color {
    slint::Color::from_argb_encoded(argb)
}

/// Converts a [`slint::Color`] into a `0xAARRGGBB` ARGB integer.
#[must_use]
pub fn slint_to_argb(c: slint::Color) -> u32 {
    (u32::from(c.alpha()) << 24)
        | (u32::from(c.red()) << 16)
        | (u32::from(c.green()) << 8)
        | u32::from(c.blue())
}

/// Adaptively stride-samples an RGBA8 pixel slice into a compact `Vec<u32>`
/// (`0xAARRGGBB`) bounded to roughly `64x64` (~4,096) opaque pixels.
///
/// Returns an empty vector if `width == 0`, `height == 0`, or `rgba` is shorter
/// than `width * height * 4` bytes.
#[must_use]
pub fn sample_argb_pixels_rgba8(rgba: &[u8], width: u32, height: u32) -> Vec<u32> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let width_px = width as usize;
    let height_px = height as usize;
    let Some(expected_bytes) = width_px
        .checked_mul(height_px)
        .and_then(|px| px.checked_mul(4))
    else {
        return Vec::new();
    };
    if rgba.len() < expected_bytes {
        return Vec::new();
    }

    let step = (width.max(height) / SAMPLE_TARGET_DIM).max(1) as usize;
    let est_cap = (width_px / step + 1) * (height_px / step + 1);
    let mut out = Vec::with_capacity(est_cap);

    for row in (0..height_px).step_by(step) {
        let row_base = row * width_px;
        for col in (0..width_px).step_by(step) {
            let off = (row_base + col) * 4;
            let alpha = rgba[off + 3];
            if alpha < MIN_PIXEL_ALPHA {
                continue;
            }
            let red = rgba[off];
            let green = rgba[off + 1];
            let blue = rgba[off + 2];
            let argb = (u32::from(alpha) << 24)
                | (u32::from(red) << 16)
                | (u32::from(green) << 8)
                | u32::from(blue);
            out.push(argb);
        }
    }
    out
}

/// Extracts a dominant ARGB seed color from sampled `0xAARRGGBB` pixels using
/// `QuantizerCelebi` and `Score`.
///
/// Returns `None` when `argb_pixels` is empty, no suitable chromatic cluster
/// survives scoring, or the top-ranked candidate has HCT chroma below
/// [`MIN_COVER_CHROMA`] (grayscale/monochrome artwork).
#[must_use]
pub fn extract_cover_seed(argb_pixels: &[u32]) -> Option<u32> {
    /// Transparent sentinel passed as `ScoreOptions::fallback_color_argb`.
    /// Because `sample_argb_pixels_rgba8` filters to `alpha >= 200`, this
    /// value can never be a real quantized pixel and unambiguously signals
    /// that `Score` filtered out every candidate (e.g. monochrome art),
    /// preventing MCU's default Google Blue fallback (`0xFF4285F4`).
    const FALLBACK_SENTINEL: u32 = 0x0000_0000;

    if argb_pixels.is_empty() {
        return None;
    }
    let quantized = QuantizerCelebi::quantize(argb_pixels, QUANTIZE_MAX_COLORS);
    let options = ScoreOptions::default()
        .with_desired(4)
        .with_filter(true)
        .with_fallback_color(FALLBACK_SENTINEL);
    let ranked = Score::score_options(&quantized, options);
    let candidate = ranked.first().copied()?;

    if candidate == FALLBACK_SENTINEL {
        return None;
    }
    let hct = Hct::from_int(candidate);
    if hct.chroma() < MIN_COVER_CHROMA {
        return None;
    }
    Some(candidate)
}

/// Convenience helper: samples an RGBA8 buffer and extracts its cover-art seed.
#[must_use]
pub fn extract_cover_seed_rgba8(rgba: &[u8], width: u32, height: u32) -> Option<u32> {
    let sampled = sample_argb_pixels_rgba8(rgba, width, height);
    extract_cover_seed(&sampled)
}

/// Resolves the active dark [`MaterialScheme`] for a preset, optional cover-art
/// seed, and [`CoverThemeMode`].
#[must_use]
pub fn resolve_scheme(
    preset: ThemePreset,
    cover_seed: Option<u32>,
    mode: CoverThemeMode,
) -> MaterialScheme {
    let base = preset.material_scheme_dark();
    let Some(seed) = cover_seed else {
        return base;
    };

    match mode {
        CoverThemeMode::Off => base,
        CoverThemeMode::Harmonize => harmonize_scheme(&base, seed),
        CoverThemeMode::SubtleTint => subtle_tint_scheme(&base, seed),
        CoverThemeMode::AutoCover => auto_cover_scheme(seed),
    }
}

/// Harmonizes a single accent color toward `seed`.
///
/// For chromatic accents uses `Blend::harmonize` (capped hue nudge preserving
/// tone & chroma). For achromatic accents (`chroma < 5.0`, e.g. `StudioDark`),
/// applies a light `Blend::cam16_ucs` tint toward `achromatic_target` (the
/// dark-mode-legible primary of the cover scheme) so `onPrimary` contrast stays
/// intact even for very dark cover seeds.
fn harmonize_accent(color: slint::Color, seed: u32, achromatic_target: u32) -> slint::Color {
    let argb = slint_to_argb(color);
    let blended = if Hct::from_int(argb).chroma() < ACHROMATIC_CHROMA_THRESHOLD {
        Blend::cam16_ucs(argb, achromatic_target, ACHROMATIC_HARMONIZE_STRENGTH)
    } else {
        Blend::harmonize(argb, seed)
    };
    argb_to_slint(blended)
}

/// Blends two [`slint::Color`] values in perceptually uniform CAM16-UCS space.
fn blend_cam16(from: slint::Color, to: slint::Color, amount: f64) -> slint::Color {
    argb_to_slint(Blend::cam16_ucs(
        slint_to_argb(from),
        slint_to_argb(to),
        amount,
    ))
}

/// Keeps the preset's neutral surfaces intact and harmonizes only accent roles.
fn harmonize_scheme(base: &MaterialScheme, seed: u32) -> MaterialScheme {
    let cover_primary = slint_to_argb(auto_cover_scheme(seed).primary);
    let primary = harmonize_accent(base.primary, seed, cover_primary);
    let secondary = harmonize_accent(base.secondary, seed, cover_primary);
    let primary_container = ThemePreset::avg_color(primary, base.background, 1, 3);
    let secondary_container = ThemePreset::avg_color(secondary, base.background, 1, 3);

    MaterialScheme {
        primary,
        surfaceTint: primary,
        primaryContainer: primary_container,
        secondary,
        secondaryContainer: secondary_container,
        inversePrimary: primary_container,
        primaryFixed: primary,
        primaryFixedDim: primary,
        onPrimaryFixedVariant: primary_container,
        secondaryFixed: secondary,
        secondaryFixedDim: secondary,
        onSecondaryFixedVariant: secondary_container,
        ..base.clone()
    }
}

/// Blends preset accents and surfaces gently toward the cover-art scheme in
/// CAM16-UCS space while preserving dual-accent separation and dark legibility.
fn subtle_tint_scheme(base: &MaterialScheme, seed: u32) -> MaterialScheme {
    let cover_scheme = auto_cover_scheme(seed);
    let tint = |from, to| blend_cam16(from, to, SUBTLE_TINT_SURFACE_STRENGTH);

    // Blend toward the dark-mode-legible M3 primary (tone ~80) rather than raw
    // `seed` so very dark cover seeds never pull `primary` down into `onPrimary`.
    let primary = blend_cam16(
        base.primary,
        cover_scheme.primary,
        SUBTLE_TINT_ACCENT_STRENGTH,
    );
    let secondary = blend_cam16(
        base.secondary,
        cover_scheme.primary,
        SUBTLE_TINT_ACCENT_STRENGTH,
    );

    let background = tint(base.background, cover_scheme.background);
    let surface = tint(base.surface, cover_scheme.surface);
    let surface_dim = tint(base.surfaceDim, cover_scheme.surfaceDim);
    let surface_bright = tint(base.surfaceBright, cover_scheme.surfaceBright);
    let surface_container_lowest = tint(
        base.surfaceContainerLowest,
        cover_scheme.surfaceContainerLowest,
    );
    let surface_container_low = tint(base.surfaceContainerLow, cover_scheme.surfaceContainerLow);
    let surface_container = tint(base.surfaceContainer, cover_scheme.surfaceContainer);
    let surface_container_high = tint(base.surfaceContainerHigh, cover_scheme.surfaceContainerHigh);
    let surface_container_highest = tint(
        base.surfaceContainerHighest,
        cover_scheme.surfaceContainerHighest,
    );
    let outline = tint(base.outline, cover_scheme.outline);
    let outline_variant = tint(base.outlineVariant, cover_scheme.outlineVariant);
    let surface_variant = tint(base.surfaceVariant, cover_scheme.surfaceVariant);

    let primary_container = ThemePreset::avg_color(primary, background, 1, 3);
    let secondary_container = ThemePreset::avg_color(secondary, background, 1, 3);

    MaterialScheme {
        primary,
        surfaceTint: primary,
        primaryContainer: primary_container,
        secondary,
        secondaryContainer: secondary_container,
        background,
        surface,
        surfaceVariant: surface_variant,
        outline,
        outlineVariant: outline_variant,
        inverseOnSurface: background,
        inversePrimary: primary_container,
        primaryFixed: primary,
        primaryFixedDim: primary,
        onPrimaryFixedVariant: primary_container,
        secondaryFixed: secondary,
        secondaryFixedDim: secondary,
        onSecondaryFixedVariant: secondary_container,
        surfaceDim: surface_dim,
        surfaceBright: surface_bright,
        surfaceContainerLowest: surface_container_lowest,
        surfaceContainerLow: surface_container_low,
        surfaceContainer: surface_container,
        surfaceContainerHigh: surface_container_high,
        surfaceContainerHighest: surface_container_highest,
        ..base.clone()
    }
}

/// Builds a complete dark Material 3 [`MaterialScheme`] from `seed` using
/// `SchemeContent` and `MaterialDynamicColors`.
#[must_use]
pub fn auto_cover_scheme(seed: u32) -> MaterialScheme {
    let scheme = SchemeContent::new(Hct::from_int(seed), true, 0.0);
    let c = |dc: DynamicColor| argb_to_slint(dc.get_argb(&scheme));

    MaterialScheme {
        primary: c(MaterialDynamicColors::primary()),
        surfaceTint: c(MaterialDynamicColors::surface_tint()),
        onPrimary: c(MaterialDynamicColors::on_primary()),
        primaryContainer: c(MaterialDynamicColors::primary_container()),
        onPrimaryContainer: c(MaterialDynamicColors::on_primary_container()),
        secondary: c(MaterialDynamicColors::secondary()),
        onSecondary: c(MaterialDynamicColors::on_secondary()),
        secondaryContainer: c(MaterialDynamicColors::secondary_container()),
        onSecondaryContainer: c(MaterialDynamicColors::on_secondary_container()),
        tertiary: c(MaterialDynamicColors::tertiary()),
        onTertiary: c(MaterialDynamicColors::on_tertiary()),
        tertiaryContainer: c(MaterialDynamicColors::tertiary_container()),
        onTertiaryContainer: c(MaterialDynamicColors::on_tertiary_container()),
        error: c(MaterialDynamicColors::error()),
        onError: c(MaterialDynamicColors::on_error()),
        errorContainer: c(MaterialDynamicColors::error_container()),
        onErrorContainer: c(MaterialDynamicColors::on_error_container()),
        background: c(MaterialDynamicColors::background()),
        onBackground: c(MaterialDynamicColors::on_background()),
        surface: c(MaterialDynamicColors::surface()),
        onSurface: c(MaterialDynamicColors::on_surface()),
        surfaceVariant: c(MaterialDynamicColors::surface_variant()),
        onSurfaceVariant: c(MaterialDynamicColors::on_surface_variant()),
        outline: c(MaterialDynamicColors::outline()),
        outlineVariant: c(MaterialDynamicColors::outline_variant()),
        shadow: c(MaterialDynamicColors::shadow()),
        scrim: c(MaterialDynamicColors::scrim()),
        inverseSurface: c(MaterialDynamicColors::inverse_surface()),
        inverseOnSurface: c(MaterialDynamicColors::inverse_on_surface()),
        inversePrimary: c(MaterialDynamicColors::inverse_primary()),
        primaryFixed: c(MaterialDynamicColors::primary_fixed()),
        onPrimaryFixed: c(MaterialDynamicColors::on_primary_fixed()),
        primaryFixedDim: c(MaterialDynamicColors::primary_fixed_dim()),
        onPrimaryFixedVariant: c(MaterialDynamicColors::on_primary_fixed_variant()),
        secondaryFixed: c(MaterialDynamicColors::secondary_fixed()),
        onSecondaryFixed: c(MaterialDynamicColors::on_secondary_fixed()),
        secondaryFixedDim: c(MaterialDynamicColors::secondary_fixed_dim()),
        onSecondaryFixedVariant: c(MaterialDynamicColors::on_secondary_fixed_variant()),
        tertiaryFixed: c(MaterialDynamicColors::tertiary_fixed()),
        onTertiaryFixed: c(MaterialDynamicColors::on_tertiary_fixed()),
        tertiaryFixedDim: c(MaterialDynamicColors::tertiary_fixed_dim()),
        onTertiaryFixedVariant: c(MaterialDynamicColors::on_tertiary_fixed_variant()),
        surfaceDim: c(MaterialDynamicColors::surface_dim()),
        surfaceBright: c(MaterialDynamicColors::surface_bright()),
        surfaceContainerLowest: c(MaterialDynamicColors::surface_container_lowest()),
        surfaceContainerLow: c(MaterialDynamicColors::surface_container_low()),
        surfaceContainer: c(MaterialDynamicColors::surface_container()),
        surfaceContainerHigh: c(MaterialDynamicColors::surface_container_high()),
        surfaceContainerHighest: c(MaterialDynamicColors::surface_container_highest()),
    }
}
