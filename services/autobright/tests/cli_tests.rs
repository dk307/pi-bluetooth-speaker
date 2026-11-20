//! Table-driven subprocess tests for the `autobright` CLI.
//!
//! Every probe runs with an explicit kill-on-hang deadline so a regression
//! that blocks instead of exiting fails loudly on CI rather than stalling the
//! suite. User-facing rejections must be backtrace-free.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use speaker_common::diag::ScopedTempDir;

#[derive(Debug)]
struct CliCase {
    name: &'static str,
    args: &'static [&'static str],
    expect_success: bool,
    stdout_contains: &'static str,
    stderr_contains: &'static str,
}

fn run_cli(args: &[&str]) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_autobright"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("autobright test binary must be executable");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child
            .try_wait()
            .expect("polling child process status must succeed")
            .is_some()
        {
            let output = child
                .wait_with_output()
                .expect("collecting child output must succeed");
            return (
                output.status.success(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("autobright CLI {args:?} did not exit within 10 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "table-driven CLI cases stay in one declarative list for reviewability"
)]
#[test]
fn cli_argument_table() {
    let cases = [
        CliCase {
            name: "help",
            args: &["--help"],
            expect_success: true,
            stdout_contains: "Usage:",
            stderr_contains: "",
        },
        CliCase {
            name: "short help",
            args: &["-h"],
            expect_success: true,
            stdout_contains: "Usage:",
            stderr_contains: "",
        },
        CliCase {
            name: "version",
            args: &["--version"],
            expect_success: true,
            stdout_contains: "autobright",
            stderr_contains: "",
        },
        CliCase {
            name: "unknown option",
            args: &["--bogus"],
            expect_success: false,
            stdout_contains: "",
            stderr_contains: "unknown option",
        },
        CliCase {
            name: "tick zero",
            args: &["--tick-ms", "0"],
            expect_success: false,
            stdout_contains: "",
            stderr_contains: "at least 1 ms",
        },
        CliCase {
            name: "tick negative",
            args: &["--tick-ms", "-1"],
            expect_success: false,
            stdout_contains: "",
            stderr_contains: "sign prefixes",
        },
        CliCase {
            name: "tick nan",
            args: &["--tick-ms", "nan"],
            expect_success: false,
            stdout_contains: "",
            stderr_contains: "invalid --tick-ms",
        },
        CliCase {
            name: "tick overflow",
            args: &["--tick-ms", "18446744073709551616"],
            expect_success: false,
            stdout_contains: "",
            stderr_contains: "invalid --tick-ms",
        },
        CliCase {
            name: "idle reset zero",
            args: &["--idle-reset-secs", "0"],
            expect_success: false,
            stdout_contains: "",
            stderr_contains: "positive period",
        },
        CliCase {
            name: "duration nan",
            args: &["--duration-secs", "nan"],
            expect_success: false,
            stdout_contains: "",
            stderr_contains: "invalid --duration-secs",
        },
        CliCase {
            name: "missing config value",
            args: &["--config"],
            expect_success: false,
            stdout_contains: "",
            stderr_contains: "missing value for --config",
        },
        CliCase {
            name: "missing device value",
            args: &["--backlight-device"],
            expect_success: false,
            stdout_contains: "",
            stderr_contains: "missing value for --backlight-device",
        },
    ];

    for case in cases {
        let (success, stdout, stderr) = run_cli(case.args);
        assert_eq!(
            success, case.expect_success,
            "case '{}' exit status",
            case.name
        );
        assert!(
            stdout.contains(case.stdout_contains),
            "case '{}' stdout {stdout:?} missing {case:?}",
            case.name
        );
        assert!(
            stderr.contains(case.stderr_contains),
            "case '{}' stderr {stderr:?} missing {case:?}",
            case.name
        );
        assert!(
            !stderr.contains("Backtrace") && !stderr.contains("panicked"),
            "case '{}' leaked a backtrace/panic: {stderr:?}",
            case.name
        );
    }
}

#[test]
fn valid_arguments_reach_daemon_main_and_report_config_failure() {
    // Earlier CLI cases all exit during argument parsing; this one parses
    // successfully so the binary exercises runtime creation and the call into
    // `daemon::main`. An explicitly missing config file fails deterministically
    // before any hardware or D-Bus access is attempted.
    let temp = ScopedTempDir::new("autobright-cli-resolve-error");
    let missing = temp.path().join("absent.json");
    let path = missing.to_str().expect("temporary path is UTF-8");
    let (success, _stdout, stderr) = run_cli(&["--config", path, "--duration-secs", "1"]);

    assert!(!success, "an explicit missing config must fail: {stderr:?}");
    assert!(
        stderr.contains("failed to resolve autobright configuration"),
        "expected daemon resolve failure, got {stderr:?}"
    );
    assert!(
        !stderr.contains("Backtrace") && !stderr.contains("panicked"),
        "user-facing config failure leaked a panic: {stderr:?}"
    );
}

#[test]
fn bundled_short_and_long_flags_parse_together() {
    // Parsing these options must succeed up to the point where hardware is
    // opened; use --help last so the process exits before touching sysfs or
    // D-Bus while still exercising the earlier flag parsers.
    let (success, stdout, stderr) = run_cli(&[
        "-d",
        "rpi_backlight",
        "-t",
        "50",
        "--idle-reset-secs",
        "600",
        "--duration-secs",
        "0",
        "--help",
    ]);
    assert!(success, "combined flags should parse: {stderr:?}");
    assert!(stdout.contains("Usage:"));
    assert!(!stderr.contains("Backtrace") && !stderr.contains("panicked"));
}
