//! Audio Manager — single owner of Merus MA12070P amplifier volume/mute.
//!
//! This daemon is the exclusive hardware owner of the `A.Mstr Vol` register.
//! All other components (bt-manager, screen-manager, future sources) are
//! clients that call the D-Bus API: `SetLevel(percent)`, `SetMute(bool)`.
//!
//! Design principles (per docs/audio_manager_plan.md):
//! - **Event-driven, zero polling**: only `tokio::select!` over incoming D-Bus
//!   method calls; no timers, no keepalive, no periodic re-sync.
//! - **Single writer**: all mixer transactions serialized through one
//!   `Mutex<AmpState>`; state mutated only after successful mixer write.
//! - **Transactional mutation**: fallible ALSA write first, then commit state.
//! - **Mute = raw 0** (-144 dB, true hardware mute); level remembered and
//!   restored on unmute; level changes while muted are stored only.
//! - **Boot & crash safety**: one-shot state restore from
//!   `/var/lib/speaker/amp-state.json`; on exit leave amp at last state.
//! - **Hardware validation at startup**: fail fast if `sndrpimerusamp` or
//!   `A.Mstr Vol` control is missing (systemd restarts).
//! - **Mockable backend** for 100% host test coverage.

// The bin target compiles these modules privately; test-only helpers (mocks,
// unused error variants) trip dead_code. Scope the allow to the bin only.
#[allow(dead_code)]
mod amp_backend;
#[allow(dead_code)]
mod amp_state;
mod config;
#[allow(dead_code)]
mod dbus_api;
#[allow(dead_code)]
mod error;
mod state_file;

use std::sync::Arc;

use anyhow::Context;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};

use crate::amp_backend::{AmpBackend, RealAmpBackend};
use crate::amp_state::AmpState;
use crate::config::Config;
use crate::dbus_api::AudioManagerInterface;
use crate::error::AmpError;
use crate::state_file::StateFile;
use zbus::names::WellKnownName;

/// Main entry point for the audio-manager daemon.
async fn run_daemon(config: Config) -> anyhow::Result<()> {
    // 1. Validate hardware before claiming D-Bus name
    let backend = RealAmpBackend::open(&config.alsa_card)
        .await
        .context("Failed to open ALSA backend for amp hardware validation")?;
    info!(card = %config.alsa_card, "Hardware validation passed");

    // 2. Connect to system bus
    let connection = zbus::Connection::system()
        .await
        .context("Failed to connect to system D-Bus")?;

    // 3. Request the well-known name
    let fdo = zbus::fdo::DBusProxy::new(&connection).await?;
    let name: WellKnownName = "org.speaker.AudioManager"
        .try_into()
        .context("Invalid well-known name")?;
    let name_for_release = name.clone();
    let request_result = fdo
        .request_name(
            name,
            zbus::fdo::RequestNameFlags::AllowReplacement | zbus::fdo::RequestNameFlags::DoNotQueue,
        )
        .await
        .context("Failed to call RequestName")?;

    if request_result != zbus::fdo::RequestNameReply::PrimaryOwner {
        return Err(AmpError::DBusNameTaken.into());
    }
    info!("D-Bus name org.speaker.AudioManager acquired");

    // 4. Notify systemd we're ready (Type=dbus)
    sd_notify::notify(true, &[sd_notify::NotifyState::Ready])
        .context("Failed to notify systemd READY=1")?;

    // 5. Load persisted state and apply to hardware
    let state_file = StateFile::new(&config.state_path);
    let persisted = state_file.load().unwrap_or_else(|e| {
        warn!(error = %e, "Failed to load state file, using defaults");
        AmpState::default()
    });
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
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down gracefully");
        }
        _ = sigint.recv() => {
            info!("Received SIGINT, shutting down gracefully");
        }
        // The zbus connection will naturally end if the bus goes away;
        // we don't need to explicitly await it because the connection
        // is held by the object_server and interface. If the bus closes,
        // the select will hang until a signal arrives, which is fine:
        // systemd will kill us and restart.
    }

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
    // Install panic hook to log panics with tracing
    std::panic::set_hook(Box::new(|info| {
        error!(panic = %info, "Panic occurred");
    }));

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::load()?;
    info!(?config, "Starting audio-manager");

    // Run the async runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to create tokio runtime")?;

    rt.block_on(run_daemon(config))
}
