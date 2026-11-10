# Architecture Documentation & Technical Specifications

This directory contains the system audit reports, architectural specifications, and implementation plans for the **Raspberry Pi Audio Bluetooth & DSP Appliance**.

---

## Documents

| Document | Format | Description |
| :--- | :--- | :--- |
| [rpi_audio_bluetooth_report.md](rpi_audio_bluetooth_report.md) | Markdown | Comprehensive hardware, audio pipeline, and system audit report for the Raspberry Pi 3 running Debian 13 (Trixie) / DietPi. Includes the AVRCP absolute-volume sync architecture, the BlueZ gate analysis (`[AVRCP] VolumeCategory` 5.88 default, info-file `Volume=` seed, `ControllerMode`), and the verified transport→Merus volume mapping. |
| [volume_sync_plan.md](volume_sync_plan.md) | Markdown | Volume-sync plan — **implemented 2026-11-09**: phone AVRCP → bluez-alsa native volume → bt-manager `sync_volume` (deduped) → `audio-manager` D-Bus `SetLevel` → Merus amp. Root cause was BlueZ 5.88 `[AVRCP] VolumeCategory = true` rejecting Pixel's AVRCP TG; fixed via `main.conf` in `baseline/etc/bluetooth/main.conf`. Includes the single-writer ownership map (audio-manager is the only amp writer). |
| [audio_manager_plan.md](audio_manager_plan.md) | Markdown | **Planned** `services/audio-manager`: single event-driven hardware owner of the Merus amp (`A.Mstr Vol`) fixing the mute-is-a-no-op finding (Merus has no ALSA mute switch); D-Bus `SetLevel/SetMute`, zero polling, unit ordering fixes, phased delivery (amp owner → BT volume client → future source routing/handover). |
| [ui_architecture_plan_rust.md](ui_architecture_plan_rust.md) | Markdown | **Primary Architecture Plan:** Native Rust 2024 & Slint UI touch console, Tokio actor model, CamillaDSP WebSocket, BlueZ D-Bus, and PeppyMeter integration. |
| [ui_architecture_plan_python.md](ui_architecture_plan_python.md) | Markdown | Alternative / reference architecture plan utilizing Python and `pygame-ce` directly over the Linux framebuffer (`/dev/fb0`). |
| [requirements-python-ui.txt](requirements-python-ui.txt) | Text | Pinned Python dependencies for the Python UI reference implementation and diagnostic utilities. |
