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
├── Cargo.toml          # Crate dependencies & build configuration (package: speaker-core, bins: core, temp-read, light-read, amp-info, bt-info, encoder-read, dsp-info)
├── README.md           # Service documentation
├── AGENTS.md           # Agent instructions & guidelines
├── build.rs            # Slint ahead-of-time (AOT) UI compiler
├── src/
│   ├── diag.rs         # Shared CLI diagnostic helpers (formatting, flag parsing, exit codes)
│   ├── lib.rs          # Core library root (hardware drivers, mocks, domain types, task runtime)
│   ├── main.rs         # Core daemon entrypoint, Tokio runtime, and event dispatch
│   ├── runtime.rs      # Asynchronous task join helpers & panic propagation
│   ├── bin/
│   │   ├── amp-info.rs     # MA12070P I2C amplifier diagnostic utility
│   │   ├── bt-info.rs      # Bluetooth & BlueALSA D-Bus diagnostic utility
│   │   ├── dsp-info.rs     # CamillaDSP WebSocket pipeline diagnostic utility
│   │   ├── encoder-read.rs # EC12 rotary encoder diagnostic utility
│   │   ├── light-read.rs   # BH1750 I2C ambient light diagnostic utility
│   │   └── temp-read.rs    # DS18B20 1-Wire temperature diagnostic utility
│   └── hardware/
│       ├── amp.rs       # MA12070P I2C amplifier driver, domain types, and mock controller
│       ├── bluetooth.rs # BlueZ & BlueALSA driver, domain types, and mock controller
│       ├── dsp.rs       # CamillaDSP WebSocket client, domain types, and mock controller
│       ├── encoder.rs   # EC12 rotary encoder & RGB LED driver and mock controller
│       ├── light.rs     # BH1750 I2C driver, domain types, and mock controller
│       ├── mod.rs       # Hardware GPIO pin types & shared abstractions
│       └── temp.rs      # DS18B20 1-Wire driver, parser, and mock controller
├── tests/
│   ├── amp_device_integration.rs   # Integration tests for MA12070P amplifier
│   ├── bluetooth_integration.rs    # Integration tests for Bluetooth & BlueALSA
│   ├── cli_tests.rs                # Subprocess CLI tests for all diagnostic utilities
│   ├── dsp_integration.rs          # Integration tests for CamillaDSP pipeline
│   ├── encoder_integration.rs      # Integration tests for EC12 rotary encoder
│   ├── light_sensor_integration.rs # Integration tests for I2C & mocked light sensor
│   └── temp_sensor_integration.rs  # Integration tests for sysfs & mocked temperature sensor
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

### Diagnostic CLI Utilities

The crate provides standalone diagnostics for testing and inspecting physical hardware sensors:
- `temp-read`: Scans `/sys/bus/w1/devices/28-*/w1_slave` for attached DS18B20 1-Wire temperature sensors.
- `light-read`: Queries `/dev/i2c-1` for BH1750 ambient light sensors at addresses `0x23` and `0x5C`.
- `amp-info`: Reports MA12070P Class-D amplifier power mode, audio input format, DCU channels, and error flags over I2C, with latched error-log clearing (`--clear`).
- `bt-info`: Inspects BlueZ adapters and BlueALSA PCM streams over D-Bus, supports radio state control (`--power`, `--discoverable`, `--pairable`), device filters, and paired-device listings.
- `dsp-info`: Inspects CamillaDSP crossover pipeline state, CPU load, buffer fill, rate adjust ratio, master volume/mute, real-time dBFS level meters with graphical VU bars, and clipped sample counter reset (`--reset-clipped`).
- `encoder-read`: Samples the EC12 rotary encoder and RGB indicator LED driver state via GPIO, with an optional LED color-sweep verification (`--verify-leds`).

All utilities default to once mode (`--once`) so a single bounded read exits successfully; pass `--repeat` for continuous polling, `--count N` for bounded sampling, or `--version` for the semantic version. They output in human-readable or NDJSON formats (`--json`), print a statistical session summary on exit for multi-sample human-readable runs (suppressed for once mode), report human-mode read errors to `stderr`, and return exit code `0` on successful reads or `1` if all polling attempts fail.
