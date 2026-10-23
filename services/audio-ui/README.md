# Audio UI Console Service (`audio-ui`)

**`audio-ui`** is an audiophile-grade touch user interface and hardware console built specifically for the Raspberry Pi 3 Model B running Debian 13 (Trixie) / DietPi.

---

## Technical Highlights

- **Framework:** [Slint UI](https://slint.dev/) (`slint-ui` =1.9.2 pinned) with native Linux Framebuffer (`/dev/fb0`) & KMS/DRM backends.
- **Async Runtime:** Tokio (`tokio` =1.53.3) multi-threaded runtime.
- **Audio Stack Integration:**
  - Native CamillaDSP WebSocket connection (`tokio-tungstenite`) for real-time 60 FPS Peak/RMS dBFS meter telemetry.
  - BlueZ 5 & BlueALSA D-Bus integration (`zbus`) for metadata, AVRCP volume sync, and auto-pairing agent.
- **Hardware Integration:**
  - Direct GPIO & I2C via `rppal` for TOP-UP EC12 rotary encoder, RGB LED, Noctua PWM fan, and BH1750 ambient light sensor.

---

## Directory Structure

```text
services/audio-ui/
├── Cargo.toml          # Crate dependencies & build configuration
├── build.rs            # Slint ahead-of-time (AOT) UI compiler
├── src/
│   └── main.rs         # Entrypoint, Tokio runtime, and event dispatch
└── ui/
    └── appwindow.slint # Slint declarative UI components (800x480)
```

---

## Build & Run

### Cross-Compiling for Raspberry Pi (Target: Cortex-A53 / aarch64)

```bash
cargo build --release -p audio-ui
```

### Running Desktop Simulator (Workstation Test Mode)

```bash
cargo run -p audio-ui --features simulator
```
