//! Audio Manager — single owner of Merus MA12070P amplifier volume/mute.
//!
//! This daemon is the exclusive hardware owner of the MA12070P amplifier.
//! All other components (bt-manager, screen-manager, future sources) are
//! clients that call the D-Bus API: `SetLevel(percent)`, `SetMute(bool)`.
//!
//! Two control modes (config `control_mode`):
//! - **`i2c`** (default, Phase 2): event-driven sole hardware supervisor in
//!   `dtoverlay=hifiberry-dac` mode. GPIO `/ENABLE`/`/MUTE`/`error` bring-up,
//!   VLP volume registers, serial mute, PLL-lock-gated unmute, fault
//!   recovery — see [`amp_controller`].
//! - **`alsa`** (legacy, retained until cutover is verified): kernel-driver
//!   mixer mode (`A.Mstr Vol`) with I2C serial mute — see [`amp_backend`].
//!
//! Design principles (per docs/audio_manager_plan.md + Phase 2 plan):
//! - **Event-driven, zero polling as primary path**: `tokio::select!` over
//!   D-Bus commands, lifecycle events, GPIO edges; only a bounded one-shot
//!   PLL handshake (~200 ms) and an optional low-frequency safety tick.
//! - **Single writer / transactional mutation**: in-memory state commits
//!   strictly after successful hardware writes.
//! - **Boot & crash safety**: one-shot state restore from
//!   `/var/lib/speaker/amp-state.json`; amp left awake + muted between
//!   streams (PLL handshake releases mute).
//! - **Mockable seams** (`I2cAmpBackend::new_mocked`, `AmpGpio::new_mocked`)
//!   for host test coverage.

// The bin target compiles these modules privately; test-only helpers (mocks,
// unused error variants) trip dead_code. Scope the allow to the bin only.
#[expect(
    dead_code,
    reason = "bin-only compilation leaves mock/test helpers unused"
)]
mod amp_backend;
#[expect(
    dead_code,
    reason = "bin-only compilation leaves mock/test helpers unused"
)]
mod amp_controller;
#[expect(
    dead_code,
    reason = "bin-only compilation leaves mock/test helpers unused"
)]
mod amp_gpio;
#[expect(
    dead_code,
    reason = "bin-only compilation leaves mock/test helpers unused"
)]
mod amp_state;
mod config;
#[expect(
    dead_code,
    reason = "bin-only compilation leaves mock/test helpers unused"
)]
mod dbus_api;
mod dsp_watch;
#[expect(
    dead_code,
    reason = "bin-only compilation leaves mock/test helpers unused"
)]
mod error;
mod state_file;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use speaker_hardware::{AmpI2cAddress, MA12070P_DEFAULT_I2C_BUS};

use crate::amp_backend::{AmpBackend, I2cAmpBackend, RealAmpBackend};
use crate::amp_controller::{AmpCommand, AmpController};
use crate::amp_gpio::AmpGpioConfig;
use crate::amp_state::AmpState;
use crate::config::Config;
use crate::dbus_api::AudioManagerInterface;
use crate::error::AmpError;
use crate::state_file::StateFile;
use zbus::names::WellKnownName;

/// Main entry point for the audio-manager daemon.
async fn run_daemon(config: Config) -> anyhow::Result<()> {
    // Log the complete resolved configuration and subsystem states (logging
    // discipline: never boot silently).
    info!(
        control_mode = %config.control_mode,
        alsa_card = %config.alsa_card,
        enable_pin = config.enable_gpio_pin,
        mute_pin = config.mute_gpio_pin,
        error_pin = config.error_gpio_pin,
        safety_tick = config.safety_tick,
        dsp_watch_enabled = config.dsp_watch_enabled,
        dsp_poll_interval_ms = config.dsp_poll_interval_ms,
        state_path = %config.state_path.display(),
        "Audio-manager configuration resolved"
    );

    match config.control_mode.as_str() {
        "i2c" => run_i2c_mode(config).await,
        "alsa" => run_alsa_mode(config).await,
        // Config validation already rejects other values; defensive.
        other => Err(anyhow::anyhow!("Unsupported control_mode '{other}'")),
    }
}

/// Requests the daemon's well-known D-Bus name on the system bus.
///
/// Shared by both control modes; returns the proxy (for name release on
/// shutdown) and the claimed name.
async fn claim_dbus_name<'conn>(
    connection: &'conn zbus::Connection,
) -> anyhow::Result<(zbus::fdo::DBusProxy<'conn>, WellKnownName<'conn>)> {
    let fdo = zbus::fdo::DBusProxy::new(connection).await?;
    let name: WellKnownName = "org.speaker.AudioManager"
        .try_into()
        .context("Invalid well-known name")?;
    let request_result = fdo
        .request_name(
            name.clone(),
            zbus::fdo::RequestNameFlags::AllowReplacement | zbus::fdo::RequestNameFlags::DoNotQueue,
        )
        .await
        .context("Failed to call RequestName")?;
    if request_result != zbus::fdo::RequestNameReply::PrimaryOwner {
        return Err(AmpError::DBusNameTaken.into());
    }
    info!("D-Bus name org.speaker.AudioManager acquired");
    Ok((fdo, name))
}

/// Loads the persisted amp state, falling back to defaults on failure.
fn load_persisted_state(state_path: &Path) -> AmpState {
    StateFile::new(state_path).load().unwrap_or_else(|e| {
        warn!(error = %e, "Failed to load state file, using defaults");
        AmpState::default()
    })
}

/// Waits for SIGTERM or SIGINT and logs the shutdown reason.
async fn wait_shutdown_signal() {
    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => info!("Received SIGTERM, shutting down gracefully"),
        _ = sigint.recv() => info!("Received SIGINT, shutting down gracefully"),
    }
}

/// Phase 2 i2c mode: event-driven sole hardware supervisor.
async fn run_i2c_mode(config: Config) -> anyhow::Result<()> {
    // 1. Claim the GPIO lines and wake the amp BEFORE the first I2C write:
    //    with hifiberry-dac the chip boots in /ENABLE hardware reset and
    //    NACKs all writes until GPIO14 is driven LOW.
    let gpio_cfg = AmpGpioConfig {
        enable: config.enable_gpio_pin,
        mute: config.mute_gpio_pin,
        booster: config.booster_gpio_pin,
        error: config.error_gpio_pin,
    };
    let gpio = tokio::task::spawn_blocking(move || crate::amp_gpio::AmpGpio::open(gpio_cfg))
        .await
        .context("GPIO open task panicked")?
        .context("Failed to claim amp GPIO lines")?;
    let gpio_for_watch = gpio.clone();

    // 2. Open the I2C backend (validation read follows boot_init).
    let backend = I2cAmpBackend::open(MA12070P_DEFAULT_I2C_BUS, AmpI2cAddress::default())
        .await
        .context("Failed to open I2C amp backend")?;
    let device = backend.device().clone();
    let backend: Arc<dyn AmpBackend> = Arc::new(backend);

    // 3. Load persisted state.
    let state_file = StateFile::new(&config.state_path);
    let persisted = load_persisted_state(&config.state_path);

    // 4. Boot bring-up: wake, probe-equivalent register init, reconcile.
    let mut controller = AmpController::new(
        Arc::clone(&backend),
        device,
        gpio,
        state_file.clone(),
        persisted,
    );
    controller
        .boot_init(persisted)
        .await
        .context("Amp boot bring-up failed")?;

    // 5. GPIO error-edge watch: blocking rppal poll in a spawn_blocking
    //    task feeding an async mpsc (plan §5.3). The callback returns
    //    `false` (channel closed) to stop the watch; the task runs for the
    //    daemon lifetime otherwise.
    let (edge_tx, edge_rx) = mpsc::channel::<()>(16);
    let edge_tx_close = edge_tx.clone();
    tokio::task::spawn_blocking(move || {
        gpio_for_watch.watch_error_edges(
            move || edge_tx.blocking_send(()).is_ok(),
            // Exit promptly once the receiver is gone (controller loop
            // ended): no edge is required to notice the shutdown.
            move || !edge_tx_close.is_closed(),
        )
    });

    // 6. Connect to system D-Bus, claim the name, export the interface.
    let connection = zbus::Connection::system()
        .await
        .context("Failed to connect to system D-Bus")?;
    let (fdo, name) = claim_dbus_name(&connection).await?;
    let name_for_release = name;

    let (cmd_tx, cmd_rx) = mpsc::channel::<AmpCommand>(32);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

    // 6b. DSP stream-lifecycle watcher (plan §11.3): polls the CamillaDSP
    //     engine state and emits StreamStarting/Stopping edges onto the
    //     command channel. Exits when the channel closes with the
    //     controller loop.
    if config.dsp_watch_enabled {
        let dsp_device = speaker_dsp::DspDevice::new();
        let watcher_cmd_tx = cmd_tx.clone();
        tokio::spawn(dsp_watch::run(
            dsp_device,
            watcher_cmd_tx,
            Duration::from_millis(config.dsp_poll_interval_ms),
        ));
        info!(
            poll_interval_ms = config.dsp_poll_interval_ms,
            "DSP stream-lifecycle watcher enabled"
        );
    } else {
        warn!(
            "DSP stream-lifecycle watcher disabled by configuration: no lifecycle \
             source can emit StreamStarting, so the amp stays hardware-muted while \
             no stream runs (idle SetMute(false) records intent only)"
        );
    }

    let mirror = controller.mirror();
    let interface =
        AudioManagerInterface::with_commands(mirror.clone(), state_file.clone(), cmd_tx.clone());
    connection
        .object_server()
        .at("/org/speaker/amp", interface)
        .await
        .context("Failed to export D-Bus object")?;

    sd_notify::notify(true, &[sd_notify::NotifyState::Ready])
        .context("Failed to notify systemd READY=1")?;
    info!("Audio manager ready (i2c mode), entering event loop");

    // 7. Run the controller event loop (drives everything) with a signal
    //    watcher feeding the shutdown channel.
    let signal_task = tokio::spawn(async move {
        wait_shutdown_signal().await;
        let _ = shutdown_tx.send(()).await;
    });

    controller
        .run(cmd_rx, Some(edge_rx), shutdown_rx, config.safety_tick)
        .await;
    signal_task.abort();

    // 8. Graceful shutdown: persist state and release the D-Bus name.
    let final_state = *mirror.lock().await;
    state_file.save(&final_state).await?;
    fdo.release_name(name_for_release).await?;
    sd_notify::notify(true, &[sd_notify::NotifyState::Stopping])
        .context("Failed to notify systemd STOPPING=1")?;

    info!("Audio manager stopped cleanly (i2c mode)");
    Ok(())
}

/// Legacy alsa mode (kernel-driver mixer + I2C serial mute).
async fn run_alsa_mode(config: Config) -> anyhow::Result<()> {
    // 1. Validate hardware before claiming D-Bus name
    let backend = RealAmpBackend::open(&config.alsa_card)
        .await
        .context("Failed to open ALSA backend for amp hardware validation")?;
    info!(card = %config.alsa_card, "Hardware validation passed");

    // 2. Connect to system bus and request the well-known name
    let connection = zbus::Connection::system()
        .await
        .context("Failed to connect to system D-Bus")?;
    let (fdo, name) = claim_dbus_name(&connection).await?;
    let name_for_release = name;

    // 4. Notify systemd we're ready (Type=dbus)
    sd_notify::notify(true, &[sd_notify::NotifyState::Ready])
        .context("Failed to notify systemd READY=1")?;

    // 5. Load persisted state and apply to hardware
    let state_file = StateFile::new(&config.state_path);
    let persisted = load_persisted_state(&config.state_path);
    backend
        .write_state(&persisted)
        .await
        .context("Failed to apply persisted state to hardware")?;
    info!(
        level = persisted.level_raw,
        muted = persisted.muted,
        "Restored amp state"
    );

    // 6. Create shared state and D-Bus interface
    let state = Arc::new(tokio::sync::Mutex::new(persisted));
    let backend = Arc::new(backend);
    let state_file_for_shutdown = state_file.clone();
    let interface = AudioManagerInterface::new(state.clone(), backend, state_file);

    // 7. Export the D-Bus object
    let object_server = connection.object_server();
    object_server
        .at("/org/speaker/amp", interface)
        .await
        .context("Failed to export D-Bus object")?;

    info!("Audio manager ready, entering event loop");

    // 8. Event loop: wait for D-Bus method calls or shutdown signal
    wait_shutdown_signal().await;

    // 9. Graceful shutdown: persist state and release name
    let final_state = *state.lock().await;
    state_file_for_shutdown.save(&final_state).await?;
    fdo.release_name(name_for_release).await?;
    sd_notify::notify(true, &[sd_notify::NotifyState::Stopping])
        .context("Failed to notify systemd STOPPING=1")?;

    info!("Audio manager stopped cleanly");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // Initialize tracing first so the panic hook below can emit events.
    // `init_daemon_logging` defaults to `info` when `RUST_LOG` is unset;
    // a raw `EnvFilter::from_default_env()` yields an empty filter that
    // silences the daemon entirely under systemd (no `RUST_LOG` set).
    speaker_common::logging::init_daemon_logging();

    // Install panic hook to log panics with tracing
    std::panic::set_hook(Box::new(|info| {
        error!(panic = %info, "Panic occurred");
    }));

    let config = Config::load()?;
    info!(?config, "Starting audio-manager");

    // Run the async runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to create tokio runtime")?;

    rt.block_on(run_daemon(config))
}
