# Real-Time Audio Visualizer Architecture & Implementation Plan

**Status:** Approved (audited against `Pi 3 Audio Visualizer Architecture.pdf`, `camilladsp-schema` rev `1ea8b9c`, and Slint `1.19.0`)  
**Worktree:** `/root/pi-bluetooth-speaker-visualizer` (`feat/audio-visualizer`)

---

## 1. Scope & Architecture Overview

Implements the full-screen (`800×480`) real-time audio visualizer in `speaker-dsp` and `screen-manager` driven by CamillaDSP 5.0.0-beta.2 native WebSocket streaming:

- **Two Full-Screen Visualizer Modes** (extensible registry):
  1. **Analog & Linear PeppyMeter VU** — supports all **25 skins** in `assets/skins/800x480/meters.txt` (**17 circular needle skins + 8 linear bar/indicator skins**), selected from Settings only.
  2. **24-Band ERB Frequency Spectrum** — Glasberg–Moore ERB-spaced bars (`30 Hz – 18 kHz`) with `+3.0 dB/octave` pink-noise tilt (1 kHz ref), frame-rate-independent ballistics, 3 px peak-hold caps, and toggleable retro horizontal-slit LED mask PNG overlay.
- **Navigation & Lifecycle**:
  - Launched from a dedicated button on the Now Playing screen (replacing the Tab 1 placeholder and dead `left_peak_ratio`/`right_peak_ratio` plumbing).
  - **Tap anywhere** on the visualizer to return to Now Playing.
  - **Left / Right swipe** cycles between registered visualizations (`VU Meter` ↔ `24-Band ERB Spectrum`).
  - **Volume HUD overlay** continues to render on top when the rotary encoder or volume changes.
  - **Auto-closes** back to Now Playing on Bluetooth disconnect, playback stop, display blank/idle, or when another full-screen overlay opens.
  - **Zero idle overhead**: CamillaDSP WebSocket subscriptions are opened only while the visualizer is visible and unsubscribed (`StopSubscription`) immediately when closed.

---

## 2. Verified Design Decisions (Post-Audit)

1. **One Subscription Per WebSocket Connection:** `camilladsp-schema` accepts no other commands on a connection once `SubscribeVuLevels` or `SubscribeSpectrum` is active. Each visualizer mode opens one dedicated streaming connection for the single stream it needs; switching modes closes/unsubscribes the previous stream before opening the next.
2. **Server-Side Rate-Capped VU Stream (`SubscribeVuLevels`):** Uses `WsCommand::SubscribeVuLevels { max_rate: 40.0, attack: 65.0, release: 300.0 }` (IEC 60268-17 style) instead of unthrottled `SubscribeSignalLevels`, capping socket traffic and preserving inter-frame transient peaks.
   - Calibration: **0 VU = −18.0 dBFS** (configurable via `vu_zero_dbfs` in `CoreConfig`), mapped onto each skin's `start.angle`..`stop.angle` (circular) or `position.regular` + `position.overload` steps (linear).
3. **Event-Driven Spectrum Resubscribe (`SubscribeState`):** Because `SubscribeSpectrum` returns `ProcessingNotRunningError` when idle and cancels with `ProcessingStopped` on pause (`silence_timeout_s: 3.0`) or rate change (`stop_on_rate_change: true`), the stream worker monitors `DspDevice::subscribe_state()` and automatically resubscribes when `ProcessingState::Running` returns—emitting silent frames while stopped so bars decay smoothly.
4. **Dense Grid → 24 ERB Bands:** Queries `SubscribeSpectrum { side: Capture, channel: None, min_freq: 30.0, max_freq: 18000.0, n_bins: 128, max_rate: Some(40.0) }` and max-aggregates into 24 Glasberg–Moore ERB bands (`ERB(f) = 21.4 * log10(0.00437*f + 1)`), rebuilding the LUT whenever the incoming `frequencies` slice changes. Applies `+3.0 dB/oct` pink-noise tilt (`3.0 * log2(f / 1000.0)`).
5. **Thread & Timer Split:**
   - **Background worker thread** (`libc::setpriority(PRIO_PROCESS, 0, 15)` on Linux): owns the Tokio `current_thread` runtime, WebSocket read/parse, and ERB LUT aggregation, publishing into a single `std::sync::Mutex<Option<VisualizerFrame>>` latest-frame slot (no per-message `invoke_from_event_loop`).
   - **Slint UI `30–33 Hz` `Timer` (`30 ms`)**: drains the latest-frame slot, steps frame-rate-independent spectrum ballistics (`15 ms` attack / `180 ms` decay, `500 ms` peak-hold latch + linear dB decay) using actual `Δt`, and updates `Rc<VecModel<SpectrumBandItem>>` in place (`set_row_data`).
6. **Slint 1.19 Skia-OpenGL Compliance:**
   - Needles in `assets/skins/800x480/` are already tightly cropped; place at `(origin.x - w/2, origin.y - (h/2 + distance))` and rotate with `transform-rotation` / `transform-origin` inside a `clip: true` container (supporting off-screen pivots like `blue` at `y = 605`).
   - Use `cache-rendering-hint: true` on static `-bgr` / `-fgr` / LED-mask layers; keep `border-radius: 0px`, no drop shadows, no `SharedPixelBuffer`, and no `set_rendering_notifier`.

---

## 3. Out of Scope (Deferred)

- Systemd CPU/RT isolation drop-ins (`camilladsp.service.d/realtime.conf`, `screen-manager.service.d/isolation.conf`)
- 4-step exclusive ALSA source handoff for MPD / Shairport-Sync / Librespot in `audio-manager`
- 60-minute multi-rate burn-in (will still verify `GetProcessingLoad` live on Pi 3 with `#470` active)
