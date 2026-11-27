//! D-Bus interface (`org.speaker.AudioSources1`) at `/org/speaker/sources`.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, warn};
use zbus::fdo::Error as ZBusError;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface};

use crate::sources::arbiter::SourceStatusView;
use crate::sources::registry::{AutoSwitchPolicy, SourceId};

/// D-Bus object path for multi-source management.
pub const AUDIO_SOURCES_OBJECT_PATH: &str = "/org/speaker/sources";

/// D-Bus interface name for multi-source management.
pub const AUDIO_SOURCES_INTERFACE: &str = "org.speaker.AudioSources1";

/// Command payload sent from D-Bus handlers to [`crate::sources::SourceManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceCommandKind {
    /// Switch active source immediately to `id`.
    SelectSource(SourceId),
    /// Pin playback to `Some(id)`, or unpin on `None`.
    PinSource(Option<SourceId>),
    /// Change the auto-switch policy.
    SetAutoSwitchPolicy(AutoSwitchPolicy),
    /// Pause/release `id`.
    ReleaseSource(SourceId),
    /// Application-level play/resume intent (e.g. MPD `Paused -> Playing` while ALSA stays open).
    ReportPlayIntent(SourceId),
}

/// Command envelope with a synchronous reply channel for D-Bus callers.
#[derive(Debug)]
pub struct SourceCommand {
    /// Requested operation.
    pub kind: SourceCommandKind,
    /// Optional reply channel (`Ok(())` or error string).
    pub reply: Option<oneshot::Sender<Result<(), String>>>,
}

impl SourceCommand {
    /// Convenience constructor without a reply channel.
    #[must_use]
    pub fn fire_and_forget(kind: SourceCommandKind) -> Self {
        Self { kind, reply: None }
    }
}

/// Thread-safe read snapshot published by [`crate::sources::SourceManager`] for D-Bus reads.
#[derive(Debug, Clone)]
pub struct AudioSourcesSnapshot {
    inner: Arc<Mutex<SnapshotInner>>,
    active_tx: watch::Sender<Option<SourceId>>,
}

impl Default for AudioSourcesSnapshot {
    fn default() -> Self {
        Self::new(AutoSwitchPolicy::default())
    }
}

#[derive(Debug, Clone, Default)]
struct SnapshotInner {
    active_source: Option<SourceId>,
    pinned_source: Option<SourceId>,
    policy: AutoSwitchPolicy,
    sources: Vec<SourceStatusView>,
}

impl AudioSourcesSnapshot {
    /// Creates an empty snapshot with default policy.
    #[must_use]
    pub fn new(policy: AutoSwitchPolicy) -> Self {
        let (active_tx, _) = watch::channel(None);
        Self {
            inner: Arc::new(Mutex::new(SnapshotInner {
                active_source: None,
                pinned_source: None,
                policy,
                sources: Vec::new(),
            })),
            active_tx,
        }
    }

    /// Subscribes to `ActiveSource` transitions.
    #[must_use]
    pub fn subscribe_active(&self) -> watch::Receiver<Option<SourceId>> {
        self.active_tx.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SnapshotInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replaces the published snapshot atomically.
    pub fn update(
        &self,
        active_source: Option<SourceId>,
        pinned_source: Option<SourceId>,
        policy: AutoSwitchPolicy,
        sources: Vec<SourceStatusView>,
    ) {
        let mut g = self.lock();
        g.active_source = active_source.clone();
        g.pinned_source = pinned_source;
        g.policy = policy;
        g.sources = sources;
        drop(g);
        self.active_tx.send_if_modified(|cur| {
            if *cur == active_source {
                false
            } else {
                *cur = active_source;
                true
            }
        });
    }

    /// Reads the active source wire string (`""` when none).
    #[must_use]
    pub fn active_source_str(&self) -> String {
        self.lock()
            .active_source
            .as_ref()
            .map_or_else(String::new, ToString::to_string)
    }

    /// Reads the pinned source wire string (`""` when auto-switching).
    #[must_use]
    pub fn pinned_source_str(&self) -> String {
        self.lock()
            .pinned_source
            .as_ref()
            .map_or_else(String::new, ToString::to_string)
    }

    /// Reads the auto-switch policy wire string.
    #[must_use]
    pub fn policy_str(&self) -> String {
        self.lock().policy.as_str().to_owned()
    }

    /// Reads all registered source status tuples `(id, display_name, state, priority, rate_hz, is_active)`.
    #[must_use]
    pub fn sources_tuples(&self) -> Vec<(String, String, String, u8, u32, bool)> {
        self.lock()
            .sources
            .iter()
            .map(|s| {
                (
                    s.id.to_string(),
                    s.display_name.to_string(),
                    s.state.to_owned(),
                    s.priority,
                    s.sample_rate_hz,
                    s.is_active,
                )
            })
            .collect()
    }
}

/// D-Bus interface serving `org.speaker.AudioSources1`.
#[derive(Debug)]
pub struct AudioSourcesInterface {
    snapshot: AudioSourcesSnapshot,
    commands: mpsc::Sender<SourceCommand>,
}

impl AudioSourcesInterface {
    /// Creates an interface backed by `snapshot` and command channel `commands`.
    #[must_use]
    pub fn new(snapshot: AudioSourcesSnapshot, commands: mpsc::Sender<SourceCommand>) -> Self {
        Self { snapshot, commands }
    }

    async fn dispatch(&self, kind: SourceCommandKind) -> Result<(), ZBusError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(SourceCommand {
                kind,
                reply: Some(reply_tx),
            })
            .await
            .map_err(|e| ZBusError::Failed(format!("source manager channel closed: {e}")))?;
        reply_rx
            .await
            .map_err(|e| ZBusError::Failed(format!("source manager dropped reply: {e}")))?
            .map_err(ZBusError::Failed)
    }
}

#[interface(name = "org.speaker.AudioSources1")]
impl AudioSourcesInterface {
    /// Selects `id` as the active audio source immediately.
    pub async fn select_source(&self, id: &str) -> Result<(), ZBusError> {
        let parsed = SourceId::parse(id).map_err(ZBusError::InvalidArgs)?;
        self.dispatch(SourceCommandKind::SelectSource(parsed)).await
    }

    /// Pins playback to `id`, or clears the pin when `id` is empty (`""`).
    pub async fn pin_source(&self, id: &str) -> Result<(), ZBusError> {
        let parsed = if id.trim().is_empty() {
            None
        } else {
            Some(SourceId::parse(id).map_err(ZBusError::InvalidArgs)?)
        };
        self.dispatch(SourceCommandKind::PinSource(parsed)).await
    }

    /// Sets the auto-switch policy (`"last-play"`, `"first-play"`, or `"priority"`).
    pub async fn set_auto_switch_policy(&self, policy: &str) -> Result<(), ZBusError> {
        let parsed = AutoSwitchPolicy::parse(policy).map_err(ZBusError::InvalidArgs)?;
        self.dispatch(SourceCommandKind::SetAutoSwitchPolicy(parsed))
            .await
    }

    /// Requests that `id` pause/yield playback.
    pub async fn release_source(&self, id: &str) -> Result<(), ZBusError> {
        let parsed = SourceId::parse(id).map_err(ZBusError::InvalidArgs)?;
        self.dispatch(SourceCommandKind::ReleaseSource(parsed))
            .await
    }

    /// Currently active source ID, or `""` when idle.
    #[zbus(property)]
    pub async fn active_source(&self) -> String {
        self.snapshot.active_source_str()
    }

    /// Currently pinned source ID, or `""` when auto-switching.
    #[zbus(property)]
    pub async fn pinned_source(&self) -> String {
        self.snapshot.pinned_source_str()
    }

    /// Active auto-switch policy string.
    #[zbus(property)]
    pub async fn auto_switch_policy(&self) -> String {
        self.snapshot.policy_str()
    }

    /// Registered source snapshots `(id, display_name, state, priority, sample_rate_hz, is_active)`.
    #[zbus(property)]
    pub async fn sources(&self) -> Vec<(String, String, String, u8, u32, bool)> {
        self.snapshot.sources_tuples()
    }
}

/// Exports [`AudioSourcesInterface`] at `/org/speaker/sources` and returns its [`SignalEmitter`].
///
/// # Errors
/// Returns [`ZBusError`] if registering the object or creating the signal emitter fails.
pub async fn export_sources_interface(
    connection: &Connection,
    interface: AudioSourcesInterface,
) -> Result<SignalEmitter<'static>, ZBusError> {
    connection
        .object_server()
        .at(AUDIO_SOURCES_OBJECT_PATH, interface)
        .await?;
    let emitter = SignalEmitter::new(connection, AUDIO_SOURCES_OBJECT_PATH)
        .map_err(|e| ZBusError::Failed(e.to_string()))?
        .to_owned();
    debug!("Exported {AUDIO_SOURCES_INTERFACE} at {AUDIO_SOURCES_OBJECT_PATH}");
    Ok(emitter)
}

/// Emits `PropertiesChanged` signals for updated source properties on `connection`.
pub(crate) async fn emit_property_changes(
    connection: &Connection,
    emitter: &SignalEmitter<'static>,
    active_changed: bool,
    pinned_changed: bool,
    policy_changed: bool,
    sources_changed: bool,
) {
    let Ok(iface_ref) = connection
        .object_server()
        .interface::<_, AudioSourcesInterface>(AUDIO_SOURCES_OBJECT_PATH)
        .await
    else {
        return;
    };
    let iface = iface_ref.get().await;
    if active_changed && let Err(e) = iface.active_source_changed(emitter).await {
        warn!(error = %e, "Failed to emit ActiveSource PropertiesChanged");
    }
    if pinned_changed && let Err(e) = iface.pinned_source_changed(emitter).await {
        warn!(error = %e, "Failed to emit PinnedSource PropertiesChanged");
    }
    if policy_changed && let Err(e) = iface.auto_switch_policy_changed(emitter).await {
        warn!(error = %e, "Failed to emit AutoSwitchPolicy PropertiesChanged");
    }
    if sources_changed && let Err(e) = iface.sources_changed(emitter).await {
        warn!(error = %e, "Failed to emit Sources PropertiesChanged");
    }
}
