//! CLI integration tests for `bt-manager` service binary.

use std::process::{Command, ExitStatus};

fn run_cli(bin_path: &str, args: &[&str]) -> (ExitStatus, String, String) {
    let output = Command::new(bin_path)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute {bin_path}: {e}"));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status, stdout, stderr)
}

fn assert_cli_rejects(bin_path: &str, cases: &[(&[&str], &str)]) {
    for (args, expected_fragment) in cases {
        let (status, _, stderr) = run_cli(bin_path, args);
        assert!(
            !status.success(),
            "expected non-zero exit for '{bin_path} {}', got: {status:?}",
            args.join(" ")
        );
        assert!(
            stderr.contains(expected_fragment),
            "expected stderr to contain '{expected_fragment}', got: '{stderr}'"
        );
    }
}

#[test]
fn bt_manager_help_display() {
    for flag in ["--help", "-h"] {
        let (status, stdout, _) = run_cli(env!("CARGO_BIN_EXE_bt-manager"), &[flag]);
        assert!(status.success(), "expected success for 'bt-manager {flag}'");
        assert!(stdout.contains("DietPi Bluetooth Manager Service"));
        assert!(stdout.contains("--interval"));
        assert!(stdout.contains("--config"));
        assert!(stdout.contains("--no-auto-trust"));
        assert!(stdout.contains("--capability"));
    }
}

#[test]
fn bt_manager_version_display() {
    for flag in ["--version", "-V"] {
        let (status, stdout, _) = run_cli(env!("CARGO_BIN_EXE_bt-manager"), &[flag]);
        assert!(status.success(), "expected success for 'bt-manager {flag}'");
        assert!(
            stdout.contains(&format!("bt-manager {}", env!("CARGO_PKG_VERSION"))),
            "expected version output, got: '{stdout}'"
        );
    }
}

#[test]
fn bt_manager_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_bt-manager");
    assert_cli_rejects(
        bin_path,
        &[
            (&["--invalid-flag"], "unknown option '--invalid-flag'"),
            (&["--config"], "missing value for --config"),
            (&["--interval"], "missing value for --interval"),
            (
                &["--interval", "not-a-number"],
                "invalid interval seconds 'not-a-number'",
            ),
            (&["--interval", "0"], "must be at least 1 second"),
            (&["--ticks"], "missing value for --ticks"),
            (
                &["--ticks", "not-a-number"],
                "invalid tick count 'not-a-number'",
            ),
            (&["--ticks", "0"], "omit --ticks to run forever"),
            (&["--capability"], "missing value for --capability"),
            (&["--capability", "invalid-mode"], "invalid capability mode"),
        ],
    );
}
