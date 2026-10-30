# Custom Peppy-Inspired Touch UI & Audio Console — Rust Architecture & Implementation Plan

**Document:** `ui_architecture_plan_rust.md`  
**Target Hardware:** Raspberry Pi 3 Model B (DietPi / Debian 13 Trixie `aarch64`)  
**Language & Runtime:** **Rust 2024 Edition (rustc 1.99.1 pinned)**  
**GUI Framework:** **Slint (`slint-ui` =1.9.2 pinned)** — Native Linux Framebuffer (`/dev/fb0`) & DRM/KMS  
**Async & Concurrency:** **Tokio =1.53.3** (Multi-Threaded Work-Stealing Runtime)  
**Display:** Freenove 4.3" MIPI DSI Display (800x480 IPS Framebuffer `/dev/fb0`)  
**Touchscreen:** Native VideoCore Firmware Touch Driver (`raspberrypi-ts` via Linux `evdev` / `libinput`)  
**Audio Hardware:** InnoMaker Merus MA12070P I2S Amplifier (`card 1`) via CamillaDSP v4.1.3 Active Crossover  
**Hardware Controls:** TOP-UP EC12 Rotary Encoder + Push Switch + RGB LED, Noctua 4-Pin PWM Fan, DS18B20 Temp, BH1750 Lux  
**Deployment Artifact:** Single, self-contained static executable (`/usr/local/bin/audio-ui`, ~15–20 MB stripped)  

---

## 1. Executive Summary & Design Philosophy

This document outlines the full architectural design, screen layouts, subsystem integrations, and implementation phases for a dedicated, audiophile-grade touch user interface for the Raspberry Pi Audio Server built entirely in **Rust** using **Slint**.

### Core Principles
1. **Zero-Interpreter, Bare-Metal Performance:** Compiled directly to ARM64 native machine code with zero runtime garbage collection pauses. Starts in **< 150 ms**, uses **~25–40 MB of RAM** (active skin bitmaps + Tokio runtime + Slint render buffers), and draws **< 1.5% CPU** during active 60 FPS playback. This is 10× leaner than moOde Audio (~400 MB) and fits comfortably within the Pi 3's 917 MB total RAM.
2. **Declarative Slint UI with Compile-Time Optimization:** Fluid UI layouts, vector curves, and theme switching are written in declarative `.slint` markup, compiled ahead-of-time (AOT) into Rust code with zero runtime layout overhead.
3. **Peppy-Inspired Aesthetics:** Clean, high-contrast touch interface featuring high-visibility typography, large touch targets, smooth transitions, and iconic analog/digital **PeppyMeter dual VU meters** (Channel 0 = Subwoofer, Channel 1 = Coaxial Satellite).
4. **Unified Multi-Source Audio Pipeline:** Native integration with **Bluetooth A2DP (active now)**, with modular support for external **USB CD Player**, and future network streaming protocols (**AirPlay via Shairport-Sync** and **UPnP/DLNA**). *Note: Secondary sources (CD Player, AirPlay, UPnP) only show up in the UI when explicitly enabled; currently they are disabled by default.*
5. **Direct Hardware Safety & Fearless Concurrency:** Tokio asynchronous runtime coupled with `rppal` (Raspberry Pi Peripheral Access Library) ensures thread-safe, lock-free hardware polling with compile-time memory safety.
6. **Read-Only Appliance Hardening:** Operates seamlessly on a 100% read-only filesystem (`ro` mode) with zero SD card wear, instant power-cut safety, silent console boot, and hardware MA12070P I2C protection alerts.

---

## 2. System Architecture & Audio Pipeline

All input audio streams feed directly into the ALSA `default` PCM plug, flowing through the exact same 48 kHz resampler, active 2-way CamillaDSP crossover, and Infineon MERUS MA12070P hardware amplifier.

```text
┌────────────────────────────────────────────────────────────────────────┐
│                        AUDIO INPUT SOURCES                             │
│                                                                        │
│  [Bluetooth A2DP]      [USB CD Player]      [AirPlay]       [UPnP]     │
│   (bluealsa-aplay)      (mpv cdda://)     (shairport)     (upmpdcli)   │
└───────────┬───────────────────┬─────────────────┬──────────────┬───────┘
            │                   │                 │              │
            └───────────────────┴────────┬────────┴──────────────┘
                                         ▼
                      [ALSA Default: pcm.!default]
                                         │
                                         ▼ (speexrate_medium resampler)
                     [pcm.dsp_dmix @ 48 kHz / 32-bit LE]
                                         │
                                         ▼
                     [ALSA Loopback Card 0: hw:Loopback,0,0]
                                         │
                                         ▼ (48 kHz / 32-bit LE Capture)
                   [CamillaDSP Engine (PID 434, Port 1234)]
                      ├── Master Gain & Stream Rate Adjust
                      ├── Mono Sum & Headroom Attenuation (-6 dB)
                      ├── Ch0 (Subwoofer): 45Hz HPF + 180Hz LPF + Limiter (-3.5dB)
                      └── Ch1 (Coaxial):  180Hz HPF + 3kHz Shelf + Limiter (-3.0dB)
                                         │
                                         ▼ (48 kHz / 32-bit LE Playback)
                   [InnoMaker Merus MA12070P Amp (Card 1)]
                      ├── Hardware Mixer Unity Gain (0.00 dB)
                      └── Hardware Protection Limiter Active
                                         │
                                         ▼
                    [Physical Speakers: Subwoofer + Satellite]
```

---

## 2.1. Comparative Analysis: moOde, Volumio, Peppy vs. Our Rust Architecture

| Feature / Capability | moOde Audio | Volumio | Peppy Player | Our Custom Rust + Slint Engine |
| :--- | :---: | :---: | :---: | :--- |
| **Runtime & Language** | PHP / Python / JS | Node.js / Angular | Python 2/3 (Pygame) | **Pure Rust 2024:** Zero-cost abstractions, fearless concurrency, memory safety. |
| **Memory Footprint** | 250–400 MB | 300–450 MB | 40–60 MB | **< 10 MB RAM total.** |
| **Cold Boot to UI** | 25–40 seconds | 30–50 seconds | 8–15 seconds | **< 150 milliseconds (Instant on).** |
| **CamillaDSP Sync** | Native WebUI | Plugin | None | **Native Tokio WebSocket (`tokio-tungstenite`):** CamillaDSP is also written in Rust. Zero-copy JSON parsing at 60 FPS. |
| **PeppyMeter Visualizers** | Plugin | Plugin | Native Pygame | **Native Slint Dual Meters:** Hardware-accelerated needle rotation, logarithmic dBFS physics, 60 FPS at < 1.5% CPU. |
| **Audio Stream Inspector** | Yes | Yes | Basic | **Adopted:** Live pill displaying input sample rate, bit depth, codec bitrate, and CamillaDSP drift ratio (`0.999974`). |
| **Absolute Volume Sync** | Yes (AVRCP) | Yes | Partial | **Adopted:** Bidirectional sync across phone volume, physical EC12 rotary knob, Slint slider, and web remote. |
| **Universal Volume HUD** | Basic bar | Basic popup | None | **Adopted:** 800x380 full-viewport middle overlay across ALL screens with 2.5s auto-dismiss. |
| **Soft Fade Mute/Unmute** | Yes (DSP fade) | Yes | No | **Adopted:** 300 ms linear gain ramp on mute/unmute to eliminate acoustic shock. |
| **Night Mode / Blanking** | Yes (Screen off) | Yes | Screensaver | **Adopted:** 1-tap top bar icon shutting off DSI backlight (`brightness=0`) with tap-anywhere-to-wake. |
| **Sleep Timer** | Yes (Timer) | Yes | Yes | **Adopted:** 15m, 30m, 45m, 60m countdown with gradual volume fade-out into amp standby. |
| **Silent Boot & Takeover** | Splash plugin | Splash image | No (Console) | **Adopted:** Zero-console flicker (`console=tty3 quiet vt.global_cursor_default=0`) with direct framebuffer takeover in < 150 ms. |
| **Source Arbitration** | Manual Switch | Manual Switch | N/A | **Adopted:** Priority-driven arbitration; playing CD automatically pauses Bluetooth phone over AVRCP. |
| **180° Screen Inversion** | Manual X11 config | Plugin setting | Manual config | **Adopted:** 1-tap toggle in settings rotating Slint canvas and touch transform matrix. |
| **Fault Recovery Loops** | Restart service | Restart service | Script crash | **Adopted:** Non-blocking async exponential backoff for CamillaDSP, BlueZ, and 1-Wire sensor failover. |
| **On-Screen Keyboard** | OSK plugin | WebUI only | Partial | **Adopted:** Modal touch QWERTY keyboard in Slint for Bluetooth broadcast renaming and Wi-Fi credentials. |
| **Amp Protection Alert** | None | None | None | **Adopted:** Real-time MA12070P I2C register `0x6D` fault latch monitoring with 1-tap hardware reset banner. |
| **Read-Only Appliance Mode**| Paraphrased Safe Shutdown | Paraphrased Safe Shutdown | Shutdown Menu | **Optimized for Read-Only Filesystem:** Instant power-cut safety. Zero temporary SD card writes. Kept: `[ 🔄 Reboot ]` and `[ 🔁 Restart Audio ]`. |

---

## 3. UI Shell & Global Layout (800x480 Screen)

The display is divided into three functional zones:

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ [Top Status Bar (40px)]: [Source Pill ▼]      Device / Track Info      [Codec] [🌙] │
├──────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│                                                                              │
│                         MAIN SCREEN VIEWPORT (380px)                         │
│                    (Displays active screen: 0 to 5)                          │
│                                                                              │
│                                                                              │
├──────────────────────────────────────────────────────────────────────────────┤
│ [Bottom Dock (60px)]:  [🎵 Player] [🎚️ VU] [📶 BT] [🎛️ DSP] [⚙️ Config]       │
│                        (Optional when enabled: [💿 CD])                      │
└──────────────────────────────────────────────────────────────────────────────┘
```

1. **Top Status Bar (Height: 40px):**
   - **Streamlined Audio-Centric Design:** To preserve a focused, distraction-free audiophile aesthetic, ambient enclosure temperature and system clock are excluded from the top bar (thermals remain viewable in Screen 6: System Health & Thermals).
   - **Active Source Pill:** Tap to switch active source. Displays currently enabled sources (e.g. `📶 Bluetooth`; auxiliary sources like `💿 CD Player`, `📻 AirPlay`, and `🌐 UPnP` appear only when enabled).
   - **Connected Client / Quick Info:** Shows connected device name (e.g., `Pixel 10 Pro XL`) or current track with auto-clipping.
   - **Codec Badge:** Real-time badge (`aptX-HD`, `AAC`, `SBC-XQ`, `48kHz`).
   - **Night Mode Toggle:** One-tap button (`🌙`) to blank the DSI backlight.

2. **Main Viewport (Height: 380px):**
   - Displays the active screen. Supports smooth transitions and reactive view switching.

3. **Bottom Navigation Dock (Height: 60px):**
   - Touch-friendly icon bar showing currently enabled services (minimum touch target > 48px):
     - `🎵 Player` (Now Playing & Transport)
     - `🎚️ VU Meter` (PeppyMeter Dual Visualizer)
     - `📶 Bluetooth` (Device Manager & Pairing)
     - `💿 CD Player` (*Conditionally visible:* Only displayed when CD player subsystem is enabled; currently disabled by default)
     - `🎛️ DSP & Tone` (CamillaDSP Levels & Presets)
     - `⚙️ Settings` (Preferences, Hardware Health & Thermals)

---

### 3.1. Unified Global Theme System (Coordinated Across All Screens)

In Slint, theming is defined using a global reactive singleton (`global Theme`):

```slint
export struct ThemePalette {
    chassis: color,
    card_surface: color,
    primary_accent: color,
    secondary_accent: color,
    needle_peak: color,
    dock_active_text: color,
    meter_skin_id: string,
}

export global Theme {
    in-out property <ThemePalette> active: {
        chassis: #090D12,
        card_surface: #111923,
        primary_accent: #00C8FF,
        secondary_accent: #70E1FF,
        needle_peak: #FF3344,
        dock_active_text: #090D12,
        meter_skin_id: "mcintosh_800x480",
    };
}
```

#### Theme Palette Matrix:

| Token / Element | 1. Vintage McIntosh Blue | 2. Marantz / Accuphase Amber | 3. Retro Cyberpunk Neon | 4. Minimalist Studio Dark |
| :--- | :--- | :--- | :--- | :--- |
| **Chassis / Background** | Deep Obsidian (`#090D12`) | Brushed Dark Charcoal (`#12100D`) | Midnight Violet (`#0D0B18`) | Matte Studio Carbon (`#121212`) |
| **Card / Surface Tint** | Deep Navy Glass (`#111923`) | Warm Champagne Tint (`#1C1712`) | Deep Synthwave Purple (`#1B142E`) | Studio Neutral Dark (`#1E1E1E`) |
| **Primary Accent Glow** | Electric Cyan (`#00C8FF`) | Incandescent Amber (`#E5A93C`) | Neon Cyan / Fuchsia (`#00F0FF`) | Precision Crisp White (`#F0F0F0`) |
| **Secondary Accent** | Ice Blue (`#70E1FF`) | Warm Gold (`#FFD166`) | Hot Magenta (`#FF007F`) | Brushed Silver (`#A0A0A0`) |
| **Needle / Peak Highlight**| Vivid Crimson (`#FF3344`) | Vintage Ruby (`#D93829`) | Hot Pink Peak (`#FF1493`) | Studio Red/Green (`#00E676`) |
| **Active Dock & Badges** | Cyan Pill with Navy Text | Amber Pill with Dark Text | Magenta Border with Cyan Text | Clean Monospaced White Border |
| **PeppyMeter Skin** | `mcintosh_800x480` | `marantz_800x480` | `cyberpunk_800x480` | `studio_dark_800x480` |

* **Zero-Allocation Hot Swapping:** Updating `Theme.active` in Rust triggers an instantaneous Slint reactive re-render in < 1 ms without memory allocation or audio disruption.
* **Cold Boot Persistence:** Saved in `/data/config/ui_settings.json` and loaded during startup.

---

### 3.2. Universal Full-Viewport Volume HUD Overlay (Active on All Screens)

Whenever the physical **EC12 rotary knob is turned** (or a volume change is received from a phone over Bluetooth AVRCP or the web companion), the entire middle viewport ($800 \times 380\text{ px}$) is immediately intercepted by a dedicated **Full-Screen Volume HUD**:

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ [📶 Bluetooth ▼]  Pixel 10 Pro XL   [aptX-HD 576k]   [🌙]   [26.4°C]   [12:05]│
├──────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│                              MASTER VOLUME                                   │
│                                                                              │
│                                  78%                                         │
│                               -11.0 dBFS                                     │
│                                                                              │
│       0%  [━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━●──────────]  100%            │
│                                                                              │
│          [ 🔈 Min: 0% ]         [ 🔇 MUTE ]         [ 🔊 Max: 100% ]          │
│                                                                              │
├──────────────────────────────────────────────────────────────────────────────┤
│ [Bottom Dock (Dimmed 40%)]: 🎵 Player | 🎚️ VU | 📶 BT | 💿 CD | 🎛️ DSP | ⚙️  │
└──────────────────────────────────────────────────────────────────────────────┘
```

#### Key Functional Behaviors:
1. **Universal Screen Interception:** Appears over **any active screen** (Now Playing, full-screen PeppyMeter, Bluetooth Manager, CD Player, DSP Tone, or Settings) without losing the user's navigational state.
2. **High-Visibility Typography & Themed Accents:**
   - Huge percentage readout (`78%` in 64pt bold) and calibrated decibel readout (`-11.0 dBFS`).
   - Massive glowing horizontal track styled dynamically with `Theme.active.primary_accent`.
   - If soft-muted, pulses `🔇 MUTED` with a vivid red indicator.
3. **Auto-Dismiss Timeout (2.5 Seconds):**
   - An asynchronous Tokio timer resets on each rotary detent tick.
   - After **2.5 seconds** of inactivity, the overlay executes a 150 ms smooth alpha fade-out and reveals the underlying screen.
4. **Touchscreen Interactivity:**
   - Users can drag their finger along the horizontal track to adjust volume via touch, tap `[ 🔇 MUTE ]` for soft-fade muting, or tap outside the bar to dismiss immediately.

---

## 4. Screen-by-Screen Breakdown & Wireframes

### Screen 1: Now Playing (Playback & Artwork)
* **Visual Layout:**
  - **Left Column (260x260):** Artwork container with rounded corners and subtle drop shadow.
  - **Right Column (500x260):** 
    - Track Title (large bold typography with auto-scroll marquee for long names).
    - Artist Name & Album Name.
    - **Audio Stream Inspector Pill:** Tap to view exact stream telemetry (`44.1 kHz / 16-bit | aptX-HD 576 kbps | CamillaDSP 48k/32bit | Clock Drift: 0.999974`).
    - Track playback scrubber / progress bar with elapsed and remaining time.
    - Transport buttons: `⏮️ Prev`, `⏯️ Play/Pause`, `⏭️ Next`, `🔇 Soft-Mute`.
    - Arc / Slider Volume Indicator (bidirectionally synchronized with phone volume and physical EC12 rotary knob).
  - **Bottom Span:** Mini real-time stereo LED peak meters linked to CamillaDSP output.
  - **Night Mode Button (`🌙`):** Located on top bar; tapping immediately blacks out display backlight (`brightness=0`) for dark-room listening while music plays. Tap anywhere on screen to wake.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ [📶 Bluetooth ▼]  Pixel 10 Pro XL   [aptX-HD 576k]   [🌙]   [26.4°C]   [12:05]│
├──────────────────────────────────┬───────────────────────────────────────────┤
│                                  │  Track Title (Marquee Scroll...)          │
│   ┌──────────────────────────┐   │  Artist Name • Album Title                │
│   │                          │   │  [ℹ️ 44.1kHz/16b ➔ DSP 48kHz/32b | S:0.99] │
│   │     260x260 COVER ART    │   │                                           │
│   │    (AVRCP 1.6 BIP /      │   │  [01:45] ━━━━━━━●──────────── [03:50]     │
│   │     MusicBrainz / Vinyl) │   │                                           │
│   │                          │   │     [⏮️]     [ ⏯️ ]     [⏭️]     [🔊 78%] │
│   └──────────────────────────┘   ├───────────────────────────────────────────┤
│                                  │  Mini VU: L [████████░░] R [████████░░]   │
└──────────────────────────────────┴───────────────────────────────────────────┘
```

---

### Screen 2: PeppyMeter (Dual Analog/Digital VU Visualizer)
* **Real-Time DSP Signal Feeds:**
  - Directly subscribes to CamillaDSP WebSocket (`127.0.0.1:1234`) via `tokio-tungstenite`.
  - Reads `levels.playback_peak()` and `levels.playback_rms()` at 60 FPS with zero copy.
  - **Left Meter (Channel 0):** Dedicated Subwoofer output (45 Hz – 180 Hz).
  - **Right Meter (Channel 1):** Dedicated Coaxial Satellite output (180 Hz+).
* **Selectable Global Themes / Skins (Tap Screen to Cycle):**
  - Tapping anywhere cycles both the PeppyMeter faceplate and the system-wide accent palette across all screens:
  1. **Vintage McIntosh Blue:** Deep blue backlit meters, black chassis, red-tipped analog needles, electric cyan UI glow.
  2. **Vintage Marantz / Amber:** Warm incandescent glow, brushed gold/champagne background, ruby needles, warm amber UI accents.
  3. **Retro Cyberpunk Neon:** Segmented vertical LED bars with peak-hold indicators, neon magenta and cyan UI accents.
  4. **Minimalist Studio Dark:** Dark charcoal background with crisp white precision tick marks and clean studio green highlights.
* **Auto-Screensaver Behavior:**
  - Configurable timeout (default: 30 seconds of active music playback).
  - Automatically transitions to full-screen PeppyMeter.
  - Any screen touch returns instantly to the Now Playing screen.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ [Tap Anywhere to Cycle Skins]                                     [Skin: 1/4]│
├──────────────────────────────────────┬───────────────────────────────────────┤
│                                      │                                       │
│          SUBWOOFER (CH 0)            │       COAXIAL SATELLITE (CH 1)        │
│              -10  -5   0             │              -10  -5   0              │
│          -20   \       +3            │          -20   /       +3             │
│        -40      \                    │        -40    /                       │
│       ┌──────────\──────────┐        │       ┌──────/──────────────┐         │
│       │           \         │        │       │     /               │         │
│       └─────────────────────┘        │       └─────────────────────┘         │
│             [SUB 45-180Hz]           │             [COAX 180Hz+]             │
│                                      │                                       │
└──────────────────────────────────────┴───────────────────────────────────────┘
```

#### 4.2.1. PeppyMeter Skin Compatibility & Slint Ingestion Architecture

Rather than forcing users to discard hundreds of existing audiophile community skins, our Slint engine includes a **Native PeppyMeter Ingestion Layer (`peppy_loader.rs`)**. It parses standard community `meters.txt` files and maps them directly into Slint's hardware-accelerated 2D transformation pipeline:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                       PEPPYMETER TO SLINT INGESTION ENGINE                  │
├──────────────────────────────────────┬──────────────────────────────────────┤
│ 1. Standard PeppyMeter Assets        │ 2. Slint Native Execution            │
├──────────────────────────────────────┼──────────────────────────────────────┤
│ • bgr.png (800x480 dial faceplate)   │ ➔ slint::Image (Loaded to GPU/FB)    │
│ • needle.png (transparent sprite)    │ ➔ Image { rotation-angle: ... }      │
│ • fpl.png (glass / bezel overlay)    │ ➔ Alpha-blended top layer            │
│ • meters.txt (INI coordinates)       │ ➔ Ingested by peppy_loader.rs        │
└──────────────────────────────────────┴──────────────────────────────────────┘
```

##### 1. Slint Declarative Dual-Needle Component (`peppy_meter.slint`):
```slint
export component PeppyMeterView {
    in property <image> bgr_texture;
    in property <image> needle_texture;
    in property <image> fpl_texture;
    in property <angle> left_needle_angle;
    in property <angle> right_needle_angle;
    in property <length> left_pivot_x;
    in property <length> left_pivot_y;
    in property <length> right_pivot_x;
    in property <length> right_pivot_y;

    // 1. Dial Faceplate Background
    Image {
        width: 100%;
        height: 100%;
        source: root.bgr_texture;
    }

    // 2. Left Channel Needle (Subwoofer: 45 Hz - 180 Hz)
    Image {
        source: root.needle_texture;
        rotation-angle: root.left_needle_angle;
        rotation-origin-x: root.left_pivot_x;
        rotation-origin-y: root.left_pivot_y;
    }

    // 3. Right Channel Needle (Coaxial Satellite: 180 Hz+)
    Image {
        source: root.needle_texture;
        rotation-angle: root.right_needle_angle;
        rotation-origin-x: root.right_pivot_x;
        rotation-origin-y: root.right_pivot_y;
    }

    // 4. Front Panel / Glass Bezel Reflection Overlay
    if root.fpl_texture.width > 0 : Image {
        width: 100%;
        height: 100%;
        source: root.fpl_texture;
    }
}
```

##### 2. Dynamic Skin Parser (`src/peppy_loader.rs`):
* At startup, the Rust engine parses standard `meters.txt` definitions:
  - `left.origin.x` / `left.origin.y` $\to$ `left_pivot_x` / `left_pivot_y`
  - `right.origin.x` / `right.origin.y` $\to$ `right_pivot_x` / `right_pivot_y`
  - `left.min.angle`, `left.max.angle`, `right.min.angle`, `right.max.angle`
* **Zero Asset Conversion Needed:** Any standard 800×480 PeppyMeter skin folder (e.g. *McIntosh*, *Marantz*, *Accuphase*, *Technics*, *Studer ReVox*) can be copied directly into `/opt/audio-ui/skins/` without re-editing PNG files.
* **Ballistic Needle Smoothing Math:** Rust calculates logarithmic VU ballistics with smooth attack ($15\text{ ms}$) and gentle exponential decay ($300\text{ ms}$) with critically damped spring physics directly from CamillaDSP's real-time dBFS readings.

---

### Screen 3: Bluetooth Device Manager
* **Header Controls:**
  - **"Make Discoverable (180s)" Toggle:** Turns on BlueZ pairing mode. Displays a live countdown ring and pulses the EC12 RGB LED in Blue.
  - **"Scan Nearby" Button:** Performs an active inquiry scan and displays discoverable devices nearby.
* **Paired Devices List:**
  - Each item shows: Device Name, Bluetooth Address, Connection Badge (`🟢 Active` or `⚪ Disconnected`).
  - **Action 1: Connect / Disconnect:** 1-tap connection toggle.
  - **Action 2: Forget / Remove:** Trashcan icon with confirmation to unpair from BlueZ.
* **Integrated Auto-Trust Pairing Agent (`org.bluez.Agent1`):** The native Rust `audio-ui` process hosts the `org.bluez.Agent1` D-Bus interface directly via `zbus`, automatically authorizing and trusting incoming pairing requests without PIN prompts and eliminating the legacy Python `bt-agent.service`.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ BLUETOOTH DEVICE MANAGER               [📡 Make Discoverable (180s)] [🔍 Scan]│
├──────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  📱 Pixel 10 Pro XL          [30:E0:44:67:38:A2]                             │
│     Status: 🟢 Connected (Active Stream)           [Disconnect]  [🗑️ Forget] │
│  ──────────────────────────────────────────────────────────────────────────  │
│  💻 ThinkPad X1              [E8:48:B8:C2:19:04]                             │
│     Status: ⚪ Disconnected                        [ Connect  ]  [🗑️ Forget] │
│  ──────────────────────────────────────────────────────────────────────────  │
│  🎧 Sony WH-1000XM5          [CC:98:8B:11:22:33]                             │
│     Status: ⚪ Disconnected                        [ Connect  ]  [🗑️ Forget] │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
```

---

### Screen 4: CD Player (Optical Disc Control)
*Note: Conditionally enabled. This screen and its navigation entry are displayed only when the CD player service is explicitly enabled in configuration; currently disabled by default.*
* **Disc Status & Metadata:**
  - Detects optical disc presence via `udisks2` / `/dev/cdrom`.
  - Disc Type Indicator: `Audio CD (CD-DA 44.1 kHz / 16-bit PCM)`.
  - Queries CDDB / MusicBrainz by disc TOC calculation to display Album Title and Track Names.
* **CD Transport Controls:**
  - Direct controls: `⏮️ Previous Track`, `⏪ 10s Rewind`, `⏯️ Play/Pause`, `⏩ 10s Fast-Forward`, `⏭️ Next Track`.
  - `⏏️ Eject Disc` button with confirmation prompt.
* **Visuals:**
  - Spinning optical disc graphic rotating at realistic speeds matching playback state.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ OPTICAL CD PLAYER                     [CD-DA 44.1kHz / 16-bit]   [⏏️ Eject]   │
├──────────────────────────────────┬───────────────────────────────────────────┤
│                                  │  Track 04: Bohemian Rhapsody (05:55)      │
│   ┌──────────────────────────┐   │  Queen • A Night at the Opera (1975)      │
│   │                          │   │                                           │
│   │     SPINNING OPTICAL     │   │  [02:14] ━━━━━━━━●─────────── [05:55]     │
│   │        DISC ART          │   │                                           │
│   │                          │   │     [⏮️]   [⏪ 10s]  [ ⏯️ ]  [⏩ 10s]  [⏭️]  │
│   └──────────────────────────┘   ├───────────────────────────────────────────┤
│                                  │  Track List: [01] [02] [03] [▶ 04] [05]   │
└──────────────────────────────────┴───────────────────────────────────────────┘
```

---

### Screen 5: DSP & Tone Control (CamillaDSP Tuning)
* **Master Tone & Equalization:**
  - 3-Band Parametric Graphic EQ: Bass (100 Hz Shelf), Mid (1 kHz Peak), Treble (10 kHz Shelf) with `[-6 dB ... 0 dB ... +6 dB]` sliders.
  - Subwoofer Trim Slider: Independent gain adjustment for Channel 0 (`[-10 dB ... +3 dB]`).
* **Active Crossover Presets:**
  - Preset Selector Pills: `Flat / Reference`, `Bass Boost (+3dB Sub)`, `Late Night (Reduced Bass)`, `Vocal Forward`.
* **Hardware Limiter & Protection Telemetry:**
  - Live status indicators for CamillaDSP clipping and Infineon MA12070P hardware limiter attenuation (`0.0 dB` headroom remaining).

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ DSP & ACTIVE CROSSOVER TUNING                          [Presets: Reference ▼]│
├──────────────────────────────────────────────────────────────────────────────┤
│  BASS (100 Hz)             MIDRANGE (1 kHz)           TREBLE (10 kHz)        │
│   +2.0 dB                   0.0 dB                     -1.0 dB               │
│   [━━━━━━●━━━━━]            [━━━━━●━━━━━━]             [━━━━●━━━━━━━]        │
│                                                                              │
│  SUBWOOFER TRIM (Ch 0)     COAXIAL TRIM (Ch 1)        LIMITER HEADROOM       │
│   +1.5 dB                   0.0 dB                     3.5 dB (Safe 🟢)      │
│   [━━━━━━●━━━━━]            [━━━━━●━━━━━━]             [██████████░░]        │
└──────────────────────────────────────────────────────────────────────────────┘
```

---

### Screen 6: System Health, Thermals & Fan
* **Thermal Telemetry:**
  - Enclosure Internal Air Temperature from DS18B20 1-Wire probe (`26.4°C`).
  - Raspberry Pi SoC Core Temperature from `/sys/class/thermal/` (`48.2°C`).
* **Cooling Fan & Tachometer:**
  - Live fan status: `Active (PWM: 45%)`.
  - Live measured fan speed from GPIO 6 interrupt counter (`1420 RPM`).
  - Manual override toggle: `Auto (PID Curve)` vs. `Manual Fixed Speed`.
* **Hardware Power & Amp Health:**
  - InnoMaker Merus MA12070P PVDD Voltage (`24.2 V`), Power Mode Profile (`PMP1 Dynamic`), and Error Latches (`0x00 - Normal`).

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ SYSTEM HEALTH & THERMAL STATUS                                [Fan Mode: Auto]│
├──────────────────────────┬──────────────────────────┬────────────────────────┤
│ ENCLOSURE TEMPERATURE    │ BCM2837 SOC TEMPERATURE  │ NOCTUA COOLING FAN     │
│       26.4 °C            │        48.2 °C           │    1420 RPM (45% PWM)  │
│ 1-Wire DS18B20 (Valid)   │ Sysfs Thermal Zone 0     │ GPIO 6 Tach Feedback   │
├──────────────────────────┼──────────────────────────┼────────────────────────┤
│ AMBIENT ROOM LIGHT       │ MERUS MA12070P AMPLIFIER │ SYSTEM UPTIME & LOAD   │
│       142 Lux            │ PVDD: 24.2V | PMP1 Idle  │ Uptime: 4d 12h 18m     │
│ BH1750 I2C (Address 0x23)│ I2C Status: 0x00 (Normal)│ Load: 0.08, 0.12, 0.09 │
└──────────────────────────┴──────────────────────────┴────────────────────────┘
```

---

### Screen 7: System Preferences & Behavior Settings
* **EC12 Knob Push Button Actions:**
  - Single Click: `Play / Pause` (default), `Mute / Unmute`, `Next Track`.
  - Double Click: `Next Track` (default), `Mute`, `Cycle Screens`.
  - Long Press (>1.0s): `Display Sleep / Standby` (default), `Mute / Unmute`.
* **Display & Theme Preferences:**
  - PeppyMeter Screensaver Timeout: `Disabled`, `15s`, `30s` (default), `1m`, `2m`.
  - Active Global Theme: `McIntosh Blue`, `Marantz Amber`, `Cyberpunk Neon`, `Studio Dark`.
  - Display Orientation: `Normal (0°)` vs. `Inverted (180°)`.
  - Auto-Backlight Dimming: `Enabled` (via BH1750 lux curve) vs. `Manual Fixed`.
* **Sleep Timer:**
  - Duration Selector: `Disabled`, `15 min`, `30 min`, `45 min`, `60 min`.
  - Behavior: Ramps CamillaDSP master gain from current level to −∞ over the final 30 seconds, then writes `0` to `/sys/class/backlight/rpi_backlight/brightness` for display off.
* **System Maintenance Actions:**
  - `[ 🔄 Reboot System ]` and `[ 🔁 Restart Audio Stack ]`.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ SYSTEM PREFERENCES & HARDWARE CONTROLS                                       │
├──────────────────────────┬──────────────────────────┬────────────────────────┤
│ ROTARY ENCODER BUTTON    │ BLUETOOTH POLICIES       │ DISPLAY & THEME        │
│ • Single: [Play/Pause ▼] │ • Auto-Reconnect: [ON  ] │ • Theme: [McIntosh ▼]  │
│ • Double: [Next Track ▼] │ • AVRCP Auto-Play:[ON  ] │ • Timeout: [30s ▼]     │
│ • Long:   [Standby    ▼] │ • Pairing: [Auto-Accept] │ • Invert 180°: [OFF ]  │
├──────────────────────────┴──────────────────────────┴────────────────────────┤
│ SLEEP TIMER: [Disabled ▼]  (15m / 30m / 45m / 60m with gradual DSP gain fade)  │
├──────────────────────────────────────────────────────────────────────────────┤
│ APPLIANCE ACTIONS:   [ 🔄 Reboot System ]   [ 🔁 Restart Audio Stack (DSP/BT) ]│
└──────────────────────────────────────────────────────────────────────────────┘
```

---

### Screen 8: Ethernet Network Setup (DHCP & Static Configuration)
* **Interface Status:** Displays `eth0` link speed (100 Mbps Full Duplex), MAC address, and current IP (`192.168.2.110`).
* **Mode Selection:** `DHCP (Automatic)` vs. `Static IP (Manual)`.
* **Touch Numeric Keypad:** Dedicated on-screen keypad to enter IP, Subnet Mask, Gateway, and DNS with 30-second failsafe rollback.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ ETHERNET CONFIGURATION (eth0)                                [Mode: Static ▼]│
├──────────────────────────────────────┬───────────────────────────────────────┤
│ IP ADDRESS:     192.168.2.110        │     [ 1 ]    [ 2 ]    [ 3 ]           │
│ SUBNET MASK:    255.255.255.0        │     [ 4 ]    [ 5 ]    [ 6 ]           │
│ GATEWAY:        192.168.2.1          │     [ 7 ]    [ 8 ]    [ 9 ]           │
│ PRIMARY DNS:    1.1.1.1              │     [ . ]    [ 0 ]    [ ⌫ ]           │
├──────────────────────────────────────┴───────────────────────────────────────┤
│ [ 💾 Apply Configuration (30s Failsafe Rollback) ]          [ Revert / Cancel]│
└──────────────────────────────────────────────────────────────────────────────┘
```

---

### Screen 9: Clean Startup Splash Screen (Shown on Boot)
* Rendered instantaneously upon service start (< 150 ms).
* Displays branding logo, firmware version, and real-time Power-On Self-Test (POST) progress checks.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│                                                                              │
│                         ◆ DIETPI AUDIOPHILE CONSOLE ◆                        │
│                                                                              │
│                      High-Resolution Digital Audio Sink                      │
│                               Version 2.0-rust                               │
│                                                                              │
│                    [ Hardware Initialization: 100% ]                         │
│                    ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━                         │
│                    ✓ I2S Amplifier: InnoMaker Merus MA12070P                 │
│                    ✓ CamillaDSP WebSocket: Connected                         │
│                    ✓ BlueZ 5 Stack: UB500 Controller Ready                   │
│                    ✓ DSI Touch Controller: raspberrypi-ts                    │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
```

---

### Screen 10: Semi-Hidden Hardware Diagnostics & Bench-Test Page
* Accessible by tapping the Audio Codec Badge 5 times consecutively.
* Provides full hardware verification tests without external equipment:
  - 50 Hz Subwoofer & 1 kHz Coaxial Sine Wave generator.
  - Live virtual rotary dial and push button touch visualizer.
  - RGB LED test buttons (Red, Green, Blue, White).
  - Noctua fan speed ramp & tachometer RPM benchmark.
  - Live sensor readings & MA12070P fault clear trigger.
  - Interactive touchscreen accuracy crosshair targets.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ HARDWARE DIAGNOSTICS & BENCH-TEST MODE                     [ ❌ Exit to Main ]│
├──────────────────────────┬──────────────────────────┬────────────────────────┤
│ AUDIO GENERATOR TEST     │ ENCODER & RGB LED TEST   │ COOLING FAN BENCHMARK  │
│ • [ ▶ 50 Hz Sine (Sub) ] │ • Knob Angle: +14 detents│ • Duty: [ 30% | 70% ]  │
│ • [ ▶ 1 kHz Sine (Coax)] │ • Button: [ RELEASED ]   │ • Measured: 1840 RPM   │
│ • [ ▶ Pink Noise Sweep ] │ • LED: [R] [G] [B] [W]   │ • Tachometer: 🟢 OK    │
├──────────────────────────┼──────────────────────────┼────────────────────────┤
│ SENSORS & THROTTLING     │ I2C AMP REGISTERS        │ TOUCHSCREEN CALIBRATE  │
│ • 1-Wire DS18B20: 26.4°C │ • Address 0x20: Active   │ • Crosshair Calibration│
│ • BH1750 Lux: 142 lx     │ • Error Reg 0x6D: 0x00   │ • Raw Touch: (412, 230)│
│ • Throttled: 0x0 (None)  │ • [ Clear Latched Faults]│ • Points Detected: 1   │
└──────────────────────────┴──────────────────────────┴────────────────────────┘
```

---

### Screen 11: Modal Touch QWERTY Keyboard & System Alert Overlays

#### 1. Modal Touch QWERTY Keyboard (Overlay on Bottom 240px)
Activated automatically whenever an editable text field is tapped (e.g., Bluetooth Broadcast Name in Settings or Wi-Fi Security Key):

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ EDIT BLUETOOTH BROADCAST NAME                        [Current: DietPi Speaker]│
│ ┌──────────────────────────────────────────────────────────────┬───────────┐ │
│ │ DietPi Living Room Audio_                                    │  [CLEAR]  │ │
│ └──────────────────────────────────────────────────────────────┴───────────┘ │
├──────────────────────────────────────────────────────────────────────────────┤
│  [ q ] [ w ] [ e ] [ r ] [ t ] [ y ] [ u ] [ i ] [ o ] [ p ]   [  ⌫ BACK  ]  │
│    [ a ] [ s ] [ d ] [ f ] [ g ] [ h ] [ j ] [ k ] [ l ]       [  ✓ DONE  ]  │
│  [ ⇧ SHIFT ]  [ z ] [ x ] [ c ] [ v ] [ b ] [ n ] [ m ] [ - ]  [ ⇧ SHIFT ]  │
│  [ ?123 SYM ]   [             SPACEBAR             ]   [ . ]   [ CANCEL ]    │
└──────────────────────────────────────────────────────────────────────────────┘
```

#### 2. Hardware Amp Fault Alert Banner (Drop-Down Top Notification)
Appears instantaneously when the MA12070P triggers protection:

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ ⚠️ AMP FAULT: Overcurrent Latch on Ch 0 [Sub]         [ 🔄 Reset Amp Stage ] │
├──────────────────────────────────────────────────────────────────────────────┤
│ (Active Screen Content Continues Below with Muted Master DSP Output)         │
```

#### 3. DSP Reconnecting Pill (Non-Blocking Top-Bar Indicator)
Displayed whenever CamillaDSP WebSocket communication drops or is delayed during startup:

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ [📶 Bluetooth ▼] Pixel 10 Pro XL   [ ⚠️ DSP Connecting... ]   [🌙]   [ 26.4°C]│
└──────────────────────────────────────────────────────────────────────────────┘
```

---

## 5. Rust Architecture & Module Specifications

### 5.1. Concurrency Architecture: Tokio Actor Model
The system uses **Tokio channels (`tokio::sync::mpsc` and `tokio::sync::broadcast`)** to communicate between isolated, non-blocking actor tasks:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                           TOKIO ACTOR ARCHITECTURE                          │
└─────────────────────────────────────────────────────────────────────────────┘

  [Actor 1: Hardware Encoder & Button]       [Actor 2: BlueZ D-Bus & Pairing Agent]
  - Phase A: BCM 25 (Active-LOW, pull-UP)    - zbus connection to org.bluez
  - Phase B: BCM 24 (Active-LOW, pull-UP)    - Track metadata & AVRCP events
  - Push:   BCM 22 (Active-HIGH, pull-DOWN)  - Hosts org.bluez.Agent1 (No-PIN Pairing)
  - Rotary Quadrature State Machine
  - RGB LED: BCM 20/16/12 (Active-LOW sink)
             │                                          │
             ▼                                          ▼
   (Event::VolumeDelta)                       (Event::TrackMeta / PairRequest)
             │                                          │
             └──────────────────┬───────────────────────┘
                                │
                                ▼  mpsc::Sender<AppEvent>
                ┌───────────────────────────────┐
                │     CENTRAL APP DISPATCHER    │
                │  - Updates State Model        │
                │  - Dispatches to Slint UI     │
                └───────────────┬───────────────┘
                                │
             ┌──────────────────┴───────────────────────┐
             │                                          │
             ▼                                          ▼
  [Actor 3: CamillaDSP Client]              [Actor 4: Slow Hardware Poller]
  - tokio-tungstenite WebSocket              - 1-Wire DS18B20 (750 ms read)
  - 60 FPS Peak & RMS Telemetry              - BH1750 I2C @ 0x23 Ambient Light
  - Rate adjust & clip counter               - MA12070P I2C @ 0x20 Fault Monitor
                                             - Fan PID Thermal Controller
                                             ⚠ I2C-1 bus shared: BH1750 + MA12070P
                                               Must serialize via Arc<Mutex<I2c>>
```

---

### 5.2. Pinned Toolchain & Complete `Cargo.toml` Production Dependencies

#### 1. Toolchain Pinning (`rust-toolchain.toml`):
To guarantee 100% deterministic compilation and prevent compiler version drift, the toolchain is strictly pinned to the latest stable release:
```toml
[toolchain]
channel = "1.99.1"
components = ["rustfmt", "clippy"]
targets = ["aarch64-unknown-linux-gnu"]
```

#### 2. Exact Pinned `Cargo.toml`:
All crates are pinned to their exact latest release version using the `=` prefix:

```toml
[package]
name = "audio-ui"
version = "2.0.0"
edition = "2024"
rust-version = "1.99.1"
authors = ["Audio Appliance Team"]
description = "High-Performance Audiophile Appliance UI & Hardware Console for Raspberry Pi"

[dependencies]
# UI Framework (Slint with Linux Framebuffer & KMS/DRM backends)
slint = { version = "=1.9.2", default-features = false, features = ["compat-1-2", "backend-linuxkms", "renderer-software"] }

# Asynchronous Concurrency Runtime
tokio = { version = "=1.53.3", features = ["full"] }
tokio-tungstenite = { version = "=0.30.0", features = ["connect"] }
futures-util = "=0.3.34"

# D-Bus Engine for BlueZ 5 & BlueALSA
zbus = { version = "=5.19.0", default-features = false, features = ["tokio"] }

# Serialization & JSON-RPC
serde = { version = "=1.0.229", features = ["derive"] }
serde_json = "=1.0.140"

# Raspberry Pi Hardware Peripherals (GPIO, Hardware PWM, I2C)
rppal = { version = "=0.22.1", features = ["hal"] }

# Image Processing (Cover Art Scaling & Procedural Vinyl)
image = { version = "=0.25.10", default-features = false, features = ["png", "jpeg"] }

# System Telemetry & Thermals
sysinfo = "=0.39.6"

# Lightweight Web Remote Companion
axum = "=0.8.9"
tower-http = { version = "=0.7.1", features = ["cors", "fs"] }

# PeppyMeter meters.txt INI Configuration Parser
rust-ini = "=0.21.3"

# Logging & Structured Tracing
tracing = "=0.1.44"
tracing-subscriber = "=0.3.23"

# Async Trait for Trait-based Dependency Injection / Mocks
async-trait = "=0.1.86"

[features]
default = []
simulator = ["slint/backend-winit", "slint/renderer-femtovg"]

[dev-dependencies]
tokio-test = "=0.4.4"
tempfile = "=3.17.1"

[build-dependencies]
slint-build = "=1.9.2"

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
incremental = false
```

---

### 5.3. Compiler Optimization Matrix: ARM NEON SIMD & Cortex-A53 Codegen Tuning

To extract the absolute maximum performance from the Raspberry Pi 3's Broadcom BCM2837 SoC (quad-core ARM Cortex-A53 @ 1.2 GHz), compilation is strictly configured via **`.cargo/config.toml`** and release profile flags:

```toml
# File: .cargo/config.toml
[build]
target = "aarch64-unknown-linux-gnu"
rustflags = [
    # 1. Target the exact in-order dual-issue pipeline of the BCM2837 Cortex-A53
    "-C", "target-cpu=cortex-a53",

    # 2. Force hardware ARM NEON 128-bit vectorization & ARMv8 hardware floating point
    "-C", "target-feature=+neon,+fp-armv8",

    # 3. LLVM vectorizer passes for pixel blitting, Slint rasterization & meter math
    "-C", "llvm-args=-vectorize-loops",
    "-C", "llvm-args=-vectorize-slp",

    # 4. Linker optimizations: strip unused sections & garbage collect dead code
    "-C", "link-arg=-Wl,--as-needed",
    "-C", "link-arg=-Wl,--gc-sections",
    "-C", "link-arg=-Wl,-O1",
]
```

#### Detailed Breakdown of Codegen Optimizations:

| Optimization Flag | Layer | How It Optimizes the Pi 3 Appliance |
| :--- | :--- | :--- |
| **`target-cpu=cortex-a53`** | LLVM Codegen | Configures LLVM’s instruction scheduler specifically for the Cortex-A53’s 8-stage dual-issue in-order superscalar pipeline and L1/L2 cache latency, avoiding pipeline stalls. |
| **`target-feature=+neon`** | ARM SIMD | Activates the 128-bit **ARM NEON vector execution units**. Slint's software renderer and image alpha-blending process **4 pixels (16 bytes) per instruction cycle**, accelerating needle rendering and glass reflections by over 300%. |
| **`target-feature=+fp-armv8`** | Floating Point | Enforces hardware ARMv8 VFPv4 floating-point instructions for real-time logarithmic VU needle ballistics and CamillaDSP dBFS scaling. |
| **`-vectorize-loops / -slp`** | LLVM Passes | Enables Superword-Level Parallelism (SLP) vectorization, merging scalar operations into single NEON vector instructions automatically across image scaling loops. |
| **`lto = "fat"`** | Linker (LTO) | Monolithic cross-crate Link-Time Optimization. Allows LLVM to inline functions across `slint`, `tokio`, `image`, and `zbus` boundaries, turning cross-module function calls into direct machine code. |
| **`codegen-units = 1`** | Rustc | Eliminates multi-threading during final machine code generation so LLVM has global visibility over the entire binary, unlocking aggressive dead-code elimination. |
| **`panic = "abort"`** | Runtime | Strips unwinding tables and landing pads. Reduces binary size by ~25% and eliminates stack-unwinding branch checks. |
| **`--gc-sections`** | GNU ld / lld | Garbage-collects unused C/Rust functions and symbols at link time, producing an ultra-lean binary (< 12 MB) that fits comfortably in L2 cache. |

---

## 6. Artwork & Metadata Pipeline

```text
[Bluetooth AVRCP 1.6 Event]
              │
              ▼
  [Check BIP CoverArt Token]
         ├── Token Present? ──► [Download over OBEX BIP] ──► [Display Image]
         │
         └── No BIP Token?
                   │
                   ▼
       [Query MusicBrainz / iTunes API] (Online Cache)
              ├── Found? ──► [Download & Cache JPEG] ──► [Display Image]
              │
              └── Offline / Not Found?
                            │
                            ▼
              [Procedural Vinyl Generator]
                 - Renders high-res vinyl grooves
                 - Center label printed with Artist + Title
```

---

## 7. Implementation Roadmap & Phases

### Phase 1: Rust Workspace Setup, Trait Abstractions & Automated Test Rig
- Initialize Cargo project with `slint`, `tokio`, `zbus`, and `rppal`.
- Define trait abstractions (`PeripheralController`, `DspClient`, `BluetoothManager`) to isolate target-only hardware dependencies.
- Implement headless Slint test harness (`slint::testing::init()`) for unit testing touches, auto-dismiss timers, and screensaver transitions (see Section 9).
- Build in-memory mock BlueZ D-Bus service (`mock_bluetooth.rs`) and mock CamillaDSP WebSocket server (`mock_dsp.rs`).
- Configure workstation interactive desktop simulator (`cargo run --features simulator`) with developer keybindings.

### Phase 2: Core Bluetooth Backend (`bluetooth_manager.rs`)
- Implement typed BlueZ proxy traits using `zbus`.
- Implement device enumeration, pairing trust, and 180s discoverable toggle.
- Implement native `org.bluez.Agent1` D-Bus agent directly inside Rust (auto-authorize & auto-trust).
- Decommission and disable legacy Python `bt-agent.service` (`/usr/local/bin/bt-auto-trust-agent.py`).
- Add AVRCP track metadata listener (Title, Artist, Album, Progress, Volume).

### Phase 3: Framebuffer UI Framework in Slint
- Write declarative `.slint` definitions for Status Bar, Navigation Dock, and Main Viewport.
- Map capacitive touch inputs from `raspberrypi-ts` via Linux `evdev`.
- Implement the 4 global themes in `ThemePalette` with instant hot-swapping.

### Phase 4: PeppyMeter VU Needle Visualizer & Skin Ingestion Engine
- Build `peppy_loader.rs` to parse standard community `meters.txt` INI definitions (origins, angles, image paths).
- Connect high-speed `tokio-tungstenite` client to CamillaDSP on `127.0.0.1:1234`.
- Implement logarithmic dB scale needle physics with damping math in Rust.
- Build Slint needle animation components with 60 FPS update loop mapping Channel 0 (Sub) and Channel 1 (Coax).
- Support dropping standard community 800x480 skins directly into `/opt/audio-ui/skins/`.
- Integrate 30-second auto-screensaver timer.

### Phase 5: Hardware Peripherals Integration
- Implement EC12 rotary encoder quadrature state machine using `rppal::gpio` asynchronous interrupts:
  - **Phase A (BCM 25) & Phase B (BCM 24):** Configure as `InputPin` with `set_pullup()`, detect on **falling edge** (Active-LOW quadrature).
  - **Push Button (BCM 22):** Configure as `InputPin` with `set_pulldown()`, detect on **rising edge** (Active-HIGH, wired to 3.3V).
  - ⚠️ Push button polarity is opposite to Phase A/B — do not use the same edge detection config.
- Integrate EC12 volume changes to trigger the universal 800x380 full-viewport Volume HUD overlay across all screens with 2.5s auto-dismiss.
- Implement push button actions (play/pause, mute, screen toggle).
- **RGB LED Status Lighting (Active-LOW Common-Anode):**
  - LEDs on BCM 20 (Red, 330Ω), BCM 16 (Green, 180Ω), BCM 12 (Blue, 180Ω).
  - ⚠️ **Inverted logic:** `pin.set_low()` = LED **ON**, `pin.set_high()` = LED **OFF**.
  - Color mixing: White = all three LOW, Off = all three HIGH.
- **Noctua Fan PWM Speed Control (BCM 13):**
  - Use `rppal::pwm::Pwm` hardware PWM channel 1 (PWM1) at 25 kHz for silent, jitter-free operation if DTS permits.
  - Fallback: software PWM via `rppal::gpio::OutputPin` at 100 Hz using a Tokio interval timer (matching existing Python behavior).
  - Fan tachometer on BCM 6: falling-edge interrupt with external 10kΩ pull-up, 2 pulses per revolution → `RPM = (pulses / 2) × (60 / elapsed_seconds)`.
- **I2C-1 Bus Multiplexing (Shared Bus Warning):**
  - BH1750 light sensor (addr `0x23`) and MA12070P amplifier (addr `0x20`) share I2C-1.
  - All I2C access must be serialized through a shared `Arc<Mutex<rppal::i2c::I2c>>` wrapper to prevent bus contention.
- Integrate DS18B20 1-Wire temperature polling (sysfs read at `/sys/bus/w1/devices/28-*/w1_slave`) and BH1750 auto-backlight dimming curve.

### Phase 6: CD Player Interface & Stubs for AirPlay/UPnP
- Implement CD Player UI screen with spinning disc animation and track list.
- Build backend disc detection using UDisks2 D-Bus interface via `zbus` (object path `/org/freedesktop/UDisks2/block_devices/sr0`).
- **mpv CD Audio Playback Backend:**
  - Spawn `mpv --idle --input-ipc-server=/tmp/mpv-cd.sock --ao=alsa --audio-device=alsa/dsp_dmix cdda://` as a managed child process.
  - Control via JSON IPC over Unix socket (`tokio::net::UnixStream`) — commands: `loadfile cdda://1-`, `seek`, `playlist-next/prev`, `pause`, `stop`.
  - Query CDDB/MusicBrainz by disc TOC for track metadata.
- Add AirPlay and UPnP status listeners for seamless source switching.

### Phase 7: Boot Splash (POST) & Interactive Diagnostics Screen
- Implement startup Power-On Self-Test (POST) checking sound cards, DSP socket, Bluetooth, sensors, fan, and display before opening main view.
- Build interactive hardware diagnostics screen (`Screen 10`).

### Phase 8: Appliance Hardening & Systemd Deployment
- Deploy `/etc/systemd/system/audio-ui.service` with framebuffer permissions and restart policies.
- Configure silent boot parameters in `cmdline.txt` (suppress console flicker and cursor).
- Implement DSI backlight udev rules and 180° touch coordinate inversion matrix.
- Integrate modal QWERTY touch keyboard for Bluetooth renaming and network setup.
- Add MA12070P I2C fault latch alert banner with 1-tap hardware reset.

---

## 8. Appliance Production Hardening & Operational Edge Cases

### 8.1. Appliance Boot & Systemd Service Lifecycle
* **Silent Boot Experience (Zero Console Flicker):**
  * Suppress standard Linux kernel console output, text scroll, and blinking tty cursor on the DSI display via `/boot/firmware/cmdline.txt`:
    ```text
    console=tty3 loglevel=3 quiet vt.global_cursor_default=0
    ```
  * The compiled Rust binary takes over `/dev/fb0` in **< 150 ms**, rendering the clean splash logo instantly.
* **Production Systemd Service Unit (`/etc/systemd/system/audio-ui.service`):**
  ```ini
  [Unit]
  Description=Audiophile Appliance UI & Framebuffer Engine (Rust)
  After=sound.target camilladsp.service bluealsa.service bluetooth.service systemd-udevd.service
  Wants=camilladsp.service bluealsa.service

  [Service]
  Type=simple
  User=dietpi
  Group=dietpi
  SupplementaryGroups=video input i2c gpio
  WorkingDirectory=/opt/audio-ui
  Environment=SLINT_BACKEND=linuxkms
  Environment=FRAMEBUFFER=/dev/fb0
  ExecStart=/usr/local/bin/audio-ui
  Restart=always
  RestartSec=1s
  StandardOutput=journal
  StandardError=journal

  [Install]
  WantedBy=multi-user.target
  ```

* **Service Consolidation & Decommissioning of `bt-agent.service`:**
  * In the existing setup, an external Python daemon `/usr/local/bin/bt-auto-trust-agent.py` was running as `bt-agent.service` to authorize Bluetooth connections.
  * In this Rust architecture, `audio-ui` **natively implements the BlueZ `org.bluez.Agent1` D-Bus interface** using `zbus`.
  * The legacy service is disabled and masked during deployment:
    ```bash
    systemctl disable --now bt-agent.service
    systemctl mask bt-agent.service
    ```
  * **Result (Zero Python Daemons):** 100% of the runtime audio appliance services on DietPi are pure compiled binaries:
    - **CamillaDSP:** Native Rust binary (`/usr/local/bin/camilladsp`)
    - **Audio UI & Agent:** Native Rust binary (`/usr/local/bin/audio-ui`)
    - **BlueALSA & BlueZ:** Native C binaries (`/usr/bin/bluealsa`, `/usr/libexec/bluetooth/bluetoothd`)

---

### 8.2. Audio Source Arbitration & CamillaDSP Mixer Routing
* **Source Hierarchy & Priority:**
  1. **Optical CD Player (`cdda://`):** Highest priority. Inserting a disc or pressing Play sends an AVRCP `Pause` command to any connected Bluetooth smartphone and switches the active source pill.
  2. **Bluetooth A2DP (Smartphone):** Resumes on phone Play command or source pill selection.
  3. **AirPlay / UPnP:** Secondary network streaming inputs.
* **Unified DSP Crossover Ingestion:**
  * Both BlueALSA (`bluealsa-aplay`) and the CD player backend (`mpv --ao=alsa --audio-device=dsp_dmix`) route exclusively to the ALSA `dsp_dmix` plugin on `hw:Loopback,0,0`.
  * CamillaDSP captures continuously from `hw:Loopback,1,0`, applying crossover filtering and limiters before outputting to the InnoMaker Merus amp (`hw:sndrpimerusamp,0`).

---

### 8.3. Display Backlight Sysfs Path & Enclosure Rotation
* **Sysfs Backlight Path & Udev Permissions:**
  * Install `/etc/udev/rules.d/99-backlight.rules`:
    ```udev
    SUBSYSTEM=="backlight", RUN+="/bin/chmod 666 /sys/class/backlight/%k/brightness /sys/class/backlight/%k/bl_power"
    ```
* **180° Enclosure Mounting & Touch Inversion:**
  * When `display_rotation == 180` in `/data/config/ui_settings.json`:
    * Slint window is rotated 180°.
    * Touchscreen coordinates are transformed:
      $$X_{touch} = 799 - X_{raw}, \quad Y_{touch} = 479 - Y_{raw}$$

---

### 8.4. Resilient Error Recovery & Reconnect State Machines
* **CamillaDSP WebSocket Crash or Delayed Startup:**
  * Tokio background loop reconnects with exponential backoff (1s, 2s, 5s, max 10s).
  * A non-blocking status pill appears on the top bar: `[ ⚠️ DSP Connecting... ]`.
* **BlueZ D-Bus & Bluetooth Adapter Disconnect:**
  * `zbus` catches the disconnect signal, sets the status pill to `[ 📶 BT Offline ]`, and resubscribes upon adapter re-enumeration.
* **1-Wire Temperature Sensor Fallback:**
  * If the DS18B20 probe disconnects or generates CRC errors, the fan controller defaults to a **safe failover duty cycle of 50% PWM**.

---

### 8.5. On-Screen Modal QWERTY Touch Keyboard
* Modal QWERTY Virtual Keyboard overlay covering the bottom 240px with large touch targets ($55 \times 45\text{ px}$), `[Shift]`, `[123/Symbols]`, and `[✓ Done]` for editing the Bluetooth Broadcast Name and entering Wi-Fi credentials.

---

### 8.6. Hardware Amp Protection Alert Banner
* Real-time monitoring of MA12070P I2C register `0x6D`.
* High-visibility top-bar warning banner on overcurrent, undervoltage, or thermal limit with an instant 1-tap `[ 🔄 Reset Amp Stage ]` action executing the I2C register clear command.

---

## 9. Comprehensive Testing, Mocking & Verification Strategy

Developing an audiophile appliance with low-level Linux hardware bindings (`/dev/fb0`, `rppal` GPIO/I2C/PWM, 1-Wire sysfs, D-Bus, CamillaDSP WebSockets) requires a multi-layered testing rig. The goal is: **100% of the UI, state machines, and business logic must be testable locally on a workstation (x86_64 Linux/macOS/WSL) and in CI without requiring physical Raspberry Pi hardware.**

```text
┌────────────────────────────────────────────────────────────────────────┐
│                        TESTING & VERIFICATION SUITE                    │
├───────────────────────────────────┬────────────────────────────────────┤
│ LOCAL / CI (x86_64, No Hardware) │ TARGET HARDWARE (Live Pi 3 Model B)│
├───────────────────────────────────┼────────────────────────────────────┤
│ • Slint Headless UI Tests         │ • Startup Power-On Self-Test (POST)│
│ • Trait Mocks (GPIO/I2C/1-Wire)   │ • Screen 10: Interactive Diagnostics│
│ • Mock CamillaDSP WebSocket Server│ • Automated 25-Point `verify.sh`   │
│ • Mock BlueZ D-Bus Event Stream   │ • Real I2S / MA12070P Audio Output │
│ • Interactive Desktop Simulator   │ • Physical DSI 800x480 Framebuffer │
└───────────────────────────────────┴────────────────────────────────────┘
```

---

### 9.1. Architectural Philosophy: Trait-Based Dependency Injection (DI)

To eliminate hardcoded calls to target-only drivers (such as `rppal` and `/sys/bus/w1`), all hardware peripherals and external daemons are abstracted behind thread-safe Rust traits:

```rust
// 1. Hardware Peripherals Trait
pub trait PeripheralController: Send + Sync {
    fn set_rgb_led(&self, r: bool, g: bool, b: bool) -> Result<()>;
    fn set_fan_pwm(&self, duty_cycle_pct: f32) -> Result<()>;
    fn read_fan_rpm(&self) -> Result<u32>;
    fn read_temperature(&self) -> Result<f32>;
    fn read_ambient_lux(&self) -> Result<f32>;
    fn check_amp_fault(&self) -> Result<Option<AmpFault>>;
    fn reset_amp(&self) -> Result<()>;
}

// 2. CamillaDSP Client Trait
#[async_trait::async_trait]
pub trait DspClient: Send + Sync {
    async fn set_master_volume(&self, db: f32) -> Result<()>;
    async fn set_mute(&self, mute: bool) -> Result<()>;
    async fn subscribe_meter_levels(&self) -> Result<tokio::sync::broadcast::Receiver<MeterLevels>>;
}

// 3. Bluetooth Manager Trait
#[async_trait::async_trait]
pub trait BluetoothManager: Send + Sync {
    async fn set_discoverable(&self, discoverable: bool) -> Result<()>;
    async fn disconnect_device(&self, address: &str) -> Result<()>;
    async fn subscribe_events(&self) -> Result<tokio::sync::broadcast::Receiver<BluetoothEvent>>;
}
```

* **Production Target (`aarch64`):** `HardwarePeripherals` binds to `rppal::gpio`, `rppal::pwm`, `rppal::i2c`, and reads `/sys/bus/w1/devices/28-*/w1_slave`.
* **Test / Simulator Target:** `MockPeripherals` uses atomic variables (`AtomicU32`, `AtomicBool`) and in-memory channels (`tokio::sync::broadcast`) with zero system dependencies.

---

### 9.2. Headless UI Testing Rig (`slint::testing`)

Slint provides a built-in headless test harness that renders entirely in memory without requiring an X11, Wayland, or KMS/DRM display driver. Tests run in standard `cargo test` in milliseconds:

#### 1. Touch & Click Simulation:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use slint::testing::send_mouse_click;
    use slint::LogicalPosition;

    #[test]
    fn test_volume_slider_touch_bounds() {
        let ui = AppWindow::new().unwrap();
        
        // Tap directly on the Volume + button inside the Volume HUD
        send_mouse_click(&ui, LogicalPosition::new(550.0, 240.0));
        assert_eq!(ui.get_master_volume(), 76);

        // Tap on Mute button
        send_mouse_click(&ui, LogicalPosition::new(250.0, 240.0));
        assert!(ui.get_is_muted());
    }
}
```

#### 2. Time Fast-Forwarding (Animations, Auto-Dismiss & Screensaver):
```rust
#[test]
fn test_screensaver_auto_transition() {
    let ui = AppWindow::new().unwrap();
    ui.set_screensaver_enabled(true);
    ui.set_active_screen_id(ScreenId::NowPlaying as i32);

    // Fast-forward mock clock by 31 seconds
    slint::testing::mock_elapsed_time(std::time::Duration::from_secs(31));

    // Verify view automatically transitioned to PeppyMeter
    assert_eq!(ui.get_active_screen_id(), ScreenId::PeppyMeter as i32);
}

#[test]
fn test_volume_hud_auto_dismiss() {
    let ui = AppWindow::new().unwrap();
    ui.invoke_show_volume_hud();
    assert!(ui.get_volume_hud_visible());

    // Fast-forward mock clock past the 2.5s auto-dismiss threshold
    slint::testing::mock_elapsed_time(std::time::Duration::from_millis(2600));
    assert!(!ui.get_volume_hud_visible());
}
```

---

### 9.3. Hardware Input & Peripherals Mock Harness

Physical input devices are tested using event injector streams:

#### 1. EC12 Rotary Encoder & Push Button Mock:
The state machine converts raw quadrature transitions into discrete steps. The test harness feeds synthetic events through a channel:
```rust
#[tokio::test]
async fn test_encoder_step_updates_volume() {
    let (input_tx, input_rx) = tokio::sync::mpsc::channel(16);
    let app_state = AppState::new_mock(input_rx);

    // Simulate 3 fast clockwise clicks (+3 dB)
    input_tx.send(InputEvent::EncoderStep(1)).await.unwrap();
    input_tx.send(InputEvent::EncoderStep(1)).await.unwrap();
    input_tx.send(InputEvent::EncoderStep(1)).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    assert_eq!(app_state.get_volume(), 78);
}
```

#### 2. Fault Injection & Thermal Throttling:
```rust
#[tokio::test]
async fn test_thermal_overheat_fan_ramp() {
    let mock_hw = Arc::new(MockPeripherals::default());
    let fan_controller = FanController::new(mock_hw.clone());

    // Inject 72.0°C reading
    mock_hw.set_mock_temperature(72.0);
    fan_controller.evaluate_step().await;

    // Verify fan duty cycle jumped to 100% PWM and status alert flagged
    assert_eq!(mock_hw.get_fan_pwm_pct(), 100.0);
    assert!(mock_hw.is_rgb_alert_active());
}

#[tokio::test]
async fn test_probe_disconnect_safe_fallback() {
    let mock_hw = Arc::new(MockPeripherals::default());
    let fan_controller = FanController::new(mock_hw.clone());

    // Simulate sensor disconnection (I/O error reading 1-Wire file)
    mock_hw.set_sensor_error(true);
    fan_controller.evaluate_step().await;

    // Failsafe rule: Must default to 50% PWM
    assert_eq!(mock_hw.get_fan_pwm_pct(), 50.0);
}
```

---

### 9.4. Mocking External Daemons & Services

#### 1. Mock CamillaDSP WebSocket Server (`tests/mocks/mock_dsp.rs`):
An in-process WebSocket actor running on an ephemeral localhost port:
* Responds to CamillaDSP JSON-RPC calls (`GetState`, `SetVolume`, `SetMute`).
* Streams synthetic dual-channel peak/RMS frames at 60 Hz:
  - **Sine Wave Sweep:** Tests smooth needle movement and dynamic damping math.
  - **Overload Bursts (> -0.5 dBFS):** Verifies the digital limiter warning indicators flash on the VU meter screen.
  - **Silence (-100 dBFS):** Verifies needles rest gently on the zero-pegs without jitter.

#### 2. Mock BlueZ / BlueALSA D-Bus Client (`tests/mocks/mock_bluetooth.rs`):
* Injects synthetic pairing, connection, and disconnection events.
* Injects rich AVRCP 1.6 metadata payloads (Title, Artist, Album, embedded JPEG cover art bytes).
* Verifies UI layout updates, text truncation rules on long artist names, and cover art aspect-ratio scaling.

#### 3. Mock MA12070P I2C Device:
* Injects fault register states into register `0x6D` (bit 0 = DC protection, bit 2 = over-temperature, bit 4 = overcurrent).
* Asserts that the top-bar emergency alert banner drops down immediately.
* Asserts that tapping `[ 🔄 Reset Amp Stage ]` issues the exact register clear byte sequence over I2C.

---

### 9.5. Local Interactive Desktop Simulator (`cargo run --features simulator`)

For instant visual iteration on developer workstations without deploying to the Pi:

```bash
cargo run --features "simulator"
```

The simulator boots the full Slint UI in a native desktop window (via `backend-winit`) with a live interactive debug sidecar:

| Developer Keybinding | Simulated Action |
| :--- | :--- |
| **`Up Arrow` / `Down Arrow`** | Turn EC12 rotary encoder clockwise / counter-clockwise (+/- 1 dB) |
| **`Spacebar`** | Press EC12 push button (Toggle Play/Pause or dismiss HUD) |
| **`Key 1` – `Key 4`** | Hot-swap between the 4 themes (Dark, Charcoal, Amber, Blue) |
| **`Key B`** | Toggle simulated Bluetooth phone connection on / off |
| **`Key T`** | Simulate temperature spike (> 70°C) to watch fan ramp up |
| **`Key F`** | Trigger simulated MA12070P I2C hardware fault alert banner |
| **`Key V`** | Switch PeppyMeter skins instantly |

---

### 9.6. On-Target Verification & Hardware POST (On the Live Pi 3)

When the binary is deployed to the physical appliance at `192.168.2.110`, verification proceeds in three automated tiers:

1. **Self-Contained Power-On Self-Test (POST):**
   * Built directly into the boot sequence (`Screen 9: Splash Screen`).
   * Iterates through all hardware buses before unveiling the main view:
     - MA12070P I2C ping (`addr 0x20`) $\rightarrow$ `[PASS]`
     - BH1750 Lux sensor ping (`addr 0x23`) $\rightarrow$ `[PASS]`
     - DS18B20 1-Wire probe presence $\rightarrow$ `[PASS]`
     - CamillaDSP WebSocket handshake $\rightarrow$ `[PASS]`
     - BlueZ D-Bus default adapter presence $\rightarrow$ `[PASS]`
   * Any failure displays an explicit warning on the splash screen and logs details to `systemd-journald`.

2. **Screen 10: Interactive Hardware Bench-Test Page:**
   * Secret gesture: Triple-tap the status bar center logo.
   * Provides on-device interactive tests without requiring an SSH terminal:
     - **Sound Generator Test:** Plays pure test tones (40 Hz Sub, 1 kHz Coaxial, Pink Noise) directly through CamillaDSP to verify crossover filters.
     - **RGB LED Cycle:** Cycles Red, Green, Blue, White, and Off to verify inverted active-LOW logic.
     - **Fan Step Benchmark:** Steps fan PWM through 0%, 25%, 50%, 75%, 100% and displays real-time RPM tachometer feedback.
     - **Encoder Counter:** Displays live tick count and button push state.

3. **Automated Baseline Audit Script ([`baseline/verify.sh`](../baseline/verify.sh)):**
   * Run remotely over SSH:
     ```bash
     ssh root@192.168.2.110 "/root/baseline/verify.sh"
     ```
   * Verifies all 25 critical kernel modules, ports, sound cards, and systemd services.

