# Raspberry Pi Audiophile Bluetooth Speaker & DSP Sink

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Platform: Raspberry Pi 3](https://img.shields.io/badge/Platform-Raspberry%20Pi%203%20Model%20B-c51a4a.svg)](https://www.raspberrypi.com/products/raspberry-pi-3-model-b/)
[![OS: Debian 13 (Trixie) / DietPi](https://img.shields.io/badge/OS-DietPi%20aarch64-green.svg)](https://dietpi.com/)
[![Rust: 2024 Edition](https://img.shields.io/badge/Rust-2024%20Pinned-orange.svg)](https://www.rust-lang.org/)

An audiophile-grade Bluetooth audio receiver, active loudspeaker DSP crossover server, and interactive touch console engineered for the **Raspberry Pi 3 Model B (DietPi / Debian 13 Trixie, aarch64)**.

Repository: [https://github.com/dk307/pi-bluetooth-speaker](https://github.com/dk307/pi-bluetooth-speaker)

---

## 1. Architecture Overview

```text
[Phone / Bluetooth Client]
         │ (A2DP: aptX-HD / AAC / Opus / SBC-XQ)
         ▼
  [TP-Link UB500 USB BT Adapter]
         │
         ▼
  [bluealsa.service (v5.0.0)]
         │
         ▼
  [bluealsa-aplay.service -D default]
         │
         ▼
  [ALSA Loopback Card 0: pcm.dsp_dmix @ 48kHz S32_LE]
         │
         ▼
  [camilladsp.service (PID 434, Port 1234)]
     ├── Mono Sum & Split (-6 dB)
     ├── Ch0 (Subwoofer): 45Hz HPF + 180Hz LPF + Limiter (-3.5 dB)
     └── Ch1 (Coaxial):  180Hz HPF + 3kHz Shelf (-2 dB) + Limiter (-3.0 dB)
         │
         ▼
  [InnoMaker Merus MA12070P Amp: Card 1 (hw:sndrpimerusamp,0)]
         │
         ▼
  [Physical Speakers: Subwoofer + Coaxial Satellite]
```

---

## 2. Repository Layout

```text
pi-bluetooth-speaker/
├── Cargo.toml                # Root Cargo workspace manifest
├── rust-toolchain.toml       # Pinned Rust toolchain (rustc 1.99.1)
├── LICENSE                   # Apache License 2.0
├── .gitignore                # Rust and build artifact exclusions
├── baseline/                 # Verified baseline snapshot of Pi OS configuration
│   ├── README.md             # Baseline provisioning guide
│   ├── install.sh            # Automated end-to-end setup script
│   ├── verify.sh             # 25-point automated hardware/services audit
│   ├── boot/                 # Firmware config and custom Merus Amp DTBO
│   ├── etc/                  # ALSA, BlueALSA, CamillaDSP, and Systemd unit files
│   └── scripts/              # Hardware diagnostic and test scripts
├── docs/                     # Architectural documentation & technical reports
│   ├── README.md             # Documentation index
│   ├── rpi_audio_bluetooth_report.md # Comprehensive system and hardware audit report
│   ├── ui_architecture_plan_rust.md  # Rust & Slint UI architecture plan
│   ├── ui_architecture_plan_python.md# Python & pygame-ce reference architecture
│   └── requirements-python-ui.txt    # Pinned Python requirements for reference UI
└── services/                 # Microservices powering the appliance
    ├── README.md             # Service architecture and roadmap
    └── core/                 # Central Speaker Logic, Appliance UI & Hardware Console
        ├── .cargo/
        │   └── config.toml   # Cortex-A53 NEON vectorization & cross-compile tuning
        ├── Cargo.toml        # speaker-core package manifest (binary: core)
        ├── README.md         # Service documentation
        ├── agent.md          # Agent instructions & guidelines
        ├── build.rs          # Slint AOT build script
        ├── src/main.rs       # Entrypoint & Tokio actor runtime
        └── ui/appwindow.slint# Declarative 800x480 UI layout
```

---

## 3. Hardware Bill of Materials

| Component | Hardware Model | Connection / Bus | Purpose |
| :--- | :--- | :--- | :--- |
| **SBC** | Raspberry Pi 3 Model B Rev 1.2 | — | Quad-core ARM Cortex-A53 (aarch64) |
| **Amplifier HAT** | InnoMaker Merus MA12070P | I2S (`card 1`) & I2C-1 (`0x20`) | Multi-level Class-D active 2-way amp |
| **Bluetooth Dongle** | TP-Link UB500 (Bluetooth 5.0) | USB 2.0 (`2357:0604`) | High-res A2DP reception |
| **Display & Touch** | Freenove 4.3" MIPI DSI IPS Screen | MIPI DSI ribbon cable | 800x480 native framebuffer (`/dev/fb0`) & touch (`/dev/input/event0`) |
| **Rotary Controller**| TOP-UP EC12 Encoder + Push + RGB | BCM 24/25, BCM 22, BCM 12/16/20 | Hardware volume, mute/pause, audio-reactive LED |
| **Cooling Fan** | Noctua 4-pin PWM 5V Fan | BCM 13 (PWM), BCM 6 (Tach) | Thermal loop cooling & RPM monitoring |
| **Light Sensor** | BH1750 Ambient Light Sensor | I2C-1 (`0x23`) | Auto-dimming display backlight |
| **Temperature Sensor**| Dallas DS18B20 Digital Sensor | 1-Wire (`BCM 26`) | Precision chassis temperature tracking |

---

## 4. Quick Start

### Hardware Audit

To verify all system services, ALSA routes, CamillaDSP filter pipelines, and hardware sensors on the Raspberry Pi:

```bash
cd baseline
./verify.sh
```

### Building the Speaker Core Service

```bash
# Build release binary optimized for Cortex-A53
cargo build --release -p speaker-core

# Run interactive desktop simulator on workstation
cargo run -p speaker-core --features simulator
```
