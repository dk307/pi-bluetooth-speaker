//! CLI integration tests for `screen-manager` binary.

use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const CLI_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const SCREEN_MANAGER_MAIN_TIMEOUT: Duration = Duration::from_secs(30);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

fn wait_with_deadline(mut child: Child, timeout: Duration, context: &str) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child
                    .wait_with_output()
                    .unwrap_or_else(|e| panic!("failed to collect output for '{context}': {e}"));
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(WAIT_POLL_INTERVAL);
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("subprocess '{context}' timed out after {timeout:?}");
            }
            Err(e) => panic!("wait failed for '{context}': {e}"),
        }
    }
}

fn run_cli_with_timeout(
    bin: &str,
    args: &[&str],
    timeout: Duration,
) -> (ExitStatus, String, String) {
    let child = Command::new(bin)
        .env("SPEAKER_CORE_HEADLESS", "1")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn '{bin}' with args {args:?}: {e}"));
    let output = wait_with_deadline(child, timeout, bin);
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_cli(bin: &str, args: &[&str]) -> (ExitStatus, String, String) {
    run_cli_with_timeout(bin, args, CLI_PROBE_TIMEOUT)
}

#[test]
fn screen_manager_binary_executes() {
    let bin_path = env!("CARGO_BIN_EXE_screen-manager");
    let (status, _, stderr) = run_cli_with_timeout(bin_path, &[], SCREEN_MANAGER_MAIN_TIMEOUT);

    assert!(status.success());
    assert!(stderr.contains("DietPi Screen Manager Service running on Raspberry Pi 3 (aarch64)"));
    assert!(stderr.contains("arm-cortex-a53"));
    assert!(stderr.contains("Screen manager service started"));

    // Test help flags
    for flag in &["-h", "--help"] {
        let (status, stdout, _) = run_cli(bin_path, &[flag]);
        assert!(status.success());
        assert!(stdout.contains("DietPi Touchscreen Appliance Console & Screen Manager Service"));
        assert!(stdout.contains("--preview"));
    }

    // Test version flags
    for flag in &["-V", "--version"] {
        let (status, stdout, _) = run_cli(bin_path, &[flag]);
        assert!(status.success());
        assert!(stdout.contains(&format!("screen-manager {}", env!("CARGO_PKG_VERSION"))));
    }

    // Test zero duration preview
    let (status, _, stderr) = run_cli(bin_path, &["-p", "0"]);
    assert!(status.success());
    assert!(stderr.contains("Screen Manager Service"));

    // Test error cases
    for (args, expected) in [
        (
            &["--preview", "invalid"][..],
            "invalid preview seconds 'invalid'",
        ),
        (&["--preview", "+5"][..], "sign prefixes are not accepted"),
        (&["--preview", "-5"][..], "sign prefixes are not accepted"),
        (&["--preview"][..], "missing value for --preview"),
        (&["--unknown-flag"][..], "unknown option '--unknown-flag'"),
    ] {
        let (status, _, stderr) = run_cli(bin_path, args);
        assert!(!status.success());
        assert!(stderr.contains(expected), "stderr: {stderr}");
        assert!(!stderr.contains("Backtrace") && !stderr.contains("panicked"));
    }
}
