#![cfg(feature = "ui")]
//! Integration tests for Material 3 dynamic cover-art auto-theming.

use mcu_material_color::prelude::{Contrast, Hct};
use screen_manager::config::CoreConfig;
use screen_manager::settings_catalog::{SettingId, apply_step, find_setting, row_state};
use screen_manager::ui::{
    AppShell, AppTheme, BoundedSeedCache, CoverThemeMode, Framebuffer, MainWindow, MaterialPalette,
    ThemePreset, argb_to_slint, auto_cover_scheme, extract_cover_seed, extract_cover_seed_rgba8,
    resolve_scheme, sample_argb_pixels_rgba8, slint_to_argb,
};
use slint::ComponentHandle;

/// Builds a solid RGBA8 image buffer filled with `(r, g, b, a)`.
fn solid_rgba(width: u32, height: u32, r: u8, g: u8, b: u8, a: u8) -> Vec<u8> {
    let px = [r, g, b, a];
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..(width * height) {
        out.extend_from_slice(&px);
    }
    out
}

/// Computes the WCAG 2.1 contrast ratio between two Slint colors via MCU's HCT tone delta.
fn contrast_ratio(a: slint::Color, b: slint::Color) -> f64 {
    let tone_a = Hct::from_int(slint_to_argb(a)).tone();
    let tone_b = Hct::from_int(slint_to_argb(b)).tone();
    Contrast::ratio_of_tones(tone_a, tone_b)
}

#[test]
fn cover_theme_mode_indexing_stepping_and_slugs() {
    assert_eq!(CoverThemeMode::default(), CoverThemeMode::SubtleTint);
    assert_eq!(CoverThemeMode::from_index(0), CoverThemeMode::Off);
    assert_eq!(CoverThemeMode::from_index(1), CoverThemeMode::Harmonize);
    assert_eq!(CoverThemeMode::from_index(2), CoverThemeMode::SubtleTint);
    assert_eq!(CoverThemeMode::from_index(3), CoverThemeMode::AutoCover);
    assert_eq!(CoverThemeMode::from_index(4), CoverThemeMode::Off);
    assert_eq!(CoverThemeMode::from_index(-1), CoverThemeMode::AutoCover);

    assert_eq!(CoverThemeMode::Off.step(1), CoverThemeMode::Harmonize);
    assert_eq!(CoverThemeMode::AutoCover.step(1), CoverThemeMode::Off);
    assert_eq!(CoverThemeMode::Off.step(-1), CoverThemeMode::AutoCover);

    let cases = [
        ("off", CoverThemeMode::Off),
        ("predefined", CoverThemeMode::Off),
        ("0", CoverThemeMode::Off),
        ("harmonize", CoverThemeMode::Harmonize),
        ("harmonized", CoverThemeMode::Harmonize),
        ("1", CoverThemeMode::Harmonize),
        ("subtle-tint", CoverThemeMode::SubtleTint),
        ("subtle_tint", CoverThemeMode::SubtleTint),
        ("tint", CoverThemeMode::SubtleTint),
        ("2", CoverThemeMode::SubtleTint),
        ("auto-cover", CoverThemeMode::AutoCover),
        ("auto", CoverThemeMode::AutoCover),
        ("3", CoverThemeMode::AutoCover),
    ];
    for (input, expected) in cases {
        assert_eq!(CoverThemeMode::parse_slug(input).unwrap(), expected);
    }
    CoverThemeMode::parse_slug("bogus-mode").unwrap_err();
    CoverThemeMode::parse_slug("").unwrap_err();

    // Display and serde JSON round-trip.
    for idx in 0..4 {
        let mode = CoverThemeMode::from_index(idx);
        assert_eq!(format!("{mode}"), mode.display_name());
        let json = serde_json::to_string(&mode).unwrap();
        let parsed: CoverThemeMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mode);
    }
}

#[test]
fn adaptive_stride_sampling_bounds_pixel_count_and_filters_alpha() {
    // Zero dimensions or truncated slice yield empty output.
    assert_eq!(sample_argb_pixels_rgba8(&[], 0, 100), Vec::<u32>::new());
    assert_eq!(sample_argb_pixels_rgba8(&[], 100, 0), Vec::<u32>::new());
    assert_eq!(
        sample_argb_pixels_rgba8(&[255; 16], 10, 10),
        Vec::<u32>::new()
    );

    // Transparent pixels (a < 200) are filtered out.
    let transparent = solid_rgba(64, 64, 220, 40, 40, 100);
    assert_eq!(
        sample_argb_pixels_rgba8(&transparent, 64, 64),
        Vec::<u32>::new()
    );

    // 200x200 and 360x360 both sample into a bounded compact buffer (~4k-5k px).
    let img200 = solid_rgba(200, 200, 230, 80, 30, 255);
    let s200 = sample_argb_pixels_rgba8(&img200, 200, 200);
    assert!(
        (3_000..=6_000).contains(&s200.len()),
        "200x200 sampled {} pixels",
        s200.len()
    );
    assert_eq!(s200[0], 0xFFE6_501E);

    let img360 = solid_rgba(360, 360, 20, 180, 90, 255);
    let s360 = sample_argb_pixels_rgba8(&img360, 360, 360);
    assert!(
        (3_000..=6_500).contains(&s360.len()),
        "360x360 sampled {} pixels",
        s360.len()
    );
    assert_eq!(s360[0], 0xFF14_B45A);
}

#[test]
fn extract_cover_seed_identifies_vibrant_art_and_rejects_monochrome() {
    assert_eq!(extract_cover_seed(&[]), None);

    // Vibrant orange-red album cover yields a chromatic seed.
    let vibrant = solid_rgba(128, 128, 232, 122, 36, 255);
    let seed = extract_cover_seed_rgba8(&vibrant, 128, 128).expect("vibrant art yields seed");
    assert!(
        Hct::from_int(seed).chroma() >= 15.0,
        "extracted seed chroma should meet minimum threshold"
    );

    // Pure grayscale / black-and-white cover art returns None.
    let gray = solid_rgba(128, 128, 128, 128, 128, 255);
    assert_eq!(extract_cover_seed_rgba8(&gray, 128, 128), None);

    let black = solid_rgba(64, 64, 12, 12, 12, 255);
    assert_eq!(extract_cover_seed_rgba8(&black, 64, 64), None);
}

#[test]
fn bounded_seed_cache_evicts_fifo_and_updates_in_place() {
    let mut cache = BoundedSeedCache::new(0); // Clamped to capacity 1.
    assert!(cache.is_empty());
    cache.insert("/a.jpg".into(), Some(0xFF11_2233));
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.get("/a.jpg"), Some(Some(0xFF11_2233)));

    // Updating existing key does not grow cache.
    cache.insert("/a.jpg".into(), None);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.get("/a.jpg"), Some(None));

    // Inserting a second key evicts `/a.jpg`.
    cache.insert("/b.jpg".into(), Some(0xFF44_5566));
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.get("/a.jpg"), None);
    assert_eq!(cache.get("/b.jpg"), Some(Some(0xFF44_5566)));
}

#[test]
fn resolve_scheme_preserves_presets_and_wcag_contrast_across_all_modes() {
    let presets = [
        ThemePreset::McIntoshBlue,
        ThemePreset::MarantzAmber,
        ThemePreset::CyberpunkNeon,
        ThemePreset::StudioDark,
        ThemePreset::Peppy,
    ];
    let cover_seed = 0xFFE8_7A24; // Warm cover orange

    for preset in presets {
        let base = preset.material_schemes().dark;

        // Off mode and None seed always return the exact hand-crafted preset.
        assert_eq!(
            resolve_scheme(preset, None, CoverThemeMode::SubtleTint),
            base
        );
        assert_eq!(
            resolve_scheme(preset, None, CoverThemeMode::AutoCover),
            base
        );
        assert_eq!(
            resolve_scheme(preset, Some(cover_seed), CoverThemeMode::Off),
            base
        );

        // Harmonize keeps neutral surfaces identical while nudging accents.
        let harmonized = resolve_scheme(preset, Some(cover_seed), CoverThemeMode::Harmonize);
        assert_eq!(harmonized.background, base.background);
        assert_eq!(harmonized.surface, base.surface);
        assert_eq!(harmonized.onSurface, base.onSurface);
        assert_eq!(harmonized.outline, base.outline);
        assert_ne!(
            harmonized.primary, base.primary,
            "Harmonize should nudge primary even for achromatic preset {preset:?}"
        );

        // SubtleTint shifts primary and background gently while keeping dark text legible.
        let tinted = resolve_scheme(preset, Some(cover_seed), CoverThemeMode::SubtleTint);
        assert_ne!(tinted.primary, base.primary);
        assert!(
            contrast_ratio(tinted.onBackground, tinted.background) >= 4.5,
            "SubtleTint onBackground/background contrast too low for {preset:?}"
        );
        assert!(
            contrast_ratio(tinted.onSurface, tinted.surface) >= 4.5,
            "SubtleTint onSurface/surface contrast too low for {preset:?}"
        );
        assert!(
            contrast_ratio(tinted.onPrimary, tinted.primary) >= 4.5,
            "SubtleTint onPrimary/primary contrast too low for {preset:?}"
        );
    }

    // Extreme dark (deep navy) and bright (pastel yellow) cover seeds must still
    // keep `onPrimary / primary` WCAG AA legible (>= 4.5) in SubtleTint & Harmonize.
    for extreme_seed in [0xFF0A_143C, 0xFFFF_F59D] {
        for preset in presets {
            for mode in [CoverThemeMode::SubtleTint, CoverThemeMode::Harmonize] {
                let s = resolve_scheme(preset, Some(extreme_seed), mode);
                assert!(
                    contrast_ratio(s.onPrimary, s.primary) >= 4.5,
                    "onPrimary/primary contrast < 4.5 for {preset:?} {mode:?} seed {extreme_seed:#010X}"
                );
            }
        }
    }

    // CyberpunkNeon preserves dual-accent separation (primary != secondary) in SubtleTint.
    let cyber_tinted = resolve_scheme(
        ThemePreset::CyberpunkNeon,
        Some(cover_seed),
        CoverThemeMode::SubtleTint,
    );
    assert_ne!(cyber_tinted.primary, cyber_tinted.secondary);

    // AutoCover produces a full dark M3 scheme with WCAG AA contrast >= 4.5:1.
    for seed in [0xFFE8_7A24, 0xFF1E_88E5, 0xFF2E_7D32, 0xFFD8_1B60] {
        let auto = auto_cover_scheme(seed);
        assert_eq!(
            resolve_scheme(
                ThemePreset::McIntoshBlue,
                Some(seed),
                CoverThemeMode::AutoCover
            ),
            auto
        );
        assert!(
            contrast_ratio(auto.onBackground, auto.background) >= 4.5,
            "AutoCover onBackground/background contrast < 4.5 for seed {seed:#010X}"
        );
        assert!(
            contrast_ratio(auto.onSurface, auto.surface) >= 4.5,
            "AutoCover onSurface/surface contrast < 4.5 for seed {seed:#010X}"
        );
        assert!(
            contrast_ratio(auto.onPrimary, auto.primary) >= 4.5,
            "AutoCover onPrimary/primary contrast < 4.5 for seed {seed:#010X}"
        );
    }
}

#[test]
fn settings_catalog_and_core_config_round_trip_cover_theme_mode() {
    let mut cfg = CoreConfig::default();
    assert_eq!(cfg.cover_theme_mode, CoverThemeMode::SubtleTint);

    let desc = find_setting(SettingId::CoverThemeMode);
    assert_eq!(row_state(&cfg, desc).value_text, "Subtle Tint");

    assert!(apply_step(&mut cfg, SettingId::CoverThemeMode, 1));
    assert_eq!(cfg.cover_theme_mode, CoverThemeMode::AutoCover);
    assert_eq!(row_state(&cfg, desc).value_text, "Auto Cover");

    assert!(apply_step(&mut cfg, SettingId::CoverThemeMode, 1));
    assert_eq!(cfg.cover_theme_mode, CoverThemeMode::Off);
    assert_eq!(row_state(&cfg, desc).value_text, "Off");

    assert!(apply_step(&mut cfg, SettingId::CoverThemeMode, 1));
    assert_eq!(cfg.cover_theme_mode, CoverThemeMode::Harmonize);
    assert_eq!(row_state(&cfg, desc).value_text, "Harmonize");

    // Backward compatibility: missing field in legacy JSON defaults to SubtleTint.
    let legacy: CoreConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(legacy.cover_theme_mode, CoverThemeMode::SubtleTint);

    // Explicit JSON serialization round-trip.
    let json = serde_json::to_string(&cfg).unwrap();
    assert!(json.contains("\"cover_theme_mode\":\"harmonize\""));
    let parsed: CoreConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.cover_theme_mode, CoverThemeMode::Harmonize);
}

#[test]
fn app_shell_applies_cover_seed_preserves_preset_and_reverts_on_clear() {
    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("Failed to initialize platform");
    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);
    shell.apply_theme(ThemePreset::McIntoshBlue);
    let base_primary = shell.window().global::<MaterialPalette>().get_primary();

    // Setting a vibrant cover seed under default SubtleTint updates MaterialPalette
    // without altering AppTheme.active_preset.
    let seed = 0xFFE8_7A24;
    shell.set_cover_seed(Some(seed));
    assert_eq!(shell.active_cover_seed(), Some(seed));
    assert_eq!(shell.current_theme(), ThemePreset::McIntoshBlue);
    assert_eq!(
        shell.window().global::<AppTheme>().get_active_preset(),
        ThemePreset::McIntoshBlue.index()
    );
    let tinted_primary = shell.window().global::<MaterialPalette>().get_primary();
    assert_ne!(tinted_primary, base_primary);

    // Switching mode to Off restores exact base preset primary while keeping cached seed.
    shell.set_cover_theme_mode(CoverThemeMode::Off);
    assert_eq!(
        shell.window().global::<MaterialPalette>().get_primary(),
        base_primary
    );
    assert_eq!(shell.active_cover_seed(), Some(seed));

    // Switching mode to AutoCover via UI setting_step callback uses cached seed immediately.
    shell
        .window()
        .invoke_setting_step("cover_theme_mode".into(), -1);
    assert_eq!(shell.cover_theme_mode(), CoverThemeMode::AutoCover);
    let expected_auto = auto_cover_scheme(seed);
    assert_eq!(
        shell.window().global::<MaterialPalette>().get_primary(),
        expected_auto.primary
    );
    assert_eq!(
        shell.window().global::<MaterialPalette>().get_background(),
        expected_auto.background
    );

    // Clearing cover art clears active_cover_seed and reverts to base preset.
    shell.set_has_cover_art(false);
    assert_eq!(shell.active_cover_seed(), None);
    assert_eq!(
        shell.window().global::<MaterialPalette>().get_primary(),
        base_primary
    );

    // Helper round-trip for argb_to_slint / slint_to_argb.
    assert_eq!(slint_to_argb(argb_to_slint(0xFFAB_CDEF)), 0xFFAB_CDEF);
}
