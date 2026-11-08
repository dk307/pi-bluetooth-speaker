//! Integration tests for the appliance configuration store (`CoreConfig`).
#![cfg(feature = "ui")]

use screen_manager::config::{
    ConfigError, CoreConfig, DEFAULT_MARQUEE_FPS, MarqueeFps, VolumeHudFadeMs, VolumeHudTimeoutMs,
    VolumeStepPercent,
};
use screen_manager::ui::ThemePreset;
use speaker_common::diag::ScopedTempDir;
use std::error::Error as _;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[test]
fn defaults_are_sensible() {
    let config = CoreConfig::default();
    assert_eq!(config.theme, None);
    assert_eq!(config.marquee_fps, MarqueeFps::FPS_60);
    assert_eq!(config.marquee_fps.value(), 60);
    assert_eq!(DEFAULT_MARQUEE_FPS, 60);
}

#[test]
fn load_missing_file_returns_defaults() {
    let dir = ScopedTempDir::new("cfg_missing");
    let path = dir.path().join("does-not-exist.json");
    let config = CoreConfig::load(&path).expect("missing file must yield defaults");
    assert_eq!(config, CoreConfig::default());
}

#[test]
fn load_missing_file_logs_warning_field_with_subscriber() {
    let dir = ScopedTempDir::new("cfg_missing_logged");
    let path = dir.path().join("absent.json");
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let config = CoreConfig::load(&path).expect("missing file must yield defaults");
        assert_eq!(config, CoreConfig::default());
    });
}

#[test]
fn save_and_load_roundtrip() {
    let dir = ScopedTempDir::new("cfg_roundtrip");
    let path = dir.path().join("config.json");
    let config = CoreConfig {
        theme: Some(ThemePreset::Peppy),
        marquee_fps: MarqueeFps::FPS_30,
        ..CoreConfig::default()
    };
    config.save(&path).expect("save");
    let loaded = CoreConfig::load(&path).expect("load");
    assert_eq!(loaded, config);
    // Raw on-disk form is the slug-based JSON contract.
    let raw = std::fs::read_to_string(&path).expect("raw read");
    assert!(raw.contains("\"theme\": \"peppy\""), "{raw}");
    assert!(raw.contains("\"marquee_fps\": 30"), "{raw}");
}

#[test]
fn minimal_config_deserializes_with_defaults() {
    let config: CoreConfig = serde_json::from_str("{}").expect("empty object");
    assert_eq!(config, CoreConfig::default());
    let config: CoreConfig =
        serde_json::from_str(r#"{"theme": "studio-dark"}"#).expect("theme only");
    assert_eq!(config.theme, Some(ThemePreset::StudioDark));
    assert_eq!(config.marquee_fps, MarqueeFps::FPS_60);
}

#[test]
fn load_malformed_json_is_hard_parse_error() {
    let dir = ScopedTempDir::new("cfg_malformed");
    let path = dir.path().join("config.json");
    std::fs::write(&path, "{not json").expect("write");
    let err = CoreConfig::load(&path).expect_err("malformed JSON must fail");
    assert!(err.is_config());
    // config-rs reports malformed files as FileParse failures during build().
    assert!(err.to_string().contains("failed to read config"));
    // Full Display appends the backtrace only when captured (RUST_BACKTRACE=1);
    // `summary()` never contains one regardless of the capture status.
    assert!(err.summary().contains("config error"));
    assert!(!err.summary().contains("Backtrace"));
    if err.backtrace().status() == std::backtrace::BacktraceStatus::Captured {
        assert!(err.to_string().contains("Backtrace:\n"));
    } else {
        assert!(!err.to_string().contains("Backtrace:\n"));
    }
    assert!(err.source().is_some());
}

#[test]
fn load_rejects_unknown_theme_slug() {
    let dir = ScopedTempDir::new("cfg_bad_theme");
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"theme": "bogus"}"#).expect("write");
    let err = CoreConfig::load(&path).expect_err("unknown slug must fail");
    assert!(err.is_config());
    assert!(err.to_string().contains("unknown theme preset 'bogus'"));
}

#[test]
fn load_rejects_invalid_marquee_fps() {
    for bad in [0u32, 15, 45, 120, u32::MAX] {
        let dir = ScopedTempDir::new("cfg_bad_fps");
        let path = dir.path().join("config.json");
        std::fs::write(&path, format!(r#"{{"marquee_fps": {bad}}}"#)).expect("write");
        let err = CoreConfig::load(&path).expect_err("bad fps must fail");
        // The invalid rate cannot inhabit `MarqueeFps`, so the layered
        // reader reports it as a deserialize failure.
        assert!(err.is_config(), "{err}");
        let rendered = err.to_string();
        assert!(
            rendered.contains(&format!("invalid marquee fps '{bad}': expected 30 or 60")),
            "{rendered}"
        );
    }
}

#[test]
fn load_accepts_allowed_marquee_fps_values() {
    for (good, expected) in [(30u32, MarqueeFps::FPS_30), (60, MarqueeFps::FPS_60)] {
        let dir = ScopedTempDir::new("cfg_good_fps");
        let path = dir.path().join("config.json");
        std::fs::write(&path, format!(r#"{{"marquee_fps": {good}}}"#)).expect("write");
        let config = CoreConfig::load(&path).expect("valid fps must load");
        assert_eq!(config.marquee_fps, expected);
    }
}

#[test]
fn marquee_fps_tryfrom_enforces_allowed_values() {
    for good in [30u32, 60] {
        assert_eq!(
            MarqueeFps::try_from(good).expect("allowed rate").value(),
            good
        );
    }
    for bad in [0u32, 15, 45, 120, u32::MAX] {
        let err = MarqueeFps::try_from(bad).expect_err("out-of-domain rate must be rejected");
        assert!(err.is_invalid_value());
        assert_eq!(
            err.summary(),
            format!("invalid marquee fps '{bad}': expected 30 or 60")
        );
    }
}

#[test]
fn marquee_fps_serde_rejects_invalid_rate() {
    let err = serde_json::from_str::<MarqueeFps>("45").expect_err("invalid rate");
    assert!(
        err.to_string().contains("invalid marquee fps '45'"),
        "{err}"
    );
    let fps: MarqueeFps = serde_json::from_str("30").expect("allowed rate");
    assert_eq!(fps, MarqueeFps::FPS_30);
    // Serialization is transparent u32.
    assert_eq!(
        serde_json::to_string(&MarqueeFps::FPS_60).expect("serialize"),
        "60"
    );
    // Invalid rates are unrepresentable in CoreConfig, so `save()` can
    // never persist them.
    assert_eq!(u32::from(MarqueeFps::FPS_30), 30);
    assert_eq!(MarqueeFps::default(), MarqueeFps::FPS_60);
}

#[test]
fn load_io_failure_on_directory_path() {
    let dir = ScopedTempDir::new("cfg_dir_path");
    let err = CoreConfig::load(dir.path()).expect_err("directory read must fail");
    // The layered reader surfaces the EISDIR access failure.
    assert!(err.is_config());
    assert!(err.to_string().contains("failed to read config"));
    assert!(err.summary().contains("config error"));
    assert!(!err.summary().contains("Backtrace"));
    assert!(err.source().is_some());
}

#[test]
fn save_io_failure_on_directory_target() {
    let dir = ScopedTempDir::new("cfg_save_dir");
    let config = CoreConfig::default();
    // Renaming the temp file over an existing directory must fail.
    let target = dir.path().join("blocked");
    std::fs::create_dir(&target).expect("create blocking directory");
    let err = config
        .save(&target)
        .expect_err("rename over directory must fail");
    assert!(err.is_io());
    assert!(err.to_string().contains("failed to install config"));
    // The temp file lives as a sibling of the rename target; a failed
    // persist consumes it, so no `.tmp` litter may survive the save.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("list dir")
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp file must be removed on failed save"
    );
}

#[test]
fn save_replaces_existing_config_atomically() {
    let dir = ScopedTempDir::new("cfg_save_replace");
    let path = dir.path().join("config.json");
    CoreConfig {
        theme: None,
        marquee_fps: MarqueeFps::FPS_60,
        ..CoreConfig::default()
    }
    .save(&path)
    .expect("initial save");
    CoreConfig {
        theme: Some(ThemePreset::StudioDark),
        marquee_fps: MarqueeFps::FPS_30,
        ..CoreConfig::default()
    }
    .save(&path)
    .expect("replacement save");
    let loaded = CoreConfig::load(&path).expect("reload replaced config");
    assert_eq!(loaded.theme, Some(ThemePreset::StudioDark));
    assert_eq!(loaded.marquee_fps, MarqueeFps::FPS_30);
    // Only the final artifact remains; no temp files left behind.
    let leftovers: Vec<_> = dir
        .path()
        .read_dir()
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, vec!["config.json".to_string()]);
}

#[test]
fn load_ignores_path_extension_for_json_format() {
    // The on-disk contract is always JSON regardless of the operator's
    // filename choice: format detection is pinned, not guessed.
    for name in ["config.txt", "conf", "settings.json"] {
        let dir = ScopedTempDir::new("cfg_extless");
        let path = dir.path().join(name);
        std::fs::write(&path, r#"{"marquee_fps": 30}"#).expect("write");
        let config = CoreConfig::load(&path).expect("valid fps must load");
        assert_eq!(config.marquee_fps, MarqueeFps::FPS_30, "{name}");
    }
}

#[test]
fn resolve_path_uses_override_and_default() {
    assert_eq!(
        CoreConfig::resolve_path_with(None, |_| false),
        PathBuf::from(speaker_common::diag::DEFAULT_CONFIG_PATH)
    );
    assert_eq!(
        CoreConfig::resolve_path_with(None, |p| p
            == Path::new(speaker_common::diag::DEFAULT_CONFIG_PATH)),
        PathBuf::from(speaker_common::diag::DEFAULT_CONFIG_PATH)
    );
    assert_eq!(
        CoreConfig::resolve_path_with(None, |p| p
            == Path::new(speaker_common::diag::FALLBACK_CONFIG_PATH)),
        PathBuf::from(speaker_common::diag::FALLBACK_CONFIG_PATH)
    );
    assert_eq!(
        CoreConfig::resolve_path(Some(Path::new("/tmp/custom.json"))),
        PathBuf::from("/tmp/custom.json")
    );
}

#[test]
fn error_predicates_and_backtrace_accessor() {
    let err = ConfigError::invalid("synthetic");
    assert!(err.is_invalid_value());
    assert!(!err.is_io() && !err.is_config());
    assert_eq!(err.summary(), "synthetic");
    assert!(matches!(
        err.backtrace().status(),
        std::backtrace::BacktraceStatus::Captured | std::backtrace::BacktraceStatus::Disabled
    ));
    let io_err = ConfigError::io("ctx", io::Error::other("boom"));
    assert!(io_err.is_io());
    let layered_err = ConfigError::config("ctx", config::ConfigError::Message("boom".into()));
    assert!(layered_err.is_config());
}

#[test]
fn save_installs_world_readable_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    CoreConfig::default().save(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o644);
}

#[test]
fn volume_config_newtypes_enforce_domain_bounds() {
    for good_step in [1u8, 2, 5, 25] {
        let parsed = VolumeStepPercent::try_from(good_step).expect("valid step");
        assert_eq!(parsed.value(), good_step);
        assert_eq!(u8::from(parsed), good_step);
    }
    for bad_step in [0u8, 26, u8::MAX] {
        let err = VolumeStepPercent::try_from(bad_step).expect_err("invalid step");
        assert!(err.is_invalid_value());
        assert!(err.summary().contains("invalid volume_step_percent"));
    }

    for good_timeout in [250u32, 2_500, 30_000] {
        let parsed = VolumeHudTimeoutMs::try_from(good_timeout).expect("valid timeout");
        assert_eq!(parsed.value(), good_timeout);
        assert_eq!(u32::from(parsed), good_timeout);
    }
    for bad_timeout in [0u32, 249, 30_001, u32::MAX] {
        let err = VolumeHudTimeoutMs::try_from(bad_timeout).expect_err("invalid timeout");
        assert!(err.is_invalid_value());
        assert!(err.summary().contains("invalid volume_hud_timeout_ms"));
    }

    for good_fade in [0u32, 200, 2_000] {
        let parsed = VolumeHudFadeMs::try_from(good_fade).expect("valid fade");
        assert_eq!(parsed.value(), good_fade);
        assert_eq!(u32::from(parsed), good_fade);
    }
    for bad_fade in [2_001u32, u32::MAX] {
        let err = VolumeHudFadeMs::try_from(bad_fade).expect_err("invalid fade");
        assert!(err.is_invalid_value());
        assert!(err.summary().contains("invalid volume_hud_fade_ms"));
    }
}

#[test]
#[should_panic(expected = "volume_step_percent out of range 1..=25")]
fn volume_step_percent_new_rejects_zero() {
    let _ = VolumeStepPercent::new(0);
}

#[test]
#[should_panic(expected = "volume_step_percent out of range 1..=25")]
fn volume_step_percent_new_rejects_overflow() {
    let _ = VolumeStepPercent::new(26);
}

#[test]
#[should_panic(expected = "volume_hud_timeout_ms out of range 250..=30000")]
fn volume_hud_timeout_ms_new_rejects_underflow() {
    let _ = VolumeHudTimeoutMs::new(249);
}

#[test]
#[should_panic(expected = "volume_hud_timeout_ms out of range 250..=30000")]
fn volume_hud_timeout_ms_new_rejects_overflow() {
    let _ = VolumeHudTimeoutMs::new(30_001);
}

#[test]
#[should_panic(expected = "volume_hud_fade_ms out of range 0..=2000")]
fn volume_hud_fade_ms_new_rejects_overflow() {
    let _ = VolumeHudFadeMs::new(2_001);
}
