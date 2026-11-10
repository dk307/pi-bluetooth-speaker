# Volume Sync Plan - Raspberry Pi Bluetooth Speaker

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
**bluealsa v5.0.0 has no AVRCP target profile** - cannot receive phone volume commands.

Evidence:
- `busctl tree org.bluez` shows only phone's AVRCP controller (`/dev_.../avrcp/player0`)
- No `/org/bluealsa/avrcp` target registered
- Phone volume changes → nowhere to go

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

1. **Test Option 1** (current buffer-tuned config) under real usage for 24-48h
2. If underruns persist → **Option 2** (upgrade bluealsa)
3. If upgrade not feasible → **Option 3** (AVRCP target in bt-manager)
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