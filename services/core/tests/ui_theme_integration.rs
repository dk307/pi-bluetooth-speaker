//! Integration tests for the UI Theme Presets and Palettes.

use speaker_core::ui::ThemePreset;

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
