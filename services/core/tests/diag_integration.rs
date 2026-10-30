//! Integration tests for shared CLI diagnostic helpers and numeric stats.

use std::process::ExitCode;
use std::time::Duration;

use speaker_core::diag::{
    CliExitAction, CliParse, CommonCliOptions, CommonFlagStatus, NumericStats, OutputFormat,
    ScopedTempDir, env_args, exit_code_for_reads, foot_candles, format_min_max_avg,
    format_timestamp, next_arg_value, parse_args_with_common, parse_count_arg, parse_interval_arg,
    print_min_max_avg, program_name, resolve_cli_opts, round_2dp, run_poll_loop,
    should_print_summary,
};

#[test]
fn format_timestamp_returns_valid_hh_mm_ss() {
    let ts = format_timestamp();
    assert_eq!(ts.len(), 8);
    assert_eq!(&ts[2..3], ":");
    assert_eq!(&ts[5..6], ":");
}

#[test]
fn parse_count_and_interval_validate_bounds() {
    assert_eq!(parse_count_arg(Some("5".into())).unwrap(), 5);
    assert!(parse_count_arg(None).unwrap_err().contains("missing value"));
    assert!(
        parse_count_arg(Some("0".into()))
            .unwrap_err()
            .contains("at least 1")
    );
    assert!(
        parse_count_arg(Some("abc".into()))
            .unwrap_err()
            .contains("invalid count")
    );

    assert_eq!(
        parse_interval_arg(Some("0.5".into())).unwrap(),
        Duration::from_millis(500)
    );
    assert!(
        parse_interval_arg(None)
            .unwrap_err()
            .contains("missing value")
    );
    for bad in ["0", "-1.0", "nan", "inf", "1e400", "abc"] {
        assert!(
            parse_interval_arg(Some(bad.into())).is_err(),
            "accepted {bad}"
        );
    }
}

#[test]
fn exit_code_and_arg_helpers() {
    assert_eq!(exit_code_for_reads(0, 0), ExitCode::SUCCESS);
    assert_eq!(exit_code_for_reads(3, 1), ExitCode::SUCCESS);
    assert_eq!(exit_code_for_reads(3, 0), ExitCode::FAILURE);
    assert!(should_print_summary(OutputFormat::Human, None, 2));
    assert!(should_print_summary(OutputFormat::Human, Some(2), 2));
    assert!(!should_print_summary(OutputFormat::Human, Some(1), 2));
    assert!(!should_print_summary(OutputFormat::Human, None, 1));
    assert!(!should_print_summary(OutputFormat::Json, None, 5));
    assert_ne!(program_name("fallback"), "");
    let _ = env_args().count();

    let dir_path = {
        let temp = ScopedTempDir::try_new("diag_scoped_test").unwrap();
        assert!(temp.path().is_dir());
        temp.path().to_path_buf()
    };
    assert!(!dir_path.exists());

    let mut no_sample = NumericStats::default();
    no_sample.record_success_no_sample();
    no_sample.record_error();
    assert_eq!(no_sample.total_reads(), 2);
    assert_eq!(no_sample.successful_reads(), 1);
    assert_eq!(no_sample.errors(), 1);
    assert_eq!(no_sample.error_rate_percent(), 50.0);
    assert!(no_sample.average().is_none());
    assert!(no_sample.min().is_none());
    assert!(no_sample.max().is_none());

    let mut stats = NumericStats::default();
    assert_eq!(stats.total_reads(), 0);
    assert_eq!(stats.error_rate_percent(), 0.0);
    assert_eq!(stats.fraction_of_reads_percent(0), 0.0);
    assert!(stats.average().is_none());
    stats.record_success(10.0);
    stats.record_success(30.0);
    stats.record_error();
    assert_eq!(stats.total_reads(), 3);
    assert_eq!(stats.successful_reads(), 2);
    assert_eq!(stats.errors(), 1);
    assert_eq!(stats.min(), Some(10.0));
    assert_eq!(stats.max(), Some(30.0));
    assert!((stats.average().unwrap() - 20.0).abs() < 1e-6);
    assert!((stats.average_f32().unwrap() - 20.0).abs() < 1e-6);
    assert!((stats.error_rate_percent() - (100.0 / 3.0)).abs() < 1e-4);
    assert!((stats.fraction_of_reads_percent(1) - (100.0 / 3.0)).abs() < 1e-4);

    assert!((round_2dp(12.3456) - 12.35).abs() < 1e-6);

    let mut args = ["val".to_string()].into_iter();
    assert_eq!(next_arg_value(&mut args, "--flag").unwrap(), "val");
    assert!(
        next_arg_value(&mut args, "--flag")
            .unwrap_err()
            .contains("missing value for --flag")
    );
}

#[test]
fn resolve_cli_opts_and_common_flag_parsing() {
    fn noop_usage(_: &str) {}

    assert_eq!(
        resolve_cli_opts(Ok(CliParse::Options(42)), "bin", noop_usage).unwrap(),
        42
    );
    assert_eq!(
        resolve_cli_opts::<i32>(Ok(CliParse::Help), "bin", noop_usage).unwrap_err(),
        ExitCode::SUCCESS
    );
    assert_eq!(
        resolve_cli_opts::<i32>(Ok(CliParse::Version), "bin", noop_usage).unwrap_err(),
        ExitCode::SUCCESS
    );
    assert_eq!(
        resolve_cli_opts::<i32>(Err("bad arg".into()), "bin", noop_usage).unwrap_err(),
        ExitCode::FAILURE
    );

    let mut common = CommonCliOptions::with_default_interval(Duration::from_secs(1));
    let mut empty = std::iter::empty::<String>();
    assert_eq!(common.max_count, Some(1), "once mode is the default");
    assert_eq!(
        common.parse_common_flag("--help", &mut empty).unwrap(),
        CommonFlagStatus::Exit(CliExitAction::Help)
    );
    assert_eq!(
        common.parse_common_flag("--version", &mut empty).unwrap(),
        CommonFlagStatus::Exit(CliExitAction::Version)
    );
    assert_eq!(
        common.parse_common_flag("--repeat", &mut empty).unwrap(),
        CommonFlagStatus::Consumed
    );
    assert_eq!(common.max_count, None, "--repeat enables polling");
    assert_eq!(
        common.parse_common_flag("--once", &mut empty).unwrap(),
        CommonFlagStatus::Consumed
    );
    assert_eq!(common.max_count, Some(1));
    assert_eq!(
        common.parse_common_flag("--verbose", &mut empty).unwrap(),
        CommonFlagStatus::Consumed
    );
    assert!(common.verbose);
    assert_eq!(
        common.parse_common_flag("--json", &mut empty).unwrap(),
        CommonFlagStatus::Consumed
    );
    assert_eq!(common.format, OutputFormat::Json);
    assert_eq!(
        common.parse_common_flag("--custom", &mut empty).unwrap(),
        CommonFlagStatus::Custom
    );
}

#[test]
fn parse_args_with_common_helper_tests() {
    #[derive(Default, PartialEq, Debug)]
    struct TestOpts {
        common: CommonCliOptions,
        flag: bool,
    }

    let parsed = parse_args_with_common(
        TestOpts::default(),
        ["--once".into(), "--flag".into()],
        |o| &mut o.common,
        |opts, arg, _args| match arg {
            "--flag" => {
                opts.flag = true;
                Ok(true)
            }
            _ => Ok(false),
        },
    )
    .unwrap();
    let opts = match parsed {
        CliParse::Options(o) => o,
        other => panic!("expected CliParse::Options, got {other:?}"),
    };
    assert_eq!(opts.common.max_count, Some(1));
    assert!(opts.flag);

    let err = parse_args_with_common(
        TestOpts::default(),
        ["--unknown".into()],
        |o| &mut o.common,
        |_opts, _arg, _args| Ok(false),
    )
    .unwrap_err();
    assert!(err.contains("unknown option '--unknown'"));

    let help = parse_args_with_common(
        TestOpts::default(),
        ["--help".into()],
        |o| &mut o.common,
        |_opts, _arg, _args| Ok(false),
    )
    .unwrap();
    assert!(matches!(help, CliParse::Help));

    let version = parse_args_with_common(
        TestOpts::default(),
        ["--version".into()],
        |o| &mut o.common,
        |_opts, _arg, _args| Ok(false),
    )
    .unwrap();
    assert!(matches!(version, CliParse::Version));
}

#[test]
#[should_panic(expected = "numeric sample must be finite")]
fn numeric_stats_record_success_panics_on_nan() {
    let mut stats = NumericStats::default();
    stats.record_success(f32::NAN);
}

#[test]
#[should_panic(expected = "numeric sample must be finite")]
fn numeric_stats_record_success_panics_on_infinity() {
    let mut stats = NumericStats::default();
    stats.record_success(f32::INFINITY);
}

#[test]
fn run_poll_loop_terminates_on_is_done() {
    let mut count = 0;
    run_poll_loop(Duration::from_millis(1), |_timestamp| {
        count += 1;
        count >= 3
    });
    assert_eq!(count, 3);
}

#[test]
fn format_min_max_avg_formatting_and_bounds() {
    let mut stats = NumericStats::default();
    stats.record_success(10.0);
    stats.record_success(30.0);
    let formatted_light =
        format_min_max_avg(&stats, "Light", "lx", Some((foot_candles, "fc"))).unwrap();
    assert_eq!(formatted_light[0], "Light Min   :   10.00 lx (  0.93 fc)");
    assert_eq!(formatted_light[1], "Light Max   :   30.00 lx (  2.79 fc)");
    assert_eq!(formatted_light[2], "Light Avg   :   20.00 lx (  1.86 fc)");

    let formatted_plain = format_min_max_avg(&stats, "Chassis", "°C", None).unwrap();
    assert_eq!(formatted_plain[0], "Chassis Min :   10.00 °C");
    assert_eq!(formatted_plain[1], "Chassis Max :   30.00 °C");
    assert_eq!(formatted_plain[2], "Chassis Avg :   20.00 °C");

    let empty_stats = NumericStats::default();
    assert_eq!(format_min_max_avg(&empty_stats, "Test", "lx", None), None);

    // Exercise print_min_max_avg to cover stdout rendering for both Some and None paths
    print_min_max_avg(&stats, "Light", "lx", Some((foot_candles, "fc")));
    print_min_max_avg(&empty_stats, "Empty", "lx", None);
}
