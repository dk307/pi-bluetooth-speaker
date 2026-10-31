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

## Appliance UI Architecture (800x480 Framebuffer)

The appliance interface is structured into three coordinated zones rendered directly to `/dev/fb0`:
1. **Top Status Bar (`TopStatusBar`):** Dynamic source indicator (`Bluetooth`, `CD Player`, `DSP`), connected device / track title, active codec pill (`aptX-HD 48kHz`), vector signal strength bars, and display brightness/night mode control.
2. **Main Viewport (`MainViewport`):** Active tab view container hosting 6 screens:
   - **Tab 0: Now Playing (`NowPlayingView`):** Vinyl spinning indicator, track title, artist, album, elapsed & duration timestamps, interactive progress seek bar, volume control slider, vector transport controls (Previous, Play/Pause, Next), and audio stream inspector pill.
   - **Tab 1: VU Meter (`VuMeterView`):** Stereo dual analog needle meters with ballistics, peak LEDs, and calibrated dBFS scale.
   - **Tab 2: Bluetooth Manager (`BluetoothView`):** Adapter status, paired device list, connection state, signal level, and pairing controls.
   - **Tab 3: CD Player (`CdPlayerView`):** Optical disc drive status, TOC track listing, disc transport controls (gated by service enablement).
   - **Tab 4: DSP & Tone (`DspToneView`):** CamillaDSP master volume, clipping sample indicators, filter profiles, and parametric EQ status.
   - **Tab 5: Settings & Preferences (`SettingsView`):** Theme selector pills, display backlight dimming, thermal monitor, and hardware diagnostics.
3. **Bottom Navigation Dock (`NavDock`):** Typographic navigation pills (`PLAYER`, `VU`, `BT`, `CD`, `DSP`, `SETTINGS`) with dynamic active theme highlights and touch responsiveness.

### Boot Sequence & Appliance Lifecycle
- **`ScreenTarget::Appliance` (Default):** On service start, displays the POST **Boot Splash Screen** (`SplashScreen`) verifying hardware subsystems (Thermal, Merus Amp, Ambient Light, Bluetooth D-Bus, CamillaDSP), then transitions seamlessly to the **UI Shell** (`MainWindow`) without screen flicker or platform re-initialization.
- **Targeted Preview Modes:** Can be run in isolated mode using `-s splash` or `-s shell`.

### Global Theme Presets
All screens (Splash, Shell, and Viewport tabs) dynamically adapt to the active theme palette:
- **`peppy` (Vintage Peppy / Hi-Fi):** Warm Cream background (`#FAF8F5`), Warm Amber accents (`#D97706`), Walnut Wood border styling (`#292524`).
- **`mcintosh` (McIntosh Blue):** Deep Audiophile Blue (`#0A0F1D`) with Signature Cyan needles (`#00E5FF`).
- **`marantz` (Marantz Amber):** Champagne Gold faceplate with Warm Amber glowing accents (`#F59E0B`).
- **`cyberpunk` (Cyberpunk Neon):** Pitch Black background with High-Contrast Neon Magenta (`#EC4899`) and Cyan (`#06B6D4`).
- **`studio-dark` (Studio Dark):** Professional neutral Charcoal (`#18181B`) with Crisp White accents (`#FAFAFA`).

### Pure Vector Graphics (No Unicode Picture Dependencies)
To ensure robust, pixel-perfect rendering across minimal Linux environments (like DietPi without full emoji fonts installed), all UI controls are rendered with pure Slint vector primitives (`Path`, `Rectangle`):
- Vector Play triangle & Pause double-bars.
- Vector Next / Previous fast-forward triangles and end-stops.
- Vector Wi-Fi / Bluetooth signal bars.
- Vector dropdown chevrons.
- Vector status check dots and bullet dividers.

---

## Directory Structure

```text
services/core/
├── .cargo/
│   └── config.toml     # Cortex-A53 NEON vectorization & cross-compile tuning
├── Cargo.toml          # Crate dependencies & build configuration
├── README.md           # Service documentation
├── AGENTS.md           # Agent instructions & guidelines
├── build.rs            # Slint ahead-of-time (AOT) UI compiler
├── src/
│   ├── diag.rs         # Shared CLI diagnostic helpers (formatting, flag parsing, exit codes)
│   ├── lib.rs          # Core library root (hardware drivers, mocks, domain types, task runtime)
│   ├── main.rs         # Core binary entrypoint
│   ├── runtime.rs      # Asynchronous task join helpers & panic propagation
│   ├── bin/
│   │   ├── amp-info.rs     # MA12070P I2C amplifier diagnostic utility
│   │   ├── bt-info.rs      # Bluetooth & BlueALSA D-Bus diagnostic utility
│   │   ├── dsp-info.rs     # CamillaDSP WebSocket pipeline diagnostic utility
│   │   ├── encoder-read.rs # EC12 rotary encoder diagnostic utility
│   │   ├── light-read.rs   # BH1750 I2C ambient light diagnostic utility
│   │   └── temp-read.rs    # DS18B20 1-Wire temperature diagnostic utility
│   ├── cli/
│   │   └── core_daemon.rs  # Core appliance daemon, CLI options & display orchestration
│   ├── hardware/
│   │   ├── amp.rs          # MA12070P I2C amplifier driver, domain types, and mock controller
│   │   ├── bluetooth.rs    # BlueZ & BlueALSA driver, domain types, and mock controller
│   │   ├── dsp.rs          # CamillaDSP WebSocket client, domain types, and mock controller
│   │   ├── encoder.rs      # EC12 rotary encoder & RGB LED driver and mock controller
│   │   ├── light.rs        # BH1750 I2C driver, domain types, and mock controller
│   │   ├── mod.rs          # Hardware GPIO pin types & shared abstractions
│   │   └── temp.rs         # DS18B20 1-Wire driver, parser, and mock controller
│   └── ui/
│       ├── framebuffer.rs  # Linux Framebuffer (/dev/fb0) & BGRX scanout backend
│       ├── shell.rs        # AppShell view controller, transport wiring, and event dispatch
│       └── theme.rs        # ThemePreset enum, color tokens, and palette application
├── tests/
│   ├── amp_device_integration.rs   # Integration tests for MA12070P amplifier
│   ├── bluetooth_integration.rs    # Integration tests for Bluetooth & BlueALSA
│   ├── boot_splash_integration.rs  # Integration tests for boot splash screen
│   ├── cli_tests.rs                # Subprocess CLI tests for all diagnostic utilities
│   ├── core_daemon_integration.rs  # Integration tests for core appliance daemon & options
│   ├── dsp_integration.rs          # Integration tests for CamillaDSP pipeline
│   ├── encoder_integration.rs      # Integration tests for EC12 rotary encoder
│   ├── light_sensor_integration.rs # Integration tests for I2C & mocked light sensor
│   ├── temp_sensor_integration.rs  # Integration tests for sysfs & mocked temperature sensor
│   ├── ui_framebuffer_integration.rs # Integration tests for framebuffer & scanout
│   ├── ui_shell_integration.rs     # Integration tests for UI shell & callbacks
│   └── ui_theme_integration.rs     # Integration tests for theme presets and palettes
└── ui/
    ├── appwindow.slint   # Top-level window assembly (800x480)
    ├── nav_dock.slint    # Bottom navigation dock
    ├── now_playing.slint # Now Playing playback & transport view
    ├── splash.slint      # POST Boot splash screen
    ├── status_bar.slint  # Top status bar
    ├── theme.slint       # Global Theme singleton & palette properties
    └── viewport.slint    # Central multi-tab viewport (VU, BT, CD, DSP, Settings)
```

---

## Build & Run

### Cross-Compiling for Raspberry Pi (Target: Cortex-A53 / aarch64)

```bash
cargo build --config services/core/.cargo/config.toml --manifest-path services/core/Cargo.toml --target aarch64-unknown-linux-gnu --release --bins
```

### Core Appliance Daemon CLI Options

```text
Usage: core [OPTIONS]

Options:
  -s, --screen <appliance|splash|shell> Target screen to render (default: appliance)
  -t, --theme <PRESET>        Appliance theme preset (mcintosh, marantz, cyberpunk, studio-dark, peppy)
  -T, --tab <TAB>             Active UI tab on startup (0-5 or name: player, vu, bt, cd, dsp, settings)
  -p, --preview <SECS>        Render UI for SECS seconds
  -l, --loop                  Hold UI on screen continuously
  -V, --version               Print version and exit
  -h, --help                  Print this help text and exit
```

Examples:
```bash
# Start standard appliance (boot splash for 2s, then Now Playing shell):
core --loop

# Start directly in Vintage Peppy theme on the VU Meter tab:
core --theme peppy --tab vu --loop

# Render 5-second preview of the boot splash screen:
core -s splash -p 5
```

### Diagnostic CLI Utilities

The crate provides standalone diagnostics for testing and inspecting physical hardware sensors:
- `temp-read`: Scans `/sys/bus/w1/devices/28-*/w1_slave` for attached DS18B20 1-Wire temperature sensors.
- `light-read`: Queries `/dev/i2c-1` for BH1750 ambient light sensors at addresses `0x23` and `0x5C`.
- `amp-info`: Reports MA12070P Class-D amplifier power mode, audio input format, DCU channels, and error flags over I2C, with latched error-log clearing (`--clear`).
- `bt-info`: Inspects BlueZ adapters and BlueALSA PCM streams over D-Bus, supports radio state control (`--power`, `--discoverable`, `--pairable`), device filters, and paired-device listings.
- `dsp-info`: Inspects CamillaDSP crossover pipeline state, CPU load, buffer fill, rate adjust ratio, master volume/mute, real-time dBFS level meters with graphical VU bars, and clipped sample counter reset (`--reset-clipped`).
- `encoder-read`: Samples the EC12 rotary encoder and RGB indicator LED driver state via GPIO, with an optional LED color-sweep verification (`--verify-leds`).

All utilities default to once mode (`--once`) so a single bounded read exits successfully; pass `--repeat` for continuous polling, `--count N` for bounded sampling, or `--version` for the semantic version.
