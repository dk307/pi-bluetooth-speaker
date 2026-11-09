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
    let presets = [
        ThemePreset::McIntoshBlue,
        ThemePreset::MarantzAmber,
        ThemePreset::CyberpunkNeon,
        ThemePreset::StudioDark,
        ThemePreset::Peppy,
    ];
    for preset in presets {
        assert_ne!(preset.display_name(), "");
        assert_ne!(preset.slug(), "");
        assert_ne!(preset.meter_skin_id(), "");
        assert_eq!(preset.font_family(), "Inter");
        assert_eq!(preset.to_string(), preset.display_name());

        // Each preset's Material scheme carries its own primary (no shared default).
        let schemes = preset.material_schemes();
        assert_eq!(schemes.dark, schemes.light);
    }
    // Meter skins are distinct per preset (VU skin selection must not collide).
    let skins: std::collections::HashSet<_> = presets.iter().map(|p| p.meter_skin_id()).collect();
    assert_eq!(
        skins.len(),
        presets.len(),
        "meter skin ids must be distinct"
    );
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

const ALL_PRESETS: [ThemePreset; 5] = [
    ThemePreset::McIntoshBlue,
    ThemePreset::MarantzAmber,
    ThemePreset::CyberpunkNeon,
    ThemePreset::StudioDark,
    ThemePreset::Peppy,
];

#[test]
fn preset_swatches_match_material_scheme() {
    // Pins settings_view.slint literal swatches to the Rust scheme source,
    // via the public material_schemes() API.
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
        let dark = preset.material_schemes().dark;
        assert_eq!(dark.primary, slint::Color::from_rgb_u8(pr, pg, pb));
        assert_eq!(dark.secondary, slint::Color::from_rgb_u8(sr, sg, sb));
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
fn material_schemes_core_tokens_map_legacy_palette() {
    // Legacy -> Material derivation contract (see THEME_MIGRATION_PLAN.md P1),
    // asserted through the public scheme output.
    let expected_base = [
        // (primary, chassis, card_surface, card_border, text_primary, text_muted, needle_peak, on_primary)
        (
            (0x00, 0xC8, 0xFF),
            (0x09, 0x0D, 0x12),
            (0x11, 0x19, 0x23),
            (0x30, 0x36, 0x3D),
            (0xF0, 0xF6, 0xFC),
            (0x8B, 0x94, 0x9E),
            (0xFF, 0x33, 0x44),
            (0x09, 0x0D, 0x12),
        ),
        (
            (0xE5, 0xA9, 0x3C),
            (0x12, 0x10, 0x0D),
            (0x1C, 0x17, 0x12),
            (0x3A, 0x2E, 0x22),
            (0xFD, 0xFB, 0xF7),
            (0x9C, 0x92, 0x83),
            (0xD9, 0x38, 0x29),
            (0x12, 0x10, 0x0D),
        ),
        (
            (0x00, 0xF0, 0xFF),
            (0x0D, 0x0B, 0x18),
            (0x1B, 0x14, 0x2E),
            (0x3C, 0x23, 0x5A),
            (0xF5, 0xEE, 0xFD),
            (0xA3, 0x90, 0xC5),
            (0xFF, 0x14, 0x93),
            (0x0D, 0x0B, 0x18),
        ),
        (
            (0xF0, 0xF0, 0xF0),
            (0x12, 0x12, 0x12),
            (0x1E, 0x1E, 0x1E),
            (0x2D, 0x2D, 0x2D),
            (0xFF, 0xFF, 0xFF),
            (0x88, 0x88, 0x88),
            (0x00, 0xE6, 0x76),
            (0x00, 0x00, 0x00),
        ),
        (
            (0xF5, 0xB8, 0x3D),
            (0x14, 0x11, 0x0D),
            (0x20, 0x1A, 0x14),
            (0x48, 0x38, 0x24),
            (0xFF, 0xFD, 0xF5),
            (0xA6, 0x9B, 0x8D),
            (0xE6, 0x39, 0x46),
            (0x14, 0x11, 0x0D),
        ),
    ];
    for (preset, (p, ch, s, b, tp, tm, np, op)) in ALL_PRESETS.iter().zip(expected_base) {
        let dark = preset.material_schemes().dark;
        let c = |(r, g, bb)| slint::Color::from_rgb_u8(r, g, bb);
        assert_eq!(dark.primary, c(p), "{preset:?} primary");
        assert_eq!(dark.background, c(ch), "{preset:?} background");
        assert_eq!(dark.surface, c(s), "{preset:?} surface");
        assert_eq!(dark.outline, c(b), "{preset:?} outline");
        assert_eq!(dark.outlineVariant, c(b), "{preset:?} outlineVariant");
        assert_eq!(dark.onBackground, c(tp), "{preset:?} onBackground");
        assert_eq!(dark.onSurface, c(tp), "{preset:?} onSurface");
        assert_eq!(dark.onSurfaceVariant, c(tm), "{preset:?} onSurfaceVariant");
        assert_eq!(dark.error, c(np), "{preset:?} error");
        assert_eq!(dark.onPrimary, c(op), "{preset:?} onPrimary");
        assert_eq!(dark.onSecondary, c(op), "{preset:?} onSecondary");
    }
}

#[test]
fn settings_view_swatches_match_preset_constants() {
    // Guards the .slint side: ThemePresetCard literals in settings_view.slint
    // must carry the same hexes as ThemePreset::PRESET_SWATCHES. A Rust-only
    // comparison cannot see drift introduced by editing the view.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/settings_view.slint");
    let src = std::fs::read_to_string(&path).expect("settings_view.slint must exist");
    for ((pr, pg, pb), (sr, sg, sb)) in ThemePreset::PRESET_SWATCHES {
        let primary_hex = format!("#{:02X}{:02X}{:02X}", pr, pg, pb);
        let secondary_hex = format!("#{:02X}{:02X}{:02X}", sr, sg, sb);
        assert!(
            src.contains(&primary_hex),
            "settings_view.slint missing preset primary {primary_hex}"
        );
        assert!(
            src.contains(&secondary_hex),
            "settings_view.slint missing preset secondary {secondary_hex}"
        );
    }
}
