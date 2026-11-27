# Appliance Services

This directory contains the modular microservices that power the **Raspberry Pi Audio Bluetooth & DSP Appliance**.

---

## Service Inventory

| Service | Language / Stack | Status | Description |
| :--- | :--- | :--- | :--- |
| [`audio-manager`](audio-manager) | Rust 2024 / Tokio / zbus | **Production** | Single hardware owner of the Merus MA12070P amplifier volume and mute over I2C (`org.speaker.AudioManager` D-Bus service), dynamic ALSA loopback sample-rate switching (`rate_watch` -> CamillaDSP `SetConfigJson`), and CamillaDSP `SubscribeState` PLL-gated unmute (`dsp_watch`). |
| [`screen-manager`](screen-manager) | Rust 2024 / Slint (Skia OpenGL) / Tokio | **Production** | Audiophile touchscreen console (800x480 LinuxKMS/DRM + libinput + Skia OpenGL on VideoCore IV), radial FFT & 24-band ERB spectrum visualizer, Material 3 dynamic cover-art auto-theming, EC12 rotary encoder volume sync, and BH1750 ambient auto-brightness loop (`screen-manager.service`). |
| [`bt-manager`](bt-manager) | Rust 2024 / Bluer / Tokio / zbus | **Production** | Dedicated Bluetooth manager daemon, BlueZ & BlueALSA status monitoring, native `org.bluez.Agent1` auto-trust pairing agent, single-active-source arbiter (`org.speaker.Bluetooth`), AVRCP volume sync to `audio-manager`, and BIP cover art D-Bus service (`bt-manager.service`, replaces legacy `bt-agent.service`). |

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
