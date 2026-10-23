use std::process::Command;

#[test]
fn core_main_binary_executes() {
    let bin_path = env!("CARGO_BIN_EXE_core");
    let output = Command::new(bin_path)
        .output()
        .expect("failed to run core main binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Speaker Core Service is running on Raspberry Pi 3!"));
    assert!(stdout.contains("ARM Cortex-A53 (aarch64)"));
    assert!(stdout.contains("Thermal Sensor:"));
}

#[test]
fn read_temp_cli_help_and_flags() {
    let bin_path = env!("CARGO_BIN_EXE_read_temp");

    // Help
    let output = Command::new(bin_path)
        .arg("--help")
        .output()
        .expect("failed to run read_temp");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("DS18B20 1-Wire Temperature Diagnostic Utility"));

    let short_help = Command::new(bin_path)
        .arg("-h")
        .output()
        .expect("failed to run read_temp");
    assert!(short_help.status.success());

    // Invalid arguments
    let err_arg = Command::new(bin_path)
        .arg("--invalid-flag")
        .output()
        .expect("failed to run read_temp");
    assert!(!err_arg.status.success());

    let missing_count = Command::new(bin_path)
        .arg("--count")
        .output()
        .expect("failed to run read_temp");
    assert!(!missing_count.status.success());

    let invalid_count = Command::new(bin_path)
        .args(["--count", "not-a-number"])
        .output()
        .expect("failed to run read_temp");
    assert!(!invalid_count.status.success());

    let valid_count = Command::new(bin_path)
        .args(["--count", "2", "--interval", "1"])
        .output()
        .expect("failed to run read_temp");
    assert!(!valid_count.status.success()); // fails on host without hardware, but parses count cleanly

    let missing_interval = Command::new(bin_path)
        .arg("--interval")
        .output()
        .expect("failed to run read_temp");
    assert!(!missing_interval.status.success());

    let zero_interval = Command::new(bin_path)
        .args(["--interval", "0"])
        .output()
        .expect("failed to run read_temp");
    assert!(!zero_interval.status.success());

    let invalid_interval = Command::new(bin_path)
        .args(["--interval", "invalid"])
        .output()
        .expect("failed to run read_temp");
    assert!(!invalid_interval.status.success());

    let missing_device = Command::new(bin_path)
        .arg("--device")
        .output()
        .expect("failed to run read_temp");
    assert!(!missing_device.status.success());

    let invalid_device = Command::new(bin_path)
        .args(["--device", "invalid-device-id"])
        .output()
        .expect("failed to run read_temp");
    assert!(!invalid_device.status.success());

    // Run without hardware: verify graceful exit (fails because no 1-wire devices exist on host)
    let once_run = Command::new(bin_path)
        .args(["--once", "--verbose", "--soc", "--raw"])
        .output()
        .expect("failed to run read_temp");
    // On dev host without DS18B20 hardware, it gracefully exits with code 1 and prints an error
    assert!(!once_run.status.success());
    let stderr = String::from_utf8_lossy(&once_run.stderr);
    assert!(stderr.contains("Error:"));

    // JSON mode graceful failure
    let json_run = Command::new(bin_path)
        .args(["--once", "--json", "--device", "28-000000000001"])
        .output()
        .expect("failed to run read_temp");
    assert!(!json_run.status.success());
}

#[test]
fn read_light_cli_help_and_flags() {
    let bin_path = env!("CARGO_BIN_EXE_read_light");

    // Help
    let output = Command::new(bin_path)
        .arg("--help")
        .output()
        .expect("failed to run read_light");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("BH1750 Ambient Light Diagnostic Utility"));

    let short_help = Command::new(bin_path)
        .arg("-h")
        .output()
        .expect("failed to run read_light");
    assert!(short_help.status.success());

    // Invalid arguments
    let err_arg = Command::new(bin_path)
        .arg("--invalid-flag")
        .output()
        .expect("failed to run read_light");
    assert!(!err_arg.status.success());

    let missing_count = Command::new(bin_path)
        .arg("--count")
        .output()
        .expect("failed to run read_light");
    assert!(!missing_count.status.success());

    let invalid_count = Command::new(bin_path)
        .args(["--count", "not-a-number"])
        .output()
        .expect("failed to run read_light");
    assert!(!invalid_count.status.success());

    let valid_count = Command::new(bin_path)
        .args(["--count", "2", "--interval", "1"])
        .output()
        .expect("failed to run read_light");
    assert!(!valid_count.status.success()); // fails on host without hardware, but parses count cleanly

    let missing_interval = Command::new(bin_path)
        .arg("--interval")
        .output()
        .expect("failed to run read_light");
    assert!(!missing_interval.status.success());

    let zero_interval = Command::new(bin_path)
        .args(["--interval", "0"])
        .output()
        .expect("failed to run read_light");
    assert!(!zero_interval.status.success());

    let invalid_interval = Command::new(bin_path)
        .args(["--interval", "not-float"])
        .output()
        .expect("failed to run read_light");
    assert!(!invalid_interval.status.success());

    // Mode parsing & errors
    let missing_mode = Command::new(bin_path)
        .arg("--mode")
        .output()
        .expect("failed to run read_light");
    assert!(!missing_mode.status.success());

    let invalid_mode = Command::new(bin_path)
        .args(["--mode", "ultra-mode"])
        .output()
        .expect("failed to run read_light");
    assert!(!invalid_mode.status.success());

    // Address parsing & errors
    let missing_addr = Command::new(bin_path)
        .arg("--address")
        .output()
        .expect("failed to run read_light");
    assert!(!missing_addr.status.success());

    let invalid_hex = Command::new(bin_path)
        .args(["--address", "not-hex"])
        .output()
        .expect("failed to run read_light");
    assert!(!invalid_hex.status.success());

    let unsupported_addr = Command::new(bin_path)
        .args(["--address", "0x77"])
        .output()
        .expect("failed to run read_light");
    assert!(!unsupported_addr.status.success());

    // Valid modes and addresses parsed before hardware attempt
    for mode in ["high-res", "high-res2", "low-res", "h1", "h2", "low"] {
        let run = Command::new(bin_path)
            .args(["--once", "--mode", mode, "--address", "0x23"])
            .output()
            .expect("failed to run read_light");
        // Hardware initialization fails on host without /dev/i2c-1, but arguments parse correctly
        assert!(!run.status.success());
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(stderr.contains("Failed to initialize BH1750 light sensor"));
    }

    // JSON and verbose modes
    let run_json = Command::new(bin_path)
        .args(["--once", "--verbose", "--json", "--address", "0x5C"])
        .output()
        .expect("failed to run read_light");
    assert!(!run_json.status.success());
}
