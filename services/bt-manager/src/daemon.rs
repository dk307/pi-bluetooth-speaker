//! Bluetooth manager appliance service logic.
//!
//! A lightweight service analogous to the `core` daemon: it installs `tracing`
//! telemetry, parses a minimal command line (`--help`, `--version`,
//! `--interval`, `--ticks`), runs a periodic Bluetooth status monitor loop,
//! and (by default) hosts the direct-BIP cover-art provider that publishes
//! the active track's artwork on the system D-Bus for future consumers.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::agent::AgentCapabilityMode;
use crate::config::BtManagerConfig;
use speaker_bluetooth::{BluetoothDevice, BluetoothStatus, BtError};
use speaker_common::diag::{
    CliParse, DEFAULT_BT_CONFIG_PATH, env_args, next_arg_value, resolve_cli_opts, run_poll_loop,
};

/// Default status poll interval (5 seconds).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Bluetooth manager service command-line options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtManagerOptions {
    /// Status poll interval between monitor ticks.
    pub interval: Duration,
    /// Optional tick cap for preview/testing; `None` runs the monitor forever.
    pub max_ticks: Option<u64>,
    /// Hosts the D-Bus cover-art provider alongside the monitor loop
    /// (default `true`; host tests disable it so no D-Bus/hardware is touched).
    pub cover_art: bool,
    /// Runs the Bluetooth auto-trust agent for incoming pairing requests
    /// (default `true`; tests disable it so no D-Bus/agent is spawned).
    pub auto_trust: bool,
    /// Pairing agent capability mode override (`None` defaults to config file / `DisplayYesNo`).
    pub capability: Option<AgentCapabilityMode>,
    /// JSON configuration file override path (`None` uses default /etc/bt-manager/config.json).
    pub config: Option<PathBuf>,
    /// Whether `--interval` was explicitly specified on the command line.
    pub explicit_interval: bool,
    /// Optional private D-Bus bus address for auto-trust agent (used by hermetic tests).
    pub auto_trust_bus_address: Option<String>,
}

impl Default for BtManagerOptions {
    fn default() -> Self {
        Self {
            interval: DEFAULT_POLL_INTERVAL,
            max_ticks: None,
            cover_art: true,
            auto_trust: true,
            capability: None,
            config: None,
            explicit_interval: false,
            auto_trust_bus_address: None,
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
    println!("  -c, --config <PATH>         JSON config file (default: {DEFAULT_BT_CONFIG_PATH})");
    println!(
        "  -i, --interval <SECS>       Status poll interval in seconds (default: {})",
        DEFAULT_POLL_INTERVAL.as_secs()
    );
    println!("  -n, --ticks <COUNT>         Exit after COUNT monitor ticks (at least 1)");
    println!("      --no-auto-trust         Disable Bluetooth auto-trust pairing agent");
    println!(
        "      --capability <MODE>     Agent capability ('display-yes-no' or 'no-input-no-output')"
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
            "-c" | "--config" => {
                let val = next_arg_value(&mut iter, "--config")?;
                opts.config = Some(PathBuf::from(val));
            }
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
                opts.explicit_interval = true;
            }
            "-n" | "--ticks" => {
                let val = next_arg_value(&mut iter, "--ticks")?;
                let ticks = parse_ticks(&val)?;
                if ticks == 0 {
                    return Err("invalid tick count '0': omit --ticks to run forever".to_string());
                }
                opts.max_ticks = Some(ticks);
            }
            "--no-auto-trust" => {
                opts.auto_trust = false;
            }
            "--capability" => {
                let val = next_arg_value(&mut iter, "--capability")?;
                let mode = val.parse::<AgentCapabilityMode>()?;
                opts.capability = Some(mode);
            }
            _ => return Err(format!("unknown option '{arg}', use --help for usage")),
        }
    }
    Ok(CliParse::Options(opts))
}

/// Executes one monitor tick.
///
/// Returns the resulting status so callers can react to transitions; transient
/// poll errors are logged as warnings and reported to the caller. Unchanged
/// snapshots log at `debug` only — steady-state ticks would otherwise flood
/// the journal on a long-lived daemon; callers log at `info` on transitions.
///
/// # Errors
/// Propagates [`crate::hardware::BtError`] when the status snapshot fails.
pub fn run_tick(bt: &BluetoothDevice) -> Result<BluetoothStatus, BtError> {
    let status = bt.status()?;
    tracing::debug!(
        connected = status.connected_count(),
        streaming = status.is_streaming(),
        "Bluetooth status tick"
    );
    Ok(status)
}

fn spawn_auto_trust_subsystem(
    opts: &BtManagerOptions,
    capability: AgentCapabilityMode,
) -> Option<crate::agent::AutoTrustShutdown> {
    tracing::info!(
        capability = capability.as_capability_str(),
        "Starting auto-trust agent subsystem"
    );
    let spawn_res = match &opts.auto_trust_bus_address {
        Some(addr) => {
            let addr = addr.clone();
            crate::agent::spawn_auto_trust_with(capability, move || {
                let addr = addr.clone();
                async move {
                    zbus::connection::Builder::address(addr.as_str())?
                        .build()
                        .await
                }
            })
        }
        None => crate::agent::spawn_auto_trust(capability),
    };
    match spawn_res {
        Ok(handle) => Some(handle),
        Err(e) => {
            tracing::error!(error = %e, "auto-trust agent failed to start; continuing without it");
            None
        }
    }
}

fn spawn_cover_art_subsystem(
    bt: &BluetoothDevice,
) -> Option<crate::cover_provider::CoverArtShutdown> {
    match crate::cover_provider::spawn_production(speaker_bluetooth::COVER_ART_CACHE_DIR.into(), bt)
    {
        Ok(handle) => Some(handle),
        Err(e) => {
            tracing::error!(error = %e, "cover-art provider failed to start; continuing without it");
            None
        }
    }
}

/// Runs the Bluetooth manager monitor loop with parsed options.
///
/// Polls [`BluetoothDevice::status`] every `opts.interval`, logging each tick.
/// When `opts.max_ticks` is set, the loop terminates after that many ticks
/// (used by tests and preview runs); otherwise it runs until the process is
/// stopped by systemd.
///
/// Runs the Bluetooth manager application loop.
///
/// With `opts.cover_art` (the default), the direct-BIP cover-art provider is
/// spawned on a dedicated thread; it publishes the active track's artwork on
/// the system D-Bus independently of the monitor loop.
///
/// Attempts to install a SIGINT/SIGTERM handler for graceful shutdown; if a handler
/// is already installed (e.g. in test runners), a warning is logged and the loop proceeds.
#[must_use]
pub fn run_app(bt: &BluetoothDevice, opts: &BtManagerOptions) -> ExitCode {
    let path = BtManagerConfig::resolve_path(opts.config.as_deref());
    let cfg = match BtManagerConfig::load(&path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(error = %e, "failed to load config {}", path.display());
            return ExitCode::FAILURE;
        }
    };

    let effective_interval = if opts.explicit_interval {
        opts.interval
    } else {
        cfg.interval_secs.as_duration()
    };
    let enable_cover_art = opts.cover_art && cfg.cover_art;
    let enable_auto_trust = opts.auto_trust && cfg.auto_trust;
    let effective_capability = opts.capability.unwrap_or(cfg.capability);

    tracing::info!(
        interval_secs = effective_interval.as_secs(),
        cover_art = enable_cover_art,
        auto_trust = enable_auto_trust,
        capability = effective_capability.as_capability_str(),
        "Bluetooth Manager Service running on Raspberry Pi 3 (aarch64)"
    );

    // Set up signal handling for graceful shutdown.
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let shutdown_requested_signal = Arc::clone(&shutdown_requested);
    if let Err(e) = ctrlc::set_handler(move || {
        shutdown_requested_signal.store(true, Ordering::SeqCst);
    }) {
        tracing::warn!(
            error = %e,
            "signal handler already registered or could not be set; graceful shutdown on SIGINT/SIGTERM may be degraded"
        );
    }

    let auto_trust_shutdown = if enable_auto_trust {
        spawn_auto_trust_subsystem(opts, effective_capability)
    } else {
        tracing::info!("Auto-trust agent disabled by configuration");
        None
    };

    let cover_art_shutdown = if enable_cover_art {
        spawn_cover_art_subsystem(bt)
    } else {
        tracing::info!("Cover-art provider disabled by configuration");
        None
    };

    let mut ticks: u64 = 0;
    let mut last_status: Option<(usize, bool)> = None;
    run_poll_loop(effective_interval, |_timestamp| {
        if shutdown_requested.load(Ordering::SeqCst) {
            tracing::info!("shutdown requested, stopping monitor loop");
            return true;
        }
        match run_tick(bt) {
            Ok(status) => {
                let signature = (status.connected_count(), status.is_streaming());
                if last_status != Some(signature) {
                    tracing::info!(
                        connected = signature.0,
                        streaming = signature.1,
                        "Bluetooth status changed"
                    );
                    last_status = Some(signature);
                    if signature.0 > 0
                        && let Some(cover_art) = &cover_art_shutdown
                    {
                        cover_art.wake();
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "Bluetooth status poll failed"),
        }
        ticks = ticks.saturating_add(1);
        opts.max_ticks.is_some_and(|limit| ticks >= limit)
    });

    // Trigger auto-trust agent shutdown if it was started.
    if let Some(handle) = auto_trust_shutdown {
        tracing::info!("shutting down auto-trust agent");
        handle.shutdown();
    }

    // Trigger cover-art provider shutdown if it was started.
    if let Some(handle) = cover_art_shutdown {
        tracing::info!("shutting down cover-art provider");
        handle.shutdown();
        // Give the worker thread a moment to clean up before the process exits.
        std::thread::sleep(Duration::from_millis(500));
    }

    ExitCode::SUCCESS
}

/// CLI main entrypoint for the `bt-manager` binary.
///
/// Installs the global `tracing` subscriber before any other work so startup
/// events are captured; verbosity is controlled by `RUST_LOG` (default `info`,
/// see [`speaker_common::logging`]).
pub fn main() -> ExitCode {
    speaker_common::logging::init_daemon_logging();
    let opts = match resolve_cli_opts(parse_cli_args_from(env_args()), "bt-manager", print_usage) {
        Ok(opts) => opts,
        Err(exit_code) => return exit_code,
    };
    let bt = match BluetoothDevice::new() {
        Ok(bt) => bt,
        Err(e) => {
            tracing::error!(error = %e, "failed to connect to the system D-Bus");
            return ExitCode::FAILURE;
        }
    };
    run_app(&bt, &opts)
}
