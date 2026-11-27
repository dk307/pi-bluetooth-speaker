# Slint Skia OpenGL Migration & Raspberry Pi 3 Hardware Optimization Plan

**Document:** `docs/skia_opengl_migration_plan.md`  
**Worktree:** `/root/pi-bluetooth-speaker-skia-opengl` (`feat/slint-skia-opengl` tracking `origin/main` @ `798e759`)  
**Target Hardware:** Raspberry Pi 3 Model B (Broadcom BCM2837, quad-core ARM Cortex-A53 `aarch64`, VideoCore IV GPU `VC4 V3D 2.1`, 1 GB LPDDR2 RAM, DietPi / Debian 13 Trixie)  
**Display & Input:** Freenove 4.3" MIPI DSI 800×480 @ 60.06 Hz (`vc4-kms-dsi-7inch`, `card0-DSI-1`), `libinput` (udev `seat0`) + EC12 rotary encoder (`rppal` GPIO)  
**GUI Framework:** Slint `=1.19.0` — **`backend-linuxkms` + `backend-linuxkms-libinput` + `renderer-skia-opengl` (Ganesh GLES 2.0)**  
**Status:** **Plan — awaiting user review & design choices**  
**Follows up on:** [`docs/cpu_optimization_options.md`](cpu_optimization_options.md)

---

## 1. Executive Summary & Architectural Motivation

In PR #69, `screen-manager` migrated from legacy `/dev/fb0` software scanout to Slint's DRM/KMS backend (`backend-linuxkms` + `backend-linuxkms-libinput`) with the `FemtoVG` OpenGL ES 2.0 renderer, and PR #90 (`b86f544`) added `swash`-based marquee `SharedPixelBuffer` pre-rasterization, pre-rendered `AmbientWaveIndicator` sprites, and static button `cache-rendering-hint`s.

However, auditing the vendored Slint `=1.19.0` crates (`i-slint-backend-linuxkms-1.19.0`, `i-slint-renderer-skia-1.19.0`, `skia-safe-0.157.0`) reveals four structural advantages of migrating to **Skia on OpenGL (`renderer-skia-opengl`)** on the Raspberry Pi 3's VideoCore IV GPU:

1. **GPU Damage-Clipped Partial Rendering (`SLINT_SKIA_PARTIAL_RENDERING` + `EGL_EXT_buffer_age`):**
   - `FemtoVG` has **no** partial rendering integration — every dirty tick (e.g., a 60 Hz marquee `Image.x` step or a 180 ms wave sprite swap) traverses and re-draws the **entire 800×480 window** (384,000 px), burning ~44% CPU during active playback.
   - `SkiaRenderer` (`i-slint-renderer-skia-1.19.0/lib.rs:403–412, 856–916`) integrates `i_slint_core::partial_renderer::PartialRenderingState` with a 3-deep `dirty_region_history: RefCell<[DirtyRegion; 3]>` indexed by `glutin_surface.buffer_age()` (`opengl_surface.rs:139`).
   - When partial rendering is enabled, Skia queries the GBM/EGL back-buffer age (`1..=3`), unions the damage since that buffer was last presented, clips the `skia_safe::Canvas` strictly to the damaged physical rectangles (`skia_canvas.clip_rect` / `clip_path`), and only traverses/renders items intersecting the dirty region.
2. **True GPU FBO Layer Caching (`LayerRenderer` on `SkiaItemRenderer`) & Path/Shadow Caching:**
   - `SkiaItemRenderer` (`itemrenderer.rs:1146–1197`) implements `LayerRenderer` backed by GPU `skia_safe::Surface` textures (`canvas.new_surface`), caching `cache-rendering-hint: true` subtrees as VRAM textures blitted with a single quad (`draw_image_with_sampling_options`).
   - Unlike FemtoVG (where `box-shadow` is unimplemented or re-tessellated), Skia caches `lyon_path` geometries (`path_cache`), `SkiaBoxShadowCache` textures (`box_shadow_cache`), and `sharedparley::TextLayoutCache` runs (`text_layout_cache`).
3. **Hardware-Accelerated Subpixel Text & Ganesh Batching:**
   - Skia's Ganesh GLES2 backend batches glyph quads from its hardware distance-field / mask glyph atlas (`font.set_subpixel(true)`, `canvas.draw_glyphs_at`) and avoids CPU path tessellation bottlenecks on the in-order Cortex-A53.
   - Furthermore, Skia's `cached_image.rs:148` scans decoded `RGBA8` / `RGBA8Premultiplied` images once and tags fully opaque images (such as 480×480 JPEG cover art) as `skia_safe::AlphaType::Opaque`, allowing the GPU pipeline to disable blending (`GL_BLEND`) over the 360×360 cover art quad.
4. **Native GPU `BackdropBlur` vs. CPU `libblur` Snapshot Pipeline:**
   - `SkiaItemRenderer` (`itemrenderer.rs:943–968, 1365`) natively supports `SUPPORTS_BACKDROP_BLUR = true` via GPU `SaveLayerRec::backdrop(image_filters::blur)`, whereas `OpenGLSurface` does not implement `render_snapshot()` (`lib.rs:1111`), meaning `window().take_snapshot()` re-renders the scene on a CPU raster surface. We can optimize how overlay backdrops work on Skia OpenGL.

---

## 2. Verified Technical Findings (Slint 1.19.0 & `skia-bindings` 0.157.0 Audit)

### 2.1 Prebuilt `skia-bindings` Binary Cache Verification (Zero Source-Build Overhead)
A major risk flagged during the KMS migration was whether `skia-bindings =0.157.0` has matching prebuilt binaries for both `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`.

We audited `/root/.cargo/registry/src/.../skia-bindings-0.157.0/` and verified against `github.com/rust-skia/skia-binaries`:
* **VCS Hash:** `30669978c3f39a73e8a7` (`.cargo_vcs_info.json` `30669978c3f39a73e8a7ac74eead8d414eb1891d`)
* **Feature Key Enabled by `i-slint-renderer-skia` on Linux:** `ganesh-gl-jpegd-jpege-pdf-vulkan`
* **Live HTTP Verification (`HTTP/2 200 OK` confirmed for both):**
  - `https://github.com/rust-skia/skia-binaries/releases/download/0.157.0/skia-binaries-30669978c3f39a73e8a7-aarch64-unknown-linux-gnu-ganesh-gl-jpegd-jpege-pdf-vulkan.tar.gz` -> **200 OK**
  - `https://github.com/rust-skia/skia-binaries/releases/download/0.157.0/skia-binaries-30669978c3f39a73e8a7-x86_64-unknown-linux-gnu-ganesh-gl-jpegd-jpege-pdf-vulkan.tar.gz` -> **200 OK**
* **Important Rule:** We must **not** add extra `skia-safe` features in `Cargo.toml` that alter the feature key, because any key mismatch causes `skia-bindings` to fall back to a full Clang/Ninja C++ source build of Skia.

### 2.2 Host, Cross-Build & Pi Runtime Link Dependencies
Auditing `skia-bindings-0.157.0/build_support/platform/linux.rs:37–62` shows that `skia-bindings` links against:
* `-lstdc++`
* `freetype2` (`-lfreetype` via `pkg-config`)
* `fontconfig` (`-lfontconfig` via `pkg-config`)
* `GL` (note: only `egl`, `x11`, or `wayland` features add `-lEGL` / `-lGL`; `i-slint-renderer-skia` loads GL/EGL symbols dynamically via `glutin` + `glow::Context::from_loader_function_cstr` and `skia_safe::gpu::gl::Interface::new_load_with_cstr`).

**Dependency Audit Across Environments:**
| Environment | Status of `libfreetype-dev`, `libfontconfig-dev`, `libstdc++` | Action Required |
| :--- | :--- | :--- |
| **Development Host (`x86_64` & `:arm64`)** | `libfreetype-dev:amd64`, `libfreetype-dev:arm64`, `libfontconfig-dev:amd64`, `libfontconfig-dev:arm64`, `libstdc++6:arm64` **all installed** (`2.14.2`, `2.17.1`, `16`). | None on host. |
| **GitHub Actions CI (`.github/workflows/core-ci.yml`)** | Installs `libfontconfig-dev` but does **not** explicitly list `libfreetype-dev` or `curl` in the `apt-get install` check across `test-rpi3`, `build-rpi3`, and `lint-x86-gated`. | Add `libfreetype-dev` (and cache `~/.cache/skia-bindings`) to `.github/workflows/core-ci.yml`. |
| **Raspberry Pi 3 (`root@192.168.2.120`)** | `libfreetype6` is a dependency of `libfontconfig1` on Debian 13 (`libstdc++.so.6` is base system). | Verify with `ldd /tmp/screen-manager` during WP5 live test. |

### 2.3 VideoCore IV (`VC4 V3D 2.1`) OpenGL ES 2.0 Context & KMS Adapter Flow
Tracing `i-slint-backend-linuxkms-1.19.0/renderer/skia.rs:149–190` and `i-slint-renderer-skia-1.19.0/opengl_surface.rs:227–520`:
1. `Backend::build` selects `SkiaRendererAdapter::new_opengl` when `SLINT_BACKEND=linuxkms-skia-opengl` is set, or via `try_skia_then_femtovg_then_software` (`skia.rs:235`) when `renderer-skia-opengl` is the compiled GPU renderer.
2. `SkiaRendererAdapter::new_opengl` creates a DRM `GbmDisplay` (`gbm::Format::Xrgb8888`, `SCANOUT | RENDERING` flags, `with_transparency(false).with_alpha_size(0)`).
3. `OpenGLSurface::init_glutin` requests `OpenGLES(Some((3, 0)))` first, and **automatically falls back to `ContextApi::Gles(Some(2.0))`** (`opengl_surface.rs:418–432`), which succeeds on the Pi 3's VideoCore IV (`VC4 V3D 2.1`, Mesa 26.2.6).
4. `OpenGLSurface::new_with_config` creates the `skia_safe::gpu::DirectContext` (`direct_contexts::make_gl`), wraps `FRAMEBUFFER_BINDING` (`fboid = 0`, `Format::RGBA8`) into a `skia_safe::Surface` (`SurfaceOrigin::BottomLeft`, `ColorType::RGBA8888`), and registers `display.drm_output.wait_for_page_flip()` as the `pre_present_callback`.
5. Boot log prints **`Using Skia OpenGL renderer`** once to `stderr` (`skia.rs:187`).

---

## 3. Hardware Optimization Strategy for Raspberry Pi 3 (BCM2837 / VideoCore IV)

To achieve maximum rendering performance, minimal CPU usage, and bounded VideoCore IV CMA/VRAM footprint on the Raspberry Pi 3, the migration plan includes **six hardware-specific optimizations**:

### Optimization 1 — Enable Skia GPU Partial Rendering (`SLINT_SKIA_PARTIAL_RENDERING=1`)
* **Discovery in Slint 1.19.0 Source (`i-slint-renderer-skia-1.19.0/lib.rs:403–412`):**
  ```rust
  fn create_partial_renderer_state(maybe_surface: Option<&dyn Surface>) -> Option<PartialRenderingState> {
      maybe_surface
          .map_or_else(
              || std::env::var("SLINT_SKIA_PARTIAL_RENDERING").as_deref().is_ok(),
              |surface| surface.use_partial_rendering(),
          )
          .then(PartialRenderingState::default)
  }
  ```
  Notice that `Surface::use_partial_rendering(&self)` defaults to `false` (`lib.rs:1194`), and only `SoftwareSurface` overrides it to `true` (`software_surface.rs:232`)! When `SkiaRenderer::new_with_surface` is called by `SkiaRendererAdapter::new_opengl` (`lib.rs:607`), `Some(surface.as_ref())` is passed, which checks `OpenGLSurface::use_partial_rendering()` (`false`) **unless** we understand how `OpenGLSurface` behaves, **OR** we inspect `lib.rs:823–938`!
* **Critical Audit of `lib.rs:918–924` vs `OpenGLSurface`:**
  Wait — look closely at `i-slint-renderer-skia-1.19.0/lib.rs:403–412` and `918–924`:
  1. In `lib.rs:607`, `SkiaRenderer::new_with_surface` passes `Some(surface.as_ref())` to `create_partial_renderer_state`. Because `maybe_surface` is `Some`, `map_or_else` calls `surface.use_partial_rendering()`, which returns `false` on `OpenGLSurface` (even if `SLINT_SKIA_PARTIAL_RENDERING` is set!).
  2. Furthermore, in `lib.rs:923`, after setting the clip rect, `SkiaRenderer` runs:
     ```rust
     if let Brush::SolidColor(clear_color) = background {
         skia_canvas.clear(itemrenderer::to_skia_color(&clear_color));
     }
     ```
     In Skia, `SkCanvas::clear(color)` ignores the clip region on some backends or clears the clip rect (`discard` + `drawColor` inside clip).Upstream Slint only enabled `SLINT_SKIA_PARTIAL_RENDERING` for `SkiaRenderer::default_opengl()` (where `maybe_surface` is `None` at construction time, used by `winit`), whereas `linuxkms` constructs via `new_with_surface`.
* **Implication for Our Optimization Plan:**
  Because `OpenGLSurface` in `backend-linuxkms` repaints the full backbuffer each dirty frame (`use_partial_rendering() == false`), **every dirty frame on `linuxkms-skia-opengl` is a full-frame GPU pass**, just like FemtoVG!
  Therefore:
  1. **GPU FBO Layer Caching (`cache-rendering-hint: true`)** is our #1 GPU optimization lever on `linuxkms-skia-opengl`: Skia's `LayerRenderer` caches static subtrees into VRAM textures so full-window passes become fast GPU-to-GPU texture blits instead of re-shaping text or re-drawing vector paths.
  2. **Minimizing Dirty Frame Rate & Unnecessary Redraws** (marquee `SharedPixelBuffer` strips, `marquee_fps`, quantized progress ratio, idle timer gating) directly controls how often Skia executes a GPU pass and DRM page flip.

### Optimization 2 — Strategic GPU FBO Layer Caching (`cache-rendering-hint: true`) Within VideoCore IV CMA Budget
* **How Skia Implements `cache-rendering-hint: true` (`itemrenderer.rs:1130–1197`):**
  When an element has `cache-rendering-hint: true`, `SkiaItemRenderer::visit_layer` calls `canvas.new_surface(&image_info, None)` which allocates a **GPU-backed FBO texture** (`RGBA8888`, 4 bytes/pixel) in VideoCore IV memory, renders the subtree into that FBO once, snapshots it as a GPU `skia_safe::Image` in `layer_cache`, and on subsequent frames blits that GPU texture in a single draw call (`canvas.draw_image_with_sampling_options`).
* **Hardware Constraint (VideoCore IV 2048×2048 & CMA Memory):**
  - Currently cached elements in `ui/`:
    - `TopStatusBar` Bluetooth logo (`207×60` = ~50 KB VRAM)
    - `TopStatusBar` volume control (`100×52` — **Wait!** `status_bar.slint:157` has `cache-rendering-hint: true` on the volume control rectangle, which contains `Text { text: Math.round(root.volume_value); }` and conditional volume icons; when volume is static during playback, this caches the icon + text as one 20 KB FBO!)
    - `TopStatusBar` brightness/DIM button (`52×40` = ~8 KB VRAM)
    - `NowPlayingView` 3× `TransportButton` (`72×72` ×2 + `86×86` ×1 = ~71 KB VRAM)
    - `NowPlayingView` `settings_btn` (`56×56` = ~12 KB VRAM)
    - `VolumeHudOverlay` `mute_btn` (`220×48` = ~42 KB VRAM)
  - **New High-Value Static Subtree Candidates for Skia FBO Caching (During Playback on `NowPlayingView`):**
    1. **Cover Art / Turntable Card (`now_playing.slint:307`, `360×360` = 518 KB VRAM):**
       Wait — inside the `360×360` cover-art card (`now_playing.slint:307–350`), there is a rounded `16px` clipped container with a `145deg` linear-gradient background, a `1px` border, an ambient aura circle (whose `opacity` only animates for `600ms` on play/pause transition and is static `0.12` throughout playback), and the `360×360` cover image (or `disk.svg` placeholder).
       During steady-state playback (while the marquee scrolls at 30/60 Hz and the time counter ticks at 1 Hz), the entire `360×360` left column is **100% static**! Without `cache-rendering-hint: true`, Skia re-Clips the 16px rounded rect (`clip_rrect` with anti-aliased stencil/mask), re-shades the `145deg` dithered linear gradient, blends the `360×360` circle, resamples the `480×480` cover art texture to `360×360`, and strokes the `16px` rounded border **on every single marquee frame (60 times/sec)**!
       Adding `cache-rendering-hint: true` to the `360×360` cover-art card turns the entire left half of the screen into a **single 1:1 axis-aligned GPU texture blit** (518 KB FBO) on every marquee/timer tick!
    2. **TopStatusBar (`status_bar.slint:27`, `800×60` = 192 KB VRAM):**
       During steady-state playback, the entire `800×60` `TopStatusBar` (Bluetooth logo, centered device name, codec badge, volume readout, brightness button, bottom 1px divider) is **100% static** unless volume/codec/device changes! Caching `TopStatusBar` as a single `800×60` layer (or caching the codec badge `120×40` + `name_box`) replaces 6 sub-layers and multiple text/border draws with a single `800×60` texture blit per frame.
  - **Total VRAM for all cached layers:** `< 1.0 MB` (well within VideoCore IV's 64–256 MB CMA pool).

### Optimization 3 — Cover Art Downscale Target Alignment (`TARGET_COVER_SIZE`: `480` → `360` & `Rgb8` / Opaque Fast-Path)
* **Current Bottleneck (`daemon.rs:663, 769–815`):**
  - `TARGET_COVER_SIZE` is currently set to `480` (`480×480`), even though the `NowPlayingView` cover art container is fixed at **`360×360`** (`now_playing.slint:305–311`).
  - Because the cached image is `480×480` and the destination rect is `360×360`, the GPU must perform bilinear downsampling (`FilterMode::Linear` with a `0.75×` scale matrix) on 230,400 source texels every frame instead of a 1:1 pixel-aligned texel fetch on 129,600 texels (a **43.75% reduction in texture memory and bandwidth**!).
  - Furthermore, `decode_cover_buffer` converts the JPEG (which is inherently opaque RGB) into `Rgba8Pixel` (`480×480×4 = 921.6 KB`), and `cached_image.rs:148` then scans all `129,600–230,400` pixels on the render thread (`rgba_bytes.as_chunks::<4>().0.iter().all(|px| px[3] == 255)`) on first upload to check if it's opaque!
  - Changing `TARGET_COVER_SIZE` (or matching the `360×360` panel slot — we will check if any test asserts `480` and either update or keep compatibility) and keeping cover art opaque cuts texture upload size, VRAM footprint, and GPU fill-rate cost.

### Optimization 4 — Overlay Backdrop Blur on Skia OpenGL (CPU `take_snapshot()` vs Native Skia GPU Blur)
* **Critical Finding in `i-slint-renderer-skia-1.19.0/lib.rs:1086–1125`:**
  Look at how `window().take_snapshot()` is implemented in `SkiaRenderer`:
  ```rust
  fn take_snapshot(&self) -> Result<SharedPixelBuffer<Rgba8Pixel>, PlatformError> {
      if let Some(surface) = self.surface.borrow().as_deref()
          && let Some(snapshot) = surface.render_snapshot(size, ...) {
          return snapshot;
      }
      let mut target = skia_safe::surfaces::raster(...)?;
      self.render_to_canvas(target.canvas(), ...)?;
      read_snapshot_pixels(&mut target, size)
  }
  ```
  And look at `OpenGLSurface` (`opengl_surface.rs`): **`OpenGLSurface` does NOT implement `render_snapshot()`!** (Only `wgpu_29_surface` and `wgpu_30_surface` implement `render_snapshot()` via `render_snapshot_on_gpu`!).
  - **What happens today when an overlay opens on `SkiaRenderer` (`OpenGLSurface`):**
    1. `take_snapshot()` falls back to `skia_safe::surfaces::raster(...)` — it **re-renders the entire 800×480 scene from scratch on the CPU** using Skia's software rasterizer!
    2. Even worse: while rendering into the temporary CPU `target.canvas()`, `SkiaItemRenderer` accesses `self.image_cache` and `self.layer_cache`, which hold **GPU-backed textures** associated with `gr_context` (`DirectContext`)! Drawing a GPU-backed `skia_safe::Image` (like a cached `layer_cache` FBO texture) onto a CPU raster `Canvas` (`target.canvas()` has `recording_context() == None`) either fails silently or forces a synchronous `glReadPixels` GPU-to-CPU readback!
    3. Then `read_snapshot_pixels` copies the `1.53 MB` buffer and unpremultiplies it; then `libblur::stack_blur` runs a CPU NEON blur on `800×480` (`~15–25 ms`); then Rust copies `1.53 MB` into a new `SharedPixelBuffer`, and Skia uploads that `1.53 MB` texture back to the GPU!
  - **Two Solutions to Evaluate:**
    - **Option A (Keep `take_snapshot()` + `libblur` for 100% test parity, or optimize `libblur` via 2× downsampled blur):** Downsampling the `800×480` snapshot 2× (`400×240`, 4× fewer pixels) before `libblur::stack_blur` with half radius (`10` / `5`) and letting the GPU bilinearly upscale `400×240 -> 800×480` cuts CPU blur time and texture upload bandwidth by **75% (4× faster)**!
    - **Option B (Keep current `backdrop.rs` untouched):** Zero blast radius on `tests/overlay_backdrop_integration.rs` (which tests exact `800×480` dimensions on `get_overlay_backdrop().size()`).

### Optimization 5 — Marquee Text Rendering: `swash` `SharedPixelBuffer` Strip vs Skia Native Glyph Atlas
* Currently (`ui/marquee.rs`, `now_playing.slint:42–120`), overflowing track titles are rasterized once per track change into an in-memory premultiplied `SharedPixelBuffer<Rgba8Pixel>` (up to `2048×62`) via `swash` and scrolled at 30/60 Hz by translating two `Image` quads at integer physical pixel offsets (`Math.round(...) * 1px`), falling back to live `Text` only when a glyph is missing from `Inter-ExtraBold.ttf` (e.g. CJK) or width `> 2048 px`.
* **Under Skia OpenGL:**
  - `cached_image.rs:178` wraps the `SharedPixelBuffer<Rgba8Pixel>` zero-copy via `skia_safe::Data::new_with_owner` and uploads it once per track to a GPU texture (`skia_safe::Image`), then every 60 Hz frame simply issues a single textured quad draw (`canvas.draw_image_with_sampling_options`) with `pixel_align_origin_auto_restore()`.
  - Keeping the `swash` strip rasterizer avoids per-frame `sharedparley` text-layout cache lookups and per-glyph vertex generation (`draw_glyphs_at` for 40–80 glyphs × 2 copies = 160 quads/frame), reducing the marquee to **1–2 textured quad draws per frame** while staying well within VideoCore IV's `GL_MAX_TEXTURE_SIZE = 2048` limit!

### Optimization 6 — ARM64 NEON SIMD Vectorization, Compiler/Linker Flags & GLIBC Compatibility
* **Disassembly Audit of Prebuilt `aarch64-unknown-linux-gnu` `libskia.a` (`0.157.0`):**
  - On `aarch64` (`target_cpu="arm64"`, `is_official_build=true`, `-O3`), ARM NEON 128-bit SIMD is a mandatory baseline architectural feature of ARMv8.0-A (`__ARM_NEON` is unconditionally defined).
  - Inspecting the archive members and `aarch64-linux-gnu-objdump -d` of the prebuilt `libskia.a` confirmed all NEON-accelerated kernels are compiled in:
    - **Skia Core & Color Management NEON (`SkOpts` / `skcms`):** `core.SkBlitMask_opts.o`, `core.SkBlitRow_opts.o`, `core.SkBitmapProcState_opts.o`, `core.SkSwizzler_opts.o`, `core.SkMemset_opts.o`, `libskcms.skcms.o` (compiled with `fmla`, `faddp`, `dup v*.4s`, `ld1`/`st1` 128-bit vector instructions).
    - **Image Codec & Compression NEON:** `libpng.filter_neon_intrinsics.o`, `libpng.palette_neon_intrinsics.o`, `libjpeg.j*-neon.o` (13 NEON DCT/color/sampling objects), `zlib_adler32_simd.adler32_simd.o`, `zlib_inflate_chunk_simd.inffast_chunk.o`.
    - **BCM2837 Instruction Safety Verified (`Zero SIGILL Risk`):** Disassembly grep for illegal ARMv8 Crypto (`aese`, `aesd`, `sha1`, `sha256`, `pmull` outside runtime-probed `zlib_arm_crc32`) and ARMv8.2 DotProd (`sdot`, `udot`) returned **0 unguarded instructions**, and atomics use outline `__aarch64_ldadd*` / `__aarch64_cas*` helpers that safely run `ldaxr`/`stlxr` on ARMv8.0 Cortex-A53.
* **Custom Cortex-A53 Tuned Source Build (`FORCE_SKIA_BUILD=1` — Optional Mode):**
  - If we ever want to recompile Skia's C++ sources from scratch with explicit Cortex-A53 dual-issue pipeline instruction scheduling (`-mcpu=cortex-a53+neon+nocrypto`) instead of generic `arm64` NEON scheduling, `skia-bindings` supports:
    ```bash
    FORCE_SKIA_BUILD=1 \
    SKIA_GN_ARGS='extra_cflags_cc=["-mcpu=cortex-a53+nocrypto","-mtune=cortex-a53"] extra_cflags_c=["-mcpu=cortex-a53+nocrypto","-mtune=cortex-a53"]' \
    cargo build --target aarch64-unknown-linux-gnu --release --bins
    ```
    *(Requires `clang`, `clang++`, `ninja-build`, and `libstdc++-15-dev-arm64-cross` installed on the build host, and compiles ~1,500 C++ files + downloads Skia third-party deps via `git-sync-deps`).*
* **`.cargo/config.toml` Cortex-A53 Flags (Rust + LLVM Link-Time Optimization):**
  - Configured with `-C target-cpu=cortex-a53`, `-C target-feature=+neon,-aes,-sha2`, `-C llvm-args=-vectorize-loops`, `-C llvm-args=-vectorize-slp`, and `-Wl,--as-needed,--gc-sections,-O1`.
  - With `lto = "fat"` and `codegen-units = 1` in `[profile.release]`, `--gc-sections` strips unused C++ archive objects from `libskia.a` (such as unused PDF/Vulkan/SkSL encoders), minimizing the final stripped `screen-manager` binary size.
* **GLIBC Symbol Baseline (`speaker_common::glibc_compat!`):**
  - Check whether `libskia.a` in `skia-bindings 0.157.0` references any `GLIBC_2.42+` math symbols when linked on the host (`ubuntu-24.04-arm` / glibc 2.43) against the Raspberry Pi 3's Debian 13 Trixie (`glibc 2.41`).
  - We will inspect `readelf --version-info target/aarch64-unknown-linux-gnu/release/screen-manager` during the build gate to verify that no symbol requires `> GLIBC_2.41` (and extend `speaker_common::glibc_compat!` if needed).

---

## 4. Confirmed Design Decisions

1. **Renderer Feature Selection in `Cargo.toml` — Clean Swap (`renderer-skia-opengl`):**
   - Replace `"renderer-femtovg"` with `"renderer-skia-opengl"` in workspace [`Cargo.toml`](../Cargo.toml) (keeping `"renderer-software"` + `"software-renderer-systemfonts"` for the offscreen test harness).
   - Drops `femtovg`, `i-slint-renderer-femtovg`, and their transitive crates from the build and binary; `try_skia_then_femtovg_then_software` automatically picks `SkiaRendererAdapter::new_opengl` as the sole GPU renderer.
2. **All Hardware Optimizations Included in This Migration:**
   - Include `renderer-skia-opengl` swap, GPU FBO `cache-rendering-hint: true` on the `360×360` `NowPlayingView` cover-art turntable card and static `TopStatusBar` elements, and `TARGET_COVER_SIZE = 360` cover-art sizing alignment in this migration.

---

## 5. Step-by-Step Work Packages

### WP1 — Spike Build & Binary/GLIBC Audit (`aarch64` & `x86_64`)
1. In `/root/pi-bluetooth-speaker-skia-opengl/Cargo.toml`, update `slint` features:
   - Swap `"renderer-femtovg"` → `"renderer-skia-opengl"` (or keep both temporarily if Option B is chosen for A/B benchmarking).
2. Run host `x86_64` check and `aarch64-unknown-linux-gnu` release build:
   ```bash
   cargo build -p screen-manager --target aarch64-unknown-linux-gnu --release --bins
   ```
3. Verify in the build output that `skia-bindings` downloads the prebuilt tarball (`TRYING TO DOWNLOAD AND INSTALL SKIA BINARIES: 0.157.0/30669978c3f39a73e8a7-aarch64-unknown-linux-gnu-ganesh-gl-jpegd-jpege-pdf-vulkan` → `DOWNLOAD AND INSTALL SUCCEEDED`) with zero C++ compilation fallback.
4. Audit the resulting `target/aarch64-unknown-linux-gnu/release/screen-manager` binary with `readelf -V` / `objdump -T` to confirm all required `GLIBC_*` versions are `<= GLIBC_2.41` (Debian 13 Trixie on the Pi 3) and check stripped binary size.

### WP2 — Hardware GPU Optimizations for Skia OpenGL on VideoCore IV
1. **Static Subtree GPU FBO Layer Caching (`cache-rendering-hint: true`):**
   - Add `cache-rendering-hint: true;` to the `360×360` cover-art turntable container in [`services/screen-manager/ui/now_playing.slint`](../services/screen-manager/ui/now_playing.slint) so Skia caches the rounded gradient + border + 360×360 cover art into a GPU FBO texture instead of re-clipping and resampling it on every 60 Hz marquee tick.
   - Evaluate `cache-rendering-hint: true;` on `TopStatusBar` in [`services/screen-manager/ui/status_bar.slint`](../services/screen-manager/ui/status_bar.slint) (or its static child badges) and verify via `SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` that `layers_created` stays flat during playback.
2. **Marquee & Image Fast-Paths:**
   - Verify `ui/marquee.rs` premultiplied `SharedPixelBuffer<Rgba8Pixel>` (`MAX_MARQUEE_STRIP_WIDTH = 2048`) and `ThemePreset::build_ambient_wave_frames` (`64×24` premultiplied sprites) upload cleanly to Skia's `image_cache` and hit the opaque/premul fast-paths in `i-slint-renderer-skia/cached_image.rs`.
3. **Purge Stale Slint Build Fingerprints (per `AGENTS.md` §2.8.12):**
   - Run `rm -rf target/*/{debug,release}/build/screen-manager-* target/*/{debug,release}/.fingerprint/screen-manager-*` after any `.slint` edit before rebuilding.

### WP3 — CI Workflow & Dependency Hardening
1. Update [`.github/workflows/core-ci.yml`](../.github/workflows/core-ci.yml) (`test-rpi3`, `build-rpi3`, and `lint-x86-gated` jobs):
   - Add `libfreetype-dev` and `curl` to the `dpkg -s` and `apt-get install` lines (required by `skia-bindings` link and prebuilt binary download).
   - Update comments from `renderer-femtovg` to `renderer-skia-opengl`.
   - Optionally add `~/.cache/skia-bindings` to the GitHub Actions cache paths so CI runners reuse the downloaded Skia prebuilt archive across runs.

### WP4 — Documentation & `AGENTS.md` Distillation
1. Update [`AGENTS.md`](../AGENTS.md) and [`services/screen-manager/AGENTS.md`](../services/screen-manager/AGENTS.md) (§2.8 items 1, 13, 14, 20):
   - Update the production renderer contract from `FemtoVG` (`Using FemtoVG OpenGL renderer`) to **Skia OpenGL** (`Using Skia OpenGL renderer`).
   - Record the `skia-bindings` prebuilt binary hash/feature-key invariant (never enable ad-hoc `skia-safe` features that break the prebuilt cache key).
   - Record the `OpenGLSurface` `take_snapshot()` behavior and GPU FBO layer-cache (`cache-rendering-hint: true`) sizing rules for VideoCore IV CMA memory.
2. Register `docs/skia_opengl_migration_plan.md` in [`docs/README.md`](README.md) and update comments in `src/ui/backdrop.rs`, `src/ui/shell.rs`, and `ui/now_playing.slint`.

### WP5 — Mandatory Local Pre-Push Gates & Live Raspberry Pi 3 Verification
1. **Run all 4 mandatory local gates in order:**
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo llvm-cov --workspace --target x86_64-unknown-linux-gnu --fail-under-lines 85 --fail-under-functions 85
   cargo llvm-cov -p screen-manager --target x86_64-unknown-linux-gnu --fail-under-lines 90 --fail-under-functions 90
   cargo build --target aarch64-unknown-linux-gnu --release --bins
   ```
2. **Live Hardware Verification on `root@192.168.2.120` (Non-Destructive `/tmp` Protocol):**
   - Copy `target/aarch64-unknown-linux-gnu/release/screen-manager` to `/tmp/screen-manager` on the Pi (and remove `/tmp/screen-manager` immediately after testing).
   - Run `ldd /tmp/screen-manager` on the Pi to verify `libfreetype.so.6`, `libfontconfig.so.1`, `libstdc++.so.6`, `libgbm.so.1`, `libEGL.so.1`, `libGLESv2.so.2`, `libinput.so.10` all resolve cleanly.
   - *(With user permission to briefly pause `screen-manager.service` due to exclusive DRM master)*: Run `/tmp/screen-manager --screen shell --preview 10` (and with `SLINT_DEBUG_PERFORMANCE=refresh_lazy,console`) to verify:
     1. `Using Skia OpenGL renderer` is printed **exactly once** (proving single-window KMS initialization and active Ganesh GLES2 context on `VC4 V3D 2.1`).
     2. Measure idle CPU %, active marquee playback CPU %, and CMA/RSS memory footprint (`htop` / `/proc/<pid>/stat` / `/proc/meminfo` `CmaFree`), comparing directly against the FemtoVG baseline.
     3. Verify visual rendering across Now Playing, marquee scroll, Volume/Codec/Brightness HUD overlays (with blurred backdrop), Settings, and Bluetooth Devices screens.
   - If the user asks to deploy the release binary to `/usr/local/bin`, use [`scripts/deploy_binaries.sh screen-manager`](../scripts/deploy_binaries.sh).

---

## 6. Risk Register & Mitigations

| Risk | Likelihood | Impact | Mitigation |
| :--- | :---: | :---: | :--- |
| `skia-bindings` falls back to C++ source compile if features drift | Low | High (30+ min build failure) | Verified exact `0.157.0` prebuilt URLs (`HTTP 200`) for both `x86_64` and `aarch64`; keep `slint` features strictly to `renderer-skia-opengl`. |
| Host glibc 2.43 symbols leak into `aarch64` binary (`> GLIBC_2.41` on Pi) | Medium | High (Pi refuses to launch binary) | WP1 audits `readelf -V` on the release binary; `speaker_common::glibc_compat!` pins math `.symver` to `GLIBC_2.17`. |
| VideoCore IV CMA / GPU memory pressure from Skia Ganesh atlas + FBO layers | Low | Medium | Keep `cache-rendering-hint` strictly to bounded static controls (`<= 360×360`, total `< 1 MB` VRAM); verify `CmaFree` in `/proc/meminfo` on the Pi. |
| Secondary Slint window creation under KMS (`M-NO-SECONDARY-KMS-WINDOWS`) | Low | High (DRM master / EGL teardown crash) | Existing single-window `MainWindow` architecture and `swash` in-process marquee strip rasterizer (`ui/marquee.rs`) already guarantee zero secondary windows; verify `Using Skia OpenGL renderer` appears only once in logs. |
