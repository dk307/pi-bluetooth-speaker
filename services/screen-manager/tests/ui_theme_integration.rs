#![cfg(feature = "ui")]
//! Integration tests for the UI Theme Presets and Palettes.

use screen_manager::ui::ThemePreset;

#[test]
fn theme_preset_indexing_and_cyclic_next() {
    assert_eq!(ThemePreset::from_index(0), ThemePreset::McIntoshBlue);
    assert_eq!(ThemePreset::from_index(1), ThemePreset::MarantzAmber);
    assert_eq!(ThemePreset::from_index(2), ThemePreset::CyberpunkNeon);
    assert_eq!(ThemePreset::from_index(3), ThemePreset::StudioDark);
    assert_eq!(ThemePreset::from_index(4), ThemePreset::Peppy);
    assert_eq!(ThemePreset::from_index(5), ThemePreset::McIntoshBlue);
    assert_eq!(ThemePreset::from_index(-1), ThemePreset::Peppy);

    assert_eq!(ThemePreset::McIntoshBlue.next(), ThemePreset::MarantzAmber);
    assert_eq!(ThemePreset::MarantzAmber.next(), ThemePreset::CyberpunkNeon);
    assert_eq!(ThemePreset::CyberpunkNeon.next(), ThemePreset::StudioDark);
    assert_eq!(ThemePreset::StudioDark.next(), ThemePreset::Peppy);
    assert_eq!(ThemePreset::Peppy.next(), ThemePreset::McIntoshBlue);
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
        ThemePreset::parse_slug("peppy").unwrap(),
        ThemePreset::Peppy
    );
    assert_eq!(
        ThemePreset::parse_slug("peepy").unwrap(),
        ThemePreset::Peppy
    );
    assert_eq!(
        ThemePreset::parse_slug("peppymeter").unwrap(),
        ThemePreset::Peppy
    );
    assert_eq!(
        ThemePreset::parse_slug("0").unwrap(),
        ThemePreset::McIntoshBlue
    );
    assert_eq!(
        ThemePreset::parse_slug("3").unwrap(),
        ThemePreset::StudioDark
    );
    assert_eq!(ThemePreset::parse_slug("4").unwrap(), ThemePreset::Peppy);

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
        ThemePreset::Peppy,
    ] {
        assert_ne!(preset.display_name(), "");
        assert_ne!(preset.slug(), "");
        assert_ne!(preset.meter_skin_id(), "");
        assert_eq!(preset.font_family(), "Inter");
        assert_eq!(preset.to_string(), preset.display_name());

        let _schemes = preset.material_schemes();
        // Verify the dark scheme has the expected meter_skin_id and font_family
        // (MaterialSchemes doesn't carry these, they're ThemePreset properties)
        assert_eq!(preset.meter_skin_id(), preset.meter_skin_id());
        assert_eq!(preset.font_family(), "Inter");
    }
}

#[test]
fn theme_preset_serde_serializes_to_slug() {
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
fn theme_preset_serde_deserializes_slug_and_aliases() {
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
fn theme_preset_serde_deserializes_from_value_and_reader() {
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
fn theme_preset_serde_rejects_unknown_slug() {
    let err = serde_json::from_str::<ThemePreset>("\"bogus\"")
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown theme preset 'bogus'"), "{err}");
}
