# Speaker Core Service (`core`)

**`core`** is the central appliance daemon handling the core logic for the Raspberry Pi Bluetooth Speaker, including the touch user interface, hardware console, DSP WebSocket telemetry, BlueZ D-Bus integration, and peripheral orchestration.

---

## Technical Highlights

- **Framework:** [Slint UI](https://slint.dev/) (`slint-ui` =1.9.2 pinned) with native Linux Framebuffer (`/dev/fb0`) & KMS/DRM backends.
- **Async Runtime:** Tokio (`tokio` =1.53.3) multi-threaded actor runtime.
- **Audio Stack Integration:**
  - Native CamillaDSP WebSocket connection (`tokio-tungstenite`) for real-time 60 FPS Peak/RMS dBFS meter telemetry.
  - BlueZ 5 & BlueALSA D-Bus integration (`zbus`) for metadata, AVRCP volume sync, and auto-pairing agent.
- **Hardware Integration:**
  - Direct GPIO & I2C via `rppal` for TOP-UP EC12 rotary encoder, RGB LED, Noctua PWM fan, and BH1750 ambient light sensor.
- **Architecture Guidelines:** Governed by [agent.md](file:///home/deepak/server/services/core/agent.md) referencing the Microsoft Pragmatic Rust Guidelines.

---

## Directory Structure

```text
services/core/
├── .cargo/
│   └── config.toml     # Cortex-A53 NEON vectorization & cross-compile tuning
├── Cargo.toml          # Crate dependencies & build configuration (package: speaker-core, bin: core)
├── README.md           # Service documentation
├── agent.md            # Agent instructions & guidelines
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
cargo build --release -p speaker-core
```

### Running Desktop Simulator (Workstation Test Mode)

```bash
cargo run -p speaker-core --features simulator
```
