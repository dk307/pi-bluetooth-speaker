# Raspberry Pi Audiophile Bluetooth Speaker & DSP Sink

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Platform: Raspberry Pi 3](https://img.shields.io/badge/Platform-Raspberry%20Pi%203%20Model%20B-c51a4a.svg)](https://www.raspberrypi.com/products/raspberry-pi-3-model-b/)
[![OS: Debian 13 (Trixie) / DietPi](https://img.shields.io/badge/OS-DietPi%20aarch64-green.svg)](https://dietpi.com/)
[![Rust: 2024 Edition](https://img.shields.io/badge/Rust-2024%20Pinned-orange.svg)](https://www.rust-lang.org/)
[![codecov](https://codecov.io/gh/dk307/pi-bluetooth-speaker/branch/main/graph/badge.svg)](https://codecov.io/gh/dk307/pi-bluetooth-speaker)

An audiophile-grade Bluetooth audio receiver, active loudspeaker DSP crossover server, and interactive touch console engineered for the **Raspberry Pi 3 Model B (DietPi / Debian 13 Trixie, aarch64)**.

Repository: [https://github.com/dk307/pi-bluetooth-speaker](https://github.com/dk307/pi-bluetooth-speaker)

---

## 1. Architecture Overview

```text
[Phone / Bluetooth Client]
         │ (A2DP: aptX-HD / AAC / SBC-XQ)
         ▼
  [TP-Link UB500 USB BT Adapter]
         │
         ▼
  [bluealsad (v5.0.0, -S -p a2dp-sink --all-codecs -c-opus --sbc-quality=xq+)]
         │
         ▼
  [bluealsa-aplay.service -S -D default --volume=none --single-audio]
         │  (100% bit-perfect PCM; bt-manager sync_volume -> audio-manager D-Bus)
         ▼
  [ALSA Loopback Card 0: plug -> hw:Loopback,0,0 (pcm_notify=1, 44.1-192 kHz)]
         │  (audio-manager rate_watch pushes samplerate/chunksize via SetConfigJson)
         ▼
  [camilladsp.service (v5.0.0-beta.2, -w, Port 1234)]
     └── Vanilla Stereo Passthrough (stop_on_rate_change + stop_on_inactive)
         │  (audio-manager dsp_watch via SubscribeState -> MA12070P PLL-gated unmute)
         ▼
  [I2S DAC Card 1 (hw:sndrpihifiberry,0) -> Merus MA12070P Amp (I2C-1 0x20)]
         │
         ▼
  [Physical Speakers]
```

Appliance services coordinate over D-Bus and WebSocket:
- **`audio-manager`** (`org.speaker.AudioManager`): Single hardware owner of the Merus MA12070P amplifier volume/mute over I2C, dynamic sample-rate switching (`rate_watch`), and CamillaDSP state-driven PLL-gated unmute (`dsp_watch`).
- **`bt-manager`** (`org.speaker.Bluetooth`, `org.speaker.CoverArt`): BlueZ/BlueALSA pairing agent, active-source arbiter, AVRCP volume sync to `audio-manager`, and BIP cover-art fetcher.
- **`screen-manager`**: Touchscreen console (`800x480` LinuxKMS/DRM + Skia OpenGL), radial FFT & 24-band ERB spectrum visualizer, Material 3 dynamic cover-art auto-theming, EC12 rotary encoder sync, and BH1750 ambient auto-brightness.
- **`rmpd`** (user service): Headless MPD-compatible streaming engine with MPRIS integration.

---

## 2. Repository Layout

```text
pi-bluetooth-speaker/
├── .cargo/
│   └── config.toml           # Cortex-A53 NEON vectorization & cross-compile tuning
├── AGENTS.md                 # Agent instructions & guidelines (root)
├── CHANGELOG.md              # Release changelog (managed by release-plz)
├── Cargo.toml                # Root Cargo workspace manifest
├── release-plz.toml          # Automated release & conventional-commit config
├── rust-toolchain.toml       # Pinned Rust toolchain (rustc 1.99.1)
├── LICENSE                   # Apache License 2.0
├── .gitignore                # Rust and build artifact exclusions
├── assets/                   # UI fonts, icons, and PeppyMeter skins
├── baseline/                 # Verified baseline snapshot of Pi OS configuration
│   ├── README.md             # Baseline provisioning guide
│   ├── install.sh            # Automated end-to-end setup script
│   ├── verify.sh             # 25-point automated hardware/services audit
│   ├── boot/                 # Firmware config, cmdline, and custom Merus Amp DTBO
│   ├── etc/                  # ALSA, BlueALSA, CamillaDSP, rmpd, D-Bus, and Systemd unit files
│   ├── patches/              # BlueZ 5.88 patches (track duration & mediaplayer refresh)
│   └── scripts/              # Hardware diagnostic and test scripts
├── crates/                   # Shared libraries
│   ├── common/               # Shared logging, runtime, systemd, and diagnostics (speaker-common)
│   ├── hardware/             # Peripheral drivers: Amp, Encoder, Light, Temp (speaker-hardware)
│   ├── dsp/                  # CamillaDSP client & level meter/spectrum telemetry (speaker-dsp)
│   ├── bluetooth/            # BlueZ, BlueALSA, BIP cover art protocol (speaker-bluetooth)
│   └── title-breaker/        # Semantic two-line song-title breaking for UI (speaker-title-breaker)
├── services/                 # Appliance background daemons
│   ├── README.md             # Service architecture and roadmap
│   ├── audio-manager/        # Merus MA12070P volume/mute owner, rate_watch & dsp_watch (audio-manager.service)
│   ├── bt-manager/           # Bluetooth Manager daemon, active-source arbiter & BIP cover art (bt-manager.service)
│   └── screen-manager/       # Touchscreen UI (LinuxKMS + Skia OpenGL), visualizer & auto-brightness (screen-manager.service)
├── scripts/                  # Cross-compilation & deployment helpers for Raspberry Pi 3
│   ├── build_bluealsa_pi.sh  # Build private A53-tuned BlueALSA v5.0.0 stack
│   ├── build_bluez_pi.sh     # Build patched BlueZ 5.88
│   ├── build_camilladsp_pi.sh# Build CamillaDSP 5.0.0-beta.2
│   ├── build_mesa_vc4_pi.sh  # Build vc4-only Mesa for VideoCore IV Skia OpenGL
│   ├── build_rmpd_pi.sh      # Build headless rmpd streaming engine
│   ├── deploy_binaries.sh    # Deploy release binaries to Pi /usr/local/bin with sha256 check
│   └── codecs/               # Audio codec build helpers
├── tools/                    # Hardware diagnostic utilities
│   └── diagnostics/          # CLI probes: amp-info, bt-info, dsp-info, encoder-read, light-read, temp-read
└── docs/                     # Architectural documentation & technical reports
```

---

## 3. Hardware Bill of Materials

| Component | Hardware Model | Connection / Bus | Purpose |
| :--- | :--- | :--- | :--- |
| **SBC** | Raspberry Pi 3 Model B Rev 1.2 | — | Quad-core ARM Cortex-A53 (aarch64) |
| **Amplifier HAT** | InnoMaker Merus MA12070P | I2S (`card 1`, `sndrpihifiberry`) & I2C-1 (`0x20`) | Multi-level Class-D amplifier |
| **Bluetooth Dongle** | TP-Link UB500 (Bluetooth 5.0) | USB 2.0 (`2357:0604`) | High-res A2DP reception |
| **Display & Touch** | Freenove 4.3" MIPI DSI IPS Screen | MIPI DSI ribbon cable | 800x480 DRM/KMS (`vc4` Skia OpenGL) & libinput touch |
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

### Building & Deploying the Appliance Binaries

```bash
# Build release binaries optimized for Cortex-A53
cargo build --target aarch64-unknown-linux-gnu --release --bins

# Deploy release binaries to the Raspberry Pi (/usr/local/bin) with sha256 verification
./scripts/deploy_binaries.sh

# Run interactive desktop simulator on workstation
cargo run -p screen-manager --features simulator
```

### Versioning and Releases

Workspace versions are updated automatically from conventional commits by release-plz. See
[the release process](docs/releasing.md) for the bump rules and tag workflow.
