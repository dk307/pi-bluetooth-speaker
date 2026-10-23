# Custom Peppy-Inspired Touch UI & Audio Console — Architecture & Implementation Plan

**Document:** `ui_architecture_plan.md`  
**Target Hardware:** Raspberry Pi 3 Model B (DietPi / Debian 13 Trixie `aarch64`)  
**Display:** Freenove 4.3" MIPI DSI Display (800x480 IPS Framebuffer `/dev/fb0`)  
**Touchscreen:** Native VideoCore Firmware Touch Driver (`raspberrypi-ts` on `/dev/input/event0`)  
**Audio Hardware:** InnoMaker Merus MA12070P I2S Amplifier (`card 1`) via CamillaDSP v4.1.3 Active Crossover  
**Hardware Controls:** TOP-UP EC12 Rotary Encoder + Push Switch + RGB LED, Noctua 4-Pin PWM Fan, DS18B20 Temp, BH1750 Lux  

---

## 1. Executive Summary & Design Philosophy

This document outlines the full architectural design, screen layouts, subsystem integrations, and implementation phases for a dedicated, audiophile-grade touch user interface for the Raspberry Pi Audio Server.

### Core Principles
1. **Lightweight & Direct Framebuffer Rendering:** Render directly to the Linux native framebuffer (`/dev/fb0`) using Python and `pygame-ce`. No X11, Wayland, or desktop server is required. Boots in < 2 seconds with minimal CPU consumption (< 3%).
2. **Peppy-Inspired Aesthetics:** Clean, high-contrast touch interface featuring high-visibility typography, large touch targets, smooth transitions, and iconic analog/digital **PeppyMeter dual VU meters**.
3. **Multi-Source Unified Architecture:** Built to seamlessly handle **Bluetooth A2DP (active now)**, an external **USB CD Player**, and future streaming protocols (**AirPlay via Shairport-Sync** and **UPnP/DLNA**).
4. **Intelligent Cover Art Engine:** Native **AVRCP 1.6 Basic Imaging Profile (BIP)** extraction with an automated online fallback (MusicBrainz / iTunes API) and a procedural retro vinyl disc generator.
5. **Deep Hardware Integration:** Direct physical interaction via the EC12 rotary volume knob, audio-reactive RGB LED status lighting, dynamic PWM cooling fan control, and ambient-light-based display dimming.
6. **Appliance Hardening & Fault Resilience:** Zero-flicker silent boot, automatic source arbitration (CD vs. BT), non-blocking reconnect state machines, 180° touch inversion, and real-time MA12070P I2C amp fault alerting with 1-tap recovery.

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

## 2.1. Comparative Analysis: moOde Audio, Volumio & Peppy vs. Our UI

To ensure our custom UI does not miss critical capabilities found in leading Raspberry Pi audiophile platforms (such as **moOde Audio**, **Volumio**, and **Peppy Player**), we conducted a feature comparison against their architectures:

| Feature / Capability | moOde Audio | Volumio | Peppy Player | Our Custom UI Strategy |
| :--- | :---: | :---: | :---: | :--- |
| **CamillaDSP Integration** | Native WebUI | Plugin | None (Alsa/Mpd) | **Direct WebSocket Integration:** Real-time peak/RMS levels, active crossover control, and telemetry without browser overhead. |
| **PeppyMeter Visualizers** | Plugin | Plugin | Native Pygame | **Native Dual Meters:** Sourced directly from CamillaDSP output (Channel 0 = Sub, Channel 1 = Coax) at 60 FPS. |
| **Audio Stream Inspector** | Yes (Audio Info) | Yes (Track Info) | Basic | **Adopted:** Live pill displaying input sample rate, bit depth, codec bitrate, and CamillaDSP drift ratio (`0.999974`). |
| **Absolute Volume Sync** | Yes (AVRCP) | Yes | Partial | **Adopted:** Bidirectional synchronization between phone volume, physical EC12 rotary knob, and on-screen slider. |
| **Soft Fade Mute/Unmute** | Yes (DSP fade) | Yes | No | **Adopted:** 300 ms linear gain ramp on mute/unmute to prevent speaker pops or acoustic shock. |
| **Night Mode / Blanking** | Yes (Screen off) | Yes | Screensaver | **Adopted:** 1-tap top bar icon to shut off DSI backlight (`brightness=0`) with tap-anywhere-to-wake. |
| **Sleep Timer** | Yes (Timer) | Yes | Yes | **Adopted:** 15m, 30m, 45m, 60m countdown timer with gradual volume fade-out into amp standby. |
| **Power Management** | Web Menu (Shutdown) | Web Menu (Shutdown) | Menu | **Optimized for Read-Only Filesystem (Appliance Mode):** With a read-only root FS, the unit can be switched off directly at the power switch without risk of SD card corruption. Kept: `[ 🔄 Reboot System ]` and `[ 🔁 Restart Audio Stack ]`. Removed: Complex safe-shutdown rituals. |
| **Web Remote Companion** | Full WebUI | Full WebUI | WebUI | **Adopted (Lightweight):** Micro HTTP/WebSocket companion (port 8080) mirroring transport & volume for couch control. |
| **Silent Boot & Takeover** | Splash plugin | Splash image | No (Console) | **Adopted:** Zero-console flicker (`console=tty3 quiet vt.global_cursor_default=0`) with direct framebuffer takeover in < 2s. |
| **Source Arbitration** | Manual Switch | Manual Switch | N/A | **Adopted:** Priority-driven arbitration; playing CD automatically pauses Bluetooth phone over AVRCP. |
| **180° Screen Inversion** | Manual X11 config | Plugin setting | Manual config | **Adopted:** 1-tap toggle in settings rotating both framebuffer and touch coordinate transform matrix. |
| **Fault Recovery Loops** | Restart service | Restart service | Script crash | **Adopted:** Non-blocking async exponential backoff for CamillaDSP, BlueZ, and 1-Wire sensor failover. |
| **On-Screen Keyboard** | OSK plugin | WebUI only | Partial | **Adopted:** Large-format touch QWERTY modal for Bluetooth broadcast renaming and Wi-Fi credentials. |
| **Amp Protection Alert** | None | None | None | **Adopted:** Real-time MA12070P I2C fault latch detection with instant 1-tap hardware reset banner. |
| *Heavy Web/Kiosk Stack* | Chromium Kiosk | Chromium Kiosk | Pygame/X11 | **Deliberately Avoided:** moOde and Volumio use 250–400 MB of RAM and ~15% CPU running Chromium. Our native Pygame framebuffer engine consumes < 40 MB RAM and < 3% CPU. |
| *Local MPD Library Scan* | SQL/Tag indexing| SQL/Tag indexing| SQLite/Files | **Deliberately Avoided:** You stream wirelessly or play CDs; scanning local storage burns CPU cycles and causes SD card wear. |

---

## 3. UI Shell & Global Layout (800x480 Screen)

The display is divided into three functional zones:

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ [Top Status Bar (40px)]: Source Pill | Device/Title | Codec | Temp | Clock   │
├──────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│                                                                              │
│                         MAIN SCREEN VIEWPORT (380px)                         │
│                                                                              │
│                                                                              │
├──────────────────────────────────────────────────────────────────────────────┤
│ [Bottom Navigation Dock (60px)]: 🎵 Now Playing | 🎚️ VU | 📶 BT | 💿 CD | ⚙️│
└──────────────────────────────────────────────────────────────────────────────┘
```

1. **Top Status Bar (Height: 40px):**
   - **Active Source Pill:** Tap to quickly switch between `📶 Bluetooth`, `💿 CD Player`, `📻 AirPlay`, and `🌐 UPnP`.
   - **Connected Client / Quick Info:** Shows connected device name (e.g., `Pixel 10 Pro XL`) or current track.
   - **Codec Badge:** Real-time badge (`aptX-HD`, `AAC`, `SBC-XQ`, `CD-DA 44.1k`).
   - **Enclosure Temperature:** Real-time reading from DS18B20 (e.g., `26.4°C`).
   - **System Clock:** HH:MM display.

2. **Main Viewport (Height: 380px):**
   - Displays the currently active screen. Supports smooth horizontal swipe gestures and transition animations.

3. **Bottom Navigation Dock (Height: 60px):**
   - Touch-friendly icon bar:
     - `🎵 Player` (Now Playing & Transport)
     - `🎚️ VU Meter` (PeppyMeter Dual Visualizer)
     - `📶 Bluetooth` (Device Manager & Pairing)
     - `💿 CD Player` (Optical Disc Playback & Eject)
     - `🎛️ DSP & Tone` (CamillaDSP Levels & Presets)
     - `⚙️ Settings` (Preferences, Hardware Health & Thermals)

---

### 3.1. Unified Global Theme System (Coordinated Across All Screens)

To deliver an integrated, high-end audiophile appliance feel, **theming is applied globally across all 6 screens and shells** rather than being isolated to just the VU visualizer. Switching the active theme instantly repaints the entire Pygame interface:

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ [TOP BAR]       Coordinated Status Glow, Codec Pill, & Accent Colors         │
├──────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  ALL SCREENS (Now Playing, PeppyMeter, Bluetooth, CD, DSP, Settings)         │
│  dynamically consume the active Theme Palette:                               │
│  • Primary / Background surfaces & card containers                          │
│  • Glowing sliders, active volume arc, progress bars                         │
│  • Transport buttons, touch hit-states & highlight outlines                 │
│  • PeppyMeter faceplate graphics & needle colors                             │
│                                                                              │
├──────────────────────────────────────────────────────────────────────────────┤
│ [BOTTOM DOCK]   Matching active tab pill, icon glow, & border highlights     │
└──────────────────────────────────────────────────────────────────────────────┘
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

* **Zero-Latency Hot-Swapping:** Themes are driven by a centralized `ThemeManager` in Python. When cycled, all surfaces and caching layers re-render on the next frame (< 16 ms) without interrupting audio playback or CamillaDSP stream processing.
* **Persistent Preference:** The active theme selection is persisted directly in `/data/config/ui_settings.json` so the exact aesthetic is restored on every cold boot.

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
1. **Universal Screen Interception:**
   - Appears over **any active screen** (Now Playing, full-screen PeppyMeter, Bluetooth Device Manager, CD Player, DSP Tone, or Settings) without losing the user's navigational state.
2. **High-Visibility Typography & Themed Accents:**
   - Huge, high-contrast digital percentage (`78%` in 64pt bold) and calibrated decibel readout (`-11.0 dBFS`).
   - Massive glowing horizontal bar track styled in the active Theme accent color (e.g., Electric Cyan for McIntosh, Warm Amber for Marantz, Magenta for Cyberpunk).
   - If the system is soft-muted, the value flashes `🔇 MUTED` with a vivid red indicator.
3. **Auto-Dismiss Timeout (2.5 Seconds):**
   - A timer resets on every rotary detent tick.
   - After **2.5 seconds** of inactivity, the overlay executes a 150 ms smooth alpha fade-out and reveals the underlying screen.
4. **Touchscreen Interactivity:**
   - While the overlay is visible, the user can also slide their finger along the horizontal track to adjust volume via touch, tap `[ 🔇 MUTE ]` for soft-fade muting, or tap anywhere outside the bar to dismiss immediately.
5. **Non-Blocking Architecture:**
   - Driven by `VolumeOverlayManager` as a top-level compositing layer in Pygame. It uses dirty-rect updating so only the volume bar and text bounding boxes are re-rendered during rapid knob rotations.

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
  - Directly subscribes to CamillaDSP WebSocket (`127.0.0.1:1234`).
  - Reads `levels.playback_peak()` and `levels.playback_rms()` at 30–60 FPS.
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

---

### Screen 3: Bluetooth Device Manager
* **Header Controls:**
  - **"Make Discoverable (180s)" Toggle:** Turns on BlueZ pairing mode. Displays a live countdown ring and pulses the EC12 RGB LED in Blue.
  - **"Scan Nearby" Button:** Performs an active inquiry scan and displays discoverable devices nearby.
* **Paired Devices List:**
  - Each item shows: Device Name, Bluetooth Address, Connection Badge (`🟢 Active` or `⚪ Disconnected`).
  - **Action 1: Connect / Disconnect:** 1-tap connection toggle.
  - **Action 2: Forget / Remove:** Trashcan icon with confirmation to unpair from BlueZ.
* **Auto-Trust Status Indicator:** Confirms that `bt-agent.service` is actively running to authorize pairing without PIN prompts.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ BLUETOOTH DEVICE MANAGER               [📡 Make Discoverable (180s)] [🔍 Scan]│
├──────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  📱 Pixel 10 Pro XL          [30:E0:44:67:38:A2]                             │
│     Status: 🟢 Connected (Active Stream)           [Disconnect]  [🗑️ Forget] │
│  ──────────────────────────────────────────────────────────────────────────  │
│  💻 ThinkPad X1              [E8:48:B8:C2:19:04]                             │
│     Status: ⚪ Paired (Idle)                       [ Connect  ]  [🗑️ Forget] │
│  ──────────────────────────────────────────────────────────────────────────  │
│  🎧 iPad Air                 [AC:BC:32:89:41:2F]                             │
│     Status: ⚪ Paired (Idle)                       [ Connect  ]  [🗑️ Forget] │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
```

---

### Screen 4: CD Player (Optical Disc Control)
* **Integration Architecture:** Detects disc insertion via `udisks2` / Linux udev rules. Plays digital audio using `mpv --no-video --audio-device=alsa/default cdda://`.
* **Visual Components:**
  - **Left Section:** Animated spinning optical disc graphic with album art on disc label.
  - **Center Section:** 
    - Track Number display (`Track 04 of 12`).
    - Track Name & Artist (retrieved via CD-Text or online MusicBrainz DiscID lookup).
    - Track Elapsed / Total Duration progress bar.
    - Transport buttons: `⏮️ Prev Track`, `⏯️ Play/Pause`, `⏭️ Next Track`, `⏹️ Stop`.
    - **Eject Button (`⏏️ Eject`):** Software-unmounts and triggers physical drive tray ejection.
  - **Right Section (Tracklist Drawer):** Scrollable list of all CD tracks; tap any track to seek directly.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ [💿 CD Player ▼]  Pink Floyd - The Dark Side of the Moon        [CD-DA 44.1k]│
├─────────────────────────┬──────────────────────────────────┬─────────────────┤
│                         │  Track 04: Time                  │  1. Speak to Me │
│    ┌───────────────┐    │  06:53                           │  2. Breathe     │
│   /   ╭─────────╮   \   │                                  │  3. On the Run  │
│  │   │  DISC 1  │    │  │  [02:14] ━━━━━━●───────── [06:53]│ >4. Time        │
│   \   ╰─────────╯   /   │                                  │  5. Great Gig.. │
│    └───────────────┘    │  [⏮️]   [ ⏯️ ]   [⏹️]   [⏭️]  [⏏️]│  6. Money       │
└─────────────────────────┴──────────────────────────────────┴─────────────────┘
```

---

### Screen 5: DSP & Tone Control (CamillaDSP Tuning)
* **Controls & Adjustments:**
  - **Master Volume Dial:** 0 to 100% with exact dBFS readout.
  - **Subwoofer Trim Slider:** -12 dB to +6 dB (adjusts Channel 0 gain in real-time).
  - **Coaxial Treble Shelf Slider:** -6 dB to +3 dB at 3000 Hz (adjusts Channel 1 high-shelf).
* **Equalizer & Filter Presets:**
  - `[ Flat / Studio ]`: Reference crossover (45Hz HPF, 180Hz LPF/HPF, -2dB treble shelf).
  - `[ Bass Punch ]`: +3.5 dB subwoofer shelf for low-volume fullness.
  - `[ Vocal / Podcast ]`: Subwoofer attenuated -3 dB, midrange boosted +2 dB.
  - `[ Night / Protect ]`: Subwoofer high-pass shifted up to 55 Hz with strict -4 dB limiting.
* **Live DSP Telemetry:** Processing load indicator (`1.27%`), buffer fill bar (`104%`), and clipped samples counter (`0`).

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ DSP & TONE CONTROL (CamillaDSP v4.1.3)          [CPU: 1.3%] [Clip Count: 0]  │
├──────────────────────────────────────────────────────────────────────────────┤
│  MASTER GAIN:   [━━━━━━━●────────────]  -12.0 dB                             │
│  SUBWOOFER:     [━━━━━━━━━━━━●───────]  +1.5 dB (Trim)                       │
│  COAX TREBLE:   [━━━━━━━━━●──────────]  -2.0 dB (3kHz Shelf)                 │
│                                                                              │
│  PRESETS:  [  Flat / Studio  ]  [★ Bass Punch ]  [ Vocal ]  [ Night Protect ]│
└──────────────────────────────────────────────────────────────────────────────┘
```

---

### Screen 6: System Health, Thermals & Fan
* **Thermal Monitoring:**
  - Heatsink / Enclosure temperature from **DS18B20 1-Wire** (`/sys/bus/w1/devices/28-*/w1_slave`).
  - Raspberry Pi SoC temperature (`/sys/class/thermal/thermal_zone0/temp`).
* **Cooling Fan Management:**
  - **Modes:** `[ Auto Curve ]`, `[ Silent (30%) ]`, `[ Med (60%) ]`, `[ Max (100%) ]`.
  - **Live Tachometer Feedback:** Displays measured fan RPM from GPIO 6 interrupt counter.
* **Ambient Light & Display Backlight:**
  - Live illuminance readout from **BH1750 I2C sensor** (e.g. `145.2 lx`).
  - **Auto-Dimming Toggle:** Automatically dims DSI screen in dark rooms and brightens in daylight via `/sys/class/backlight/rpi_backlight`.
  - Manual slider override (10 to 255).
* **MA12070P Amplifier Register Status:**
  - Live power profile (`PMP1`), charge pumps status, and error latch report.
  - **"Clear Fault Latches" Button:** Runs `ma12070p_status.py --clear`.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ SYSTEM HEALTH & THERMAL CONTROL                                              │
├──────────────────────────────┬───────────────────────────────┬───────────────┤
│ THERMAL SENSORS              │ NOCTUA COOLING FAN            │ DSI BACKLIGHT │
│ • Enclosure: 26.4°C / 79.6°F │ • Speed: 1,420 RPM            │ • Lux: 145 lx │
│ • RPi SoC:   42.1°C / 107.8°F│ • Mode: [ Auto ] [30] [60][100]│ • Bright: 255 │
├──────────────────────────────┴───────────────────────────────┴───────────────┤
│ AMPLIFIER DIAGNOSTICS (Infineon MA12070P @ 0x20)                             │
│ • State: [OK] Normal Operation   • Mode: PMP1 Dynamic   [ Clear Faults ]     │
└──────────────────────────────────────────────────────────────────────────────┘
---

### Screen 7: System Preferences & Behavior Settings
A dedicated touch configuration screen that allows the user to customize hardware responses, Bluetooth policies, screensaver timeouts, and audio startup behaviors. All settings are persisted to a JSON configuration file (`/root/.config/audio-ui/preferences.json`).

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ SYSTEM PREFERENCES & HARDWARE BEHAVIOR                        [ Save & Apply ]│
├──────────────────────────────┬───────────────────────────────┬───────────────┤
│ HARDWARE BUTTON & ENCODER    │ BLUETOOTH POLICIES            │ SCREENSAVER   │
│ • Short Press:  [Play/Pause▼]│ • Auto-Reconnect: [ Enabled ▼]│ • Timeout:    │
│ • Long Press:   [  Mute   ▼] │ • Pairing Mode:   [ 180 sec ▼]│   [ 30 sec  ▼]│
│ • Double Click: [Next Trk ▼] │ • Auto-Play on BT: [Enabled ▼]│ • Default Skin│
│ • Knob Step:    [ 2% Step ▼] │ • Device Name: [DietPi Speaker│   [McIntosh ▼]│
│ • RGB LED Mode: [Audio-Rea▼] │ • Auto-Trust:     [Headless ▼]│ • Blank: [No▼]│
├──────────────────────────────┴───────────────────────────────┴───────────────┤
│ AUDIO & STARTUP SAFETY                                                       │
│ • Max Volume Limiter:  [ 90% (Safety Cap) ▼]   • Boot Source: [Last Used   ▼]│
│ • Startup Volume:      [ 40% (Gentle)     ▼]   • Fan Target:  [ 38°C Auto  ▼]│
└──────────────────────────────────────────────────────────────────────────────┘
```

#### Detailed Configuration Options:

1. **Hardware Button & Rotary Knob Preferences:**
   - **Short Press Action:** `Play / Pause` (default), `Mute / Unmute`, `Next Track`, `Toggle PeppyMeter`, `Cycle Screen`.
   - **Long Press (>1.0s) Action:** `Mute / Unmute` (default), `Display Sleep / Standby`, `Make Discoverable`, `Toggle PeppyMeter`.
   - **Double Click Action:** `Next Track` (default), `Cycle Input Source`, `Previous Track`, `None`.
   - **Rotary Knob Sensitivity:** `1% per step` (Precision), `2% per step` (Standard / default), `5% per step` (Fast).
   - **EC12 RGB LED Behavior:**
     - `Audio & State Reactive` (default: Pulsing blue on pairing, solid cyan on playback, amber on mute, red on fault).
     - `Volume-Mapped Gradient` (Green at low volume -> Yellow at 50% -> Red at high volume).
     - `Fixed Solid Accent` (Cyan, Cobalt Blue, Warm Amber, Emerald Green, White).
     - `Stealth / Disabled` (LED turned off).

2. **Bluetooth Connection Policies:**
   - **Auto-Reconnect Known Devices:** `Enabled` (automatically resumes link with last paired device) vs. `Manual`.
   - **Discoverable Window Duration:** `60s`, `120s`, `180s` (default), `300s`, or `Always Discoverable`.
   - **AVRCP Auto-Play on Connect:** `Enabled` (automatically sends play command when phone connects) vs. `Disabled`.
   - **Bluetooth Broadcast Name:** Editable string (default: `DietPi Speaker`).
   - **Pairing Agent Policy:** `Auto-Accept All (Headless)` vs. `On-Screen Confirmation Prompt`.

3. **Display & Theme Preferences:**
   - **PeppyMeter Screensaver Timeout:** `Disabled`, `15 seconds`, `30 seconds` (default), `1 minute`, `2 minutes`.
   - **Active Global Theme & Skin:** `Vintage McIntosh Blue` (default), `Marantz Amber`, `Cyberpunk Neon`, `Studio Minimalist` (updates all 6 screens + meter faceplates).
   - **Auto-Backlight Dimming:** `Enabled` (automatically scales DSI brightness with BH1750 lux curve) vs. `Manual Fixed`.
   - **Display Inactivity Sleep:** `Never (Always On)` (default), `5 minutes`, `15 minutes`.

4. **Audio Safety & Startup Defaults:**
   - **Max Master Volume Limiter:** Safety cap at `80%`, `90%`, or `100%` (prevents accidental speaker blowout from phones sending 100% volume bursts).
   - **Soft Fade Mute/Unmute:** `Enabled` (300 ms linear gain fade to eliminate transient pops) vs. `Instant Cut`.
   - **Default Startup Volume:** `Last Used` vs. `Fixed Preset (e.g. 40%)`.
   - **Default Startup Audio Source:** `Last Used` (default), `Bluetooth`, `CD Player`.

5. **Appliance-Mode Power Management & Read-Only Filesystem Architecture:**
   - **Why Traditional Safe Power Management is REMOVED:**
     - Traditional Linux audio systems (Moode, Volumio) run on read-write ext4 filesystems where sudden power cuts can corrupt the journal, leave dirty inodes, and brick the SD card. Consequently, they install complex safe-shutdown daemons, software shutdown menus with confirmation prompts, and paranoid warnings.
     - **In our Appliance Mode, the entire root filesystem is mounted strictly READ-ONLY (`ro` mode / OverlayFS).**
     - Because all runtime writes (`/tmp`, `/var/run`, `/var/log`) reside in `tmpfs` (RAM), **sudden power disconnection at any time carries ZERO risk of filesystem corruption or SD card degradation**.
     - **Removed Power Management Features:**
       - ❌ *Software "Shut Down" Button & Modal:* No `poweroff` button or "Are you sure you want to shut down?" dialog. A software shutdown on a headless Raspberry Pi does not cut AC/DC wall power anyway (the SoC halts but draws standby current).
       - ❌ *Graceful Shutdown Daemons & Safe Power Scripts:* No complex UPS/battery monitoring or delayed shutdown sequencing.
       - ❌ *Power-Off Hold Sequences:* Long-pressing the rotary knob or power button will never trigger an OS halt.
     - **Kept & Streamlined Power Actions:**
       - ✅ *Soft Sleep / Display Standby (`🌙 Standby`):*
         - Dim or shut off the Freenove DSI backlight completely (`brightness=0`, `bl_power=1`).
         - Send the Merus MA12070P into ultra-low quiescent standby mode via I2C (`0x20`).
         - Pause CamillaDSP audio processing to drop CPU utilization to <0.5%.
         - Wake up instantly (<50 ms) upon any touch on the screen or rotation/click of the physical encoder.
       - ✅ *Sleep Timer (Countdown):* `Off`, `15 min`, `30 min`, `45 min`, `60 min`, `90 min`. When triggered, volume gently fades to zero over 60 seconds, audio stops, display blanks, and amp enters low-power standby.
       - ✅ *Clean System Reboot (`[ 🔄 Reboot System ]`):* Kept in Settings -> Diagnostics for applying kernel/device-tree changes or network reconfigurations.
       - ✅ *Audio Stack Restart (`[ 🔁 Restart Audio ]`):* Kept for 1-tap recovery of `camilladsp`, `bluealsa`, and `bluealsa-aplay` if a phone sends malformed A2DP packets.
   - **Read-Only Persistence Strategy (Settings & Bluetooth Keys):**
     - To ensure user preferences (volume, PeppyMeter skin, EQ preset) and Bluetooth paired keys survive power cycles without keeping the whole OS partition writable:
       - **Method: Atomic Remount Wrapper (`ro_persist.py`):**
         - When a setting changes or a new device pairs, write state to `/tmp/config.json.tmp`.
         - Execute atomic sync: `mount -o remount,rw / && cp /tmp/config.json.tmp /etc/app/config.json && sync && mount -o remount,ro /`.
         - Flash writes occur strictly on demand (<50 ms window), keeping the disk 99.999% immutable.
       - **BlueZ Storage:** Symlink `/var/lib/bluetooth` to a designated persistent path or trigger the atomic remount commit upon BlueZ `Device1.Paired == True`.

6. **Web Remote Companion (Optional):**
   - Toggle to enable a lightweight micro-server on port `8080` (`http://192.168.2.110:8080`) providing couch remote control (Now Playing metadata, Play/Pause/Skip, Volume slider) from any phone or laptop browser on the local network.

---

### Screen 8: Ethernet Network Setup (DHCP & Static Configuration)
A dedicated network configuration screen for managing the physical Ethernet (`eth0`) interface. It supports both automatic DHCP addressing and manual Static IP assignment with an integrated touch numeric keypad and a 30-second failsafe reversion timer to prevent lockouts.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ ETHERNET NETWORK CONFIGURATION (eth0)                        [ Apply Changes ]│
├──────────────────────────────┬───────────────────────────────────────────────┤
│ INTERFACE STATUS             │ IP ADDRESSING MODE                            │
│ • Link: 🟢 UP (100 Mbps Full)│  (●) DHCP (Automatic Router IP)               │
│ • MAC:  b8:27:eb:4c:60:d1    │  (○) Static IP (Manual Configuration)         │
│ • State: 🟢 Online (12ms)    │                                               │
├──────────────────────────────┴───────────────────────────────────────────────┤
│ NETWORK PARAMETERS                                   TOUCH NUMERIC KEYPAD    │
│ • IP Address:   [ 192.168.002.110 ]                  ┌───┬───┬───┐           │
│ • Subnet Mask:  [ 255.255.255.000 ]                  │ 1 │ 2 │ 3 │           │
│ • Gateway:      [ 192.168.002.001 ]                  ├───┼───┼───┤           │
│ • Primary DNS:  [ 001.001.001.001 ]                  │ 4 │ 5 │ 6 │           │
│ • Secondary DNS:[ 008.008.008.008 ]                  ├───┼───┼───┤           │
│                                                      │ 7 │ 8 │ 9 │           │
│ ACTIONS:                                             ├───┼───┼───┤           │
│ [ 🔄 Renew DHCP Lease ]  [ 🌐 Test Ping (Gateway) ]  │ . │ 0 │ ⌫ │           │
│                                                      └───┴───┴───┘           │
└──────────────────────────────────────────────────────────────────────────────┘
```

#### Detailed Network Features:
1. **Live Interface Monitoring:**
   - Detects cable connection state via sysfs (`/sys/class/net/eth0/carrier`) and speed (`/sys/class/net/eth0/speed`).
   - Real-time Internet connectivity indicator (pings gateway and public DNS `1.1.1.1` to confirm active routing).
2. **Mode 1: DHCP (Default):**
   - Automatically leases IP, gateway, and nameservers from the local router.
   - Includes a **"Renew DHCP Lease"** button (triggers background `dhclient -r eth0 && dhclient eth0`).
3. **Mode 2: Static IP Assignment:**
   - Tap any parameter field (IP, Netmask, Gateway, DNS) to focus and type octets using the on-screen numeric keypad.
   - Enforces valid IPv4 ranges (0–255 per octet) with auto-advancing dots.
4. **Failsafe Reversion Timer (Lockout Protection):**
   - When applying a new Static IP configuration, the system writes to `/etc/network/interfaces` and reloads networking.
   - An on-screen confirmation modal appears:
     > *"New IP applied: 192.168.2.110. Testing gateway response... Keep this configuration? Reverting in 30 seconds..."*
   - If the user does not tap "Keep Configuration" within 30 seconds (or if the gateway is unreachable), the system automatically restores the previous working `/etc/network/interfaces` file to guarantee SSH access is never lost.
5. **Backend Management:**
   - Interacts with `/etc/network/interfaces` and `/etc/network/interfaces.d/eth0.conf` cleanly without corrupting other interfaces.

---

### Screen 9: Clean Startup Splash Screen (Shown on Boot)
When the system powers on or the UI service restarts, a sleek, minimalist splash screen appears on the 800x480 DSI display. It is designed to look polished and premium rather than exposing raw console logs or diagnostic walls:

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│                                                                              │
│                                                                              │
│                           ◆  DIETPI AUDIO ENGINE  ◆                          │
│                                                                              │
│                    Active DSP Crossover • Bluetooth Audio                    │
│                                                                              │
│                                                                              │
│                         [━━━━━━━●────────────]                               │
│                                Loading...                                    │
│                                                                              │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
```

* **Behavior:**
  - Displays for **1.5 to 2.0 seconds** while background services (CamillaDSP, BlueALSA, D-Bus) establish communication.
  - Silently verifies that the sound cards and sensors are detected.
  - Automatically fades out with a smooth cross-dissolve directly into **Screen 1 (Now Playing)**.
  - Only if a critical hardware failure occurs (e.g., sound card missing or CamillaDSP crashed) does a subtle warning icon appear with a tap option to view system health.

---

### Screen 10: Semi-Hidden Hardware Diagnostics & Bench-Test Page
The Hardware Diagnostics page is **semi-hidden** to keep the daily music listening interface clean and clutter-free. It does not appear on the main navigation dock.

* **How to Access (Engineer Menu Pattern):**
  - Navigate to **Screen 6 (System Health)**.
  - **Triple-tap** the system version banner / logo (or long-press for 3 seconds).
  - The hidden Diagnostics Console slides in with a dedicated `[ Close & Exit ]` button in the top corner.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ 🛠️ HARDWARE DIAGNOSTICS (ENGINEERING CONSOLE)                [ Close & Exit ✖ ]│
├──────────────────────────┬──────────────────────────┬────────────────────────┤
│ AUDIO SPEAKER TEST       │ ROTARY ENCODER TEST      │ NOCTUA FAN RAMP TEST   │
│ • [🔊 50Hz Sub Tone]     │ • Dial: [  ( ↻ )  ]      │ • Tach: 1,480 RPM      │
│ • [🔊 1kHz Coax Tone]    │ • Steps: CW: +12 CCW: -4 │ • Speed:               │
│ • [🔊 Pink Noise Sweep]  │ • Push Switch: [RELEASED]│   [30%] [50%] [70%][100%]│
│ • Volume: [-12 dBFS]     │ • RGB: [R] [G] [B] [W]   │ • Status: [PWM OK]     │
├──────────────────────────┼──────────────────────────┼────────────────────────┤
│ SENSOR TELEMETRY         │ MA12070P REGISTERS (0x20)│ TOUCH ACCURACY TEST    │
│ • DS18B20: 26.4°C (Valid)│ • Power: PMP1 (Dynamic)  │ • Target: [   🎯   ]   │
│ • RPi SoC: 42.0°C (OK)   │ • Charge Pump: Normal    │ • Touch X: 412 Y: 238  │
│ • Light:   145.2 lx      │ • Faults: None (Clean)   │ • Event: TouchDown     │
│ • Throttled: 0x0 (None)  │ • [ Clear Latched Faults]│ • Points Detected: 1   │
└──────────────────────────┴──────────────────────────┴────────────────────────┘
```

#### Diagnostic Subsystems Available in the Hidden Page:
1. **Audio Crossover & Speaker Test:**
   - **50 Hz Sine Wave:** Generates a low-frequency tone directly into ALSA `default`. Verifies only the subwoofer responds through the 45–180 Hz CamillaDSP filter.
   - **1 kHz Sine Wave:** Generates a midrange tone. Verifies only the coaxial satellite responds through the 180 Hz+ highpass filter.
   - **Pink Noise Sweep:** Full acoustic verification across both drivers.
2. **EC12 Rotary Encoder & Switch Test:**
   - On-screen virtual dial rotates in real time as the physical knob is turned CW (+) or CCW (-).
   - Push switch indicator changes from gray `[RELEASED]` to bright green `[PRESSED]`.
   - RGB LED test buttons to trigger individual Red, Green, Blue, and White channels manually.
3. **Noctua Fan Speed Ramp Test:**
   - Step through 30%, 50%, 70%, 100% duty cycles with real-time tachometer RPM feedback.
4. **Sensor Integrity:**
   - Live display of 1-Wire CRC checksum status and illuminance reading from the BH1750.
   - Reads `vcgencmd get_throttled` to detect any undervoltage or CPU thermal throttling.
5. **MA12070P I2C Register Audit:**
   - Real-time hardware health check (PVDD supply voltage, flying capacitor balancing, PLL lock).
   - "Clear Latched Faults" button invokes `ma12070p_status.py --clear`.
6. **Touchscreen Accuracy Test:**
   - Interactive crosshair calibration targets to verify touch coordinates and multi-touch tracking across the 800x480 panel.

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

## 5. Architecture for Testability, Optimal Python Frameworks & Industry Best Practices

### 5.1. How Industry Leaders Test Embedded Linux Audio & IoT
Before writing code, we analyzed how tier-1 projects (**Home Assistant**, **systemd**, **BlueZ upstream**, **Moode**, **Volumio**, and **PeppyPlayer**) test audio, Bluetooth, and embedded interfaces:

| Project / Ecosystem | Component | How They Test | Common Amateur Pitfall Avoided |
| :--- | :--- | :--- | :--- |
| **systemd / BlueZ / GNOME** | Bluetooth & D-Bus | **`python-dbusmock`**: Boots an in-memory virtual D-Bus daemon with pre-built `bluez5` templates. Injects virtual adapters, devices, and pairing agents. | ❌ *Never* run `bluetoothctl` subprocesses or require physical phones nearby. |
| **Home Assistant Core** | Bluetooth / IoT Sensors | **Hexagonal Architecture (`typing.Protocol`)** with **`dbus-fast`**: Pure-Python asynchronous D-Bus library. 100% of business logic tested with in-memory mock adapters. | ❌ *Never* lock tests to slow, synchronous `dbus-python` or physical I2C buses. |
| **PeppyPlayer / Pygame-ce** | Framebuffer UI & Touch | **Headless SDL (`SDL_VIDEODRIVER=dummy`)**: Boots full UI engine in memory. Simulates touch events with `pygame.event.post()`. Compares rendered surface pixel buffers against golden reference images. | ❌ *Never* require a physical display monitor or human touch to verify UI layouts. |
| **CamillaDSP / CamillaGUI** | DSP & Peak Metering | **Mock WebSocket Server (`aiohttp.test_utils` / `websockets.serve`)**: Emits synthetic JSON-RPC peak/RMS frames. Asserts logarithmic VU ballistics math and volume sync. | ❌ *Never* require live audio playing to verify visualizer needle physics and clipping flags. |
| **BalenaOS / Yocto Embedded** | Sysfs & Hardware Peripherals | **Virtual Sysfs Trees (`pytest tmp_path`)**: Points 1-Wire and Backlight drivers to temporary directories containing mock files (`w1_slave`, `brightness`). Tests CRC errors and missing sensors. | ❌ *Never* run tests that fail if a DS18B20 probe is unplugged. |

---

### 5.2. Optimal Python Stack & Tooling

To ensure maximum runtime efficiency, true multi-core parallel execution, and sub-millisecond test execution, we standardize on **Python 3.14 Free-Threaded (`--disable-gil` / `python3.14t`)** from day 1:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                            OPTIMAL PYTHON STACK                             │
├──────────────────────┬──────────────────────────────────────────────────────┤
│ Runtime Standard     │ Python 3.14 Free-Threaded (--disable-gil / python3.14t)│
│ Threading Model      │ True Multi-Core Parallelism (No Global Interpreter Lock)│
│ Dev / Test Env       │ python3.14t with --disable-gil from day 1            │
│ Target Pi Deployment │ Python 3.14t standalone environment (/opt/python3.14t)│
│ Type Safety          │ PEP 649 deferred annotations, dataclasses(slots=True)│
│ Async Framework      │ asyncio (native event loop in worker thread)         │
│ D-Bus Library        │ dbus-fast (async, high-throughput, fully typed)      │
│ Graphics / UI        │ pygame-ce 2.5+ (Pygame Community Edition)            │
│ DSP Protocol         │ websockets (asyncio WebSocket client for CamillaDSP) │
│ Test Framework       │ pytest (executed under python3.14t --disable-gil)    │
│ D-Bus Mocking        │ python-dbusmock (Martin Pitt / systemd testing standard)│
└──────────────────────┴──────────────────────────────────────────────────────┘
```

#### Why Python 3.14 Free-Threaded (`--disable-gil`) from the Beginning:
1. **Test Under True Concurrency from Day 1:** Testing with the GIL disabled from the very start ensures that any race conditions, subtle thread-safety bugs, or synchronization flaws are exposed and fixed immediately during development—rather than surfacing as elusive intermittent crashes in production.
2. **True 4-Core Parallelism on Raspberry Pi 3B:** The Pi 3B features a quad-core ARM Cortex-A53 CPU. Under standard Python, the GIL serializes execution so that only one thread executes Python bytecode at a time. With `--disable-gil`, all 4 worker threads run in parallel on separate physical cores:
   - **Core 0:** Main UI Thread (60 FPS Pygame-ce rendering to `/dev/fb0`).
   - **Core 1:** Fast Hardware Worker (EC12 rotary quadrature interrupts and push button debouncing).
   - **Core 2:** AsyncIO Worker (BlueZ D-Bus and CamillaDSP WebSocket stream).
   - **Core 3:** Slow Hardware Poller (1-Wire 750 ms ADC read and I2C light sensor).
3. **Zero Lock Contention & No Latency Spikes:** Free-threaded execution combined with our lock-free queue architecture means the 60 FPS UI rendering thread is completely immune to CPU spikes in the networking or hardware polling threads.
4. **PEP 649 (Deferred Evaluation of Annotations):** Write modern, clean Python 3.14 typing without `from __future__ import annotations` workarounds or eager evaluation overhead at import time.
5. **No Legacy Shims:** Zero code branches or backward compatibility compromises for Python 3.13.

#### Why `dbus-fast` over Legacy `dbus-python`:
1. **Asynchronous & Non-Blocking:** Legacy `dbus-python` uses synchronous C-bindings that block the Pygame 60 FPS rendering loop when querying device properties. `dbus-fast` is fully `asyncio`-native.
2. **Type-Annotated & Introspectable:** Generates strongly typed interfaces directly from D-Bus XML introspection.
3. **Mock-Friendly:** Allows injecting virtual message buses directly in unit tests without starting a system daemon.

#### Why `pygame-ce` over Standard `pygame` or Chromium Kiosk:
1. **Lightweight & Fast:** Starts in <300 ms, consumes <18 MB RAM, and uses <3% CPU on a Raspberry Pi 3B (vs. Chromium kiosk at 350 MB RAM and 40% CPU).
2. **Direct Framebuffer & DRM/KMS:** Renders natively to `/dev/fb0` with zero X11/Wayland overhead.
3. **Headless Test Driver:** Native `SDL_VIDEODRIVER=dummy` allows running 100% of the UI test suite in headless CI in seconds.

---

### 5.3. Hexagonal Architecture (Ports and Adapters)

To guarantee that every feature can be tested either **in simulation (desktop/CI)** or **on physical hardware (Raspberry Pi)**, the codebase strictly decouples business logic from OS drivers:

```text
               ┌─────────────────────────────────────────┐
               │         PURE CORE DOMAIN LOGIC          │
               │  - Audio Source State Machine           │
               │  - Volume Scaling & Mute Management     │
               │  - PeppyMeter Needle Ballistics Math    │
               │  - Settings & Failsafe Network Logic    │
               └────────────────────┬────────────────────┘
                                    │ (Implements Protocols)
                    ┌───────────────┴───────────────┐
                    ▼                               ▼
       ┌────────────────────────┐      ┌────────────────────────┐
       │   PRODUCTION ADAPTERS  │      │     TEST ADAPTERS      │
       ├────────────────────────┤      ├────────────────────────┤
       │ BlueZ D-Bus (dbus-fast)│      │ MockBlueZAdapter       │
       │ CamillaDSP WebSocket   │      │ MockCamillaWsServer    │
       │ Sysfs 1-Wire / Backlight│      │ FakeSysfsFixture       │
       │ Hardware EC12 Rotary   │      │ SimulatedEncoderQueue  │
       │ Pygame-ce /dev/fb0     │      │ HeadlessDummySurface   │
       │ Atomic Remount Script  │      │ MemoryConfigStorage    │
       └────────────────────────┘      └────────────────────────┘
```

---

### 5.4. Component-by-Component Testing Implementation

#### 1. Bluetooth Management (`tests/test_bluetooth.py`)
- **Unit & Mock Tests (`python-dbusmock`):**
  - Boots a mock BlueZ 5 adapter (`/org/bluez/hci0`).
  - Calls `manager.get_paired_devices()` -> asserts returns parsed device models.
  - Simulates device connection failure -> asserts UI transitions to `Error` state with user-friendly retry pill.
  - Calls `manager.set_discoverable(True, timeout=180)` -> verifies property update and automatic timer expiration.
- **Hardware-in-the-Loop (HIL) Tests (`@pytest.mark.hardware`):**
  - Runs only when executed directly on the Pi (`--run-hardware` flag).
  - Queries real TP-Link UB500 dongle (`20:E1:5D:B2:16:BF`).
  - Verifies `bluez-alsa` PCM device registration in ALSA.

#### 2. CamillaDSP WebSocket & VU Meter Physics (`tests/test_dsp_meter.py`)
- **Ballistics Math Verification:**
  - Tests peak-to-angle conversion across the full -60 dB to 0 dB range:
    $$\theta = \theta_{\text{min}} + \left(\frac{\text{dB} - \text{dB}_{\text{min}}}{\text{dB}_{\text{max}} - \text{dB}_{\text{min}}}\right) \times (\theta_{\text{max}} - \theta_{\text{min}})$$
  - Validates rise-time smoothing (30 ms rise, 300 ms linear decay) and peak-hold indicator behavior.
- **Mock WebSocket Client:**
  - Sends malformed JSON-RPC frames and verifies automatic reconnection logic without crashing the UI.
  - Verifies bidirectional volume sync: Changing volume from phone updates slider; changing slider sends `SetVolume` to CamillaDSP without infinite echo loops.

#### 3. Touch Screen & Pygame UI Engine (`tests/test_ui.py`)
- **Headless Event Simulation:**
  ```python
  def test_navigate_to_bluetooth_screen(app):
      # Simulate touch on bottom dock Bluetooth icon (x=330, y=450)
      app.inject_touch(330, 450)
      app.update(dt=0.016)
      assert app.current_screen == Screen.BLUETOOTH
  ```
- **Visual Snapshot Regression Testing:**
  - Renders reference screens (`Now Playing`, `PeppyMeter`, `Settings`).
  - Saves frame buffer to PNG and computes Structural Similarity Index (SSIM) or SHA256 checksum to ensure text never overflows 800x480 bounds and color palettes match design tokens.

#### 4. Hardware Sensors & Peripherals (`tests/test_hardware.py`)
- **Rotary Encoder State Machine:**
  - Evaluates the 2-bit Quadrature Gray code lookup table against simulated bounce sequences:
    $$\text{State Index} = (\text{Last A} \ll 3) \mid (\text{Last B} \ll 2) \mid (\text{Curr A} \ll 1) \mid \text{Curr B}$$
  - Asserts that erratic bounces (e.g., $00 \to 01 \to 00$) produce zero false steps.
- **DS18B20 1-Wire Parser:**
  - Injects valid temp strings (`t=26437` -> `26.4°C`), CRC error strings (`crc=00 NO` -> retains last valid temp), and disconnects (`FileNotFound` -> reports `Sensor Offline`).
- **Fan Thermal Hysteresis:**
  - Verifies that fan stays OFF at 48°C, ramps to 40% PWM at 55°C, ramps to 100% PWM at 70°C, and does not shut off until temperature drops below 45°C (preventing rapid cycling).

#### 5. Read-Only Filesystem & Network Rollback (`tests/test_system.py`)
- **Atomic Config Persistence:**
  - Tests `ro_persist.save_config()` under simulated read-only conditions:
    1. Writes to `/tmp/config.json.tmp`.
    2. Validates JSON schema before writing to root.
    3. Remounts RW, copies, syncs, remounts RO.
    4. Asserts filesystem is left in `ro` state after completion or upon exception.
- **Ethernet 30-Second Failsafe Rollback:**
  - Simulates applying a bad static IP configuration.
  - Verifies that if gateway ping fails within 30 seconds, configuration reverts automatically to DHCP and restores network connectivity.

---

### 5.5. Concurrency & Hardware Threading Architecture (Zero-Jank Design)

#### The Problem: Hardware Latency Heterogeneity
Embedded hardware interactions exhibit drastically mismatched latency profiles:
* **1-Wire DS18B20 Temperature:** Kernel read blocks for **750 ms** during ADC conversion.
* **I2C BH1750 Ambient Light:** Sensor measurement requires **120–180 ms**.
* **BlueZ D-Bus & CamillaDSP WebSocket:** Streams real-time packets asynchronously at 25–30 Hz.
* **EC12 Rotary Encoder:** Microsecond edge interrupts; missing an edge results in skipped steps or laggy volume feel.
* **Pygame Framebuffer UI:** Must render at a consistent **60 FPS (16.6 ms per frame budget)** with zero frame drops or visual stuttering.

If hardware I/O is performed synchronously on the main thread, the UI freezes for nearly a second on every temperature poll, and rotary knob turns are lost.

#### The Solution: 4-Thread Actor Model with Lock-Free Queues

To guarantee buttery-smooth 60 FPS graphics and instant hardware responsiveness, we decouple responsibilities into **4 isolated worker threads** communicating exclusively via non-blocking, thread-safe queues (`queue.Queue`):

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                          4-THREAD ACTOR ARCHITECTURE                        │
└─────────────────────────────────────────────────────────────────────────────┘

  [Thread 2: Fast Hardware Worker]          [Thread 3: AsyncIO Networking]
  - EC12 Rotary Quadrature (BCM 24, 25)      - dbus-fast (BlueZ A2DP/AVRCP)
  - Push Button Debounce (BCM 22)           - websockets (CamillaDSP Telemetry)
  - Latency: < 500 µs (Kernel IRQ)           - Latency: 25-30 Hz async stream
             │                                          │
             ▼                                          ▼
   (VolumeDelta / ButtonClick)              (MeterPeak / TrackMeta)
             │                                          │
             └──────────────────┬───────────────────────┘
                                │
                                ▼  put_nowait()
             ┌─────────────────────────────────────┐
             │   THREAD-SAFE EVENT QUEUE (C-level) │
             │    queue.Queue[AppEvent]            │
             └──────────────────┬──────────────────┘
                                │
                                ▼  get_nowait()
             ┌─────────────────────────────────────┐
             │ [Thread 1: UI Engine (Main Thread)] │
             │ - Pure Pygame-ce 60 FPS Loop        │
             │ - Consumes Events (< 0.5 ms)        │
             │ - Updates State Machine             │
             │ - Blits to /dev/fb0 (< 3 ms)        │
             │ - ZERO hardware I/O, ZERO locks     │
             └──────────────────▲──────────────────┘
                                │
                                │  put_nowait()
  [Thread 4: Slow Hardware Poller]
  - 1-Wire Temp DS18B20 (Blocks 750 ms)
  - I2C Light BH1750 (Blocks 150 ms)
  - Merus MA12070P I2C Health Poller
  - PWM Fan PID Controller
  - Interval: 1.0s to 2.0s tick
```

#### Why Free-Threaded (`--disable-gil`) Multi-Core Execution Guarantees Zero UI Jitter:
1. **True 4-Core Hardware Concurrency:** With the GIL disabled (`--disable-gil`), Python bytecode executes concurrently across all 4 ARM Cortex-A53 cores on the Pi 3B. There is no interpreter lock serializing thread execution.
2. **Immediate Detection of Race Conditions in Tests:** Testing with `--disable-gil` from Day 1 ensures any hidden concurrency bugs, race conditions, or unsafe mutable state access are identified and caught in automated unit/integration tests immediately.
3. **Lock-Free Queue Architecture (Zero Deadlocks):** Threads never share mutable objects or acquire complex locks. All inter-thread communication uses atomic, lock-free C-level FIFO queues (`queue.Queue`) passing immutable frozen dataclasses (`@dataclass(frozen=True, slots=True)`).
4. **Complete Isolation of 60 FPS UI Thread:** The main UI thread is completely insulated from the hardware worker thread's 750 ms blocking 1-Wire reads and network thread's high-frequency WebSocket frames via non-blocking queues, ensuring unwavering 60 FPS render timing without frame drops.

---

### 5.6. Production Dependency Specification & Exact Pinned Versions

To guarantee 100% reproducible deployments on the DietPi / Debian 13 (Trixie) appliance and eliminate dependency drift across builds, all Python modules are strictly pinned to their exact latest stable releases in `requirements.txt`:

| Module | Exact Pinned Version | Role / Justification |
| :--- | :--- | :--- |
| **`pygame-ce`** | `2.5.8` | Latest Community Edition release. Hardware-optimized SDL 2.30 framebuffer rendering, SIMD needle rotation math, and native multi-touch. |
| **`websockets`** | `17.3` | High-throughput, zero-copy WebSocket client for CamillaDSP real-time peak/RMS telemetry. |
| **`dbus-fast`** | `5.2.0` | Ultra-fast async D-Bus engine managing BlueZ Bluetooth devices, AVRCP playback events, and BlueALSA codecs. |
| **`Pillow`** | `13.0.0` | High-speed SIMD image scaling and resampling for album artwork thumbnails and faceplate alpha compositing. |
| **`aiohttp`** | `3.14.5` | Lightweight async HTTP & WebSocket server for the couch companion web remote (< 8 MB RAM overhead). |
| **`psutil`** | `7.2.2` | Accurate CPU load, RAM usage, and Linux system telemetry. |
| **`smbus2`** | `0.6.1` | Native I2C communication for BH1750 ambient lux sensor and Merus MA12070P hardware amplifier registers. |
| **`gpiozero`** | `2.0.1.post3` | High-level, thread-safe hardware abstraction for RotaryEncoder, Button debouncing, RGB LED control, and fan PWM. |
| **`rpi-lgpio`** | `0.6` | Modern Linux character-device GPIO backend (`/dev/gpiochipN`) powering `gpiozero` without requiring root privileges. |

* **System APT Dependencies (Debian Trixie):**
  - `libsdl2-2.0-0`, `libsdl2-image-2.0-0`, `libsdl2-ttf-2.0-0`: Core C libraries for display output and vector font rendering.
  - `i2c-tools`, `libgpiod-dev`: Hardware development and debugging tools.

---

### 5.7. Raspberry Pi 3 Hardware & CPU Optimization Strategies (Low-Latency, Low-RAM Engine)

To maximize performance on the Raspberry Pi 3 Model B (quad-core ARM Cortex-A53 @ 1.2 GHz with 1GB LPDDR2 RAM) without relying on heavy web frameworks, our custom Python/Pygame engine implements 5 targeted architectural optimizations:

#### 1. Dirty-Rect Rendering & Memory Bus Throttling (97% Reduction in Bus Traffic)
- **The Pi 3 Hardware Bottleneck:** The Pi 3 shares its LPDDR2 memory bus (~4.5 GB/s) between the CPU cores and VideoCore IV GPU. Flipping the entire 800×480 32-bit screen ($1.536\text{ MB}$) at 60 FPS transfers **$92.16\text{ MB/s}$** across the RAM bus, competing directly with CamillaDSP audio buffers.
- **The Optimization:** We use strict **Dirty-Rect updates (`pygame.display.update(dirty_rects)`)**:
  - The moving needle bounding box covers roughly $200 \times 60\text{ pixels}$ ($48\text{ KB}$).
  - Only the restored background patch and new needle position are repainted each frame.
  - Memory bus traffic drops from **$92.2\text{ MB/s} \to 2.8\text{ MB/s}$ (a 97% reduction)**, keeping L2 cache clean and preventing memory bus saturation.

#### 2. Pre-Baked Needle Angular Look-Up Table (Trig LUT)
- **The Problem:** Dynamic trigonometric matrix rotations (`math.sin`, `math.cos`) and polygon vertex transformations on every 16.6 ms frame consume floating-point CPU cycles and create temporary Python heap allocations.
- **The Optimization:**
  - Since needle deflection sweeps from $-42^\circ$ to $+42^\circ$ in $0.5^\circ$ increments, there are only **168 unique needle angles**.
  - At startup (< 50 ms), the engine pre-renders all 168 rotated needle orientations into an in-memory array of native surfaces matching the display pixel format.
  - In the 60 FPS visualizer loop: needle rendering is a direct index lookup (`needle_cache[angle_idx]`) followed by a fast C-level `memcpy` blit. **Zero trigonometry or allocation occurs in the real-time render loop.**

#### 3. Native Framebuffer Pixel Format Alignment (`.convert()` Zero-Copy Blits)
- **The Problem:** If source images (dial backgrounds, album art, icons) do not match the exact 32-bit pixel format of `/dev/fb0`, the SDL/Pygame blitter must perform pixel-by-pixel byte-swapping on every draw call.
- **The Optimization:** All loaded surfaces are immediately converted to the display format during initialization via `surface = pygame.image.load(...).convert()` (or `.convert_alpha()`). Every blit operation becomes a native memory block copy without format conversion.

#### 4. ARM NEON SIMD Vector Acceleration
- Leveraging `pygame-ce`'s compiled ARMv8 NEON SIMD assembly routines for alpha-blending, color-keying, and surface composition. NEON vectors process 4 pixels in parallel per instruction cycle on the Cortex-A53 cores.

#### 5. Adaptive Event-Driven Sleeping (`pygame.event.wait(timeout)`)
- Avoids the naive busy-waiting `while True: clock.tick(60)` loop.
- **When Idle / Paused / Settings:** Thread blocks inside the Linux kernel on `pygame.event.wait(1000)` (waking only 1×/sec for the clock or immediately upon touch interrupt). **Idle CPU: < 0.4%**.
- **When Playing (Now Playing):** Wakes at 30 Hz for mini-LED meters. **CPU: ~1.5%**.
- **When in PeppyMeter (Active VU Visualizer):** Runs at 60 FPS. **CPU: ~3.5%**.
- **When Screen Blanked / Night Mode:** Rendering suspended (0 Hz). **CPU: 0.0%**.

> [!NOTE]
> **CPU Affinity Pinning Policy:** Direct CPU core affinity pinning (`taskset` / `CPUAffinity`) is intentionally deferred for now. The Linux kernel scheduler will dynamically distribute load across all 4 Cortex-A53 cores until holistic whole-system profiling (CamillaDSP + BlueALSA + Bluetooth stack + UI) is conducted.

---

## 6. Artwork & AVRCP 1.6 Metadata Pipeline

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

### Phase 1: Core Bluetooth Backend & Automated Test Suite
- Set up `python3.14t` (`--disable-gil`) test environment.
- Implement `bluetooth_manager.py` using `dbus-fast` (async, strongly typed D-Bus client).
- Implement `tests/test_bluetooth.py` executed under `pytest` with `--disable-gil`.
- Verify device enumeration, pairing trust, 180s discoverable toggle, and connection state transitions.

### Phase 2: Artwork & Metadata Engine
- Build `cover_art_engine.py` supporting AVRCP 1.6 OBEX BIP, MusicBrainz caching, and procedural vinyl generation.
- Add AVRCP track metadata listener (Title, Artist, Album, Progress).

### Phase 3: Framebuffer UI Framework (Pygame-ce on `/dev/fb0`)
- Initialize direct framebuffer canvas (800x480, 32-bit RGBA).
- Map touch inputs directly from `/dev/input/event0` (`raspberrypi-ts`).
- Build reusable UI components (Top status bar, Bottom dock, Buttons, Sliders, Marquee text).

### Phase 4: PeppyMeter VU Needle Visualizer
- Connect real-time WebSocket client to CamillaDSP on `127.0.0.1:1234`.
- Implement analog needle physics (logarithmic dB scale, spring damping, peak hold).
- Add McIntosh Blue, Marantz Amber, and Cyberpunk Neon skins with tap-to-cycle logic.
- Integrate 30-second auto-screensaver timer.

### Phase 5: Hardware Peripherals Integration
- Integrate EC12 rotary encoder to trigger the universal 800x380 full-viewport Volume HUD overlay across all screens with 2.5s auto-dismiss.
- Implement EC12 push button actions (play/pause, mute, screen toggle).
- Connect EC12 RGB LED states (pulsing blue on pairing, cyan on playback, red on fault).
- Integrate DS18B20 temperature polling, Noctua fan PWM curve, and BH1750 auto-dimming.

### Phase 6: CD Player Interface & Stubs for AirPlay/UPnP
- Implement CD Player UI screen with spinning disc animation and track list.
- Build backend disc detection stub using `udisks2` and `mpv cdda://`.
- Add AirPlay and UPnP status listeners for seamless source switching when added.

### Phase 7: Boot Splash (POST) & Interactive Diagnostics Screen
- Implement startup Power-On Self-Test (POST) checking sound cards, DSP socket, Bluetooth, sensors, fan, and display before opening main view.
- Build interactive hardware diagnostics screen (`Screen 8`):
  - 50 Hz sub & 1 kHz coax sine wave generator test.
  - Live virtual rotary dial and push button touch visualizer.
  - RGB LED test buttons.
  - Fan speed ramp & RPM benchmark.
  - Live sensor readings & MA12070P fault clear trigger.
  - Touchscreen calibration / accuracy grid test.

### Phase 8: Appliance Hardening & Systemd Deployment
- Deploy `/etc/systemd/system/audio-ui.service` with framebuffer permissions and restart policies.
- Configure silent boot parameters in `cmdline.txt` (suppress console flicker and cursor).
- Implement DSI backlight udev rules and 180° touch coordinate inversion matrix.
- Integrate modal QWERTY touch keyboard for Bluetooth renaming and network setup.
- Add MA12070P I2C fault latch alert banner with 1-tap hardware reset.

---

## 8. Appliance Production Hardening & Operational Edge Cases

To ensure the custom audio console operates as an infallible, consumer-grade appliance on the Raspberry Pi 3, the following 6 production hardening architectures are integrated into the core system:

### 8.1. Appliance Boot & Systemd Service Lifecycle
* **Silent Boot Experience (Zero Console Flicker):**
  * Suppress standard Linux kernel console output, text scroll, and blinking tty cursor on the DSI display via `/boot/firmware/cmdline.txt`:
    ```text
    console=tty3 loglevel=3 quiet vt.global_cursor_default=0
    ```
  * Pygame takes over `/dev/fb0` immediately on service start, rendering a sleek high-contrast branding logo and hardware initialization progress ring before transitioning to the Now Playing screen.
* **Production Systemd Service Unit (`/etc/systemd/system/audio-ui.service`):**
  * Configured with strict hardware dependencies, user permissions, and auto-restart policy:
    ```ini
    [Unit]
    Description=Audiophile Appliance UI & Framebuffer Engine
    After=sound.target camilladsp.service bluealsa.service bluetooth.service systemd-udevd.service
    Wants=camilladsp.service bluealsa.service

    [Service]
    Type=simple
    User=dietpi
    Group=dietpi
    SupplementaryGroups=video input i2c gpio
    WorkingDirectory=/opt/audio-ui
    Environment=PYTHONUNBUFFERED=1
    Environment=SDL_VIDEODRIVER=kmsdrm
    Environment=SDL_FBDEV=/dev/fb0
    ExecStart=/usr/bin/python3 /opt/audio-ui/main.py
    Restart=always
    RestartSec=2s
    StandardOutput=journal
    StandardError=journal

    [Install]
    WantedBy=multi-user.target
    ```

---

### 8.2. Audio Source Arbitration & CamillaDSP Mixer Routing
* **Source Hierarchy & Arbitration:**
  1. **Optical CD Player (`cdda://`):** Highest priority. Inserting a disc or pressing Play sends an AVRCP `Pause` command to any connected Bluetooth smartphone and switches the active source pill.
  2. **Bluetooth A2DP (Smartphone):** Resumes on phone Play command or source pill selection.
  3. **AirPlay / UPnP:** Secondary network streaming inputs.
* **Unified DSP Crossover Ingestion:**
  * To guarantee that all audio sources (Bluetooth, CD, future AirPlay) pass through CamillaDSP’s active subwoofer (45–180 Hz) and coaxial (180 Hz+) crossover filters:
  * Both BlueALSA (`bluealsa-aplay`) and the CD player backend (`mpv --ao=alsa --audio-device=dsp_dmix`) route exclusively to the ALSA `dsp_dmix` plugin on `hw:Loopback,0,0`.
  * CamillaDSP captures continuously from `hw:Loopback,1,0`, applying DSP processing and hardware limiters before outputting to the InnoMaker Merus amp (`hw:sndrpimerusamp,0`).

---

### 8.3. Display Backlight Sysfs Path & Enclosure Rotation
* **Sysfs Backlight Path & Udev Permissions:**
  * Freenove 4.3" DSI display backlight brightness is controlled via `/sys/class/backlight/rpi_backlight/brightness` (values: `0` to `255`).
  * To allow the non-root `dietpi` service to adjust brightness without `sudo`, install `/etc/udev/rules.d/99-backlight.rules`:
    ```udev
    SUBSYSTEM=="backlight", RUN+="/bin/chmod 666 /sys/class/backlight/%k/brightness /sys/class/backlight/%k/bl_power"
    ```
* **180° Enclosure Mounting & Touch Inversion:**
  * In custom speaker enclosures, the DSI screen and ribbon cable may need to be mounted upside down.
  * In `/data/config/ui_settings.json`, support `display_rotation: 0` or `180`.
  * When `display_rotation == 180`:
    * Framebuffer canvas is rotated 180° (`pygame.transform.rotate(surface, 180)`).
    * Touchscreen coordinates are transformed:
      $$X_{touch} = 799 - X_{raw}, \quad Y_{touch} = 479 - Y_{raw}$$
    * Eliminates manual xinput or driver recalibration.

---

### 8.4. Resilient Error Recovery & Reconnect State Machines
* **CamillaDSP WebSocket Crash or Delayed Startup:**
  * If CamillaDSP is slow to initialize on boot or crashes under heavy load, the UI client does not throw unhandled exceptions.
  * An asynchronous background reconnect loop attempts reconnection using exponential backoff (1s, 2s, 5s, max 10s).
  * A discreet, non-blocking status pill appears on the top bar: `[ ⚠️ DSP Connecting... ]`. The rest of the UI (Bluetooth menu, Settings, CD transport) remains fully responsive.
* **BlueZ D-Bus & Bluetooth Adapter Disconnect:**
  * If the TP-Link UB500 USB adapter is unplugged or `bluetoothd` restarts, `dbus-fast` catches the disconnect signal, sets the status pill to `[ 📶 BT Offline ]`, and resubscribes to BlueZ object manager signals upon adapter re-enumeration.
* **1-Wire Temperature Sensor Fallback:**
  * If the DS18B20 1-Wire probe disconnects or generates CRC errors, the thermal management loop:
    1. Retains the last valid temperature reading for up to 10 seconds.
    2. If sensor remains offline, flags `Temp: --.-°C` on the top bar.
    3. Ramps the cooling fan to a **safe failover duty cycle of 50% PWM**, preventing thermal runaway.

---

### 8.5. On-Screen Modal QWERTY Touch Keyboard
* In addition to the numeric keypad on Screen 5 (Network Parameters), a touch-friendly **Modal QWERTY Virtual Keyboard** is provided for text input:
  * **Use Cases:**
    * Editing the Bluetooth Broadcast Name (`DietPi Speaker`) in Screen 3 / Settings.
    * Entering Wi-Fi WPA2/WPA3 passphrases when configuring wireless networking.
  * **Design & Layout:**
    * Semi-transparent dark overlay covering the bottom 240px.
    * Large touch targets ($55 \times 45\text{ px}$) with tactile hit animation and audio feedback.
    * Includes `[Shift]`, `[123 / Symbols]`, `[Space]`, `[Backspace]`, `[Clear]`, and `[✓ Done]`.
    * Dedicated text preview field with blinking cursor and character counter.

---

### 8.6. Hardware Amp Protection Alert Banner
* The Infineon MERUS MA12070P Class-D amplifier features real-time error registers accessible over I2C at address `0x20`.
* Monitored fault conditions:
  * `Flying Capacitor Overvoltage`
  * `Overcurrent Protection (OCP)`
  * `PVDD Undervoltage`
  * `Die Overtemperature Warning / Shutdown`
* **Real-Time Notification Banner:**
  * When a hardware fault bit is latched in register `0x6D`, a high-visibility warning banner drops down across the top bar:
    ```text
    ┌──────────────────────────────────────────────────────────────────────────────┐
    │ ⚠️ AMP FAULT: Overcurrent Latch Detected              [ 🔄 Reset Amp Stage ] │
    └──────────────────────────────────────────────────────────────────────────────┘
    ```
  * Tapping `[ 🔄 Reset Amp Stage ]`:
    1. Sends soft-mute gain ramp to CamillaDSP.
    2. Writes `0x01` to MA12070P I2C register `0x1E` to clear error history.
    3. Re-enables amplifier dynamic power mode without rebooting the Pi.


