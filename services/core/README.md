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
1. **Top Status Bar (`TopStatusBar`):**
   - **Source Indicator Pill (Top-Left):** Vertically centered (`y: 6px; height: 30px;`) with a symmetric Bluetooth vector icon (`18×18px`) and active source label (`Bluetooth`).
   - **Center Client Info:** Connected device name (e.g. `Pixel 10 Pro XL`) with automated text elision.
   - **Graphic Status Badges (Top-Right):** Clean, distraction-free chips:
     - **Codec Badge:** Active codec (`aptX-HD`, `SBC`, `AAC`, `LDAC`) styled in primary accent gold.
     - **Dynamic Stream Spec Badges:** Sample rate (`48 kHz`, `96 kHz`) and bit depth (`16-bit`, `24-bit`) badges in warm amber gold (`secondary_accent`) shown only for variable/scalable codecs (`LDAC`, `aptX Adaptive`, `LHDC`, `SSC`, `SBC`, `AAC`, `LC3`), and automatically hidden for fixed-spec codecs (`aptX-HD`, `aptX`, `aptX-LL`) to eliminate spec redundancy.
     - **Volume Badge (0-100):** Real-time volume chip (`VOL 75` or `MUTE`) styled in warm amber gold, touch-interactive for mute toggling.
     - **DIM Control:** Dedicated one-tap hardware backlight dimming button (`DIM`).
2. **Main Viewport (`MainViewport`):** Active tab view container hosting 6 screens:
   - **Tab 0: Now Playing (`NowPlayingView`):**
     - **Turntable Vinyl Container with Glowing Halo:** Left pane displays an authentic turntable vinyl record graphic (`disk.svg`) inside an album box, surrounded by an ambient warm halo in `Theme.active.primary_accent` that illuminates softly while audio is actively streaming.
     - **Live Ambient Wave Indicator:** A rhythmic 5-bar equalizer wave (`AmbientWaveIndicator`) animating in alternating gold/amber accent colors beside the artist/album metadata during active playback.
     - **Seamless Unidirectional 60 FPS Marquee Scrolling:** Long track titles and album names glide continuously in one direction (right-to-left) with a 2.5-second initial reading delay, precise 60 FPS frame-pacing, and an imperceptible loop wrap-around, eliminating jerkiness and ping-pong reversals.
     - **Themed Linear Gradient Depth:** Chassis and card surface styled with `@linear-gradient` shading rather than flat solid color.
     - **Cleaned-Up Distraction-Free Layout:** Eliminated redundant stream inspector pill, album subtitle ("High Resolution DSP Sink"), volume button/slider, and mini L/R speaker VU bars.
     - **Thick Progress Scrubber:** Stretched **16px** played position bar with a large **26px** draggable handle knob and bold **18px** elapsed/duration timestamps.
     - **Enlarged Vector Transport Controls:** Large **86px** circular hero Play/Pause button (`||` / `▶`), **72px** Previous button with `|◀` graphic (`left.svg`), and **72px** Next button with `▶|` graphic (`right.svg`), aligned along their shared vertical center axis.
   - **Tab 1: VU Meter (`VuMeterView`):** Stereo dual analog needle meters with ballistics, peak LEDs, and calibrated dBFS scale.
   - **Tab 2: Bluetooth Manager (`BluetoothView`):** Adapter status, paired device list, connection state, signal level, and pairing controls.
   - **Tab 3: CD Player (`CdPlayerView`):** Optical disc drive status, TOC track listing, disc transport controls (gated by service enablement).
   - **Tab 4: DSP & Tone (`DspToneView`):** CamillaDSP master volume, clipping sample indicators, filter profiles, and parametric EQ status.
   - **Tab 5: Settings & Preferences (`SettingsView`):** Theme selector pills, display backlight dimming, thermal monitor, and hardware diagnostics.
3. **Bottom Navigation Dock (`NavDock`):** Typographic navigation pills (`Player`, `VU Meter`, `Bluetooth`, `DSP & Tone`, `Settings`) with dynamic active theme highlights and touch responsiveness.

### Hardware-Accelerated Slint Delta Rendering & Zero-Idle CPU
- **Slint Built-in Delta Rendering (`LineBufferProvider`):** Implements Slint's `LineBufferProvider` trait using `SoftwareRenderer::render_by_line` and `RepaintBufferType::ReusedBuffer`. Slint automatically determines dirty scanline bounding boxes and horizontal pixel spans, rendering only modified regions directly into Linux framebuffer memory (`/dev/fb0`) with zero full-screen clearing or flashing.
- **Adaptive Timer/Event Sleep Loop:** The core daemon render loop dynamically checks `slint::platform::duration_until_next_timer_update()`, sleeping adaptively up to 750ms when idle or 250ms during active playback. Verified via `top` on the Raspberry Pi 3: **CPU usage at idle dropped from ~27% to 0.0%**.

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

### Authentic Vector Graphics & Peppy SVG Icons
To ensure robust, pixel-perfect rendering across minimal Linux environments (like DietPi without full emoji fonts installed), all UI controls use pure vector graphics and official Peppy SVG assets stored in `assets/icons/`:
- Bluetooth rune icon (`bluetooth-rune.svg`) centered in the active source pill.
- Streamlined audio badges: codec (`aptX-HD`) in primary accent gold, sample rate (`48 kHz`) and bit depth (`24-bit`) in matching secondary amber gold.
- Turntable vinyl disc icon (`disk.svg`) for album/now-playing cover container.
- Hero Play/Pause vector icons (`play.svg`, `pause.svg`).
- Skip Previous/Next vector icons (`left.svg`, `right.svg`).

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
