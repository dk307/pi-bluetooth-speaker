# CamillaDSP 5.0.0-beta.2 Upgrade & Dynamic Sample-Rate Pipeline

**Branch:** `feat/camilladsp`  
**Target Hardware:** Raspberry Pi 3 Model B (`aarch64-unknown-linux-gnu`, Debian 13 Trixie / glibc 2.41)

---

## Summary of Changes

1. **Vanilla Passthrough & Dynamic `dsp-info`:**
   - Removed all hardcoded crossover/filter/EQ blocks from `baseline/etc/camilladsp/crossover.yml` (stereo passthrough).
   - Removed hardcoded crossover frequencies and `Sub`/`Coax` playback labels from `crates/dsp` (`DspHardwareInfo`) and `tools/diagnostics/src/cli/dsp.rs` (`dsp-info`).
   - `dsp-info` now reads the live configuration (`samplerate`, `chunksize`, `capture_device`, `playback_device`, `pipeline_stages`) directly from CamillaDSP via `GetConfigJson`.

2. **CamillaDSP `5.0.0-beta.2` Build & Upgrade:**
   - Added [`scripts/build_camilladsp_pi.sh`](../scripts/build_camilladsp_pi.sh) to cross-compile `HEnquist/camilladsp` at pinned tag `v5.0.0-beta.2` (`1ea8b9c5518bf9df7ae612d3b51bd791ceb46269`) for `aarch64-unknown-linux-gnu` (Cortex-A53, NEON, fat LTO, `log10f@GLIBC_2.17` symver), validate `/etc/camilladsp/crossover.yml`, back up `4.1.3` to `/usr/local/bin/camilladsp-4.1.3`, and restart `camilladsp.service`.
   - Migrated `silence_timeout` → `silence_timeout_s` in `baseline/etc/camilladsp/crossover.yml`.

3. **Official `camilladsp-schema` Crate Adoption:**
   - Added `camilladsp-schema` (pinned to `v5.0.0-beta.2` rev `1ea8b9c5518bf9df7ae612d3b51bd791ceb46269`) to `Cargo.toml`, `crates/dsp`, and `services/audio-manager`.
   - Replaced hand-rolled WebSocket JSON payloads in `crates/dsp/src/client.rs` with typed `WsCommand`, `WsReply`, `WsResult`, and `Configuration`.

4. **Dynamic Any-Sample-Rate Pipeline (44.1–192 kHz):**
   - Added `baseline/etc/modprobe.d/snd-aloop.conf` (`options snd-aloop pcm_notify=1`).
   - Removed `pcm.dsp_dmix` from `baseline/etc/asound.conf` so `pcm.!default` passes native sample rates directly to `hw:Loopback,0,0` via `plug`.
   - Enabled `stop_on_rate_change: true` and `capture.stop_on_inactive: true` (with auto capture `format` detection) in `baseline/etc/camilladsp/crossover.yml`, and `-w` (`--wait`) in `baseline/etc/systemd/system/camilladsp.service`.
   - Added `services/audio-manager/src/rate_watch.rs` to monitor `hw:Loopback` device 1 (`PCM Slave Active`, `PCM Slave Rate`, `PCM Notify`), scale `chunksize` (`1024` ≤ 48 kHz, `2048` ≤ 96 kHz, `4096` > 96 kHz), validate `/etc/camilladsp/crossover.yml` via `camilladsp-schema`, and push `SetConfigJson`.

5. **Push-Based `SubscribeState` Lifecycle Watcher:**
   - Added `DspDevice::subscribe_state()` in `crates/dsp/src/client.rs` and switched `services/audio-manager/src/dsp_watch.rs` from 500 ms `GetState` polling to CamillaDSP 5.0's `SubscribeState` push stream (`StateEvent`).
