# Architecture Documentation & Technical Specifications

This directory contains the system audit reports, architectural specifications, and implementation plans for the **Raspberry Pi Audio Bluetooth & DSP Appliance**.

---

## Documents

| Document | Format | Description |
| :--- | :--- | :--- |
| [rpi_audio_bluetooth_report.md](file:///home/deepak/server/docs/rpi_audio_bluetooth_report.md) | Markdown | Comprehensive hardware, audio pipeline, and system audit report for the Raspberry Pi 3 running Debian 13 (Trixie) / DietPi. |
| [ui_architecture_plan_rust.md](file:///home/deepak/server/docs/ui_architecture_plan_rust.md) | Markdown | **Primary Architecture Plan:** Native Rust 2024 & Slint UI touch console, Tokio actor model, CamillaDSP WebSocket, BlueZ D-Bus, and PeppyMeter integration. |
| [ui_architecture_plan_python.md](file:///home/deepak/server/docs/ui_architecture_plan_python.md) | Markdown | Alternative / reference architecture plan utilizing Python and `pygame-ce` directly over the Linux framebuffer (`/dev/fb0`). |
| [requirements-python-ui.txt](file:///home/deepak/server/docs/requirements-python-ui.txt) | Text | Pinned Python dependencies for the Python UI reference implementation and diagnostic utilities. |
