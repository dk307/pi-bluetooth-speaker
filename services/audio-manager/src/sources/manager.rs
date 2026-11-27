//! Event-driven source manager orchestrating arbitration, DSP routing, persistence, and controls.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use speaker_dsp::DspDevice;
use speaker_mpd::{MpdDevice, MpdEvent, MpdPlaybackState, MpdSubsystem};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::object_server::SignalEmitter;

use crate::rate_watch::{
    DspRouteRequest, LoopbackState, RateWatchController, apply_route_request,
    reconcile_route_with_engine, spawn_subdevice_watcher,
};
use crate::sources::arbiter::{ArbiterOutcome, DspTarget, SourceArbiter};
use crate::sources::control::SourceControl;
use crate::sources::dbus_sources::{
    AudioSourcesSnapshot, SourceCommand, SourceCommandKind, emit_property_changes,
};
use crate::sources::registry::{AutoSwitchPolicy, ControlSpec, SourceId, SourceRegistry};
use crate::sources::state_file::{PersistedSourceState, SourceStateFile};

/// Configuration for [`SourceManager`].
#[derive(Debug, Clone)]
pub struct SourceManagerConfig {
    /// Path to the `CamillaDSP` base YAML template (`/etc/camilladsp/crossover.yml`).
    pub camilladsp_config_path: PathBuf,
    /// Path to persisted source state (`/var/lib/speaker/source-state.json`).
    pub source_state_path: PathBuf,
    /// Fallback auto-switch policy when no state file exists yet.
    pub default_policy: AutoSwitchPolicy,
}

/// Coordinates [`SourceArbiter`], `CamillaDSP` input routing, pause adapters, and D-Bus publication.
pub struct SourceManager {
    arbiter: SourceArbiter,
    state_file: SourceStateFile,
    dsp: DspDevice,
    dsp_config_path: PathBuf,
    rate_ctrl: RateWatchController,
    control: Arc<dyn SourceControl>,
    snapshot: AudioSourcesSnapshot,
    dbus: Option<(Connection, SignalEmitter<'static>)>,
    desired_route: Option<DspRouteRequest>,
    last_sources_digest: Vec<(SourceId, &'static str, u32, bool)>,
}

impl SourceManager {
    /// Builds a new [`SourceManager`], loading persisted state from `cfg.source_state_path`.
    #[must_use]
    pub fn new(
        registry: SourceRegistry,
        cfg: SourceManagerConfig,
        dsp: DspDevice,
        control: Arc<dyn SourceControl>,
        snapshot: AudioSourcesSnapshot,
        dbus: Option<(Connection, SignalEmitter<'static>)>,
    ) -> Self {
        let state_file = SourceStateFile::new(&cfg.source_state_path);
        let persisted = if cfg.source_state_path.exists() {
            state_file.load().unwrap_or_else(|e| {
                warn!(error = %e, "Failed to load source state file, using defaults");
                PersistedSourceState {
                    auto_switch_policy: cfg.default_policy,
                    ..PersistedSourceState::default()
                }
            })
        } else {
            PersistedSourceState {
                auto_switch_policy: cfg.default_policy,
                ..PersistedSourceState::default()
            }
        };
        let arbiter = SourceArbiter::new(registry, persisted);
        let initial_views = arbiter.status_views();
        snapshot.update(
            arbiter.active().cloned(),
            arbiter.pinned().cloned(),
            arbiter.policy(),
            initial_views,
        );
        let last_sources_digest = snapshot_digest(&arbiter);

        Self {
            arbiter,
            state_file,
            dsp,
            dsp_config_path: cfg.camilladsp_config_path,
            rate_ctrl: RateWatchController::new(),
            control,
            snapshot,
            dbus,
            desired_route: None,
            last_sources_digest,
        }
    }

    /// Returns a reference to the current arbiter state (useful for diagnostics/tests).
    #[must_use]
    pub const fn arbiter(&self) -> &SourceArbiter {
        &self.arbiter
    }

    /// Runs the production multi-source manager using the `hw:Loopback` ALSA ctl watcher.
    pub async fn run_with_alsa_loopback(
        mut self,
        cmd_tx: mpsc::Sender<SourceCommand>,
        cmd_rx: mpsc::Receiver<SourceCommand>,
    ) {
        for def in self.arbiter.registry().definitions() {
            if let ControlSpec::MpdTcp { host, port } = &def.control {
                let mpd = MpdDevice::with_endpoint(host.clone(), *port);
                tokio::spawn(run_mpd_play_watcher(def.id.clone(), mpd, cmd_tx.clone()));
            }
        }
        let subdevices: Vec<u32> = self
            .arbiter
            .registry()
            .loopback_subdevices()
            .into_iter()
            .map(|(sub, _)| sub)
            .collect();
        let (rx, ctl_handle) = spawn_subdevice_watcher(subdevices);
        self.run(cmd_rx, rx).await;
        if let Err(join_err) = ctl_handle.await
            && join_err.is_panic()
        {
            std::panic::resume_unwind(join_err.into_panic());
        }
    }

    /// Runs the source-manager event loop from injected command and loopback event receivers.
    pub async fn run(
        &mut self,
        mut cmd_rx: mpsc::Receiver<SourceCommand>,
        mut loopback_rx: mpsc::Receiver<(u32, LoopbackState, bool)>,
    ) {
        let expected_loopbacks = self.arbiter.registry().loopback_subdevices().len();
        let mut seeded = expected_loopbacks == 0;
        let mut pending_seed: Vec<(SourceId, bool, u32)> = Vec::new();
        let seed_deadline = tokio::time::Instant::now() + Duration::from_secs(2);

        if let Some((conn, emitter)) = self.dbus.as_ref() {
            emit_property_changes(conn, emitter, true, true, true, true).await;
        }

        loop {
            let needs_retry = self.desired_route.as_ref().is_some_and(|req| {
                (req.streaming && !self.rate_ctrl.matches(&req.capture_device, req.rate_hz))
                    || (!req.streaming && req.force_apply)
            });
            let reconcile_interval = if needs_retry {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(2)
            };
            let active_stream = self
                .desired_route
                .as_ref()
                .is_some_and(|r| r.streaming || r.force_apply);

            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else {
                        debug!("Source manager command channel closed; exiting");
                        break;
                    };
                    let res = self.handle_command(cmd.kind).await;
                    if let Some(reply) = cmd.reply {
                        let _ = reply.send(res);
                    }
                }
                maybe_evt = loopback_rx.recv() => {
                    let Some((subdevice, state, from_event)) = maybe_evt else {
                        debug!("Loopback watcher channel closed; exiting");
                        break;
                    };
                    let Some(source_id) = self.arbiter.registry().by_loopback_subdevice(subdevice).cloned() else {
                        continue;
                    };
                    if !seeded {
                        pending_seed.retain(|(id, _, _)| id != &source_id);
                        pending_seed.push((source_id, state.active, state.rate_hz));
                        if pending_seed.len() >= expected_loopbacks {
                            let outcome = self.arbiter.seed(pending_seed.drain(..));
                            seeded = true;
                            self.apply_outcome(outcome, from_event).await;
                        }
                    } else {
                        let outcome = self.arbiter.on_activity(&source_id, state.active, state.rate_hz);
                        self.apply_outcome(outcome, from_event).await;
                    }
                }
                () = tokio::time::sleep_until(seed_deadline), if !seeded => {
                    warn!(
                        observed = pending_seed.len(),
                        expected = expected_loopbacks,
                        "Seed deadline elapsed before all loopback subdevices reported; seeding with observed state"
                    );
                    let outcome = self.arbiter.seed(pending_seed.drain(..));
                    seeded = true;
                    self.apply_outcome(outcome, false).await;
                }
                () = tokio::time::sleep(reconcile_interval), if active_stream => {
                    if let Some(req) = self.desired_route.clone() {
                        if req.streaming {
                            reconcile_route_with_engine(
                                &mut self.rate_ctrl,
                                &self.dsp,
                                &self.dsp_config_path,
                                &req,
                            )
                            .await;
                        } else if req.force_apply
                            && apply_route_request(
                                &mut self.rate_ctrl,
                                &self.dsp,
                                &self.dsp_config_path,
                                &req,
                                false,
                            )
                            .await
                            && let Some(desired) = self.desired_route.as_mut()
                        {
                            desired.force_apply = false;
                        }
                    }
                }
            }
        }
    }

    /// Processes a single D-Bus / internal [`SourceCommandKind`].
    ///
    /// # Errors
    /// Returns an error string when `id` is unknown.
    pub async fn handle_command(&mut self, kind: SourceCommandKind) -> Result<(), String> {
        let outcome = match kind {
            SourceCommandKind::SelectSource(id) => self.arbiter.select_source(&id)?,
            SourceCommandKind::PinSource(id) => self.arbiter.pin_source(id.as_ref())?,
            SourceCommandKind::SetAutoSwitchPolicy(policy) => self.arbiter.set_policy(policy),
            SourceCommandKind::ReleaseSource(id) => self.arbiter.release_source(&id)?,
            SourceCommandKind::ReportPlayIntent(id) => self.arbiter.on_play_intent(&id),
        };
        self.apply_outcome(outcome, false).await;
        Ok(())
    }

    async fn apply_outcome(&mut self, outcome: ArbiterOutcome, from_event: bool) {
        if let Some(target) = outcome.dsp_target {
            let mut req = route_request_from_target(target);
            let applied = apply_route_request(
                &mut self.rate_ctrl,
                &self.dsp,
                &self.dsp_config_path,
                &req,
                from_event,
            )
            .await;
            if applied && !req.streaming {
                req.force_apply = false;
            }
            self.desired_route = Some(req);
        }

        for id in outcome.pause_sources {
            let ctrl = Arc::clone(&self.control);
            tokio::spawn(async move {
                if let Err(e) = ctrl.pause(&id).await {
                    warn!(source = %id, error = %e, "Failed to pause displaced audio source");
                } else {
                    info!(source = %id, "Paused displaced audio source");
                }
            });
        }

        if outcome.persist {
            let persisted = self.arbiter.persisted_state();
            if let Err(e) = self.state_file.save(&persisted).await {
                warn!(error = %e, "Failed to persist source state");
            }
        }

        let new_digest = snapshot_digest(&self.arbiter);
        let sources_changed = new_digest != self.last_sources_digest;
        if sources_changed
            || outcome.active_changed.is_some()
            || outcome.pinned_changed.is_some()
            || outcome.policy_changed.is_some()
        {
            self.last_sources_digest = new_digest;
            self.snapshot.update(
                self.arbiter.active().cloned(),
                self.arbiter.pinned().cloned(),
                self.arbiter.policy(),
                self.arbiter.status_views(),
            );
            if let Some((conn, emitter)) = self.dbus.as_ref() {
                emit_property_changes(
                    conn,
                    emitter,
                    outcome.active_changed.is_some(),
                    outcome.pinned_changed.is_some(),
                    outcome.policy_changed.is_some(),
                    sources_changed,
                )
                .await;
            }
        }
    }
}

fn route_request_from_target(target: DspTarget) -> DspRouteRequest {
    DspRouteRequest {
        capture_device: target.capture_device,
        rate_hz: target.rate_hz,
        streaming: target.streaming,
        force_apply: target.force_detach,
    }
}

fn snapshot_digest(arbiter: &SourceArbiter) -> Vec<(SourceId, &'static str, u32, bool)> {
    arbiter
        .status_views()
        .into_iter()
        .map(|v| (v.id, v.state, v.sample_rate_hz, v.is_active))
        .collect()
}

/// Watches an MPD source via `idle` (`MpdSubsystem::Player`) and emits
/// [`SourceCommandKind::ReportPlayIntent`] on `Stopped/Paused -> Playing` transitions
/// so unpausing `rmpd` triggers handover even when ALSA stays open across pause.
pub async fn run_mpd_play_watcher(
    source_id: SourceId,
    mpd: MpdDevice,
    cmd_tx: mpsc::Sender<SourceCommand>,
) {
    let mut backoff = Duration::from_millis(500);
    let max_backoff = Duration::from_secs(5);
    let mut was_playing = false;

    loop {
        if cmd_tx.is_closed() {
            return;
        }
        match mpd.subscribe_events().await {
            Ok(mut events) => {
                backoff = Duration::from_millis(500);
                let mut status_backoff = Duration::from_millis(500);
                let mut needs_status_sync = true;

                loop {
                    if needs_status_sync && let Ok(status) = mpd.read_status().await {
                        needs_status_sync = false;
                        status_backoff = Duration::from_millis(500);
                        let playing = status.state == MpdPlaybackState::Playing;
                        if playing && !was_playing {
                            info!(source = %source_id, "MPD player resumed; reporting play intent");
                            if cmd_tx
                                .send(SourceCommand::fire_and_forget(
                                    SourceCommandKind::ReportPlayIntent(source_id.clone()),
                                ))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        was_playing = playing;
                    }

                    tokio::select! {
                        () = cmd_tx.closed() => return,
                        () = tokio::time::sleep(status_backoff), if needs_status_sync => {
                            status_backoff = (status_backoff * 2).min(max_backoff);
                        }
                        maybe_evt = events.recv() => {
                            let Some(evt) = maybe_evt else {
                                break;
                            };
                            match evt {
                                MpdEvent::SubsystemChanged(MpdSubsystem::Player) => {
                                    needs_status_sync = true;
                                    status_backoff = Duration::from_millis(500);
                                }
                                MpdEvent::Disconnected(_) => break,
                                MpdEvent::SubsystemChanged(_) => {}
                            }
                        }
                    }
                }
            }
            Err(e) => {
                debug!(source = %source_id, error = %e, "MPD play watcher connect failed; retrying");
            }
        }

        tokio::select! {
            () = cmd_tx.closed() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}
