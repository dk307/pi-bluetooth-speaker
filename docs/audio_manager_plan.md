# Audio Manager Plan — Hardware Volume & Mute Owner

> Status: **planned** (not yet implemented). Written 2026-11-09 after live
> verification that the current mute path is an audible no-op on this hardware.
> Follows `docs/volume_sync_plan.md` (native BT volume sync, resolved) and
> `docs/rpi_audio_bluetooth_report.md` §7.6.

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
                         # perceptual dB curve (same mapping table as §7.6.4 of
                         # the report). No-op-while-muted: stores level only.
SetMute(muted b)         # mute: write raw 0 (true hardware mute);
                         # unmute: restore remembered level.
Properties:              # all emits-change
  Level y                # current target level in percent
  Muted b                # current mute state
```

Standard interfaces only (`Peer`, `Properties`, `Introspectable`). Every state
change is **pushed** to clients via `PropertiesChanged` — no polling anywhere.

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

## 6. Integration & Startup Ordering

Unit changes (current live ordering audited 2026-11-09):

```ini
# audio-manager.service (new)
After=sound.target
Requires=sound.target
Type=dbus
BusName=org.speaker.AudioManager
ExecStart=/usr/local/bin/audio-manager

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
| **2 (follow-up)** | bt-manager gains a volume-sync client (transport PropertiesChanged → `SetLevel`, purely signal-driven); **remove** `-M hw:sndrpimerusamp --mixer-control` from `bluealsa-aplay.service`; drop the obsolete uncommitted `sync_volume` code in bt-manager (`daemon.rs`/`agent.rs` — 0–100% vs 0..127 scale bug and dual-writer oscillation made it a dead end). |
| **3 (future)** | `sources` module: registry, priority policy, Airplay/line-in sources, CamillaDSP routing handover. |

## 9. Docs Updated With This Plan

- `docs/volume_sync_plan.md`: resolution banner updated — the "do not
  implement polled sync_volume fallback" warning now points here as the
  replacement design (single amp owner, event-driven).
- `docs/rpi_audio_bluetooth_report.md` §7.6: architecture becomes a
  four-stage chain (BlueZ → bt-manager → audio-manager → Merus) once phase 2
  lands.
