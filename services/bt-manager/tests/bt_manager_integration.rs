//! Integration tests for the bt-manager CLI application logic.

use bt_manager::agent::AgentCapabilityMode;
use bt_manager::daemon::{
    BtManagerOptions, parse_cli_args_from, parse_interval_secs, parse_ticks, run_app, run_tick,
};
use speaker_bluetooth::BluetoothDevice;
use speaker_common::diag::CliParse;
use std::process::ExitCode;
use std::time::Duration;

#[test]
fn defaults_match_documented_values() {
    let opts = BtManagerOptions::default();
    assert_eq!(opts.interval, Duration::from_secs(5));
    assert_eq!(opts.max_ticks, None);
    assert!(opts.cover_art);
    assert!(opts.auto_trust);
    assert_eq!(opts.capability, None);
    assert_eq!(opts.config, None);
    assert!(!opts.explicit_interval);
}

#[test]
fn parse_interval_accepts_plain_value() {
    assert_eq!(parse_interval_secs("10").expect("valid"), 10);
}

#[test]
fn parse_interval_rejects_sign_prefixes_and_garbage() {
    for bad in ["+5", "-5", "5s", "", "nan"] {
        assert!(parse_interval_secs(bad).is_err(), "must reject '{bad}'");
    }
}

#[test]
fn parse_ticks_rejects_sign_prefixes_and_garbage() {
    for bad in ["+1", "-1", "x", ""] {
        assert!(parse_ticks(bad).is_err(), "must reject '{bad}'");
    }
    assert_eq!(parse_ticks("3").expect("valid"), 3);
}

#[test]
fn cli_help_and_version_parse() {
    let args = ["-h"];
    assert!(matches!(
        parse_cli_args_from(args.iter().map(ToString::to_string)),
        Ok(CliParse::Help)
    ));
    let args = ["--version"];
    assert!(matches!(
        parse_cli_args_from(args.iter().map(ToString::to_string)),
        Ok(CliParse::Version)
    ));
}

#[test]
fn cli_parses_interval_and_ticks() {
    let args = ["--interval", "2", "-n", "4"];
    let parsed = parse_cli_args_from(args.iter().map(ToString::to_string)).expect("valid");
    let opts = match parsed {
        CliParse::Options(opts) => opts,
        other => panic!("expected options, got {other:?}"),
    };
    assert_eq!(opts.interval, Duration::from_secs(2));
    assert_eq!(opts.max_ticks, Some(4));
}

#[test]
fn cli_rejects_zero_and_invalid_interval() {
    let args = ["--interval", "0"];
    let err = parse_cli_args_from(args.iter().map(ToString::to_string)).expect_err("reject 0");
    assert!(err.contains("at least 1 second"), "got: {err}");
    let args = ["--interval"];
    assert!(
        parse_cli_args_from(args.iter().map(ToString::to_string))
            .is_err_and(|e| e.contains("missing value for --interval"))
    );
}

#[test]
fn cli_rejects_unknown_option() {
    let args = ["--bogus"];
    let err = parse_cli_args_from(args.iter().map(ToString::to_string)).expect_err("reject");
    assert!(err.contains("unknown option"), "got: {err}");
    assert!(!err.contains("Backtrace"), "user-facing error stays clean");
}

#[test]
fn cli_parses_auto_trust_and_capability_options() {
    let args = ["--no-auto-trust", "--capability", "no-input-no-output"];
    let parsed = parse_cli_args_from(args.iter().map(ToString::to_string)).expect("valid");
    let opts = match parsed {
        CliParse::Options(opts) => opts,
        other => panic!("expected options, got {other:?}"),
    };
    assert!(!opts.auto_trust);
    assert_eq!(opts.capability, Some(AgentCapabilityMode::NoInputNoOutput));

    let args = ["--capability", "display-yes-no"];
    let parsed = parse_cli_args_from(args.iter().map(ToString::to_string)).expect("valid");
    let opts = match parsed {
        CliParse::Options(opts) => opts,
        other => panic!("expected options, got {other:?}"),
    };
    assert!(opts.auto_trust);
    assert_eq!(opts.capability, Some(AgentCapabilityMode::DisplayYesNo));
}

#[test]
fn cli_rejects_malformed_capability_option() {
    let args = ["--capability"];
    let err =
        parse_cli_args_from(args.iter().map(ToString::to_string)).expect_err("missing arg value");
    assert!(err.contains("missing value for --capability"), "got: {err}");

    let args = ["--capability", "invalid-mode"];
    let err =
        parse_cli_args_from(args.iter().map(ToString::to_string)).expect_err("invalid mode value");
    assert!(err.contains("invalid capability mode"), "got: {err}");
}

#[test]
fn run_app_terminates_on_tick_cap_with_mock() {
    let (bt, _mock) = BluetoothDevice::new_mocked();
    let dir = speaker_common::diag::ScopedTempDir::new("bt_app_test");
    let opts = BtManagerOptions {
        config: Some(dir.path().join("missing.json")),
        interval: Duration::from_millis(1),
        explicit_interval: true,
        max_ticks: Some(3),
        cover_art: false,
        auto_trust: false,
        ..Default::default()
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    assert_eq!(rt.block_on(run_app(bt, opts)), ExitCode::SUCCESS);
}

#[test]
fn run_app_wakes_on_mock_property_change_before_keepalive() {
    // The monitor loop must react to a property-change signal immediately
    // instead of waiting out the full keep-alive interval: a mutation from
    // another thread wakes the second tick long before the 60 s timeout.
    let (bt, mock) = BluetoothDevice::new_mocked();
    let dir = speaker_common::diag::ScopedTempDir::new("bt_app_event_wake");
    let opts = BtManagerOptions {
        config: Some(dir.path().join("missing.json")),
        interval: Duration::from_secs(60),
        explicit_interval: true,
        max_ticks: Some(2),
        cover_art: false,
        auto_trust: false,
        ..Default::default()
    };
    let start = std::time::Instant::now();
    let changer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        mock.set_powered(false);
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    assert_eq!(rt.block_on(run_app(bt, opts)), ExitCode::SUCCESS);
    changer.join().expect("changer thread");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "monitor must wake on property change, not keep-alive; took {elapsed:?}"
    );
}

#[test]
fn run_app_terminates_on_tick_cap_with_auto_trust_and_mock() {
    let (bt, _mock) = BluetoothDevice::new_mocked();
    let dir = speaker_common::diag::ScopedTempDir::new("bt_app_trust_test");
    let missing_bus = dir.path().join("nonexistent-bus.sock");
    let opts = BtManagerOptions {
        config: Some(dir.path().join("missing.json")),
        interval: Duration::from_millis(1),
        explicit_interval: true,
        max_ticks: Some(2),
        cover_art: false,
        auto_trust: true,
        capability: Some(AgentCapabilityMode::DisplayYesNo),
        auto_trust_bus_address: Some(format!("unix:path={}", missing_bus.display())),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    assert_eq!(rt.block_on(run_app(bt, opts)), ExitCode::SUCCESS);
}

#[test]
fn run_app_respects_config_interval_when_not_explicit() {
    let (bt, _mock) = BluetoothDevice::new_mocked();
    let dir = speaker_common::diag::ScopedTempDir::new("bt_app_cfg_interval");
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"interval_secs": 1}"#).expect("write");
    let opts = BtManagerOptions {
        config: Some(path),
        interval: Duration::from_secs(5),
        explicit_interval: false,
        max_ticks: Some(1),
        cover_art: false,
        auto_trust: false,
        ..Default::default()
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    assert_eq!(rt.block_on(run_app(bt, opts)), ExitCode::SUCCESS);
}

#[test]
fn cli_rejects_removed_probe_flags() {
    for args in [
        vec!["--bip-probe", "11:22:33:44:55:66"],
        vec!["--bip-handle", "0000001"],
    ] {
        let err = parse_cli_args_from(args.iter().map(ToString::to_string))
            .expect_err("probe flags were removed");
        assert!(err.contains("unknown option"), "got: {err}");
    }
}

#[test]
fn run_app_zero_ticks_cap_is_rejected_at_parse() {
    let args = ["--ticks", "0"];
    let err = parse_cli_args_from(args.iter().map(ToString::to_string)).expect_err("reject 0");
    assert!(err.contains("omit --ticks"), "got: {err}");
}

#[test]
fn run_tick_surfaces_status_from_mock() {
    let (bt, _mock) = BluetoothDevice::new_mocked();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let status = rt.block_on(run_tick(&bt)).expect("mock status");
    // The mock initializes with one default connected peer.
    assert_eq!(status.connected_count(), 1);
}

#[test]
fn cli_parses_config_path() {
    let args = ["-c", "/tmp/custom.json", "-i", "3"];
    let parsed = parse_cli_args_from(args.iter().map(ToString::to_string)).expect("valid");
    let opts = match parsed {
        CliParse::Options(opts) => opts,
        other => panic!("expected options, got {other:?}"),
    };
    assert_eq!(
        opts.config,
        Some(std::path::PathBuf::from("/tmp/custom.json"))
    );
    assert_eq!(opts.interval, Duration::from_secs(3));
    assert!(opts.explicit_interval);
}

#[test]
fn run_app_fails_on_malformed_config() {
    let (bt, _mock) = BluetoothDevice::new_mocked();
    let dir = speaker_common::diag::ScopedTempDir::new("bt_app_malformed");
    let path = dir.path().join("config.json");
    std::fs::write(&path, "{bad-json").expect("write");
    let opts = BtManagerOptions {
        config: Some(path),
        ..Default::default()
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    assert_eq!(rt.block_on(run_app(bt, opts)), ExitCode::FAILURE);
}
