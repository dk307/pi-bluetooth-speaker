# Architecture Documentation & Technical Specifications

This directory contains the system audit reports, architectural specifications, and implementation plans for the **Raspberry Pi Audio Bluetooth & DSP Appliance**.

---

## Documents

| Document | Format | Description |
| :--- | :--- | :--- |
| [rpi_audio_bluetooth_report.md](rpi_audio_bluetooth_report.md) | Markdown | Comprehensive hardware, audio pipeline, and system audit report for the Raspberry Pi 3 running Debian 13 (Trixie) / DietPi. Includes the AVRCP absolute-volume sync architecture, the BlueZ gate analysis (`[AVRCP] VolumeCategory` 5.88 default, info-file `Volume=` seed, `ControllerMode`), and the verified transport→Merus volume mapping. |
| [volume_sync_plan.md](volume_sync_plan.md) | Markdown | Volume-sync troubleshooting plan — **resolved 2026-11-09**: root cause was BlueZ 5.88's `[AVRCP] VolumeCategory = true` default rejecting Pixel AVRCP TG (no category-2); fixed via `main.conf` and committed to `baseline/etc/bluetooth/main.conf`. Native sync needs no bt-manager code. |
| [ui_architecture_plan_rust.md](ui_architecture_plan_rust.md) | Markdown | **Primary Architecture Plan:** Native Rust 2024 & Slint UI touch console, Tokio actor model, CamillaDSP WebSocket, BlueZ D-Bus, and PeppyMeter integration. |
| [ui_architecture_plan_python.md](ui_architecture_plan_python.md) | Markdown | Alternative / reference architecture plan utilizing Python and `pygame-ce` directly over the Linux framebuffer (`/dev/fb0`). |
| [requirements-python-ui.txt](requirements-python-ui.txt) | Text | Pinned Python dependencies for the Python UI reference implementation and diagnostic utilities. |
