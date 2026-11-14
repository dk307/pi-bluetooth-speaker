# Audio Manager Plan — Hardware Volume & Mute Owner

> Status: **implemented & deployed 2026-11-10** (`services/audio-manager`).
> **Update 2026-11-12 (PR #58):** the standalone `crates/amp_curve` crate was
> folded away — `PERCENT_TO_RAW` now lives in `speaker-hardware::amp` (the
> amp's register authority) and the AVRCP rescale in
> `speaker-bluetooth::transport::avrcp_avg_to_percent`. References below are
> historical. D-Bus API verified live on the Pi: `SetLevel 50` →
> Merus raw 125, mute → raw 0, unmute → level restored. bt-manager's
> `sync_volume` task routes phone volume into it (see
> `docs/volume_sync_plan.md`, "Final Architecture"). Written 2026-11-09 after
> live verification that the current mute path is an audible no-op on this
> hardware. Follows `docs/rpi_audio_bluetooth_report.md` §7.6.

## 1. Problem Statement

Live-verified findings (2026-11-09):

1. **Mute is currently an audible no-op.** The UI mute path writes the mute bit
   (`0x80`) into the BlueALSA PCM1 `Volume` property (see
   `crates/bluetooth/src/transport.rs::encode_volume_bytes`). With
   `SoftVolume=false`, `bluealsa-aplay` maps only volume *level* bits into the
   Merus mixer; the Merus card exposes **no ALSA playback mute/switch control**
   (only `A.Mstr Vol` / `B.L Vol` / `C.R Vol` and limiter controls), so the
   mute bit is stored and survives reconnects but never silences the amp
   (verified: amp stayed at -21 dB with mute set; level mapping identical
   muted vs unmuted).
2. **The amp has two-writer risk.** `bluealsa-aplay -M hw:sndrpimerusamp`
   makes BlueALSA the mixer owner for BT volume. Any future volume/mute owner
   (UI encoder, other sources) would fight it.
3. **Coupling of output behavior to one source.** Volume/mute behavior must be
   identical for every audio source (BT today; Airplay, line-in, Spotify
   Connect in future). Mixing output control into a per-source daemon couples
   hardware behavior to that source's lifecycle.

## 2. Decision

Create **`services/audio-manager`** — a single appliance-level daemon that is
the **exclusive owner of the Merus MA12070P amplifier output**
(`A.Mstr Vol` register). All other components are clients.

- bt-manager stays the *Bluetooth* domain (pairing, AVRCP metadata, cover art,
  reconciler) and becomes a **client** of audio-manager for volume sync.
- screen-manager (rotary encoder volume/mute) becomes a **client** too — its
  commands work even when Bluetooth is down.
- Future source routing / handover (Airplay takeover, priority policy) is a
  **separate concern** — a later `sources` module (see §7). It must never touch
  the amp directly; handover = pause/resume sources + switch DSP routing,
  while volume/mute state survives untouched.

## 3. Public D-Bus API (system bus)

Service `org.speaker.AudioManager`, object `/org/speaker/amp`,
interface `org.speaker.AudioManager1`:

```
SetLevel(percent y)      # 0..100 percent; maps to A.Mstr Vol 0..144 raw via the
                         # shared perceptual dB curve table (see §3.2). 
                         # No-op-while-muted: stores level only.
SetMute(muted b)         # mute: write raw 0 (true hardware mute);
                         # unmute: restore remembered level.
Properties:              # all emits-change
  Level y                # current target level in percent
  Muted b                # current mute state
```

Standard interfaces only (`Peer`, `Properties`, `Introspectable`). Every state
change is **pushed** to clients via `PropertiesChanged` — no polling anywhere.

### 3.1 D-Bus Ownership & Startup Sequence

On startup the daemon must:

1. Validate the Merus ALSA card/control exists (§5.1).
2. Connect to the system bus (`zbus::Connection::system().await?`); on failure
   log typed error and exit (systemd `Restart=on-failure` restarts).
3. `RequestName` with `AllowReplacement=false`, `DoNotQueue=true` via
   `zbus::fdo::DBusProxy`. If the name is taken → log typed error and exit.
4. On successful `RequestName`, call `sd_notify("READY=1")` so systemd marks
   the `Type=dbus` service as started.
5. Read state file once, apply restored level/mute to the amp (§5.2).
6. Enter the event loop (`tokio::select!` over incoming method-call stream).

On connection loss (`zbus` returns `ConnectionError`): log, exit; systemd
restarts and state file restores amp (no persistent amp drift). Graceful
shutdown on SIGTERM: `ReleaseName` + `sd_notify("STOPPING=1")`.

### 3.2 Shared Percent→Raw Curve Table

Both bt-manager (AVRCP 0..127 → percent) and audio-manager (percent → raw
0..144) must use **the same curve** so round-trips are exact. Extract the
calibration points from the report §7.6.4 into a shared crate:

```rust
// crates/amp_curve/src/lib.rs
pub const PERCENT_TO_RAW: [u16; 101] = [
    0,   // 0% → 0 (-144 dB, mute)
    1,   // 1%
    ...
    144, // 100% → 144 (0.00 dB)
];
```

Compile-time invariant proofs:
```rust
const _: () = {
    assert!(PERCENT_TO_RAW[0] == 0);
    assert!(PERCENT_TO_RAW[100] == 144);
    // monotonicity, no duplicates, etc.
};
```

The table is piecewise-linear on the dB domain (per the Merus log curve);
bt-manager's AVRCP→percent uses the *inverse* table (also in the crate) so
`SetLevel(round(avrcp_to_percent(v)))` preserves the original AVRCP step.

## 4. Event-Driven Design (Hard Requirement: No Polling)

The daemon must contain **zero timers, zero interval loops, zero polling**:

- `tokio::select!` over:
  - the zbus connection's incoming method-call stream (`SetLevel`, `SetMute`)
  - nothing else. No keepalive is needed — the Merus register has no idle
    timeout (unlike the OBEX/BIP session that forced `cover_provider`'s
    fallback poll in bt-manager; that poll is BT-domain and stays there).
- **One-shot state restore** at startup: read the runtime state file once,
  apply once to the amp. No periodic re-sync.
- Clients (screen UI, bt-manager) receive state via `PropertiesChanged` push.

## 5. Core State Machine & Rules

```
AmpState { level_raw: u16 /*0..144*/, muted: bool }   // behind one Mutex
```

- **Single writer:** all mixer writes serialized through the state owner; the
  state mutex is consolidated (no per-field mutexes).
- **Transactional mutation:** internal state (`level_raw`, `muted`) is mutated
  only *after* a successful mixer write (fallible op first, then commit).
- **Mute semantics:**
  - `SetMute(true)` → remember `level_raw`, write raw 0 (-144 dB = chip's
    true hardware mute).
  - `SetMute(false)` → write remembered `level_raw`.
  - `SetLevel(p)` while muted → update stored level only; do **not** write the
    amp (unmute reveals the new level).
- **Domain rules:**
  - percent→raw mapping follows the report §7.6.4 curve; endpoints exact
    (percent 0 → raw 0; 100 → raw 144 / 0.00 dB). Guard float math with
    finiteness asserts if floats are involved; prefer integer tables.
  - Poisoned-mutex recovery: `lock().unwrap_or_else(|p| p.into_inner())` +
    `warn!` — never drop state silently.
  - Mixer oplock serializes multi-step register transactions.
- **Boot & crash safety:** restore from a small runtime state file
  (`/var/lib/speaker/amp-state.json`); on exit leave the amp at last known
  state (never silently 0). Do NOT rely on `alsactl asound.state` (documented
  stale-restore risk in report §7.6.5).
- **Boot visibility:** log resolved config and applied restored state at
  `info!`; amp card validation errors typed (`AmpError::CardMissing` etc.).
- **Testing:** `AmpBackend` trait; `new_mocked() -> (Self, AmpMockCtrl)`
  fault-injection pairing (M-MOCKABLE-SYSCALLS). Integration tests in `tests/`
  (never inline `#[cfg(test)]` in `src/`), host-executable, covering:
  mute persistence, level-while-muted, percent↔raw curve endpoints
  (0→mute, 100→0 dB, compile-time `const` invariant proofs), poisoned-mutex
  recovery, error paths, and the event-driven surface (call-driven, no timers).
  Coverage gate ≥85% lines/functions.

### 5.1 Hardware Validation at Startup

Before claiming the D-Bus name, the daemon must verify the Merus card is
present and the `A.Mstr Vol` control is writable:

- Open the ALSA control interface for `hw:sndrpimerusamp` (or the stable
  card name if indexed differently) via `alsa::ctl::Ctl::new()`.
- Find the `A.Mstr Vol` element (`snd_ctl_elem_id_t` with iface MIXER, name
  "A.Mstr Vol", index 0).
- Read its range (min/max should be 0/144); write a test value (e.g. current
  level) and read back to confirm round-trip.
- On **any failure** (card missing, control missing, range mismatch,
  read/write error): log typed `AmpError::HardwareValidation { reason }` at
  `error!` and exit with non-zero. systemd restarts; the service does **not**
  spin retrying — the validation is the gate.

This also covers the boot-race case where `sound.target` is reached but the
kernel ALSA driver hasn't probed the card yet: validation fails, systemd
restarts a few seconds later when the card is ready.

### 5.2 State File Atomic Write & tmpfiles.d

Runtime state file: `/var/lib/speaker/amp-state.json` (root:root, 0600).

- Directory created via `tmpfiles.d/speaker.conf`:
  ```
  d /var/lib/speaker 0700 root root -
  ```
- Write path: serialize `AmpState { level_raw, muted }` → temp file
  `/var/lib/speaker/amp-state.json.tmp.XXXXXX` → `fsync` → atomic `rename`
  over destination. Never partial writes.
- Read path: on startup, if file missing → start at default (percent 50,
  unmuted); if corrupt → log `warn!`, fall back to default.
- State written on: every successful `SetLevel`/`SetMute` (after mixer commit),
  and on graceful shutdown (SIGTERM).

### 5.3 zbus Connection Drop → Exit

The event loop runs `tokio::select!` over the zbus connection's incoming
method-call stream. If the stream ends (connection closed by the bus daemon
or transport error), the loop exits → `main` returns error → process exits
non-zero. systemd `Restart=on-failure` restarts the service; state file
restores the amp to the last known level/mute. **No reconnection logic
inside the daemon** — the restart is the reconnection.

### 5.4 Percent→Raw Curve Table (shared)

See §3.2. The `crates/amp_curve` crate provides:

```rust
pub const PERCENT_TO_RAW: [u16; 101];     // 0..100 → 0..144
pub const RAW_TO_DB_TENTHS: [i16; 145];   // raw 0..144 → dB × 10 (for logging)
pub const AVRCP_TO_PERCENT: [u8; 128];    // AVRCP 0..127 → 0..100 (inverse)
```

Both audio-manager (percent→raw) and bt-manager (AVRCP→percent) depend on
this crate; the curve is the single source of truth.

## 6. Integration & Startup Ordering

Unit changes (current live ordering audited 2026-11-09):

```ini
# audio-manager.service (new)
After=sound.target
Requires=sound.target
Type=dbus
BusName=org.speaker.AudioManager
ExecStart=/usr/local/bin/audio-manager
Restart=on-failure
RestartSec=2
# Note: the daemon must call sd_notify("READY=1") after RequestName succeeds
# (not automatically done by zbus).

# bt-manager.service (modified; fixes an existing gap too)
After=bluetooth.service bluealsa.service audio-manager.service
#  ^ previously had Wants=bluealsa but NO After=bluealsa — could start before
#    BlueALSA registered on D-Bus and self-heal only via retry loops.

# screen-manager.service (modified)
After=... audio-manager.service   # add
```

- `bluetooth → bluealsa → bluealsa-aplay → camilladsp` chain unchanged.
- audio-manager is deliberately **independent of bluetooth** (amp works with
  BT down); anchored to `sound.target` (ALSA cards ready).
- bluealsa-aplay keeps `-M` **until phase 2** (see §8) to avoid a
  two-writer window.

### 6.1 tmpfiles.d Fragment

Deployed with the package/install:

```
/etc/tmpfiles.d/speaker.conf
d /var/lib/speaker 0700 root root -
```

Ensures the state directory exists before `audio-manager.service` starts.

## 7. Future Extension: Source Routing (not in this PR)

Later `sources` module in the same daemon:

- **Source registry & priority policy** (e.g. `Airplay > Bluetooth > line-in`),
  manual override pin from the UI.
- **Handover = pause/resume sources + switch CamillaDSP input routing** (via
  its WebSocket). **Never touches the amp** — volume/mute survive every
  switch; that is the payoff of the amp-owner split.
- BT pause/resume commands exposed via bt-manager's D-Bus surface when the
  Airplay source lands.

## 8. Phased Delivery

| Phase | Scope |
|---|---|
| **1 (this PR)** | `services/audio-manager` crate: AmpState machine, mock backend, D-Bus surface, unit files, bt-manager/screen-manager ordering fixes, plan docs. Deployed but **not yet** authoritative for BT volume. |
| **2 (follow-up)** | bt-manager gains a volume-sync client (transport PropertiesChanged → `SetLevel`, purely signal-driven); **remove** `-M hw:sndrpimerusamp --mixer-control` from `bluealsa-aplay.service`; drop the obsolete uncommitted `sync_volume` code in bt-manager (`daemon.rs`/`agent.rs` — 0–100% vs 0..127 scale bug and dual-writer oscillation made it a dead end); **also strip bt-manager's mute-bit encoding** (`encode_volume_bytes`/`apply_mute_to_volume_bytes` in `crates/bluetooth/src/transport.rs`) since the BlueALSA PCM mute path is dead (no listener after `-M` removal). |
| **3 (future)** | `sources` module: registry, priority policy, Airplay/line-in sources, CamillaDSP routing handover. |

## 9. Docs Updated With This Plan

- `docs/volume_sync_plan.md`: resolution banner updated — the "do not
  implement polled sync_volume fallback" warning now points here as the
  replacement design (single amp owner, event-driven).
- `docs/rpi_audio_bluetooth_report.md` §7.6: architecture becomes a
  four-stage chain (BlueZ → bt-manager → audio-manager → Merus) once phase 2
  lands.
