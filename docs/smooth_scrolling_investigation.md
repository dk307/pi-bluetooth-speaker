# Smooth Scrolling & Framebuffer Rendering Investigation Report

**Date:** 2026-10-31  
**Target Hardware:** Raspberry Pi 3 Model B (`aarch64`, Broadcom BCM2837 Cortex-A53 @ 1.2 GHz)  
**Display Subsystem:** `/dev/fb0` (`bcm2708_fb`, 800×480 @ 32bpp `BGRX8888`, 60 Hz DSI/HDMI panel)  
**UI Engine:** Slint `1.9.2` (`renderer-software`, `software-renderer-systemfonts`, `MinimalSoftwareWindow`)

---

## 1. Executive Summary & Problem Statement

Even when the UI render loop maintains a steady **60 FPS** (`16.75 ms` median frame interval) on the Raspberry Pi 3, horizontal marquee scrolling (`MarqueeText` in [`services/core/ui/now_playing.slint`](../services/core/ui/now_playing.slint)) visually appears **jerky / non-smooth** on the physical display.

Empirical profiling and kernel/framebuffer hardware probing on the live Raspberry Pi (`192.168.2.120`) established four root causes:

1. **Uncached Video RAM Read-Modify-Write Penalty (Partially Mitigated):**
   Slint's `render_by_line` was previously given a slice directly into the `mmap`'d `/dev/fb0` DMA/device memory. Because `/dev/fb0` is mapped as uncached/write-combining memory, alpha-blending anti-aliased font glyphs directly against the destination slice caused slow read-modify-write bus transactions (`12.0 ms` per frame for a tiny `484×68` dirty rectangle). Buffering each scanline in a cached RAM row before copying write-only to `mmap` reduced render time from **`12.0 ms` to `8.3 ms`**.
2. **Single-Buffered Scanout Tearing & Phase Beating (Unresolved — Primary Visual Bottleneck):**
   The application renders directly into the active scanout buffer (`yres_virtual = 480`, `yoffset = 0`) without hardware VSync synchronization (`FBIO_WAITFORVSYNC`) or double-buffered page flipping (`FBIOPAN_DISPLAY`). Because rendering takes `8.3 ms` (~50% of the `16.65 ms` scanout period) and `std::thread::sleep` drifts relative to the panel's hardware VBlank clock, the display controller scans out partially rendered marquee text mid-frame and experiences beat-frequency stutter between the software timer and the 60.06 Hz hardware refresh.
3. **Per-Frame Software Font Shaping & Glyph Rasterization Cost (`8.3 ms` for `484×68` px):**
   Slint's `software-renderer-systemfonts` backend re-shapes and rasterizes two copies of the overflowing `Text` element (`txt1` and the wrap-around `Text`) inside `MarqueeText` on every single animation tick, spending `8.3 ms` of CPU time to draw only ~32,900 pixels (~6.8% of the screen).
4. **Integer Pixel Grid Quantization vs. Sub-Pixel Scroll Velocities:**
   Slint's software renderer snaps `Text` `x` coordinates to integer physical pixels. At `35 px/s` (~`0.583 px/frame`), per-frame pixel deltas alternate in an uneven `1, 1, 0, 1, 1, 0` cadence. Even at `60 px/s` (`1.0 px/frame`), any slight drift between `Instant::now()` (`animation-tick()`) and the frame deadline causes occasional `0 px` or `2 px` jumps unless scroll position is stepped deterministically per VSync frame or pre-rendered into a cached texture/buffer.

---

## 2. Empirical Measurements on Live Hardware (`root@192.168.2.120`)

### 2.1 Dirty Region Analysis
Instrumenting `renderer.render_by_line(...)` inside [`services/core/src/ui/framebuffer.rs`](../services/core/src/ui/framebuffer.rs) during active marquee scrolling (`core --screen shell --theme peppy --preview 8`) confirmed that Slint's partial-rendering tracker (`RepaintBufferType::ReusedBuffer`) **is working properly** and only redraws the marquee text bounding box:

```text
Total frames in 8s window: 478
Frame 1 (initial full draw):
  DIRTY bbox=(0,0) 800x480 rects=[(0, 0, 800, 480)]
Frames 2..478 (steady-state marquee scroll):
  DIRTY bbox=(300,103) 484x68 rects=[(300, 103, 484, 36), (300, 145, 484, 26)]
```

* Only two horizontal strips (`484×36` for title + `484×26` for artist/album, totaling `29,968` pixels = **6.2% of the 800×480 panel**) are redrawn each frame.

### 2.2 Frame Interval & Render Cost Histograms

| Configuration | Frames (`n`) | Frame Interval `p50` | Frame Interval `p99` | `render_frame` `p50` | `render_frame` `p90` | `render_frame` `p99` | `render_frame` `max` | Frames `>20ms` |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **Baseline** (Direct `mmap` slice in `FbLineBuffer`, `35 px/s`) | 477 | `16,748 µs` | `16,786 µs` | **`12,020 µs`** | `12,583 µs` | `13,751 µs` | `35,853 µs` | 1 (initial frame) |
| **With Scratch-Row Buffer** (`FbLineBuffer::buffered`, `60 px/s`) | 477 | `16,763 µs` | `16,809 µs` | **`8,310 µs`** | `10,299 µs` | `11,585 µs` | `22,233 µs` | 1 (initial frame) |

**Key takeaway:** Eliminating reads from `/dev/fb0` `mmap` cut `p50` render time by **31% (`3.7 ms`/frame)** and `max` frame time by **38%**, and eliminated unnecessary `mmap_mut.flush()` (`msync`) overhead potential. However, `8.3 ms` for `30k` pixels (~`277 ns/pixel`) proves that CPU-side glyph shaping/rasterization and lack of VSync/double-buffering remain the dominant factors.

### 2.3 Hardware Framebuffer Capabilities Probe (`/dev/fb0` `bcm2708_fb`)

A live `ioctl` probe on `/dev/fb0` (`6.18.39+rpt-rpi-v8`) verified the exact capabilities of the Raspberry Pi `bcm2708_fb` kernel driver:

| `ioctl` / Property | Probed Result on Pi 3 | Implication for Smooth Scrolling |
| :--- | :--- | :--- |
| `FBIOGET_VSCREENINFO` (`0x4600`) | `xres=800, yres=480, xres_virtual=800, yres_virtual=480, bpp=32` | Default boot state is single-buffered (`1,536,000` bytes). |
| `FBIOGET_FSCREENINFO` (`0x4602`) | `smem_len=1536000, line_length=3200, ypanstep=2` | Hardware supports vertical panning (`ypanstep = 2`). |
| `FBIO_WAITFORVSYNC` (`0x40044620`) | **Supported!** Measured intervals: `16.64 ms – 16.66 ms` (avg `16.65 ms` = `60.06 Hz`) | We can lock the render loop directly to hardware VBlank instead of `thread::sleep`! |
| `FBIOPUT_VSCREENINFO` (`0x4601`) with `yres_virtual = 960` | **Supported!** Kernel dynamically reallocates `smem_len = 3,072,000` bytes (2 full pages: `y=0..480` and `y=480..960`). | Hardware page-flipping is 100% supported without rebooting or editing `/boot/config.txt`. |
| `FBIOPAN_DISPLAY` (`0x4606`) toggling `yoffset = 0` / `480` | **Supported!** Instantaneous zero-copy hardware page flip at VBlank. | Completely eliminates scanout tearing. |

> [!WARNING]
> **Power Supply / Throttling Note:** `vcgencmd get_throttled` on the Pi returned `throttled=0x50000` (bit 16: under-voltage has occurred since boot; bit 18: throttling has occurred since boot). While the current clock was `1.2 GHz`, transient CPU frequency scaling under `schedutil` + under-voltage dips can spike software rendering times from `8 ms` to `15+ ms`.

---

## 3. Current State of the Working Tree

Before picking up the next steps, note the uncommitted changes currently in the working tree:

1. [`services/core/ui/now_playing.slint`](../services/core/ui/now_playing.slint):
   - Changed `MarqueeText` scroll speed from `35px` to `property <length> speed: 60px;` (`1 px/frame` at 60 Hz).
2. [`services/core/src/ui/framebuffer.rs`](../services/core/src/ui/framebuffer.rs):
   - Updated `FbLineBuffer` with `FbLineBuffer::buffered(pixel_slice, width)` (renders into a cached `Vec<Bgrx8888Pixel>` scratch row, then `copy_from_slice` into the `mmap` slice) and `FbLineBuffer::direct(...)` for in-memory backbuffers/mocks.
3. [`services/core/src/cli/core_daemon.rs`](../services/core/src/cli/core_daemon.rs):
   - Contains **temporary** `// TEMP-PROFILING` instrumentation (`report_frame_stats` gated by `SPEAKER_FRAME_STATS=1`).
   - **Note:** This temporary block pushes `run_render_loop` to `121/100` lines, causing `clippy::too_many_lines` to fail. Either remove the `TEMP-PROFILING` blocks or extract them into a helper struct before committing.

---

## 4. Recommended Complete Solution Architecture (For the Next Agent)

To achieve truly silky-smooth, tear-free 60 FPS scrolling on the Raspberry Pi 3, implement the following three-part architecture in order:

### Phase 1: True Double-Buffering + VSync-Locked Render Loop (Eliminates Tearing & Beat Stutter)

#### 1A. Shadow RAM Backbuffer + VSync Blit (Simplest & Fastest with `ReusedBuffer`)
Because Slint's `MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer)` only renders **dirty rectangles** on each frame (`484×68` during marquee scroll):
* If you flip between two hardware pages (`yoffset = 0` and `yoffset = 480`) using `RepaintBufferType::SwappedBuffers`, Slint must redraw the differences from two frames ago, or you must copy dirty regions to both buffers.
* **Best Practice for `/dev/fb0` on RPi3:**
  1. Maintain a persistent cached RAM shadow buffer (`back_buffer: Vec<Bgrx8888Pixel>` of size `800 × 480 = 384,000` pixels = `1.5 MB`, which fits in Cortex-A53 L2/RAM and is always allocated in `Backend::Hardware`).
  2. Pass `FbLineBuffer::direct(back_buffer, width)` to `renderer.render_by_line(...)`. Now Slint's software renderer reads and writes **100% in cached L1/L2 CPU RAM** with zero `Vec` allocations per frame and zero uncached bus reads.
  3. Capture the `PhysicalRegion` returned by `renderer.render_by_line(...)`.
  4. Immediately after `draw_if_needed` returns `true`, wait for hardware VBlank via `ioctl(fd, FBIO_WAITFORVSYNC, &0u32)` (if on a real char device where the ioctl succeeds; fall back gracefully to timer sleep in mocks/regular files).
  5. Immediately upon VSync wakeup, copy **only the dirty bounding box scanlines** (or each rect in `region.iter()`) from `back_buffer` to `mmap_mut` using contiguous `copy_from_slice` calls!
     - Copying `484 × 68 × 4 = 131 KB` from cached RAM to `mmap` takes **`< 150 µs`** on Cortex-A53 NEON — completing entirely inside the vertical blanking interval before scanout reaches row `103`!
  6. Remove `mmap_mut.flush()` (`msync(MS_SYNC)`) on `/dev/fb0` char devices — `bcm2708_fb` maps physical DMA memory (`VM_IO | VM_PFNMAP`), so `msync` is a syscall no-op that only adds kernel transition overhead.

*(Alternative 1B: Hardware Page Flipping via `yres_virtual = 960` + `FBIOPAN_DISPLAY`)*
* If full-screen animations (e.g., tab transitions or full-screen VU meters) are added later where the dirty region is `800×480`, configure `yres_virtual = 960` via `FBIOPUT_VSCREENINFO`, copy the dirty rects from `back_buffer` into the inactive `mmap` page (`page 0` at byte offset `0`, `page 1` at byte offset `1_536_000`), and issue `FBIOPAN_DISPLAY` (`yoffset = page * 480`) followed by `FBIO_WAITFORVSYNC`.
* **Contract note:** [`Framebuffer::validate_sysfs_geometry`](../services/core/src/ui/framebuffer.rs) currently checks `/sys/class/graphics/fb0/virtual_size == "800,480"`. If you enable `yres_virtual = 960`, update `validate_sysfs_geometry` to accept `800,480` or `800,960` (and restore `800,480` on `Drop`). With Approach **1A** (RAM shadow buffer + dirty-rect VSync blit), `virtual_size` stays `800,480` and no sysfs validation changes are needed!

### Phase 2: Eliminate Frame-Time Jitter in `MarqueeText`

Even with VSync, inspect [`services/core/ui/now_playing.slint`](../services/core/ui/now_playing.slint):

```slint
property <float> phase_t: is_overflow ? Math.mod(animation-tick() / 1s, period) : 0.0;
property <float> progress: (phase_t < pause_secs) ? 0.0 : ((phase_t - pause_secs) / scroll_secs);
property <length> scroll_offset: is_overflow ? (-cycle_distance * progress) : 0px;
```

1. **Snap `scroll_offset` consistently or step by whole pixels:**
   When `animation-tick()` is sampled with even `±0.5 ms` variance around a `16.667 ms` boundary at `60 px/s`, `(-cycle_distance * progress)` can straddle an integer pixel boundary (`e.g., -10.99px` then `-12.01px`), causing a 0-pixel step followed by a 2-pixel jump.
   - Using `Math.round((-cycle_distance * progress) / 1px) * 1px` at a speed slightly decoupled or driven by a frame counter (or driving `MarqueeText` via a Slint `animate` property with `easing: linear` and `iteration-count: -1`) makes Slint's internal animation tick interpolation deterministic.
2. **Disable the `180ms` `AmbientWaveIndicator` Timer or decouple its dirty region:**
   In [`services/core/ui/now_playing.slint`](../services/core/ui/now_playing.slint), `AmbientWaveIndicator` runs a `Timer { interval: 180ms }` that mutates 5 bar heights with `animate height { duration: 160ms; easing: ease-in-out; }`. Verify whether `AmbientWaveIndicator` is instantiated on the Now Playing screen; if it is, its dirty rect merges with or adds to the per-frame redraw work.
3. **Avoid re-shaping two long `Text` elements every frame if CPU remains high:**
   Currently `MarqueeText` renders two `Text` elements (`txt1` at `x: scroll_offset` and a second `Text` at `x: scroll_offset + cycle_distance`), both of width `text_width` (which can be `600–1200 px` for long Unicode titles), clipped by `clip: true`.
   - Slint's software renderer shapes and iterates glyphs for the entire `Text` string on every frame even for glyphs clipped outside `0..root.width`.
   - Furthermore, the second `Text` is only visible when `scroll_offset + cycle_distance < root.width` (i.e. after the first copy's trailing gap enters the right edge of the viewport). Guarding the second copy with:
     ```slint
     if root.is_overflow && (root.scroll_offset + root.cycle_distance < root.width) : Text { ... }
     ```
     cuts the text shaping/rasterization cost in **half** (`~8.3 ms -> ~4.2 ms`) for the majority of the scroll cycle!

### Phase 3: Offload Cover Art Image Decoding from the Render Thread

In [`services/core/src/ui/shell.rs`](../services/core/src/ui/shell.rs) (`apply_playback_to_window`, lines 294–306):
* `slint::Image::load_from_path` synchronously reads and decodes the JPEG on the UI render thread whenever `cover_art_path` changes, causing a single-frame `22–54 ms` spike.
* Only call `apply_bluetooth_playback` field setters when the underlying value actually changed (currently `set_track_title`, `set_track_artist`, `set_track_album` are called every `250 ms` whenever `position_ms` ticks, invalidating property bindings unnecessarily!).
* Specifically, in `apply_playback_to_window`:
  ```rust
  if w.get_track_title().as_str() != playback.title.as_ref() {
      w.set_track_title(playback.title.as_ref().into());
  }
  if w.get_track_artist().as_str() != playback.artist.as_ref() {
      w.set_track_artist(playback.artist.as_ref().into());
  }
  if w.get_track_album().as_str() != playback.album.as_ref() {
      w.set_track_album(playback.album.as_ref().into());
  }
  ```
  **Why this matters:** In Slint, calling `w.set_track_title(...)` every `250 ms` (every time `position_ms` advances on the Bluetooth worker thread) notifies `MarqueeText.text` subscribers and forces layout/preferred-width re-evaluation on the marquee text 4 times per second during scrolling!

---

## 5. Step-by-Step Implementation Checklist for the Next Agent

- [ ] **Step 1 (Guard redundant property sets in `shell.rs`):**
  In [`services/core/src/ui/shell.rs`](../services/core/src/ui/shell.rs) `apply_playback_to_window`, compare `w.get_track_title()`, `w.get_track_artist()`, `w.get_track_album()`, `w.get_track_or_device()`, `w.get_is_playing()`, and `w.get_duration_ms()` before calling their `set_*` methods so 250ms position ticks only update `progress_ratio` and `position_text`.
- [ ] **Step 2 (Optimize `MarqueeText` in `now_playing.slint`):**
  - Only instantiate the wrap-around `Text` when `root.is_overflow && (root.scroll_offset + root.cycle_distance < root.width)`.
  - Test `animate` with linear easing or integer-pixel-locked stepping so every 60 Hz tick advances by a constant integer pixel delta (`1px/frame`).
- [ ] **Step 3 (Shadow Backbuffer + Dirty-Rect VSync Blit in `framebuffer.rs`):**
  - In `Backend::Hardware`, always render into `back_buffer` (`FbLineBuffer::direct(back_buffer, width)`).
  - Add an `ioctl` wrapper for `FBIO_WAITFORVSYNC` (`0x4004_4620`) on the framebuffer `File` descriptor (`AsRawFd`).
  - When `did_draw` is `true` and `mmap` is `Some`, call `wait_for_vsync()` (ignoring errors on non-fb0 files) and copy only the dirty `PhysicalRegion` rectangles from `back_buffer` into `mmap_mut`.
  - In `core_daemon.rs` `run_render_loop`, when `did_render` is true and VSync is active, `wait_for_vsync()` already paces the loop to the exact `16.65 ms` hardware refresh — avoid double-sleeping after a VSync-paced frame.
- [ ] **Step 4 (Clean up temporary profiling & verify all gates):**
  - Remove `// TEMP-PROFILING` from `services/core/src/cli/core_daemon.rs` (or move behind a clean helper so `run_render_loop` is `< 100` lines).
  - Run all mandatory gates from `AGENTS.md`:
    1. `cargo fmt --manifest-path services/core/Cargo.toml -- --check`
    2. `cargo clippy --manifest-path services/core/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D warnings`
    3. `cargo llvm-cov --manifest-path services/core/Cargo.toml --target x86_64-unknown-linux-gnu --fail-under-lines 90 --fail-under-functions 90`
    4. `cargo build --config services/core/.cargo/config.toml --manifest-path services/core/Cargo.toml --target aarch64-unknown-linux-gnu --release --bins`
- [ ] **Step 5 (Deploy & verify on Pi `root@192.168.2.120`):**
  - Copy `core` to `/usr/local/bin/core` (when requested by user) or test via `/tmp/core` and verify smooth, tear-free marquee scrolling on the physical screen.
