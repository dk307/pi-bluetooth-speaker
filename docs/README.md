# Architecture Documentation & Technical Specifications

This directory contains the system audit reports, architectural specifications, and implementation plans for the **Raspberry Pi Audio Bluetooth & DSP Appliance**.

---

## Documents

| Document | Format | Description |
| :--- | :--- | :--- |
| [rpi_audio_bluetooth_report.md](rpi_audio_bluetooth_report.md) | Markdown | Comprehensive hardware, audio pipeline, and system audit report for the Raspberry Pi 3 running Debian 13 (Trixie) / DietPi. Includes the AVRCP absolute-volume sync architecture, the BlueZ gate analysis (`[AVRCP] VolumeCategory` 5.88 default, info-file `Volume=` seed, `ControllerMode`), and the verified transport→Merus volume mapping. |
| [volume_sync_plan.md](volume_sync_plan.md) | Markdown | Volume-sync plan — **implemented 2026-11-09**: phone AVRCP → bluez-alsa native volume → bt-manager `sync_volume` (deduped) → `audio-manager` D-Bus `SetLevel` → Merus amp. Root cause was BlueZ 5.88 `[AVRCP] VolumeCategory = true` rejecting Pixel's AVRCP TG; fixed via `main.conf` in `baseline/etc/bluetooth/main.conf`. Includes the single-writer ownership map (audio-manager is the only amp writer). |
| [single_active_bt_source.md](single_active_bt_source.md) | Markdown | Single active Bluetooth audio source architecture — `bluealsa-aplay --single-audio` audio-path gate + `bt-manager` `org.speaker.Bluetooth` (`ActiveDevice`) arbiter, real `avrcp/player0` AVRCP pause, and active-device scoping across `volume_sync`, `cover_provider`, and `screen-manager`. |
| [audio_manager_plan.md](audio_manager_plan.md) | Markdown | **Planned** `services/audio-manager`: single event-driven hardware owner of the Merus amp (`A.Mstr Vol`) fixing the mute-is-a-no-op finding (Merus has no ALSA mute switch); D-Bus `SetLevel/SetMute`, zero polling, unit ordering fixes, phased delivery (amp owner → BT volume client → future source routing/handover). |
| [camilladsp_5_upgrade.md](camilladsp_5_upgrade.md) | Markdown | **Implemented** CamillaDSP `5.0.0-beta.2` upgrade, `camilladsp-schema` adoption, vanilla passthrough, dynamic sample-rate switching (`rate_watch` + `snd-aloop pcm_notify=1`), and `SubscribeState` push `dsp_watch`. |
| [ui_architecture_plan_rust.md](ui_architecture_plan_rust.md) | Markdown | **Primary Architecture Plan:** Native Rust 2024 & Slint UI touch console, Tokio actor model, CamillaDSP WebSocket, BlueZ D-Bus, and PeppyMeter integration. |
| [ui_architecture_plan_python.md](ui_architecture_plan_python.md) | Markdown | Alternative / reference architecture plan utilizing Python and `pygame-ce` directly over the Linux framebuffer (`/dev/fb0`). |
| [kms_femtovg_migration_plan.md](kms_femtovg_migration_plan.md) | Markdown | **Migration plan:** replace the custom fbdev `/dev/fb0` display path with Slint's `backend-linuxkms` (noseat) and swap the software renderer for the FemtoVG GPU renderer (OpenGL ES 2.0 / GBM on VideoCore IV). Covers the single-window/single-event-loop constraint, splash overlay extraction, libinput touch, test-harness trimming, CI/runtime dependencies, and gates. Skia is deferred to a separate follow-up plan. |
| [skia_opengl_migration_plan.md](skia_opengl_migration_plan.md) | Markdown | **Migration plan:** replace `renderer-femtovg` with `renderer-skia-opengl` (Skia Ganesh OpenGL ES 2.0 on VideoCore IV `VC4 V3D 2.1`), verify ARM64 NEON prebuilt Skia binary cache (`0.157.0`), add GPU FBO layer caching (`cache-rendering-hint: true`) on the 360×360 cover-art card and static status-bar badges, and align cover-art cache sizing (`TARGET_COVER_SIZE = 360`). |
| [visualizer_architecture_plan.md](visualizer_architecture_plan.md) | Markdown | **Visualizer Plan:** Full-screen (`800×480`) 48-bar bilateral Radial FFT (`228×228` ping-pong `SharedPixelBuffer`) + 24-band Glasberg–Moore ERB linear spectrum driven by CamillaDSP 5.0 `SubscribeSpectrum` WebSocket streaming, `Nice=15` worker, latest-frame slot, and Visualizer Settings sub-screen. |
| [requirements-python-ui.txt](requirements-python-ui.txt) | Text | Pinned Python dependencies for the Python UI reference implementation and diagnostic utilities. |
