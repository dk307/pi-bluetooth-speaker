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
use futures_util::StreamExt as _;
use speaker_bluetooth::{BluetoothDevice, BluetoothStatus, BtError};
use speaker_common::diag::{
    CliParse, DEFAULT_BT_CONFIG_PATH, env_args, next_arg_value, resolve_cli_opts,
};
use std::collections::HashMap;
use tokio::time::Duration as TokioDuration;
use zbus::{Connection, MatchRule, MessageStream, Proxy};

/// Default status poll interval (5 seconds).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Syncs volume between `BlueALSA` PCM (mirrors transport) and audio-manager D-Bus service.
async fn sync_volume(
    bt: &BluetoothDevice,
    conn: &Connection,
    last_pcm_vol: &mut Option<u8>,
) -> Result<(), BtError> {
    // Read BlueALSA PCM volume (mirrors BlueZ transport volume)
    let pcm_vol = match bt.audio_pcms_async().await {
        Ok(pcms) => pcms
            .iter()
            .find(|p| p.pcm_path.contains("a2dpsnk"))
            .and_then(|p| p.volume_percent),
        Err(e) => {
            tracing::debug!(error = %e, "Failed to read BlueALSA PCMs for sync");
            None
        }
    };

    if let Some(pcm_vol) = pcm_vol {
        // Only sync if PCM volume changed
        if *last_pcm_vol == Some(pcm_vol) {
            tracing::debug!(pcm_volume = pcm_vol, "Volume sync skipped: no change");
            return Ok(());
        }

        // `volume_percent` is already 0..=100 (avg of per-channel levels);
        // forward it directly — do NOT re-map through the AVRCP 0..127 table.
        let percent = pcm_vol;

        // Call audio-manager SetLevel via D-Bus
        let proxy = Proxy::new(
            conn,
            "org.speaker.AudioManager",
            "/org/speaker/amp",
            "org.speaker.AudioManager1",
        )
        .await?;

        proxy.call_method("SetLevel", &percent).await?;

        tracing::info!(
            pcm_volume = pcm_vol,
            percent,
            "Volume sync: BlueALSA PCM -> audio-manager"
        );
        *last_pcm_vol = Some(pcm_vol);
    } else {
        // No active PCM
        tracing::debug!("No active PCM for volume sync");
    }

    Ok(())
}

/// Bluetooth manager service command-line options..
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
pub async fn run_tick(bt: &BluetoothDevice) -> Result<BluetoothStatus, BtError> {
    let status = bt.status_async().await?;
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
// Orchestrates subsystem boot, signal wiring, and the poll loop; splitting it
// would obscure the startup sequence that must stay observable in one place.
#[allow(
    clippy::too_many_lines,
    reason = "single orchestration point for subsystem boot"
)]
/// # Panics
/// Panics if a temporary single-thread runtime cannot be built for the
/// blocking status-poll fallback (unrecoverable runtime misconfiguration).
///
/// # Errors
/// Returns [`ExitCode::FAILURE`] only when CLI/D-Bus bootstrap fails; poll
/// and subsystem errors are logged and retried in-loop.
#[must_use]
pub async fn run_app(bt: BluetoothDevice, opts: BtManagerOptions) -> ExitCode {
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
        spawn_auto_trust_subsystem(&opts, effective_capability)
    } else {
        tracing::info!("Auto-trust agent disabled by configuration");
        None
    };

    let cover_art_shutdown = if enable_cover_art {
        spawn_cover_art_subsystem(&bt)
    } else {
        tracing::info!("Cover-art provider disabled by configuration");
        None
    };

    let mut ticks: u64 = 0;
    let mut last_status: Option<(usize, bool)> = None;

    // Get D-Bus connection for volume sync
    let conn = match Connection::system().await {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to connect to system D-Bus for volume sync; volume sync disabled");
            None
        }
    };

    if let Some(conn) = &conn {
        let bt_for_sync = bt.clone();
        let conn_for_sync = conn.clone();
        tokio::spawn(spawn_volume_sync_task(bt_for_sync, conn_for_sync));
    }

    // Event-driven monitor: poll once immediately (so startup state is logged
    // without waiting for a signal or keep-alive), then block on the shared
    // property-change flag with `effective_interval` as the keep-alive
    // timeout. The timeout equals the legacy poll interval, so worst-case
    // tick cadence, `--ticks` accounting and shutdown latency are identical
    // to the previous fixed-interval loop; signals only make updates faster.
    loop {
        if shutdown_requested.load(Ordering::SeqCst) {
            tracing::info!("shutdown requested, stopping monitor loop");
            break;
        }
        match run_tick(&bt).await {
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
        if opts.max_ticks.is_some_and(|limit| ticks >= limit) {
            break;
        }

        // Wait for a property change (bounded by the keep-alive timeout);
        // the blocking wait runs on the blocking pool to avoid stalling the
        // async runtime.
        let bt_for_wait = bt.clone();
        let wait_result = speaker_common::runtime::run_blocking(
            "bluetooth property wait",
            move || -> Result<bool, BtError> {
                Ok(bt_for_wait.wait_for_property_change(effective_interval))
            },
        )
        .await;
        // A keep-alive timeout (`false`) or signaled change (`true`) are both
        // fine; only a cancelled/poisoned blocking task is worth a debug note.
        if let Err(e) = wait_result {
            tracing::debug!(error = %e, "Property-change wait task failed");
        }
    }

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

/// Predicate: does this D-Bus `PropertiesChanged` signal announce a `BlueALSA`
/// A2DP PCM volume change?
///
/// Pure and host-testable; path/interface/key parsing never touches D-Bus.
/// `pub` under `test-util` so the integration suite can exercise it; the bin
/// target never calls it, hence the scoped `dead_code` allowance.
#[cfg_attr(not(feature = "test-util"), allow(dead_code))]
pub fn is_bluealsa_volume_signal<'a>(
    path: Option<&'a str>,
    interface: &str,
    mut changed_keys: impl Iterator<Item = &'a str>,
) -> bool {
    path.is_some_and(|p| p.contains("a2dpsnk"))
        && interface == "org.bluealsa.PCM1"
        && changed_keys.any(|k| k == "Volume")
}

/// Predicate: does this D-Bus `PropertiesChanged` signal announce an
/// audio-manager amp level/mute change?
///
/// Pure and host-testable; path/interface/key parsing never touches D-Bus.
/// `pub` under `test-util` so the integration suite can exercise it; the bin
/// target never calls it, hence the scoped `dead_code` allowance.
#[cfg_attr(not(feature = "test-util"), allow(dead_code))]
pub fn is_audio_manager_amp_signal<'a>(
    interface: &str,
    mut changed_keys: impl Iterator<Item = &'a str>,
) -> bool {
    interface == "org.speaker.AudioManager1" && changed_keys.any(|k| k == "Level" || k == "Muted")
}

/// Runs one sync attempt, logging failures at debug level (volume sync is
/// best-effort; the next signal retries).
async fn sync_volume_logged(
    bt: &BluetoothDevice,
    conn: &Connection,
    last_pcm_vol: &mut Option<u8>,
) {
    if let Err(e) = sync_volume(bt, conn, last_pcm_vol).await {
        tracing::debug!(error = %e, "Volume sync failed");
    }
}

/// Extracts the per-channel volume byte array from a D-Bus property value.
///
/// Pure and host-testable; no D-Bus round-trips.
#[cfg_attr(not(feature = "test-util"), allow(dead_code))]
#[must_use]
pub fn volume_bytes_from_value(value: &zbus::zvariant::Value<'_>) -> Option<Vec<u8>> {
    match value {
        zbus::zvariant::Value::Array(arr) => arr
            .iter()
            .map(|v| match v {
                zbus::zvariant::Value::U8(b) => Some(*b),
                _ => None,
            })
            .collect(),
        zbus::zvariant::Value::U8(b) => Some(vec![*b]),
        _ => None,
    }
}

/// Applies one `BlueALSA` PCM volume (0..=100 percent) to audio-manager,
/// deduplicated against the last applied value.
async fn apply_pcm_volume(
    conn: &Connection,
    last_pcm_vol: &mut Option<u8>,
    pcm_vol: u8,
) -> Result<(), BtError> {
    if *last_pcm_vol == Some(pcm_vol) {
        tracing::debug!(pcm_volume = pcm_vol, "Volume sync skipped: no change");
        return Ok(());
    }

    // `percent` is already 0..=100; forward directly — do NOT re-map through
    // the AVRCP 0..127 table.
    let proxy = Proxy::new(
        conn,
        "org.speaker.AudioManager",
        "/org/speaker/amp",
        "org.speaker.AudioManager1",
    )
    .await?;
    proxy.call_method("SetLevel", &pcm_vol).await?;

    tracing::info!(
        pcm_volume = pcm_vol,
        percent = pcm_vol,
        "Volume sync: BlueALSA PCM -> audio-manager"
    );
    *last_pcm_vol = Some(pcm_vol);
    Ok(())
}

/// Fully event-driven volume sync task — no timers, no polling.
///
/// Subscribes to `BlueALSA` D-Bus signals only:
/// - `PropertiesChanged(Volume)` on a `PCM1` object → the new volume is parsed
///   straight from the signal payload (zero extra D-Bus round-trips) and pushed
///   to audio-manager, so rapid phone volume presses take effect immediately.
/// - `InterfacesAdded` on the `BlueALSA` `ObjectManager` → a new `A2DP` PCM appeared
///   (fresh connect); one full catch-up read syncs the current volume.
///
/// A one-shot catch-up runs at startup. The task never exits: it is terminated
/// with the process when the async runtime is dropped, so no shutdown flag or
/// timer is needed. If the signal stream dies, the reconnect loop rebuilds the
/// match rule after a backoff (no hot-spin).
// Selects over two signal streams; each arm shares the same reconnect/backoff
// policy, so extracting sub-functions would just thread four mutable state
// parameters through both.
#[allow(
    clippy::too_many_lines,
    reason = "single event loop for two signal streams"
)]
async fn spawn_volume_sync_task(bt: BluetoothDevice, conn: Connection) {
    let mut last_pcm_vol: Option<u8> = None;
    // Reverse direction dedup (audio-manager -> BlueALSA): the last level /
    // mute state this task pushed to BlueALSA itself. Dampens the echo loop
    // (our own push round-trips back through BlueALSA's Volume signal and the
    // forward arm's SetLevel, both of which converge without a third write).
    let mut last_pushed_level: Option<u8> = None;
    let mut last_pushed_muted: Option<bool> = None;

    let props_rule = match MatchRule::builder()
        .sender("org.bluealsa")
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .map(zbus::match_rule::Builder::build)
    {
        Ok(rule) => rule,
        Err(e) => {
            tracing::error!(error = %e, "Failed to build BlueALSA signal match rule");
            return;
        }
    };
    let interfaces_rule = match MatchRule::builder()
        .sender("org.bluealsa")
        .and_then(|b| b.interface("org.freedesktop.DBus.ObjectManager"))
        .and_then(|b| b.member("InterfacesAdded"))
        .map(zbus::match_rule::Builder::build)
    {
        Ok(rule) => rule,
        Err(e) => {
            tracing::error!(error = %e, "Failed to build BlueALSA ObjectManager match rule");
            return;
        }
    };

    let amp_rule = match MatchRule::builder()
        .sender("org.speaker.AudioManager")
        .and_then(|b| b.path("/org/speaker/amp"))
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .map(zbus::match_rule::Builder::build)
    {
        Ok(rule) => rule,
        Err(e) => {
            tracing::error!(error = %e, "Failed to build audio-manager signal match rule");
            return;
        }
    };

    // Signal-Listener Reconnect Loop: if a stream dies or setup fails,
    // rebuild it from scratch after a backoff instead of hot-spinning.
    loop {
        let (mut props_stream, mut iface_stream, mut amp_stream) =
            match MessageStream::for_match_rule(props_rule.clone(), &conn, Some(64)).await {
                Ok(props) => {
                    match MessageStream::for_match_rule(interfaces_rule.clone(), &conn, Some(8))
                        .await
                    {
                        Ok(interfaces) => {
                            match MessageStream::for_match_rule(amp_rule.clone(), &conn, Some(8))
                                .await
                            {
                                Ok(amp) => (props, interfaces, amp),
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "Failed to subscribe to audio-manager PropertiesChanged"
                                    );
                                    tokio::time::sleep(TokioDuration::from_millis(500)).await;
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to subscribe to BlueALSA InterfacesAdded");
                            tokio::time::sleep(TokioDuration::from_millis(500)).await;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to subscribe to BlueALSA PropertiesChanged");
                    tokio::time::sleep(TokioDuration::from_millis(500)).await;
                    continue;
                }
            };

        // One-shot catch-up: apply the current volume on startup/reconnect.
        sync_volume_logged(&bt, &conn, &mut last_pcm_vol).await;

        // Reverse-direction catch-up: the amp level may have changed (amp-cli,
        // thermal offset) while this task was down; seed the dedup cache from
        // audio-manager's current state and push it to BlueALSA once.
        match Proxy::new(
            &conn,
            "org.speaker.AudioManager",
            "/org/speaker/amp",
            "org.speaker.AudioManager1",
        )
        .await
        {
            Ok(proxy) => {
                let level: Result<u8, _> = proxy.get_property("Level").await;
                let muted: Result<bool, _> = proxy.get_property("Muted").await;
                let level = level.ok();
                let muted = muted.ok();
                last_pushed_level = level;
                last_pushed_muted = muted;
                if let Some(level) = level {
                    let bt_for_push = bt.clone();
                    match tokio::task::spawn_blocking(move || bt_for_push.set_volume(level)).await {
                        Ok(Ok(())) => {
                            tracing::info!(level, "Volume catch-up: audio-manager -> BlueALSA");
                        }
                        Ok(Err(e)) => {
                            tracing::debug!(error = %e, "Reverse volume catch-up failed");
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "Reverse volume catch-up task panicked");
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "audio-manager not reachable for reverse catch-up");
            }
        }

        loop {
            tokio::select! {
                signal = props_stream.next() => match signal {
                    Some(Ok(signal)) => {
                        let path = signal.header().path().map(std::string::ToString::to_string);
                        let body = signal.body();
                        let parsed = body.deserialize::<(
                            Box<str>,
                            HashMap<String, zbus::zvariant::Value<'_>>,
                            Vec<String>,
                        )>();
                        let Ok((iface, props, _invalidated)) = parsed else {
                            continue;
                        };
                        if !is_bluealsa_volume_signal(
                            path.as_deref(),
                            &iface,
                            props.keys().map(String::as_str),
                        ) {
                            continue;
                        }
                        // Parse the new volume directly from the payload —
                        // no GetManagedObjects round-trip.
                        let Some(pcm_vol) = props
                            .get("Volume")
                            .and_then(volume_bytes_from_value)
                            .as_deref()
                            .and_then(|bytes| {
                                speaker_bluetooth::format_volume_bytes(bytes, None).2
                            })
                        else {
                            tracing::debug!("Volume property present but unparseable");
                            continue;
                        };
                        if let Err(e) = apply_pcm_volume(&conn, &mut last_pcm_vol, pcm_vol).await {
                            tracing::debug!(error = %e, "Volume sync failed");
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "BlueALSA volume signal stream error; reconnecting");
                        break;
                    }
                    None => {
                        tracing::warn!("BlueALSA volume signal stream closed; reconnecting");
                        break;
                    }
                },
                signal = amp_stream.next() => match signal {
                    Some(Ok(signal)) => {
                        // audio-manager published a new Level/Muted (speaker-side
                        // volume change, e.g. amp-cli or the controller's thermal
                        // offset): push it back to BlueALSA so the phone's AVRCP
                        // absolute volume follows the speaker.
                        let body = signal.body();
                        let parsed = body.deserialize::<(
                            Box<str>,
                            HashMap<String, zbus::zvariant::Value<'_>>,
                            Vec<String>,
                        )>();
                        let Ok((iface, props, _invalidated)) = parsed else {
                            continue;
                        };
                        if !is_audio_manager_amp_signal(
                            &iface,
                            props.keys().map(String::as_str),
                        ) {
                            continue;
                        }
                        if let Some(level) = props.get("Level").and_then(|v| v.try_into().ok())
                            && last_pushed_level != Some(level) {
                            last_pushed_level = Some(level);
                            let bt_for_push = bt.clone();
                            match tokio::task::spawn_blocking(move || bt_for_push.set_volume(level))
                                .await
                            {
                                Ok(Ok(())) => {
                                    tracing::info!(
                                        level,
                                        "Volume sync: audio-manager -> BlueALSA"
                                    );
                                }
                                Ok(Err(e)) => {
                                    tracing::debug!(error = %e, "Reverse volume sync failed");
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, "Reverse volume sync task panicked");
                                }
                            }
                        }
                        if let Some(muted) = props.get("Muted").and_then(|v| v.try_into().ok())
                            && last_pushed_muted != Some(muted) {
                            last_pushed_muted = Some(muted);
                            let bt_for_push = bt.clone();
                            match tokio::task::spawn_blocking(move || bt_for_push.set_mute(muted))
                                .await
                            {
                                Ok(Ok(())) => {
                                    tracing::info!(muted, "Mute sync: audio-manager -> BlueALSA");
                                }
                                Ok(Err(e)) => {
                                    tracing::debug!(error = %e, "Reverse mute sync failed");
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, "Reverse mute sync task panicked");
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "audio-manager signal stream error; reconnecting");
                        break;
                    }
                    None => {
                        tracing::warn!("audio-manager signal stream closed; reconnecting");
                        break;
                    }
                },
                signal = iface_stream.next() => match signal {
                    Some(Ok(signal)) => {
                        // A new PCM appeared (fresh A2DP connect); do one full
                        // catch-up read so the amp matches the phone volume.
                        let path = signal.header().path().map(std::string::ToString::to_string);
                        if path.as_deref().is_some_and(|p| p.contains("a2dpsnk")) {
                            sync_volume_logged(&bt, &conn, &mut last_pcm_vol).await;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "BlueALSA InterfacesAdded stream error; reconnecting");
                        break;
                    }
                    None => {
                        tracing::warn!("BlueALSA InterfacesAdded stream closed; reconnecting");
                        break;
                    }
                },
            }
        }

        // Backoff before rebuilding the match rules / streams.
        tokio::time::sleep(TokioDuration::from_millis(500)).await;
    }
}

/// CLI main entrypoint for the `bt-manager` binary.
///
/// Installs the global `tracing` subscriber before any other work so startup
/// events are captured; verbosity is controlled by `RUST_LOG` (default `info`,
/// see [`speaker_common::logging`]).
pub async fn main() -> ExitCode {
    speaker_common::logging::init_daemon_logging();
    let opts = match resolve_cli_opts(parse_cli_args_from(env_args()), "bt-manager", print_usage) {
        Ok(opts) => opts,
        Err(exit_code) => return exit_code,
    };
    let bt = match BluetoothDevice::new().await {
        Ok(bt) => bt,
        Err(e) => {
            tracing::error!(error = %e, "failed to connect to the system D-Bus");
            return ExitCode::FAILURE;
        }
    };
    run_app(bt, opts).await
}
