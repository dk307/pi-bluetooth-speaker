# Real-Time Audio Visualizer Architecture & Implementation Plan

**Status:** Implemented (`camilladsp-schema` rev `1ea8b9c`, Slint `1.19.0`)  
**Worktree:** `/root/pi-bluetooth-speaker-visualizer` (`feat/audio-visualizer`)

---

## 1. Scope & Architecture Overview

Implements the full-screen (`800×480`) real-time audio visualizer in `speaker-dsp` and `screen-manager` driven by CamillaDSP 5.0.0-beta.2 native `SubscribeSpectrum` WebSocket streaming:

- **Two Full-Screen Visualizer Modes** (extensible `VisualizerMode` enum):
  1. **Radial FFT (`RadialFft`)** — 48-bar bilateral radial FFT around central circular album cover art (`r_inner = 116 px`, `max_bar_len = 108 px`, `r_outer = 224 px`), rendered via a precomputed-LUT `228×228` ping-pong `SharedPixelBuffer` upscaled `2×` by the GPU sampler.
  2. **24-Band ERB Spectrum (`LinearSpectrum`)** — Glasberg–Moore ERB-spaced bars (`30 Hz – 18 kHz`) with `+3.0 dB/octave` pink-noise tilt (1 kHz ref), frame-rate-independent ballistics, 3 px peak-hold caps, and toggleable retro horizontal-slit LED mask PNG overlay.
- **Navigation, Auto-Start & Settings**:
  - Launched from the Now Playing cover-art / visualizer button or via the **Auto-Start Visualizer** single-shot timer (`1..=60 s`, default `5 s`, enabled by default when playback starts on Now Playing with no overlays open).
  - **Tap anywhere** on the visualizer to return to the previous tab; **Left / Right swipe** cycles between modes.
  - **Visualizer Settings** sub-screen under Screen Settings (`Auto-Start Visualizer`, `Auto-Start Delay`, `Default Visualizer`, `24-Band LED Mask`), plus **Visualizer FPS** (`30` / `60`) in Advanced Settings.
  - **Auto-closes** on Bluetooth disconnect or playback pause/stop; **zero idle overhead** (`StopSubscription` immediately when closed).

---

## 2. Key Design Decisions

1. **Event-Driven Spectrum Resubscribe (`SubscribeState`):** Because `SubscribeSpectrum` returns `ProcessingNotRunningError` when idle and cancels with `ProcessingStopped` on pause or rate change, the stream worker monitors `DspDevice::subscribe_state()` and automatically resubscribes when `ProcessingState::Running` or `Paused` returns—emitting silent frames while stopped so bars decay smoothly.
2. **Dense Grid → 24 ERB Bands:** Queries `SubscribeSpectrum { side: Capture, channel: None, min_freq: 30.0, max_freq: 18000.0, n_bins: 128, max_rate }` and max-aggregates into 24 Glasberg–Moore ERB bands (`ERB(f) = 21.4 * log10(0.00437*f + 1)`), rebuilding the LUT whenever `frequencies` changes and applying `+3.0 dB/oct` pink-noise tilt (`3.0 * log2(f / 1000.0)`).
3. **Thread & Timer Split:**
   - **Background worker thread** (`SYS_gettid` + `setpriority(PRIO_PROCESS, tid, 15)` on Linux): owns the Tokio `current_thread` runtime and WebSocket read/parse, publishing into `LatestFrameSlot<SpectrumSnapshot>`.
   - **Slint UI `Timer` (`30 ms` or `16 ms` when 60 FPS is enabled)**: drains `LatestFrameSlot`, steps frame-rate-independent spectrum ballistics (`15 ms` attack / `180 ms` decay, `500 ms` peak-hold latch + linear dB decay), and updates only the active mode's Slint target (with a `0.003` sub-pixel deadband skip).
4. **Pi 3 Rendering Optimizations:**
   - `bg_column` (`visible: !root.visualizer_active`) and `NowPlayingView` (`animations_paused`) pause hidden background work while the full-screen visualizer is open.
   - `cache-rendering-hint: true` caches static reference rings, the circular cover-art hub, and the LED mask PNG into GPU FBOs.
