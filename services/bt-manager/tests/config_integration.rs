//! Integration tests for the Bluetooth manager configuration store (`BtManagerConfig`).

use bt_manager::agent::AgentCapabilityMode;
use bt_manager::config::{
    BtManagerConfig, ConfigError, DEFAULT_POLL_INTERVAL_SECS, PollIntervalSecs,
};
use speaker_common::diag::ScopedTempDir;
use std::error::Error as _;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[test]
fn defaults_are_sensible() {
    let config = BtManagerConfig::default();
    assert_eq!(config.interval_secs, PollIntervalSecs::DEFAULT);
    assert_eq!(config.interval_secs.value(), 5);
    assert_eq!(config.interval_secs.as_duration(), Duration::from_secs(5));
    assert_eq!(DEFAULT_POLL_INTERVAL_SECS, 5);
    assert!(config.cover_art);
    assert!(config.auto_trust);
    assert_eq!(config.capability, AgentCapabilityMode::DisplayYesNo);
}

#[test]
fn load_missing_file_returns_defaults() {
    let dir = ScopedTempDir::new("bt_cfg_missing");
    let path = dir.path().join("does-not-exist.json");
    let config = BtManagerConfig::load(&path).expect("missing file must yield defaults");
    assert_eq!(config, BtManagerConfig::default());
}

#[test]
fn load_missing_file_logs_warning_field_with_subscriber() {
    let dir = ScopedTempDir::new("bt_cfg_missing_logged");
    let path = dir.path().join("absent.json");
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let config = BtManagerConfig::load(&path).expect("missing file must yield defaults");
        assert_eq!(config, BtManagerConfig::default());
    });
}

#[test]
fn save_and_load_roundtrip() {
    let dir = ScopedTempDir::new("bt_cfg_roundtrip");
    let path = dir.path().join("config.json");
    let config = BtManagerConfig {
        interval_secs: PollIntervalSecs::new(10),
        cover_art: false,
        auto_trust: false,
        capability: AgentCapabilityMode::NoInputNoOutput,
    };
    config.save(&path).expect("save");
    let loaded = BtManagerConfig::load(&path).expect("load");
    assert_eq!(loaded, config);
    // Raw on-disk form is pretty JSON.
    let raw = std::fs::read_to_string(&path).expect("raw read");
    assert!(raw.contains("\"interval_secs\": 10"), "{raw}");
    assert!(raw.contains("\"cover_art\": false"), "{raw}");
    assert!(raw.contains("\"auto_trust\": false"), "{raw}");
    assert!(
        raw.contains("\"capability\": \"no-input-no-output\""),
        "{raw}"
    );
}

#[test]
fn minimal_config_deserializes_with_defaults() {
    let config: BtManagerConfig = serde_json::from_str("{}").expect("empty object");
    assert_eq!(config, BtManagerConfig::default());
    let config: BtManagerConfig =
        serde_json::from_str(r#"{"interval_secs": 15}"#).expect("interval only");
    assert_eq!(config.interval_secs, PollIntervalSecs::new(15));
    assert!(config.cover_art);
    assert!(config.auto_trust);
    assert_eq!(config.capability, AgentCapabilityMode::DisplayYesNo);
}

#[test]
fn load_malformed_json_is_hard_parse_error() {
    let dir = ScopedTempDir::new("bt_cfg_malformed");
    let path = dir.path().join("config.json");
    std::fs::write(&path, "{not json").expect("write");
    let err = BtManagerConfig::load(&path).expect_err("malformed JSON must fail");
    assert!(err.is_config());
    assert!(err.to_string().contains("failed to read config"));
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
fn load_rejects_zero_interval() {
    let dir = ScopedTempDir::new("bt_cfg_bad_interval");
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"interval_secs": 0}"#).expect("write");
    let err = BtManagerConfig::load(&path).expect_err("zero interval must fail");
    assert!(err.is_config(), "{err}");
    let rendered = err.to_string();
    assert!(
        rendered.contains("invalid interval seconds '0': interval must be at least 1 second"),
        "{rendered}"
    );
}

#[test]
fn load_accepts_valid_interval_values() {
    for secs in [1u64, 2, 5, 10, 60] {
        let dir = ScopedTempDir::new("bt_cfg_good_interval");
        let path = dir.path().join("config.json");
        std::fs::write(&path, format!(r#"{{"interval_secs": {secs}}}"#)).expect("write");
        let config = BtManagerConfig::load(&path).expect("valid interval must load");
        assert_eq!(config.interval_secs.value(), secs);
        assert_eq!(
            config.interval_secs.as_duration(),
            Duration::from_secs(secs)
        );
    }
}

#[test]
fn load_io_failure_on_directory_path() {
    let dir = ScopedTempDir::new("bt_cfg_dir_path");
    let err = BtManagerConfig::load(dir.path()).expect_err("directory read must fail");
    assert!(err.is_config());
    assert!(err.to_string().contains("failed to read config"));
    assert!(err.summary().contains("config error"));
    assert!(!err.summary().contains("Backtrace"));
    assert!(err.source().is_some());
}

#[test]
fn save_io_failure_on_directory_target() {
    let dir = ScopedTempDir::new("bt_cfg_save_dir");
    let config = BtManagerConfig::default();
    let target = dir.path().join("blocked");
    std::fs::create_dir(&target).expect("create blocking directory");
    let err = config
        .save(&target)
        .expect_err("rename over directory must fail");
    assert!(err.is_io());
    assert!(err.to_string().contains("failed to install config"));
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
    let dir = ScopedTempDir::new("bt_cfg_save_replace");
    let path = dir.path().join("config.json");
    BtManagerConfig {
        interval_secs: PollIntervalSecs::new(5),
        cover_art: true,
        auto_trust: true,
        capability: AgentCapabilityMode::DisplayYesNo,
    }
    .save(&path)
    .expect("initial save");
    BtManagerConfig {
        interval_secs: PollIntervalSecs::new(10),
        cover_art: false,
        auto_trust: false,
        capability: AgentCapabilityMode::NoInputNoOutput,
    }
    .save(&path)
    .expect("replacement save");
    let loaded = BtManagerConfig::load(&path).expect("reload replaced config");
    assert_eq!(loaded.interval_secs.value(), 10);
    assert!(!loaded.cover_art);
    assert!(!loaded.auto_trust);
    assert_eq!(loaded.capability, AgentCapabilityMode::NoInputNoOutput);
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
    for name in ["config.txt", "conf", "settings.json"] {
        let dir = ScopedTempDir::new("bt_cfg_extless");
        let path = dir.path().join(name);
        std::fs::write(&path, r#"{"interval_secs": 12}"#).expect("write");
        let config = BtManagerConfig::load(&path).expect("valid interval must load");
        assert_eq!(config.interval_secs.value(), 12, "{name}");
    }
}

#[test]
fn resolve_path_uses_override_and_default() {
    assert_eq!(
        BtManagerConfig::resolve_path_with(None, |_| false),
        PathBuf::from(speaker_common::diag::DEFAULT_BT_CONFIG_PATH)
    );
    assert_eq!(
        BtManagerConfig::resolve_path_with(None, |p| p
            == Path::new(speaker_common::diag::DEFAULT_BT_CONFIG_PATH)),
        PathBuf::from(speaker_common::diag::DEFAULT_BT_CONFIG_PATH)
    );
    assert_eq!(
        BtManagerConfig::resolve_path(Some(Path::new("/tmp/custom.json"))),
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
    assert!(io_err.to_string().contains("config I/O error: ctx: boom"));
    assert_eq!(io_err.summary(), "config I/O error: ctx: boom");
    assert!(io_err.source().is_some());

    let layered_err = ConfigError::config("ctx", config::ConfigError::Message("boom".into()));
    assert!(layered_err.is_config());
    assert!(layered_err.to_string().contains("config error: ctx: boom"));
    assert_eq!(layered_err.summary(), "config error: ctx: boom");
    assert!(layered_err.source().is_some());
}

#[test]
fn save_installs_world_readable_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    BtManagerConfig::default().save(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o644);
}
