# Single Active Bluetooth Audio Source

Multiple Bluetooth devices (e.g., a phone and a laptop) may remain paired and connected to the Raspberry Pi speaker at the same time, but **only one device is allowed to play audio at a time** (first-play-wins while streaming; another device can take over once the active stream stops or disconnects).

---

## 1. Audio-Path Gate (`bluealsa-aplay --single-audio`)

`baseline/etc/systemd/system/bluealsa-aplay.service` runs:

```ini
ExecStart=/usr/local/bin/bluealsa-aplay -S -D default --volume=none --single-audio
```

- **First-play-wins:** Each connected BlueALSA A2DP PCM gets a worker thread, starting inactive. The first worker to receive audio acquires the single ALSA playback slot (`dsp_dmix` in `baseline/etc/asound.conf`) until its PCM `Running` property drops to `false` or the PCM is removed.
- **Shut-out streams:** Any other device that starts streaming while the slot is held has its audio read and discarded by `bluealsa-aplay`, preventing mixed audio.

---

## 2. Why `bt-manager` Arbitration Is Still Required

1. **BlueZ 5.88 `MediaPlayer1` Path (`avrcp/player0`):**
   `bluealsa-aplay --single-audio` attempts to pause shut-out players at `/org/bluez/hci0/dev_XX/player0`, which fails on BlueZ 5.88 (where players live at `.../avrcp/player0`). Without a real AVRCP `Pause`, a shut-out phone continues advancing silently and suddenly becomes audible when the active device stops.
2. **D-Bus Single Source of Truth (`org.speaker.Bluetooth`):**
   `bluealsa-aplay` does not publish which worker owns the slot. Without an arbiter, `crates/bluetooth`, `cover_provider`, `volume_sync`, and `screen-manager` fall back to lexicographic D-Bus object path order (e.g., `30:...` before `C4:...`), which can follow a silent shut-out device instead of the audible one.

---

## 3. Architecture

- **`bt-manager` (`services/bt-manager/src/active_source.rs`):**
  - Subscribes to `org.bluealsa` `InterfacesAdded`, `InterfacesRemoved`, and `PropertiesChanged` (`Running`, `ClientDelay`).
  - Runs `ActiveSourceState` using the same first-play-wins rule and promotes the next still-running PCM (preferring the most recent `ClientDelay` update) when the active PCM stops or disconnects.
  - Sends `org.bluez.MediaPlayer1.Pause()` once per `false -> true` transition of a shut-out device using the real player path resolved via `DbusClient::find_player_path_for_device`.
  - Publishes `ActiveDevice` (`s`, canonical MAC or `""`) on system D-Bus:
    - Bus name: `org.speaker.Bluetooth` (policy: `baseline/etc/dbus-1/system.d/org.speaker.Bluetooth.conf`)
    - Object path: `/org/speaker/bluetooth`
    - Interface: `org.speaker.Bluetooth1`
- **Volume Sync (`services/bt-manager/src/volume_sync.rs`):**
  - Scopes forward (`BlueALSA -> audio-manager`) and reverse (`audio-manager -> BlueALSA`) sync to the active device's PCM path, and re-arms fresh-PCM settle + reverse reconcile when the active device switches.
- **Consumers (`crates/bluetooth`, `cover_provider`, `screen-manager`):**
  - `DbusClient` reads `ActiveDevice` (or local arbiter state inside `bt-manager`) and passes it as `preferred` to `parse_bluez_all_for`, `resolve_pcm_volume_target_for`, and `resolve_pcm_mute_target_for`.
  - `screen-manager` passes `bt.active_device()` to `select_active_pcm_for` and `BtLinkDetails::select`.
