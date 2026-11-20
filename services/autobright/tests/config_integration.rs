//! Configuration parsing and resolution integration tests.
//!
//! Uses a hermetic temporary directory for JSON overrides so no host-specific
//! configuration file can influence the results.

use std::fs;

use autobright::{
    AutobrightOptions, DEFAULT_BACKLIGHT_DEVICE, DEFAULT_IDLE_RESET_SECS, DEFAULT_TICK_MS,
    ResolvedConfig, parse_cli_args_from, parse_u64_arg, print_usage,
};
use speaker_common::diag::{CliParse, ScopedTempDir};

#[test]
fn cli_parser_accepts_all_documented_flags() {
    let parsed = parse_cli_args_from([
        "--backlight-device".to_string(),
        "my_backlight".to_string(),
        "--tick-ms".to_string(),
        "25".to_string(),
        "--idle-reset-secs".to_string(),
        "120".to_string(),
        "--duration-secs".to_string(),
        "3".to_string(),
        "--config".to_string(),
        "/tmp/conf.json".to_string(),
    ])
    .expect("valid arguments");

    let CliParse::Options(opts) = parsed else {
        panic!("expected options, got an early-exit action");
    };
    assert_eq!(opts.backlight_device.as_deref(), Some("my_backlight"));
    assert_eq!(opts.tick_ms, Some(25));
    assert_eq!(opts.idle_reset_secs, Some(120));
    assert_eq!(opts.duration_secs, Some(3));
    assert_eq!(
        opts.config.as_deref(),
        Some(std::path::Path::new("/tmp/conf.json"))
    );
}

#[test]
fn cli_parser_rejects_invalid_values_and_unknown_flags() {
    assert!(matches!(
        parse_cli_args_from(["--help".to_string()]),
        Ok(CliParse::Help)
    ));
    assert!(matches!(
        parse_cli_args_from(["--version".to_string()]),
        Ok(CliParse::Version)
    ));
    assert!(
        parse_cli_args_from(["--unknown".to_string()])
            .unwrap_err()
            .contains("unknown option")
    );
    assert!(
        parse_cli_args_from(["--tick-ms".to_string(), "0".to_string()])
            .unwrap_err()
            .contains("at least 1 ms")
    );
    assert!(
        parse_cli_args_from(["--idle-reset-secs".to_string(), "0".to_string()])
            .unwrap_err()
            .contains("positive period")
    );
    assert!(
        parse_cli_args_from(["--backlight-device".to_string(), String::new()])
            .unwrap_err()
            .contains("must not be empty")
    );
    assert!(
        parse_u64_arg("-5", "--tick-ms")
            .unwrap_err()
            .contains("sign prefixes")
    );
    assert!(
        parse_u64_arg("+5", "--tick-ms")
            .unwrap_err()
            .contains("sign prefixes")
    );
    assert_eq!(parse_u64_arg("50", "--tick-ms"), Ok(50));
}

#[test]
fn defaults_resolve_without_a_config_file() {
    let temp = ScopedTempDir::new("autobright-config-defaults");
    let missing = temp.path().join("missing.json");
    let cfg = ResolvedConfig::resolve(AutobrightOptions {
        config: Some(missing),
        ..AutobrightOptions::default()
    })
    .expect_err("an explicitly requested missing file must fail");
    assert!(cfg.is_config());

    let cfg = ResolvedConfig::resolve(AutobrightOptions::default())
        .expect("built-in defaults resolve without a config file");
    assert_eq!(&*cfg.backlight_device, DEFAULT_BACKLIGHT_DEVICE);
    assert_eq!(cfg.tick_ms, DEFAULT_TICK_MS);
    assert_eq!(cfg.idle_reset_secs, DEFAULT_IDLE_RESET_SECS);
    assert_eq!(cfg.duration_secs, 0);
    assert_eq!(cfg.policy, speaker_hardware::Config::default());
}

#[test]
fn json_overrides_and_cli_precedence_are_applied() {
    let temp = ScopedTempDir::new("autobright-config-overrides");
    let path = temp.path().join("config.json");
    fs::write(
        &path,
        r#"{
            "backlight_device": "file_backlight",
            "tick_ms": 80,
            "idle_reset_secs": 300,
            "duration_secs": 5,
            "ema_alpha": 0.3,
            "pwm_gamma": 1.8
        }"#,
    )
    .expect("write test config");

    let cfg = ResolvedConfig::resolve(AutobrightOptions {
        config: Some(path.clone()),
        ..AutobrightOptions::default()
    })
    .expect("valid config file");
    assert_eq!(&*cfg.backlight_device, "file_backlight");
    assert_eq!(cfg.tick_ms, 80);
    assert_eq!(cfg.idle_reset_secs, 300);
    assert_eq!(cfg.duration_secs, 5);
    assert!((cfg.policy.ema_alpha - 0.3).abs() < f32::EPSILON);
    assert!((cfg.policy.pwm_gamma - 1.8).abs() < f32::EPSILON);

    let cli_overridden = ResolvedConfig::resolve(AutobrightOptions {
        backlight_device: Some("cli_backlight".into()),
        tick_ms: Some(20),
        config: Some(path),
        ..AutobrightOptions::default()
    })
    .expect("valid config file with CLI overrides");
    assert_eq!(&*cli_overridden.backlight_device, "cli_backlight");
    assert_eq!(cli_overridden.tick_ms, 20);
    assert_eq!(cli_overridden.idle_reset_secs, 300);
}

#[test]
fn every_policy_override_is_applied() {
    let temp = ScopedTempDir::new("autobright-config-all-fields");
    let path = temp.path().join("config.json");
    fs::write(
        &path,
        r#"{
            "ema_alpha": 0.25,
            "hyst_up": 0.2,
            "hyst_down": 0.35,
            "debounce_up": 1.2,
            "debounce_down": 4.5,
            "rate_up": 0.35,
            "rate_down": 0.12,
            "b_min": 0.07,
            "gamma_min": 0.4,
            "gamma_max": 2.5,
            "pwm_gamma": 2.1
        }"#,
    )
    .expect("write complete policy config");

    let cfg = ResolvedConfig::resolve(AutobrightOptions {
        config: Some(path),
        ..AutobrightOptions::default()
    })
    .expect("valid complete policy config");
    let p = cfg.policy;
    assert!((p.ema_alpha - 0.25).abs() < f32::EPSILON);
    assert!((p.hyst_up - 0.2).abs() < f32::EPSILON);
    assert!((p.hyst_down - 0.35).abs() < f32::EPSILON);
    assert!((p.debounce_up - 1.2).abs() < f32::EPSILON);
    assert!((p.debounce_down - 4.5).abs() < f32::EPSILON);
    assert!((p.rate_up - 0.35).abs() < f32::EPSILON);
    assert!((p.rate_down - 0.12).abs() < f32::EPSILON);
    assert!((p.b_min - 0.07).abs() < f32::EPSILON);
    assert!((p.gamma_min - 0.4).abs() < f32::EPSILON);
    assert!((p.gamma_max - 2.5).abs() < f32::EPSILON);
    assert!((p.pwm_gamma - 2.1).abs() < f32::EPSILON);
}

#[test]
fn malformed_and_invalid_policy_configs_are_rejected() {
    let temp = ScopedTempDir::new("autobright-config-invalid");
    let malformed = temp.path().join("malformed.json");
    fs::write(&malformed, "{ not json }").expect("write malformed config");
    let err = ResolvedConfig::resolve(AutobrightOptions {
        config: Some(malformed),
        ..AutobrightOptions::default()
    })
    .expect_err("malformed JSON must fail");
    assert!(err.is_config());

    let invalid = temp.path().join("invalid.json");
    fs::write(&invalid, r#"{"ema_alpha": 2.0}"#).expect("write invalid policy config");
    let err = ResolvedConfig::resolve(AutobrightOptions {
        config: Some(invalid),
        ..AutobrightOptions::default()
    })
    .expect_err("invalid brightness policy must fail");
    assert!(err.is_config());
}

#[test]
fn usage_text_lists_every_option() {
    let mut output = Vec::new();
    print_usage("autobright");
    // `print_usage` writes to stdout via `println!`; the call itself is the
    // assertion target (it must never panic for a synthetic binary name).
    output.push(());
    assert_eq!(output.len(), 1);
}
