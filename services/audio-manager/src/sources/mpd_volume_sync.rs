//! Bidirectional MPD ↔ Amp volume sync gated on the active audio source.
//!
//! Mirrors [`bt_manager::volume_sync`](../../bt-manager/src/volume_sync.rs) for
//! MPD (`mixer_type = "null"`):
//! - **MPD → Amp**: when this MPD source is `ActiveSource` (and outside the
//!   post-switch / post-push settle windows), `changed: mixer` idle events
//!   coalesce queued notifications, read `status.volume`, and forward it to
//!   `AmpController` (`SetLevel`) + emit D-Bus `PropertiesChanged`.
//! - **Amp → MPD**: when this MPD source is `ActiveSource` (and on `false -> true`
//!   source switch-in), amp level changes push `setvol` to MPD, with `last_synced`
//!   dedup and short echo settle so neither side bounces or loops.

use std::time::{Duration, Instant};

use speaker_mpd::{MpdDevice, MpdEvent, MpdSubsystem};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};
use zbus::Connection;

use crate::amp_controller::{AmpCommand, AmpCommandKind};
use crate::dbus_api::AudioManagerInterface;
use crate::sources::dbus_sources::AudioSourcesSnapshot;
use crate::sources::registry::SourceId;

/// Grace window after switching to an MPD source during which MPD mixer events
/// are suppressed while amp → MPD reconciliation settles.
pub const FRESH_MPD_SOURCE_SETTLE: Duration = Duration::from_secs(2);

/// Short settle window after pushing an amp volume change to MPD so delayed
/// `changed: mixer` echoes from MPD never bounce the amp backwards.
pub const AMP_PUSH_ECHO_SETTLE: Duration = Duration::from_millis(350);

/// Pure, host-testable state machine for MPD ↔ Amp volume sync.
#[derive(Debug)]
pub struct MpdVolumeSyncState {
    source_active: bool,
    suppressed_until: Option<Instant>,
    last_synced: Option<u8>,
}

impl Default for MpdVolumeSyncState {
    fn default() -> Self {
        Self::new()
    }
}

impl MpdVolumeSyncState {
    /// Creates an inactive sync state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            source_active: false,
            suppressed_until: None,
            last_synced: None,
        }
    }

    /// Returns whether this MPD source is currently the active audio source.
    #[must_use]
    pub const fn source_active(&self) -> bool {
        self.source_active
    }

    /// Updates `source_active` and returns `true` when transitioning `false -> true`
    /// (signaling the caller to reconcile current amp volume → MPD).
    pub fn set_source_active(&mut self, active: bool, now: Instant) -> bool {
        let switched_in = !self.source_active && active;
        self.source_active = active;
        if switched_in {
            self.suppressed_until = Some(now + FRESH_MPD_SOURCE_SETTLE);
            self.last_synced = None;
        } else if !active {
            self.suppressed_until = None;
            self.last_synced = None;
        }
        switched_in
    }

    /// Records a volume value pushed to MPD and extends the short echo-settle
    /// window so duplicate/stale `changed: mixer` events do not bounce back.
    pub fn record_pushed_to_mpd(&mut self, percent: u8, now: Instant) {
        let clamped = percent.min(100);
        self.last_synced = Some(clamped);
        let until = now + AMP_PUSH_ECHO_SETTLE;
        self.suppressed_until = Some(match self.suppressed_until {
            Some(existing) if existing > until => existing,
            _ => until,
        });
    }

    /// Evaluates an MPD `status.volume` update; returns `true` when it should be
    /// forwarded to the amp via `SetLevel`.
    pub fn should_forward_to_amp(&mut self, mpd_percent: u8, now: Instant) -> bool {
        if !self.source_active {
            return false;
        }
        if self.suppressed_until.is_some_and(|until| now < until) {
            return false;
        }
        let clamped = mpd_percent.min(100);
        if self.last_synced == Some(clamped) {
            return false;
        }
        self.last_synced = Some(clamped);
        true
    }

    /// Evaluates an amp `Level` change; returns `true` when it should be pushed
    /// to MPD via `set_volume`.
    pub fn should_push_to_mpd(&mut self, amp_percent: u8, now: Instant) -> bool {
        if !self.source_active {
            return false;
        }
        let clamped = amp_percent.min(100);
        if self.last_synced == Some(clamped) {
            return false;
        }
        self.record_pushed_to_mpd(clamped, now);
        true
    }
}

async fn push_amp_to_mpd(
    source_id: &SourceId,
    mpd: &MpdDevice,
    state: &mut MpdVolumeSyncState,
    amp_percent: u8,
) {
    let clamped = amp_percent.min(100);
    state.record_pushed_to_mpd(clamped, Instant::now());
    if let Err(e) = mpd.set_volume(clamped).await {
        debug!(source = %source_id, percent = clamped, error = %e, "Failed to push amp volume to MPD");
    } else {
        debug!(source = %source_id, percent = clamped, "Synced amp volume -> MPD");
    }
}

async fn forward_mpd_to_amp(
    source_id: &SourceId,
    amp_cmd_tx: &mpsc::Sender<AmpCommand>,
    dbus_conn: Option<&Connection>,
    percent: u8,
) {
    let (reply_tx, reply_rx) = oneshot::channel();
    if amp_cmd_tx
        .send(AmpCommand {
            kind: AmpCommandKind::SetLevel(percent),
            reply: Some(reply_tx),
        })
        .await
        .is_err()
    {
        return;
    }
    match reply_rx.await {
        Ok(Ok(())) => {
            info!(source = %source_id, percent, "Synced MPD volume -> amp");
            if let Some(conn) = dbus_conn
                && let Ok(iface_ref) = conn
                    .object_server()
                    .interface::<_, AudioManagerInterface>("/org/speaker/amp")
                    .await
                && let Err(e) = iface_ref
                    .get()
                    .await
                    .level_changed(iface_ref.signal_emitter())
                    .await
            {
                warn!(error = %e, "Failed to emit D-Bus Level PropertiesChanged after MPD sync");
            }
        }
        Ok(Err(e)) => {
            warn!(source = %source_id, percent, error = %e, "Amp rejected MPD volume update");
        }
        Err(_) => {}
    }
}

/// Runs the bidirectional MPD ↔ Amp volume sync task for `source_id`.
pub async fn run_mpd_volume_sync(
    source_id: SourceId,
    mpd: MpdDevice,
    snapshot: AudioSourcesSnapshot,
    amp_cmd_tx: mpsc::Sender<AmpCommand>,
    mut level_rx: watch::Receiver<u8>,
    dbus_conn: Option<Connection>,
) {
    let mut active_rx = snapshot.subscribe_active();
    let mut state = MpdVolumeSyncState::new();
    let mut backoff = Duration::from_millis(500);
    let max_backoff = Duration::from_secs(5);

    let initially_active = active_rx.borrow_and_update().as_ref() == Some(&source_id);
    if state.set_source_active(initially_active, Instant::now()) {
        let level = *level_rx.borrow_and_update();
        push_amp_to_mpd(&source_id, &mpd, &mut state, level).await;
    }

    loop {
        if amp_cmd_tx.is_closed() {
            return;
        }
        match mpd.subscribe_events().await {
            Ok(mut events) => {
                backoff = Duration::from_millis(500);
                loop {
                    tokio::select! {
                        () = amp_cmd_tx.closed() => return,
                        res = active_rx.changed() => {
                            if res.is_err() {
                                return;
                            }
                            let is_active = active_rx.borrow_and_update().as_ref() == Some(&source_id);
                            if state.set_source_active(is_active, Instant::now()) {
                                let level = *level_rx.borrow_and_update();
                                push_amp_to_mpd(&source_id, &mpd, &mut state, level).await;
                            }
                        }
                        res = level_rx.changed() => {
                            if res.is_err() {
                                return;
                            }
                            let level = *level_rx.borrow_and_update();
                            if state.should_push_to_mpd(level, Instant::now()) {
                                if let Err(e) = mpd.set_volume(level.min(100)).await {
                                    debug!(source = %source_id, percent = level, error = %e, "Failed to sync amp -> MPD volume");
                                } else {
                                    debug!(source = %source_id, percent = level, "Synced amp volume -> MPD");
                                }
                            }
                        }
                        maybe_evt = events.recv() => {
                            let Some(evt) = maybe_evt else {
                                break;
                            };
                            match evt {
                                MpdEvent::SubsystemChanged(MpdSubsystem::Mixer) => {
                                    // Coalesce any burst of queued Mixer events into a single status read.
                                    let mut disconnected = false;
                                    while let Ok(next) = events.try_recv() {
                                        if matches!(next, MpdEvent::Disconnected(_)) {
                                            disconnected = true;
                                            break;
                                        }
                                    }
                                    if let Ok(status) = mpd.read_status().await
                                        && let Some(vol) = status.volume
                                        && state.should_forward_to_amp(vol, Instant::now())
                                    {
                                        forward_mpd_to_amp(
                                            &source_id,
                                            &amp_cmd_tx,
                                            dbus_conn.as_ref(),
                                            vol.min(100),
                                        )
                                        .await;
                                        let _ = level_rx.borrow_and_update();
                                    }
                                    if disconnected {
                                        break;
                                    }
                                }
                                MpdEvent::Disconnected(_) => break,
                                MpdEvent::SubsystemChanged(_) => {}
                            }
                        }
                    }
                }
            }
            Err(e) => {
                debug!(source = %source_id, error = %e, "MPD volume sync connect failed; retrying");
            }
        }

        tokio::select! {
            () = amp_cmd_tx.closed() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}
