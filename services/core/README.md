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
- **Architecture Guidelines:** Governed by [AGENTS.md](AGENTS.md) referencing the Microsoft Pragmatic Rust Guidelines.

---

## Directory Structure

```text
services/core/
├── .cargo/
│   └── config.toml     # Cortex-A53 NEON vectorization & cross-compile tuning
├── Cargo.toml          # Crate dependencies & build configuration (package: speaker-core, bins: core, read_temp, read_light)
├── README.md           # Service documentation
├── AGENTS.md           # Agent instructions & guidelines
├── build.rs            # Slint ahead-of-time (AOT) UI compiler
├── src/
│   ├── lib.rs          # Core library root (hardware drivers, mocks, domain types)
│   ├── main.rs         # Core daemon entrypoint, Tokio runtime, and event dispatch
│   ├── bin/
│   │   ├── read_light.rs# BH1750 I2C ambient light diagnostic utility
│   │   └── read_temp.rs# DS18B20 1-Wire temperature diagnostic utility
│   └── hardware/
│       ├── light.rs    # BH1750 I2C driver, domain types, and mock controller
│       ├── mod.rs      # Hardware GPIO pin types & shared abstractions
│       └── temp.rs     # DS18B20 1-Wire driver, parser, and mock controller
├── tests/
│   ├── light_sensor_integration.rs # Integration tests for I2C & mocked light sensor
│   └── temp_sensor_integration.rs # Integration tests for sysfs & mocked temperature sensor
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
