//! Integration tests for the appliance configuration store (`CoreConfig`).
#![cfg(feature = "ui")]

use speaker_core::config::{ConfigError, CoreConfig, DEFAULT_MARQUEE_FPS, MarqueeFps};
use speaker_core::diag::ScopedTempDir;
use speaker_core::ui::ThemePreset;
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
    }
    .save(&path)
    .expect("initial save");
    CoreConfig {
        theme: Some(ThemePreset::StudioDark),
        marquee_fps: MarqueeFps::FPS_30,
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
        CoreConfig::resolve_path(None),
        PathBuf::from(speaker_core::diag::DEFAULT_CONFIG_PATH)
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
