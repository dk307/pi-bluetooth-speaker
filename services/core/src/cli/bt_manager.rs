//! Bluetooth manager appliance service logic.
//!
//! A lightweight service template analogous to the `core` daemon: it installs
//! `tracing` telemetry, parses a minimal command line (`--help`, `--version`,
//! `--interval`, `--ticks`), and runs a periodic Bluetooth status monitor loop
//! that keeps the adapter state observable under systemd/journald.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::diag::{CliParse, env_args, next_arg_value, resolve_cli_opts, run_poll_loop};
use crate::hardware::bluetooth::bip_direct::{
    BipChannel, BipImageType, RfcommBipChannel, fetch_cover_art_via, probe_session,
};
use crate::hardware::{BluetoothDevice, BtAddress};

/// Default status poll interval (5 seconds).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Parameters of a one-shot direct BIP cover-art probe run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BipProbeOptions {
    /// MAC address of the remote device to probe (`AA:BB:CC:DD:EE:FF`).
    pub address: Box<str>,
    /// Optional BIP image handle; when present a full GET is performed,
    /// otherwise only SDP + RFCOMM + OBEX CONNECT/DISCONNECT are exercised.
    pub handle: Option<Box<str>>,
}

/// Bluetooth manager service command-line options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtManagerOptions {
    /// Status poll interval between monitor ticks.
    pub interval: Duration,
    /// Optional tick cap for preview/testing; `None` runs the monitor forever.
    pub max_ticks: Option<u64>,
    /// When set, run a one-shot direct BIP cover-art probe instead of the
    /// monitor loop.
    pub bip_probe: Option<BipProbeOptions>,
}

impl Default for BtManagerOptions {
    fn default() -> Self {
        Self {
            interval: DEFAULT_POLL_INTERVAL,
            max_ticks: None,
            bip_probe: None,
        }
    }
}

/// Prints usage text for the Bluetooth manager service executable.
pub fn print_usage(bin_name: &str) {
    println!("DietPi Bluetooth Manager Service");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!(
        "  -i, --interval <SECS>       Status poll interval in seconds (default: {})",
        DEFAULT_POLL_INTERVAL.as_secs()
    );
    println!("  -n, --ticks <COUNT>         Exit after COUNT monitor ticks (at least 1)");
    println!(
        "  --bip-probe <MAC>           Probe the direct BIP cover-art transport for MAC and exit"
    );
    println!(
        "  --bip-handle <HANDLE>       With --bip-probe: also pull image HANDLE via a BIP GET"
    );
    println!("  -V, --version               Print version and exit");
    println!("  -h, --help                  Print this help text and exit");
}

/// Parses and validates a `--interval <SECS>` argument value (unsigned seconds).
///
/// # Errors
/// Returns a descriptive user-facing error string if `val` is missing, carries a
/// sign prefix, or is not a valid unsigned integer.
pub fn parse_interval_secs(val: &str) -> Result<u64, String> {
    if val.starts_with('+') || val.starts_with('-') {
        return Err(format!(
            "invalid interval seconds '{val}': sign prefixes are not accepted"
        ));
    }
    val.parse::<u64>()
        .map_err(|e| format!("invalid interval seconds '{val}': {e}"))
}

/// Parses and validates a `--ticks <COUNT>` argument value (unsigned count).
///
/// # Errors
/// Returns a descriptive user-facing error string if `val` is missing, carries a
/// sign prefix, or is not a valid unsigned integer.
pub fn parse_ticks(val: &str) -> Result<u64, String> {
    if val.starts_with('+') || val.starts_with('-') {
        return Err(format!(
            "invalid tick count '{val}': sign prefixes are not accepted"
        ));
    }
    val.parse::<u64>()
        .map_err(|e| format!("invalid tick count '{val}': {e}"))
}

/// Parses command-line arguments into structured [`BtManagerOptions`].
///
/// # Errors
/// Returns an error message if an argument or flag value is malformed.
pub fn parse_cli_args_from<I>(args: I) -> Result<CliParse<BtManagerOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter();
    let mut opts = BtManagerOptions::default();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliParse::Help),
            "-V" | "--version" => return Ok(CliParse::Version),
            "-i" | "--interval" => {
                let val = next_arg_value(&mut iter, "--interval")?;
                let secs = parse_interval_secs(&val)?;
                if secs == 0 {
                    return Err(
                        "invalid interval seconds '0': interval must be at least 1 second"
                            .to_string(),
                    );
                }
                opts.interval = Duration::from_secs(secs);
            }
            "-n" | "--ticks" => {
                let val = next_arg_value(&mut iter, "--ticks")?;
                let ticks = parse_ticks(&val)?;
                if ticks == 0 {
                    return Err("invalid tick count '0': omit --ticks to run forever".to_string());
                }
                opts.max_ticks = Some(ticks);
            }
            "--bip-probe" => {
                let val = next_arg_value(&mut iter, "--bip-probe")?;
                if opts.bip_probe.is_some() {
                    return Err("--bip-probe given more than once".to_string());
                }
                opts.bip_probe = Some(BipProbeOptions {
                    address: val.into(),
                    handle: None,
                });
            }
            "--bip-handle" => {
                let val = next_arg_value(&mut iter, "--bip-handle")?;
                let Some(probe) = opts.bip_probe.as_mut() else {
                    return Err("--bip-handle requires --bip-probe <MAC>".to_string());
                };
                if probe.handle.is_some() {
                    return Err("--bip-handle given more than once".to_string());
                }
                if val.is_empty() {
                    return Err("invalid image handle '': the handle must not be empty".to_string());
                }
                probe.handle = Some(val.into());
            }
            _ => return Err(format!("unknown option '{arg}', use --help for usage")),
        }
    }
    Ok(CliParse::Options(opts))
}

/// Executes one monitor tick, logging the current Bluetooth status snapshot.
///
/// Returns the resulting status so callers can react to transitions; transient
/// poll errors are logged as warnings and reported to the caller.
///
/// # Errors
/// Propagates [`crate::hardware::BtError`] when the status snapshot fails.
pub fn run_tick(
    bt: &BluetoothDevice,
) -> Result<crate::hardware::BluetoothStatus, crate::hardware::BtError> {
    let status = bt.status()?;
    tracing::info!(
        connected = status.connected_count(),
        streaming = status.is_streaming(),
        "Bluetooth status tick"
    );
    Ok(status)
}

/// Runs the Bluetooth manager monitor loop with parsed options.
///
/// Polls [`BluetoothDevice::status`] every `opts.interval`, logging each tick.
/// When `opts.max_ticks` is set, the loop terminates after that many ticks
/// (used by tests and preview runs); otherwise it runs until the process is
/// stopped by systemd.
#[must_use]
pub fn run_app(bt: &BluetoothDevice, opts: &BtManagerOptions) -> ExitCode {
    tracing::info!(
        interval_secs = opts.interval.as_secs(),
        "Bluetooth Manager Service running on Raspberry Pi 3 (aarch64)"
    );
    let mut ticks: u64 = 0;
    run_poll_loop(opts.interval, |_timestamp| {
        if let Err(e) = run_tick(bt) {
            tracing::warn!(error = %e, "Bluetooth status poll failed");
        }
        ticks = ticks.saturating_add(1);
        opts.max_ticks.is_some_and(|limit| ticks >= limit)
    });
    ExitCode::SUCCESS
}

/// Runs a one-shot direct BIP cover-art probe against `opts.address`.
///
/// Resolves the BIP Image Pull service via SDP, opens the RFCOMM transport,
/// and establishes an OBEX session against the AVRCP cover-art target; with
/// `opts.handle` a full BIP GET pulls the image bytes as well. Every step is
/// logged (see `bip_direct`) so live runs on the appliance are diagnosable
/// from journald alone.
///
/// # Errors
/// Logged and reported via [`ExitCode::FAILURE`] on any failure.
pub fn run_bip_probe(channel: &dyn BipChannel, opts: &BipProbeOptions) -> ExitCode {
    tracing::info!(
        address = opts.address.as_ref(),
        handle = opts.handle.as_deref(),
        "BIP probe starting"
    );
    let address = match BtAddress::parse_str(opts.address.as_ref()) {
        Ok(address) => address,
        Err(e) => {
            tracing::error!("invalid --bip-probe address: {e}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(error = %e, "failed to start the async runtime for the BIP probe");
            return ExitCode::FAILURE;
        }
    };
    let started = Instant::now();
    let result = runtime.block_on(async {
        if let Some(handle) = opts.handle.as_deref() {
            fetch_cover_art_via(channel, &address, handle, BipImageType::Native)
                .await
                .map(|body| body.len())
        } else {
            let stream = channel.open(&address).await?;
            probe_session(stream).await?;
            Ok(0)
        }
    });
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(bytes) => {
            tracing::info!(elapsed_ms, bytes, "BIP probe finished");
            println!("BIP probe OK: {elapsed_ms} ms, {bytes} bytes");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e.summary(), elapsed_ms, "BIP probe failed");
            eprintln!("BIP probe failed: {}", e.summary());
            ExitCode::FAILURE
        }
    }
}

/// CLI main entrypoint for the `bt-manager` binary.
///
/// Installs the global `tracing` subscriber before any other work so startup
/// events are captured; verbosity is controlled by `RUST_LOG` (default `info`,
/// see [`crate::logging`]).
pub fn main() -> ExitCode {
    crate::logging::init_daemon_logging();
    let opts = match resolve_cli_opts(parse_cli_args_from(env_args()), "bt-manager", print_usage) {
        Ok(opts) => opts,
        Err(exit_code) => return exit_code,
    };
    if let Some(probe) = opts.bip_probe {
        return run_bip_probe(&RfcommBipChannel, &probe);
    }
    let bt = match BluetoothDevice::new() {
        Ok(bt) => bt,
        Err(e) => {
            tracing::error!(error = %e, "failed to connect to the system D-Bus");
            return ExitCode::FAILURE;
        }
    };
    run_app(&bt, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::bluetooth::bip_direct::BipError;

    /// Zero-configuration OBEX responder: accepts CONNECT and DISCONNECT with
    /// the trait's default `Success` responses.
    struct ProbeResponder;

    impl obex_protocol::tokio::Handler for ProbeResponder {}

    /// Fake [`BipChannel`] whose stream is answered by a live in-process OBEX
    /// responder, exercising the full probe path without physical hardware.
    struct ProbeFakeChannel;

    #[async_trait::async_trait]
    impl BipChannel for ProbeFakeChannel {
        async fn open(
            &self,
            _address: &BtAddress,
        ) -> Result<crate::hardware::bluetooth::bip_direct::BipStream, BipError> {
            let (client_half, server_half) = tokio::io::duplex(4_096);
            tokio::spawn(async move {
                let mut server = obex_protocol::tokio::Server::new(server_half, ProbeResponder);
                let _ = server.run().await;
            });
            Ok(Box::new(client_half))
        }
    }

    /// Fake [`BipChannel`] whose transport open always fails.
    struct FailingProbeChannel;

    #[async_trait::async_trait]
    impl BipChannel for FailingProbeChannel {
        async fn open(
            &self,
            _address: &BtAddress,
        ) -> Result<crate::hardware::bluetooth::bip_direct::BipStream, BipError> {
            Err(BipError::service_not_found("11:22:33:44:55:66"))
        }
    }

    fn probe_opts(address: &str, handle: Option<&str>) -> BipProbeOptions {
        BipProbeOptions {
            address: address.into(),
            handle: handle.map(Box::from),
        }
    }

    #[test]
    fn defaults_match_documented_values() {
        let opts = BtManagerOptions::default();
        assert_eq!(opts.interval, Duration::from_secs(5));
        assert_eq!(opts.max_ticks, None);
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
    fn run_app_terminates_on_tick_cap_with_mock() {
        let (bt, _mock) = BluetoothDevice::new_mocked();
        let opts = BtManagerOptions {
            interval: Duration::from_millis(1),
            max_ticks: Some(3),
            bip_probe: None,
        };
        assert_eq!(run_app(&bt, &opts), ExitCode::SUCCESS);
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
        let status = run_tick(&bt).expect("mock status");
        // The mock initializes with one default connected peer.
        assert_eq!(status.connected_count(), 1);
    }

    #[test]
    fn cli_parses_bip_probe_flags() {
        let args = [
            "--bip-probe",
            "11:22:33:44:55:66",
            "--bip-handle",
            "0000001",
        ];
        let parsed = parse_cli_args_from(args.iter().map(ToString::to_string)).expect("valid");
        let opts = match parsed {
            CliParse::Options(opts) => opts,
            other => panic!("expected options, got {other:?}"),
        };
        assert_eq!(
            opts.bip_probe,
            Some(probe_opts("11:22:33:44:55:66", Some("0000001")))
        );
    }

    #[test]
    fn cli_bip_handle_requires_probe() {
        let args = ["--bip-handle", "0000001"];
        let err = parse_cli_args_from(args.iter().map(ToString::to_string)).expect_err("reject");
        assert!(err.contains("requires --bip-probe"), "got: {err}");
        assert!(!err.contains("Backtrace"), "user-facing error stays clean");
    }

    #[test]
    fn cli_rejects_empty_handle_and_duplicate_flags() {
        let args = ["--bip-probe", "11:22:33:44:55:66", "--bip-handle", ""];
        let err = parse_cli_args_from(args.iter().map(ToString::to_string)).expect_err("reject");
        assert!(err.contains("must not be empty"), "got: {err}");
        let args = [
            "--bip-probe",
            "11:22:33:44:55:66",
            "--bip-handle",
            "1",
            "--bip-handle",
            "2",
        ];
        let err = parse_cli_args_from(args.iter().map(ToString::to_string)).expect_err("reject");
        assert!(err.contains("more than once"), "got: {err}");
    }

    #[test]
    fn run_bip_probe_succeeds_against_in_process_responder() {
        assert_eq!(
            run_bip_probe(&ProbeFakeChannel, &probe_opts("11:22:33:44:55:66", None)),
            ExitCode::SUCCESS
        );
    }

    #[test]
    fn run_bip_probe_reports_transport_failure() {
        assert_eq!(
            run_bip_probe(&FailingProbeChannel, &probe_opts("11:22:33:44:55:66", None)),
            ExitCode::FAILURE
        );
    }

    #[test]
    fn run_bip_probe_rejects_invalid_address() {
        assert_eq!(
            run_bip_probe(&ProbeFakeChannel, &probe_opts("not-a-mac", None)),
            ExitCode::FAILURE
        );
    }
}
