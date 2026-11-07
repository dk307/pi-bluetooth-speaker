# Appliance Services

This directory contains the modular microservices that power the **Raspberry Pi Audio Bluetooth & DSP Appliance**.

---

## Service Inventory

| Service | Language / Stack | Status | Description |
| :--- | :--- | :--- | :--- |
| [`screen-manager`](screen-manager) | Rust 2024 / Slint / Tokio | **Production** | Audiophile touchscreen console (800x480 DSI `/dev/fb0`), dual PeppyMeter VU visualizers, rotary encoder volume sync, and display backlight controls (`screen-manager.service`). |
| [`bt-manager`](bt-manager) | Rust 2024 / Bluer / Tokio / zbus | **Production** | Dedicated Bluetooth manager daemon, BlueZ & BlueALSA status monitoring, native `org.bluez.Agent1` auto-trust pairing agent, and BIP cover art D-Bus service (`bt-manager.service`, replaces legacy `bt-agent.service`). |

---

## Future Services Roadmap

The appliance architecture is designed to support independent, lightweight daemon services:

1. **`cd-player` (Optical Disc Media Daemon):**
   - Headless CDDA audio playback engine using `mpv` / `libcdio`.
   - Automatic disc detection, CD-Text / MusicBrainz metadata fetching, and track control over local IPC/D-Bus.
2. **`stream-bridge` (Network Streaming Services):**
   - High-fidelity **AirPlay 2** sink (`shairport-sync`).
   - High-fidelity **UPnP / DLNA** renderer (`upmpdcli` / `mpd`).
3. **`telemetry-daemon` (Hardware Protection & Environmental Monitor):**
   - Decoupled I2C and 1-Wire hardware watchdog (MA12070P thermal fault latch recovery, BH1750 ambient light display dimming, and PWM fan thermal loop).

---

## Adding a New Service

To add a new Rust service to the workspace:
1. Create `services/<service-name>/` with its own `Cargo.toml`.
2. Add `"services/<service-name>"` to the `members` array in the root [Cargo.toml](../Cargo.toml).
3. The new service will automatically inherit the Cortex-A53 compiler flags and release profiles from the root workspace and `.cargo/config.toml`.
