//! Direct in-process tests of `autobright::daemon::main` failure paths.
//!
//! Passing an invalid on-disk policy makes configuration resolution fail
//! before the daemon opens the BH1750 I²C bus or the sysfs backlight, so these
//! tests stay hermetic on any development host.

use std::fs;
use std::process::ExitCode;

use autobright::{AutobrightOptions, daemon};
use speaker_common::diag::ScopedTempDir;

#[path = "common/log_capture.rs"]
mod log_capture;

use log_capture::LogCapture;

#[tokio::test]
async fn main_fails_at_resolution_when_policy_is_invalid() {
    let logs = LogCapture::install();
    let temp = ScopedTempDir::new("autobright-main-invalid-policy");
    let path = temp.path().join("invalid.json");
    fs::write(&path, r#"{"ema_alpha": 2.0}"#).expect("write invalid policy config");

    let code = daemon::main(AutobrightOptions {
        config: Some(path),
        tick_ms: Some(10),
        duration_secs: Some(1),
        ..AutobrightOptions::default()
    })
    .await;

    assert_eq!(code, ExitCode::FAILURE);
    assert!(
        logs.contains("failed to resolve autobright configuration"),
        "resolution failure must be logged: {}",
        logs.snapshot()
    );
    assert!(
        logs.contains("invalid brightness policy"),
        "the canonical config error must be preserved: {}",
        logs.snapshot()
    );
    assert!(
        !logs.contains("failed to open BH1750"),
        "hardware must not be touched after a config failure: {}",
        logs.snapshot()
    );
}
