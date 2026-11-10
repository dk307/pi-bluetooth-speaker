# Volume Sync Plan - Raspberry Pi Bluetooth Speaker

> **RESOLVED (2026-11-09):** Phone → hardware volume sync is fully working via the
> native BlueZ → bluez-alsa → `bluealsa-aplay` → Merus chain. The root cause was
> **BlueZ 5.88's new `[AVRCP] VolumeCategory = true` default** (commit `8baabd2f5`),
> which rejects phones whose AVRCP Target SDP record lacks the category-2 feature
> bit (Pixel = TG 1.4, no category-2). BlueZ then never registered
> `EVENT_VOLUME_CHANGED`, so the phone never sent `SetAbsoluteVolume` and
> `MediaTransport1.Volume` stayed frozen. Fix: `[AVRCP] VolumeCategory = false` in
> `/etc/bluetooth/main.conf` + `systemctl restart bluetooth` (no re-pair needed);
> committed as `baseline/etc/bluetooth/main.conf`. Full diagnosis and live
> verification: `docs/rpi_audio_bluetooth_report.md` § 7.6.1 (Rank 0 gate).
>
> Correction to the analysis below: "bluealsa v5.0.0 has no AVRCP target profile"
> was **not** the blocker — bluez-alsa never implements AVRCP in any version; BlueZ
> owns the AVRCP TG role and forwards volume into `MediaTransport1.Volume`, which
> bluealsad / `bluealsa-aplay` pick up natively. Options 1–4 are therefore moot and
> kept only as history. Do **not** implement Option 3/4 or a polled `sync_volume`
> fallback in bt-manager: native sync works, and a second writer with a different
> volume curve (0–100 percent fed into a 0–127 AVRCP mapper) would fight
> `bluealsa-aplay` and cause audible volume oscillation.

## Current State (as of Nov 2026)

### Hardware
- **DAC/Amp**: Merus MA12070P (Mono, 0-144 raw, fake dB scale 0-70=mute)
- **Bluetooth**: Raspberry Pi 3 Model B (BCM2837), BlueZ 5.55+, bluealsa v5.0.0

### Working Config (Minimal)
```ini
# /etc/systemd/system/bluealsa.service
ExecStart=/usr/bin/bluealsad -S -p a2dp-sink --all-codecs -c-opus --sbc-quality=xq+

# /etc/systemd/system/bluealsa-aplay.service
ExecStart=/usr/bin/bluealsa-aplay -S -D default -M hw:sndrpimerusamp --mixer-control="A.Mstr Vol" --pcm-buffer-time=500000 --pcm-period-time=100000
```

### bt-manager Features
- Auto-trust pairing agent (seeds BlueZ device info `Volume=` at pairing)
- Cover art provider (direct BIP)
- Status monitor loop (5s polling)
- **No volume sync code** (reverted)

---

## Problem Analysis

### Volume Chain (Desired)
```
Phone AVRCP (0-127) 
    → BlueZ MediaTransport1.Volume 
    → bluealsad (native A2DP volume) 
    → bluealsa-aplay --volume=auto 
    → ALSA mixer (A.Mstr Vol 0-144)
    → Merus hardware
```

### What Works
1. **Initial pairing**: bt-manager seeds `Volume=` in `/var/lib/bluetooth/.../info` → BlueZ initializes transport correctly
2. **Manual BlueZ → Hardware**: `gdbus set MediaTransport1.Volume` → bluealsa-aplay → Hardware
3. **bluealsa-aplay --volume=auto** (native): Subscribes to `MediaTransport1.Volume` signals → writes ALSA mixer

### What's Broken
**~~bluealsa v5.0.0 has no AVRCP target profile~~ → superseded:** the actual blocker was
BlueZ 5.88's `[AVRCP] VolumeCategory = true` gate (see resolution banner above).
Original (incorrect) evidence notes kept for history:
- `busctl tree org.bluez` shows only phone's AVRCP controller (`/dev_.../avrcp/player0`)
- No `/org/bluealsa/avrcp` target registered
- Phone volume changes → nowhere to go

**Actual root cause (fixed 2026-11-09):** BlueZ 5.88 `[AVRCP] VolumeCategory = true`
default rejects the Pixel's TG record (1.4, no category-2) in `avrcp_volume_supported()`,
so `target_init()` skips registering `EVENT_VOLUME_CHANGED` and the phone never sends
`SetAbsoluteVolume` (PDU 0x50). Verified via `bluetoothd -d`: after the fix the PDU 0x50
is accepted and the transport volume tracks every key press.

**Underrun bug**: bluealsa-aplay loses BlueZ subscription after "Draining ALSA playback PCM to avoid underrun" restart.

---

## Proposed Solutions (Priority Order)

### Option 1: Buffer Tuning (Already Applied)
- Increase `--pcm-buffer-time=500000` (500ms) and `--pcm-period-time=100000` (100ms)
- Prevents underrun → bluealsa-aplay keeps subscription → native sync works
- **Status**: Applied, needs stability testing under load
- **Risk**: May not fully prevent underruns on Pi 3

### Option 2: Upgrade bluealsa to v5.1+ or Latest
- Newer versions may include AVRCP target support
- Native `--volume=auto` + AVRCP target = full sync
- **Status**: Need to check availability on DietPi/Debian 13
- **Risk**: May require rebuilding from source

### Option 3: Implement AVRCP Target in bt-manager
Register `/org/bluealsa/avrcp` as `AVRCPTarget1` + `MediaPlayer1`:

```rust
// Event-based, no polling
// 1. Register with BlueZ Media1.RegisterPlayer
// 2. Handle PropertiesChanged(MediaPlayer1.Volume) from phone
// 3. Write to MediaTransport1.Volume on active transport
// 4. bluealsa-aplay --volume=auto propagates to hardware
// 5. ALSA mixer events → MediaPlayer1.Volume for reverse sync
```

**State tracking needed for mute:**
```rust
struct VolumeState {
    volume: u8,        // 1-127 (AVRCP)
    muted: bool,
    prev_volume: u8,   // for unmute restore
}
```

**Requires**: ALSA async mixer events (`snd_mixer_handle_events`)

### Option 4: Unified Volume Sync in bt-manager (Replace bluealsa-aplay sync)
- bluealsa-aplay with `--volume=none` (pure audio)
- bt-manager: AVRCP target + ALSA mixer watch + BlueZ transport watch
- Single authority, fully event-driven
- **Complexity**: High (reimplements bluealsa-aplay volume logic)

---

## Decision Matrix

| Criteria | Option 1 (Buffers) | Option 2 (Upgrade) | Option 3 (AVRCP Target) | Option 4 (Unified) |
|----------|-------------------|-------------------|------------------------|-------------------|
| Code changes | None | Build/upgrade | ~200 lines bt-manager | ~500 lines bt-manager |
| Reliability | Medium (workaround) | High (native fix) | High (event-driven) | Highest (full control) |
| Mute handling | Native | Native | Custom state needed | Custom state needed |
| Maintenance | Zero | Upstream | Our code | Our code |
| Timeline | Done | Hours-days | Days | Week+ |

---

## Recommended Path

> **Superseded 2026-11-09:** resolved by the `[AVRCP] VolumeCategory = false` fix
> (see banner). No bt-manager code changes are needed or desired for volume sync.
>
> **Follow-up design (2026-11-09):** the remaining mute/volume gaps — mute is an
> audible no-op (the Merus mixer has no ALSA mute switch for the BlueALSA mute
> bit to drive) and the amp is owned by `bluealsa-aplay` — are addressed by the
> new **audio-manager** service plan: a single event-driven hardware owner of
> `A.Mstr Vol` (mute = raw 0 / -144 dB, level restore on unmute) with bt-manager
> and the UI as clients. Do **not** implement Options 3/4 below or the obsolete
> polled `sync_volume` code in bt-manager (0–100% vs 0..127 scale bug; dual-writer
> oscillation with `bluealsa-aplay`). See **`docs/audio_manager_plan.md`** —
> Options 3/4 are replaced by its phase 2.

1. **Test Option 1** (current buffer-tuned config) under real usage for 24-48h
   *(note: the buffer flags were never actually deployed to the live unit —
   `bluealsa-aplay.service` still runs without `--pcm-buffer-time/--pcm-period-time`)*
2. If underruns persist → **Option 2** (upgrade bluealsa)
3. ~~If upgrade not feasible → **Option 3** (AVRCP target in bt-manager)~~
   → replaced by `docs/audio_manager_plan.md` phase 2
4. **Avoid Option 4** unless absolutely necessary

---

## Implementation Notes (Option 3)

### D-Bus Interfaces to Implement
```rust
// org.bluez.AVRCPTarget1 - marker interface
// org.bluez.MediaPlayer1 - writable Volume property
interface MediaPlayer1 {
    // ... existing methods ...
    // Volume: readable/writable u16 (0-127)
    // PropertiesChanged signal on change
}
```

### Registration
```bash
gdbus call -y -d org.bluez -o /org/bluez/hci0 \
  -m org.bluez.Media1.RegisterPlayer \
  /org/bluealsa/avrcp \
  "{'Name': <'BlueALSA AVRCP'>, 'Role': <'target'>}"
```

### Volume Mapping (AVRCP 0-127 ↔ Merus 0-144)
```rust
// From docs/rpi_audio_bluetooth_report.md §7.6
// Transport → Merus: 0→0, 32→124, 64→134, 96→140, 127→144
// Inverse with linear interpolation between points
```

### Mute State Machine
```
MUTE:     muted=true, prev=volume, set both sides to 0
UNMUTE:   muted=false, restore prev, set both sides
VOL_CHG:  if !muted: volume=new, write both sides
```

---

## Files to Modify (Option 3)

- `services/bt-manager/src/avrcp_target.rs` (new)
- `services/bt-manager/src/alsa_mixer.rs` (new, async ALSA events)
- `services/bt-manager/src/volume_sync.rs` (new, shared state)
- `services/bt-manager/src/daemon.rs` (spawn components)
- `services/bt-manager/Cargo.toml` (add `alsa-async` or `alsa` crate)

---

## Testing Checklist

- [ ] Fresh pairing seeds correct Volume=
- [ ] Phone volume change → Hardware follows (during playback)
- [ ] Hardware knob → Phone volume follows (during playback)
- [ ] Mute on phone → Hardware mutes, remembers level
- [ ] Unmute on phone → Hardware restores level
- [ ] Mute on hardware → Phone shows mute
- [ ] Underrun stress test (high CPU load + playback)
- [ ] Reconnect after Pi reboot preserves volume
- [ ] Multiple connect/disconnect cycles

---

## References

- `docs/rpi_audio_bluetooth_report.md` §7.6 (volume architecture)
- `services/bt-manager/src/agent.rs` (device info seeding)
- BlueZ Media API: `org.bluez.MediaTransport1`, `org.bluez.MediaPlayer1`, `org.bluez.AVRCPTarget1`
- bluealsa v5.0.0 source: volume handling in `src/aplay.c`