//! Pure, host-testable multi-source arbitration state machine.

use std::collections::HashMap;

use crate::sources::registry::{
    AutoSwitchPolicy, DEFAULT_SOURCE_RATE_HZ, SourceId, SourceRegistry,
};
use crate::sources::state_file::PersistedSourceState;

/// Desired `CamillaDSP` capture target emitted by [`SourceArbiter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DspTarget {
    /// ALSA capture device string (`hw:Loopback,1,N` or direct device).
    pub capture_device: Box<str>,
    /// Target sample rate in Hz (`> 0`).
    pub rate_hz: u32,
    /// `true` when the active source is currently streaming.
    pub streaming: bool,
    /// `true` when switching away from a previously streaming source onto an idle
    /// source, forcing `CamillaDSP` to detach immediately.
    pub force_detach: bool,
}

/// Snapshot view of one source for D-Bus reporting (`org.speaker.AudioSources1.Sources`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStatusView {
    /// Source identifier.
    pub id: SourceId,
    /// Display name.
    pub display_name: Box<str>,
    /// `"streaming"` or `"idle"`.
    pub state: &'static str,
    /// Configured priority.
    pub priority: u8,
    /// Current stream sample rate in Hz (0 when idle).
    pub sample_rate_hz: u32,
    /// Whether this source is currently active.
    pub is_active: bool,
}

/// Side-effects produced by one pure [`SourceArbiter`] transition.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ArbiterOutcome {
    /// `Some(new_active)` when [`SourceArbiter::active`] changed.
    pub active_changed: Option<Option<SourceId>>,
    /// `Some(new_pinned)` when [`SourceArbiter::pinned`] changed.
    pub pinned_changed: Option<Option<SourceId>>,
    /// `Some(new_policy)` when [`SourceArbiter::policy`] changed.
    pub policy_changed: Option<AutoSwitchPolicy>,
    /// Displaced or shut-out sources that should be paused asynchronously.
    pub pause_sources: Vec<SourceId>,
    /// Updated `CamillaDSP` target, when routing or sample rate needs evaluation.
    pub dsp_target: Option<DspTarget>,
    /// Whether [`PersistedSourceState`] should be written to disk.
    pub persist: bool,
}

#[derive(Debug, Clone)]
struct SourceRuntime {
    order: usize,
    priority: u8,
    streaming: bool,
    rate_hz: u32,
    last_active_seq: u64,
}

/// Pure arbitration state machine for multiple audio sources.
#[derive(Debug)]
pub struct SourceArbiter {
    registry: SourceRegistry,
    runtime: HashMap<SourceId, SourceRuntime>,
    active: Option<SourceId>,
    last_source: Option<SourceId>,
    pinned: Option<SourceId>,
    policy: AutoSwitchPolicy,
    seq: u64,
}

impl SourceArbiter {
    /// Creates an arbiter from `registry` and restored `persisted` state.
    #[must_use]
    pub fn new(registry: SourceRegistry, persisted: PersistedSourceState) -> Self {
        let mut runtime = HashMap::with_capacity(registry.definitions().len());
        for (order, def) in registry.definitions().iter().enumerate() {
            runtime.insert(
                def.id.clone(),
                SourceRuntime {
                    order,
                    priority: def.priority,
                    streaming: false,
                    rate_hz: 0,
                    last_active_seq: 0,
                },
            );
        }

        let pinned = persisted.pinned_source.filter(|id| registry.contains(id));
        let last_source = persisted.last_source.filter(|id| registry.contains(id));
        let active = pinned.clone().or_else(|| last_source.clone());

        Self {
            registry,
            runtime,
            active,
            last_source,
            pinned,
            policy: persisted.auto_switch_policy,
            seq: 0,
        }
    }

    /// Returns the immutable source registry.
    #[must_use]
    pub const fn registry(&self) -> &SourceRegistry {
        &self.registry
    }

    /// Returns the currently active source ID, if any.
    #[must_use]
    pub const fn active(&self) -> Option<&SourceId> {
        self.active.as_ref()
    }

    /// Returns the currently pinned source ID, if any.
    #[must_use]
    pub const fn pinned(&self) -> Option<&SourceId> {
        self.pinned.as_ref()
    }

    /// Returns the active auto-switch policy.
    #[must_use]
    pub const fn policy(&self) -> AutoSwitchPolicy {
        self.policy
    }

    /// Builds the serializable persistence snapshot.
    #[must_use]
    pub fn persisted_state(&self) -> PersistedSourceState {
        PersistedSourceState {
            last_source: self.last_source.clone(),
            pinned_source: self.pinned.clone(),
            auto_switch_policy: self.policy,
        }
    }

    /// Returns the current status snapshot of all registered sources in registry order.
    #[must_use]
    pub fn status_views(&self) -> Vec<SourceStatusView> {
        self.registry
            .definitions()
            .iter()
            .map(|def| {
                let rt = &self.runtime[&def.id];
                SourceStatusView {
                    id: def.id.clone(),
                    display_name: def.display_name.clone(),
                    state: if rt.streaming { "streaming" } else { "idle" },
                    priority: def.priority,
                    sample_rate_hz: if rt.streaming { rt.rate_hz } else { 0 },
                    is_active: self.active.as_ref() == Some(&def.id),
                }
            })
            .collect()
    }

    /// Seeds initial streaming activity across sources at startup.
    pub fn seed(
        &mut self,
        readings: impl IntoIterator<Item = (SourceId, bool, u32)>,
    ) -> ArbiterOutcome {
        let prev_active = self.active.clone();
        for (id, streaming, rate_hz) in readings {
            if let Some(rt) = self.runtime.get_mut(&id) {
                rt.streaming = streaming;
                rt.rate_hz = if streaming { rate_hz } else { 0 };
                if streaming {
                    self.seq = self.seq.saturating_add(1);
                    rt.last_active_seq = self.seq;
                }
            }
        }

        let streaming_ids: Vec<SourceId> = self
            .registry
            .definitions()
            .iter()
            .filter(|def| self.runtime[&def.id].streaming)
            .map(|def| def.id.clone())
            .collect();

        let mut pause_sources = Vec::new();
        if let Some(pinned) = &self.pinned {
            self.active = Some(pinned.clone());
            for id in streaming_ids {
                if &id != pinned {
                    pause_sources.push(id);
                }
            }
        } else if !streaming_ids.is_empty() {
            let chosen = if let Some(last) = &self.last_source
                && self.runtime[last].streaming
            {
                last.clone()
            } else {
                streaming_ids
                    .iter()
                    .max_by(|a, b| {
                        let ra = &self.runtime[*a];
                        let rb = &self.runtime[*b];
                        ra.priority
                            .cmp(&rb.priority)
                            .then_with(|| rb.order.cmp(&ra.order))
                    })
                    .expect("non-empty streaming_ids")
                    .clone()
            };
            self.active = Some(chosen.clone());
            self.last_source = Some(chosen.clone());
            for id in streaming_ids {
                if id != chosen {
                    pause_sources.push(id);
                }
            }
        }

        let active_changed = (self.active != prev_active).then(|| self.active.clone());
        let persist = active_changed.is_some() && self.active.is_some();
        let dsp_target = self.active.as_ref().and_then(|id| {
            let rt = &self.runtime[id];
            rt.streaming.then(|| self.target_for(id, false)).flatten()
        });

        ArbiterOutcome {
            active_changed,
            pinned_changed: None,
            policy_changed: None,
            pause_sources,
            dsp_target,
            persist,
        }
    }

    /// Processes a streaming/sample-rate update for `id`.
    pub fn on_activity(&mut self, id: &SourceId, streaming: bool, rate_hz: u32) -> ArbiterOutcome {
        let Some(rt) = self.runtime.get_mut(id) else {
            return ArbiterOutcome::default();
        };
        let was_streaming = rt.streaming;
        let prev_rate = rt.rate_hz;
        rt.streaming = streaming;
        rt.rate_hz = if streaming { rate_hz } else { 0 };

        if streaming && !was_streaming {
            self.seq = self.seq.saturating_add(1);
            if let Some(rt_mut) = self.runtime.get_mut(id) {
                rt_mut.last_active_seq = self.seq;
            }
            return self.on_stream_started(id);
        }

        if streaming {
            if self.active.as_ref() == Some(id) && (prev_rate != rate_hz || rate_hz > 0) {
                return ArbiterOutcome {
                    dsp_target: self.target_for(id, false),
                    ..ArbiterOutcome::default()
                };
            }
            return ArbiterOutcome::default();
        }

        if was_streaming && self.active.as_ref() == Some(id) {
            return self.on_active_stream_stopped(id);
        }

        ArbiterOutcome::default()
    }

    /// Reports an application-level playback start/resume (e.g. MPD `Paused -> Playing`
    /// while the ALSA device stayed open during pause).
    pub fn on_play_intent(&mut self, id: &SourceId) -> ArbiterOutcome {
        if !self.registry.contains(id) {
            return ArbiterOutcome::default();
        }
        self.seq = self.seq.saturating_add(1);
        if let Some(rt) = self.runtime.get_mut(id) {
            rt.last_active_seq = self.seq;
        }
        self.on_stream_started(id)
    }

    fn on_stream_started(&mut self, started: &SourceId) -> ArbiterOutcome {
        if let Some(pinned) = &self.pinned
            && pinned != started
        {
            return ArbiterOutcome {
                pause_sources: vec![started.clone()],
                ..ArbiterOutcome::default()
            };
        }

        let Some(current) = self.active.clone() else {
            return self.activate_source(started, None, false);
        };

        if &current == started {
            return ArbiterOutcome {
                dsp_target: self.target_for(started, false),
                ..ArbiterOutcome::default()
            };
        }

        if !self.runtime[&current].streaming {
            return self.activate_source(started, None, false);
        }

        let should_preempt = match self.policy {
            AutoSwitchPolicy::LastPlayWins => true,
            AutoSwitchPolicy::FirstPlayWins => false,
            AutoSwitchPolicy::Priority => {
                self.runtime[started].priority > self.runtime[&current].priority
            }
        };

        if should_preempt {
            self.activate_source(started, Some(current), false)
        } else {
            ArbiterOutcome {
                pause_sources: vec![started.clone()],
                ..ArbiterOutcome::default()
            }
        }
    }

    fn on_active_stream_stopped(&mut self, stopped: &SourceId) -> ArbiterOutcome {
        if self.pinned.as_ref() == Some(stopped) {
            return ArbiterOutcome {
                dsp_target: self.target_for(stopped, false),
                ..ArbiterOutcome::default()
            };
        }

        if let Some(next) = self.pick_next_streaming(stopped) {
            return self.activate_source(&next, None, false);
        }

        // Keep `stopped` as the active/last_source when idle so resuming it needs no switch.
        ArbiterOutcome {
            dsp_target: self.target_for(stopped, false),
            ..ArbiterOutcome::default()
        }
    }

    fn pick_next_streaming(&self, excluding: &SourceId) -> Option<SourceId> {
        let mut candidates: Vec<&SourceId> = self
            .runtime
            .iter()
            .filter_map(|(id, rt)| (id != excluding && rt.streaming).then_some(id))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        candidates.sort_by(|a, b| {
            let ra = &self.runtime[*a];
            let rb = &self.runtime[*b];
            match self.policy {
                AutoSwitchPolicy::LastPlayWins => rb
                    .last_active_seq
                    .cmp(&ra.last_active_seq)
                    .then_with(|| rb.priority.cmp(&ra.priority)),
                AutoSwitchPolicy::FirstPlayWins => ra
                    .last_active_seq
                    .cmp(&rb.last_active_seq)
                    .then_with(|| rb.priority.cmp(&ra.priority)),
                AutoSwitchPolicy::Priority => rb
                    .priority
                    .cmp(&ra.priority)
                    .then_with(|| ra.order.cmp(&rb.order)),
            }
        });
        Some(candidates[0].clone())
    }

    /// Manually selects `id` as the active source (clears any pin to another source).
    ///
    /// # Errors
    /// Returns an error string when `id` is not registered.
    pub fn select_source(&mut self, id: &SourceId) -> Result<ArbiterOutcome, String> {
        if !self.registry.contains(id) {
            return Err(format!("unknown audio source '{id}'"));
        }

        let mut pinned_changed = None;
        let mut persist = false;
        if self.pinned.as_ref().is_some_and(|p| p != id) {
            self.pinned = None;
            pinned_changed = Some(None);
            persist = true;
        }

        let previous = self.active.clone();
        if previous.as_ref() == Some(id) {
            return Ok(ArbiterOutcome {
                pinned_changed,
                persist,
                ..ArbiterOutcome::default()
            });
        }

        let displaced_streaming = previous
            .as_ref()
            .filter(|prev| self.runtime[*prev].streaming)
            .cloned();
        let force_detach = displaced_streaming.is_some() && !self.runtime[id].streaming;

        let mut outcome = self.activate_source(id, displaced_streaming, force_detach);
        if pinned_changed.is_some() {
            outcome.pinned_changed = pinned_changed;
            outcome.persist = true;
        }
        Ok(outcome)
    }

    /// Pins playback to `id`, or clears the pin when `None`.
    ///
    /// # Errors
    /// Returns an error string when `Some(id)` is not registered.
    pub fn pin_source(&mut self, id: Option<&SourceId>) -> Result<ArbiterOutcome, String> {
        if let Some(target) = id
            && !self.registry.contains(target)
        {
            return Err(format!("unknown audio source '{target}'"));
        }

        let next_pin = id.cloned();
        if self.pinned == next_pin {
            return Ok(ArbiterOutcome::default());
        }
        self.pinned = next_pin.clone();

        if let Some(target) = id
            && self.active.as_ref() != Some(target)
        {
            let displaced = self
                .active
                .as_ref()
                .filter(|prev| self.runtime[*prev].streaming)
                .cloned();
            let force_detach = displaced.is_some() && !self.runtime[target].streaming;
            let mut outcome = self.activate_source(target, displaced, force_detach);
            outcome.pinned_changed = Some(next_pin);
            outcome.persist = true;
            return Ok(outcome);
        }

        Ok(ArbiterOutcome {
            pinned_changed: Some(next_pin),
            persist: true,
            ..ArbiterOutcome::default()
        })
    }

    /// Updates the auto-switch policy.
    pub fn set_policy(&mut self, policy: AutoSwitchPolicy) -> ArbiterOutcome {
        if self.policy == policy {
            return ArbiterOutcome::default();
        }
        self.policy = policy;
        ArbiterOutcome {
            policy_changed: Some(policy),
            persist: true,
            ..ArbiterOutcome::default()
        }
    }

    /// Explicitly requests pausing/releasing `id`.
    ///
    /// # Errors
    /// Returns an error string when `id` is not registered.
    pub fn release_source(&mut self, id: &SourceId) -> Result<ArbiterOutcome, String> {
        if !self.registry.contains(id) {
            return Err(format!("unknown audio source '{id}'"));
        }
        let pause_sources = vec![id.clone()];

        if self.active.as_ref() == Some(id)
            && self.pinned.as_ref() != Some(id)
            && let Some(next) = self.pick_next_streaming(id)
        {
            let mut outcome = self.activate_source(&next, None, false);
            outcome.pause_sources = pause_sources;
            return Ok(outcome);
        }

        let dsp_target = (self.active.as_ref() == Some(id))
            .then(|| self.target_for(id, false))
            .flatten()
            .map(|mut target| {
                target.streaming = false;
                target
            });

        Ok(ArbiterOutcome {
            pause_sources,
            dsp_target,
            ..ArbiterOutcome::default()
        })
    }

    fn activate_source(
        &mut self,
        id: &SourceId,
        pause_displaced: Option<SourceId>,
        force_detach: bool,
    ) -> ArbiterOutcome {
        let changed = self.active.as_ref() != Some(id);
        self.active = Some(id.clone());
        self.last_source = Some(id.clone());

        ArbiterOutcome {
            active_changed: changed.then(|| Some(id.clone())),
            pinned_changed: None,
            policy_changed: None,
            pause_sources: pause_displaced.into_iter().collect(),
            dsp_target: self.target_for(id, force_detach),
            persist: changed,
        }
    }

    fn effective_rate(&self, id: &SourceId) -> u32 {
        let rt = &self.runtime[id];
        if rt.rate_hz > 0 {
            rt.rate_hz
        } else {
            self.registry
                .get(id)
                .map_or(DEFAULT_SOURCE_RATE_HZ, |def| def.capture.default_rate_hz())
        }
    }

    fn target_for(&self, id: &SourceId, force_detach: bool) -> Option<DspTarget> {
        let def = self.registry.get(id)?;
        let rt = self.runtime.get(id)?;
        Some(DspTarget {
            capture_device: def.capture.camilladsp_device(),
            rate_hz: self.effective_rate(id),
            streaming: rt.streaming,
            force_detach,
        })
    }
}
