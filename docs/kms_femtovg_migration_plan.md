# KMS + FemtoVG Migration Plan — Slint Backend & GPU Renderer Swap

**Document:** `kms_femtovg_migration_plan.md`
**Target Hardware:** Raspberry Pi 3 Model B (BCM2837, `aarch64`, DietPi / Debian 13 Trixie)
**Language & Runtime:** Rust 2024 Edition (rustc 1.99.1 pinned, `rust-toolchain.toml`)
**GUI Framework:** Slint `=1.18.1` — **`backend-linuxkms` (noseat) + `renderer-femtovg` (GPU OpenGL ES 2.0)**
**Display:** Freenove 4.3" MIPI DSI 800x480 (`vc4-kms-dsi-7inch`, connector `card0-DSI-1`)
**Input:** libinput (touch, via udev `seat0`) + EC12 rotary encoder (gpiod, polled by `slint::Timer`)
**Status:** **Plan — not yet implemented**
**Supersedes:** fbdev `/dev/fb0` rendering path (`src/ui/framebuffer.rs` hardware backend) and the `linuxkms` cargo feature flag.

---

## 1. Motivation & Scope

The Pi has been switched from `fkms`/framebuffer to full **KMS** (`dtoverlay=vc4-kms-v3d` +
`vc4-kms-dsi-7inch` in `/boot/firmware/config.txt`). `/dev/fb0` still exists but only as a
fbdev-emulation shim over KMS — `FBIO_WAITFORVSYNC` is gone, and the custom
`Framebuffer` scanout path is now a dead-end abstraction.

Complex graphics rendering is coming soon, so we migrate **both** in one pass
(user directive: *"Lets switch both. Do not want to spend time doing this twice."*):

1. **Display backend:** custom fbdev `Framebuffer` → Slint's built-in `backend-linuxkms`
   (owns DRM scanout, event loop, and input).
2. **Renderer:** software rasterizer → **FemtoVG GPU renderer** (OpenGL ES 2.0 / GBM on
   VideoCore IV). FemtoVG makes native Slint marquee animation cheap and unblocks the
   upcoming complex-graphics work.

**Skia is explicitly out of scope** — it is a follow-up plan (§10).

### In scope

- Workspace + screen-manager Cargo feature rewire (`compat-1-2` dropped, kms noseat + femtovg enabled).
- New `src/ui/runtime.rs`: single-window, single-event-loop, timer-driven application runtime.
- Splash→shell phase machine inside **one** window (in-window splash overlay).
- Delete `src/ui/input.rs` (touch → libinput), Rust marquee compose path, fbdev hardware backend,
  `run_ui_display_on_path*`, `run_render_loop*`.
- Trim `Framebuffer` in place to an offscreen-only **test harness** (keeps 11 test files compiling).
- Host/CI build dependencies, Pi runtime dependency (`libinput10`, requires user approval).
- Test rework, coverage gates, AGENTS.md §8 rewrite + new distilled rules.

### Out of scope

- Skia renderer (§10 follow-up plan).
- `simulator` feature (winit) — unchanged, keeps working (winit wins the selector chain when enabled).
- systemd unit changes (service already runs `User=root`; no `SLINT_BACKEND` env needed —
  backend selection is compile-time).

---

## 2. Verified Constraints (read from Slint 1.18.1 source)

These are hard constraints discovered by auditing the vendored
`i-slint-backend-linuxkms-1.18.1` crate; they drive the architecture in §3.

| # | Constraint | Evidence |
|---|---|---|
| C1 | **Event loop is single-shot.** `run_event_loop()` takes `user_event_receiver` once; a second call returns `Err("Re-entering the linuxkms event loop is currently not supported")`. | `calloop_backend.rs` `run_event_loop()` |
| C2 | **One window adapter per process lifetime.** `create_window_adapter()` runs the renderer factory (re-opens `/dev/dri/card0`) *before* overwriting `self.window`; two live adapters ⇒ DRM-master conflict. | `calloop_backend.rs` L171–222 (`*self.window.borrow_mut() = Some(adapter.clone())` L221) |
| C3 | **noseat opens DRM directly** with `O_NOCTTY\|O_CLOEXEC` — fine as root, no libseat/logind/seatd needed (Pi's logind is broken, seatd absent). | `calloop_backend.rs` L195–206 |
| C4 | **libinput (no libseat) opens input devices directly** via udev `seat0` — fine as root (`/dev/input/event*` is `root:input`). | kms `input.rs` `LibInput::new_with_udev` + `udev_assign_seat("seat0")` |
| C5 | **libseat is unviable on this Pi** (broken `systemd-logind`, no seatd; `LIBSEAT_BACKEND=noop` makes Slint fail after a 5 s activation timeout). | Pi live probe + seatd-0.9.1 source |
| C6 | **`compat-1-2` exists only to force weak `libseat`+`libinput` deps** — zero `.rs` references anywhere in the Slint tree. Dropping it is how we get libinput *without* libseat. | grep across slint + all `i-slint-*-1.18.1` + `i-slint-compiler` |
| C7 | **Renderer factory chain** = `try_skia_then_femtovg_then_software` → enabling `renderer-femtovg` makes FemtoVG the default; software remains the runtime fallback. | kms `renderer` factory match |
| C8 | **Event loop drives rendering**: each iteration = `update_timers_and_animations()` → invoke queued callbacks → `adapter.render_if_needed()` → `dispatch(next_timer_timeout)`. All per-tick app logic must become `slint::Timer`s or callbacks. | `calloop_backend.rs` loop body |
| C9 | **FemtoVG has no Vulkan path**; Pi 3 VideoCore IV has no Vulkan ICD. GLES2 works (`GL_RENDERER: VC4 V3D 2.1`, Mesa 26.2.6, verified by EGL smoke test on the Pi). | Pi live probe + `/usr/share/vulkan/icd.d/` empty |

---

## 3. Target Architecture

### 3.1 One window, one event loop, phase state machine

```text
run_ui_display_with_config(config, screen)
 └─ headless? (SPEAKER_CORE_HEADLESS=1) → skip UI (unchanged)
 └─ ui::runtime::run(config, screen)
      │
      ├─ create THE window  (exactly one, for the whole process lifetime)
      │    • ScreenTarget::Shell    → MainWindow, splash overlay hidden
      │    • ScreenTarget::Splash   → SplashScreen (standalone preview window)
      │    • ScreenTarget::Appliance→ MainWindow, splash overlay visible (boot phase)
      │
      ├─ spawn background workers (unchanged code)
      │    • bt-monitor worker  → Arc<Mutex<BtSharedUpdate>>
      │    • amp-volume worker  → AmpVolumeCommandQueue  (mocked = false — hardware panel)
      │    • boot-checks worker (Appliance/Splash) → mpsc<BootCheckUpdate>
      │
      ├─ install slint::Timers  (the app's "render loop")
      │    • ~16–33 ms: apply_shared_bt_state + poll_encoder_input   (when shell phase)
      │    • ~16 ms:    boot-phase drain (drain_boot_updates → overlay.current_check,
      │                 deadline = spawn + MAX_SPLASH_WAIT, checks_done → transition)
      │    • preview_secs: one-shot quit timer → EventLoopProxy::quit_event_loop()
      │
      └─ slint::run_event_loop()          ← called exactly once (C1)
           loop: timers/anim → callbacks → render_if_needed → dispatch(timeout)
      on return: report timing, join workers (resume_unwind on panic, M-PANIC-ON-BUG)
```

**Phase machine (Appliance only):**

```text
Phase::Splash { boot_rx, deadline }
    │ checks_done | deadline reached
    ▼
Phase::Shell  → hide splash overlay, finish_boot_worker(), shell timers active
```

No second window is ever created (C2). The standalone `ScreenTarget::Splash` preview
uses its own single `SplashScreen` window and quits via the preview timer.

### 3.2 Splash as an in-window overlay

`SplashScreen` currently `inherits Window` and cannot be nested. Extract its body into a
non-Window component:

```text
splash.slint
  export component SplashContent inherits Rectangle { …CheckRows, EqBars, current_check… }
  export component SplashScreen  inherits Window  { SplashContent { } }   // standalone preview only

appwindow.slint
  MainWindow
    in-out property <bool> boot_splash_visible: false;
    …existing bg_column / hud layers…
    if (root.boot_splash_visible): SplashContent { current_check: … }   // top-most overlay
```

`EqBars` stays frame-clock driven (animation ticks run inside the KMS loop — no
perpetual external animator needed).

### 3.3 Input: libinput (touch) + gpiod timer poll (encoder)

- **Touch:** deleted `TouchInputDevice` (custom evdev + calibration) is replaced by the
  backend's libinput handler — Slint pointer/touch events arrive natively in the event
  loop. Production never used `TouchCalibration` (defaults were identity), so libinput's
  default mapping matches 1:1. No calibration code survives.
- **Encoder:** `speaker_hardware::EncoderDevice::new()` (gpiod) is display-independent.
  `EncoderVolumeTracker::poll_and_apply` moves to a `slint::Timer` (~33 ms).

### 3.4 Marquee: live-text scroll (Option C — strip rasterizer DELETED)

**Decision (final, live-Pi tested): Option C — delete `MarqueeStripRasterizer`
entirely; the marquee scrolls live `Text` elements.** The original draft of this
section ("keep the strip rasterizer, only delete the Rust composer") was wrong: it
hard-crashed the appliance on every boot.

**SEGV root cause (diagnosed from the core dump):** under `backend-linuxkms` *any*
secondary Slint component eagerly creates a real DRM/GL window (the boot log shows a
second `Using FemtoVG OpenGL renderer` pair ~550 ms after start). The rasterizer
component was dropped inside `new()` (WARN: `Marquee strip rasterizer unavailable
(title): … window was not created by the software platform`); a second attempt ~550 ms
later crashed during the `Err`-return drop — Mesa EGL teardown: `FontCache` drop →
`driUnbindContext` → atomic CAS on NULL+8 (`si_addr=0x8`, pc in libgallium LSE stub) →
`status=11/SEGV`, service restart counter reached 67. The offscreen strip machinery
(`LAST_SECONDARY_WINDOW`, `take_last_secondary_window`) existed only to route that
secondary window back to the rasterizer and could not prevent the teardown crash.

**What shipped instead:**
- `MarqueeText` (`ui/now_playing.slint`) scrolls live `Text` copies (primary +
  wrap-around) driven by the shared integer `MarqueeCompose.clock` (one 16.65 ms
  `Timer` in `NowPlayingView`; `step_frames` maps 30/60 fps to identical on-screen
  speed). FemtoVG caches glyph shapes in its atlas, so per-frame cost is a GPU blit of
  the dirty marquee rows — no CPU re-shaping, no offscreen rasterization.
- Deleted: `MarqueeStripRasterizer` / `MarqueeStripCache` / `MarqueeTypography` /
  `StripRequest`, `ui/marquee_rasterizer.slint`, the `title_marquee_strip` properties
  and bindings (`appwindow` / `viewport` / `NowPlayingView`),
  `refresh_marquee_strips*` + the strip cache field in `shell.rs`,
  `publish_composer_state()` + strip/geometry `changed` handlers, and
  `LAST_SECONDARY_WINDOW` / `take_last_secondary_window` (`framebuffer.rs`).
  Verified: no compose/rasterizer symbols in source or in the release binary
  (`strings … | grep -iE 'MarqueeStrip|compose_in_rust|…'` → empty).
- Kept: `MarqueeCompose.compose_in_rust = false` (bluetooth-session test invariant),
  `marquee_fps`, integer-frame `wrap_visible` arithmetic (item 16 churn rule),
  `changed track_title => { marquee_clock = 0; }` scroll restart, `RgbaPremulPixel`
  (offscreen test-harness pixel type), and the `FramebufferPlatform` secondary-window
  fallback (now a fresh `MinimalSoftwareWindow`, covered by
  `framebuffer_platform_secondary_window_is_independent_of_primary`).
- Distilled rule: **never create secondary Slint components in production under KMS**
  (root AGENTS §4.3; service AGENTS item 20).

### 3.5 Renderer selection

Workspace enables `renderer-femtovg` + `renderer-software` (fallback + offscreen test
harness). The kms factory chain picks FemtoVG automatically. Optional hardening: pin
`SLINT_BACKEND=linuxkms-femtovg` in the service unit so a GL init failure fails loudly
instead of silently dropping to software — decide during live testing (§8).

FemtoVG fonts: UI `.slint` files **embed Inter TTFs directly**
(`import "../../../assets/fonts/Inter-*.ttf"`), so primary text needs no system fonts;
fontique (fontconfig) provides fallback glyphs only. `software-renderer-systemfonts`
stays for the software renderer/test harness.

---

## 4. Cargo Feature Configuration

**Workspace `Cargo.toml`** (single source of truth):

```toml
slint = { version = "=1.18.1", default-features = false,
          features = ["std",
                      "compat-1-18",               # required when default-features = false
                      "renderer-femtovg",          # GPU renderer (default of kms factory chain)
                      "renderer-software",         # runtime fallback + offscreen test harness
                      "software-renderer-systemfonts",
                      "backend-linuxkms",          # DRM/KMS display
                      "backend-linuxkms-noseat"] } # libinput WITHOUT libseat (C3/C5/C6)
# removed: "compat-1-2"
```

**`services/screen-manager/Cargo.toml`:**

- Remove the unused `linuxkms` feature (backend is now unconditional via the workspace).
- `ui`, `simulator`, `test-util` unchanged. `simulator` adds `backend-winit`, which wins
  the selector chain (`qt → winit → linuxkms`) for desktop builds — unchanged behavior.

---

## 5. Build & Runtime Dependencies

| Environment | Action | Packages |
|---|---|---|
| Host x86_64 (coverage/clippy) | **Install** (missing today) | `libinput-dev libxkbcommon-dev libgbm-dev libegl-dev libgles2-dev libudev-dev` |
| Host aarch64 cross (arm64 multiarch) | **Install** (partially present) | same list as `:arm64` ( `libinput-dev:arm64` already installed; add `libxkbcommon-dev:arm64 libgbm-dev:arm64 libegl-dev:arm64 libgles2-dev:arm64 libudev-dev:arm64`) |
| CI `.github/workflows/core-ci.yml` | **Edit** 3× apt-get lines (L33/96/165 blocks) | add `libinput-dev libxkbcommon-dev libgbm-dev libegl-dev libgles2-dev libudev-dev` |
| Pi runtime (root@192.168.2.120) | **Ask user first** (persistent `apt install`) | `libinput10` — `libinput.so.10` is MISSING on the Pi; everything else (`libxkbcommon.so.0`, `libgbm.so.1`, `libEGL/libGLESv2`, `libfontconfig`) already present. `libseat1` NOT needed (noseat). |

`.cargo/config.toml` PKG_CONFIG vars already route aarch64 builds to
`/usr/lib/aarch64-linux-gnu/pkgconfig` — no change.

**Verification:** `pkg-config --exists libinput xkbcommon gbm egl udev` on both host
architectures after install; `ldd /tmp/screen-manager` on the Pi after deploy shows
`libinput.so.10` resolved.

---

## 6. Work Packages

Ordered so the tree compiles after each package where practical.

### WP1 — Dependencies & feature rewire
1. Install host packages (§5), edit workspace + screen-manager Cargo.toml (§4), edit CI (§5).
2. Gate: `cargo check -p screen-manager --target x86_64-unknown-linux-gnu` (fbdev code still
   compiles — it calls `set_platform`, which overrides the default backend).

### WP2 — `.slint` splash overlay extraction
1. `splash.slint`: extract body → `SplashContent inherits Rectangle`; `SplashScreen` = thin
   `Window` wrapper (kept for `ScreenTarget::Splash` preview).
2. `appwindow.slint`: add `boot_splash_visible` property + conditional `SplashContent`
   overlay (top-most layer, after `hud_scrim`/HUD overlays).
3. `theme.rs` `apply_splash`: retarget to the new content component's palette bindings.
4. Gate: `cargo build -p screen-manager` (slint-build AOT compile).

### WP3 — Trim `Framebuffer` to offscreen test harness
Delete from `src/ui/framebuffer.rs`:
- `open()`, device/sysfs/scanout paths, `ScanoutSync::{CharDevice,FileMapped}`,
  sysfs unblank (`unblank`, `unblank_count`), `FrameTiming` vsync telemetry,
  `Backend::Hardware` vs `Mock` split (mock-only), `is_hardware_panel()`,
  `MarqueeOverlay`, `MarqueeRowSpec`, `set_marquee_rows`, `install_marquee_backgrounds`.

Keep (test harness core):
- Shadow buffer + `Bgrx8888Pixel` (`TargetPixel`), `SoftwareRenderer` `render_frame` /
  `render_frame_full_redraw`, `capture_region`, `render_count`, `init_platform`
  (thread-local offscreen platform), `new_mocked()` + `FramebufferMockCtrl` (fault
  injection, `pixels` accessors).

Update `ui/mod.rs` exports (drop `MarqueeRowSpec`; keep `Framebuffer*`, `FrameTiming` only
if still referenced). Adjust `ui_framebuffer_integration.rs` + any test touching deleted
methods (`unblank_count` ×10, `set_marquee_rows` ×10, `install_marquee_backgrounds` ×6,
`is_hardware_panel` ×1–2).
Gate: `cargo test -p screen-manager --target x86_64-unknown-linux-gnu` (most tests green;
daemon/marquee/input still pending WPs).

### WP4 — Delete input.rs + Rust marquee compose + legacy daemon loops
1. Delete `src/ui/input.rs`, `tests/ui_input_integration.rs`, `ui/mod.rs` input exports.
2. Delete Rust marquee compose (§3.4): `sync_marquee_overlay`, `daemon.rs:1595`
   `set_marquee_compose_in_rust(true)`, shell.rs compose methods, framebuffer leftovers
   not already removed in WP3.
3. Delete `run_ui_display_on_path_with_config`, `run_render_loop*`, `pump_splash_frames`,
   `unblank_and_init_platform`, `render_initial_frame`, `idle_sleep`, `FrameLoopTiming`
   (superseded by the event loop; keep `apply_shared_bt_state`, `poll_encoder_input`,
   `init_shell_with_config`, `init_splash_screen` (retargeted), `drain_boot_updates`,
   `finish_boot_worker`, worker spawn/join helpers as-is).
4. Gate: `cargo check --all-targets`.

### WP5 — `src/ui/runtime.rs` (the new app runtime) + daemon wiring
1. Implement §3.1: window creation per `ScreenTarget`, worker spawn, `slint::Timer`s,
   `Phase` state machine (as a pure, unit-testable type: `on_tick(...) -> Option<Transition>`),
   `slint::run_event_loop()`, quit via `EventLoopProxy` for preview, worker join with
   `resume_unwind`.
2. `daemon.rs::run_ui_display_with_config` → headless check → `ui::runtime::run(...)`.
   KMS is the only production path; the trimmed `Framebuffer` is test-only.
3. Structured logging per AGENTS §4.6: boot config at `info!`, phase transition
   (`splash → shell`) at `info!`, transient bt/amp retries at `warn!`, timer ticks at `debug!`.
4. Gate: `cargo check --all-targets`, `cargo test -p screen-manager`.

### WP6 — Test rework
1. `daemon_integration.rs` (only caller of deleted `run_*` fns): retarget to the new
   entry points — exercise `Phase` state machine, `init_shell_with_config`,
   `apply_shared_bt_state`, `poll_encoder_input`, quit-timer logic directly (no blocking
   event loop in tests).
2. `marquee_strip_integration.rs` (10) + `marquee_full_redraw_diagnostic.rs` (4): delete
   compose-path assertions; rewrite surviving ones to native-animation assertions
   (`compose_in_rust=false`, set `MarqueeCompose.clock`, `render_frame`, `capture_region`
   pixel-diff proves the strip scrolled).
3. `boot_splash_integration.rs`: retarget to `SplashContent` overlay + `boot_splash_visible`.
4. Delete: `ui_input_integration.rs` (WP4).
5. Gate: `cargo llvm-cov -p screen-manager --target x86_64-unknown-linux-gnu
   --fail-under-lines 90 --fail-under-functions 90` (service AGENTS 90/90; root AGENTS
   85/85 workspace-wide).

### WP7 — Docs & AGENTS distillation (§4.3 rule)
1. Add this plan to `docs/README.md` table.
2. Rewrite `AGENTS.md` §8 (fbdev rules → KMS rules) and any fbdev-specific lines in
   `services/screen-manager/AGENTS.md`.
3. Distill new rules into AGENTS §4.3, e.g.:
   - *Single window + single event loop per process for `backend-linuxkms`* (C1/C2) —
     splash/boot phases are in-window overlays, never a second window.
   - *noseat over libseat on appliances with broken logind* (C3/C5) — `backend-linuxkms-noseat`.
   - *Per-tick app logic belongs in `slint::Timer`s, never a manual render loop* (C8).
   - *`compat-1-2` must stay dropped while kms is enabled* (C6).

### WP8 — Gates & live Pi test
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo llvm-cov --workspace --target x86_64-unknown-linux-gnu --fail-under-lines 85 --fail-under-functions 85`
   (+ 90/90 for `-p screen-manager`)
4. `cargo build --target aarch64-unknown-linux-gnu --release --bins`
5. Live Pi (non-destructive, `/tmp` only):
   - **Ask user** before `apt install libinput10` (persistent).
   - `scp` release binary → `/tmp/screen-manager`, run `--help`, then a display run.
   - Display run needs the running `screen-manager.service` paused (DRM master conflict) —
     **ask user** before touching the service.
   - Verify: FemtoVG renderer active (not silent software fallback), touch works via
     libinput, encoder volume works, marquee scrolls, splash→shell transition, bt/amp
     workers healthy, `journalctl` log lines present.
6. Deploy via `scripts/deploy_binaries.sh` only if the user asks.

---

## 7. Risk Register

| Risk | Mitigation |
|---|---|
| GL init fails on Pi → silent software fallback (slow, defeats the purpose) | Pin `SLINT_BACKEND=linuxkms-femtovg` in unit for fail-loud; verify renderer identity during live test (§8) |
| FemtoVG + fontique fallback fonts differ from software renderer | Primary Inter fonts are embedded; visual-diff key screens during live test |
| Coverage drops below 90/90 after deleting test-covered code | WP6 rework; deleted tests are replaced by native-animation + phase-machine tests |
| Host/CI missing new -dev packages break gates | WP1 installs host pkgs first; CI edit lands in same PR |
| `libinput10` absent on Pi → runtime link failure | WP8 `ldd` check; `apt install libinput10` with user approval |
| Event loop re-entry / second window accidentally introduced later | AGENTS distilled rule (WP7) + `Phase` machine makes the single-window invariant structural |

---

## 8. Acceptance Criteria

- [x] Production binary renders via KMS (`/dev/dri/card0`), no fbdev scanout, no `/dev/fb0` open. *(verified 2026-11-15: no `/dev/fb0` reference remains in `src/`; boot log shows the KMS/FemtoVG path)*
- [x] FemtoVG is the active renderer on the Pi. *(journal: `Using FemtoVG OpenGL renderer`, printed once — a second pair would mean a stray secondary window, §3.4)*
- [ ] Touch works through libinput; no `input.rs` in the tree. *(`input.rs` deleted; live touch verification deferred to the user's follow-up check)*
- [x] Splash boot-checks → shell transition happens inside one window with no flicker/black gap. *(journal: `Appliance event loop starting` → `Boot checks settled; splash overlay hidden`; UI confirmed on panel)*
- [x] Marquee scrolls natively (no Rust compose symbols left in the binary). *(`strings` + source grep clean; Option-C live text, §3.4)*
- [x] `fmt` + `clippy -D warnings` + coverage gates (85/85 workspace, 90/90 screen-manager) + aarch64 release build all pass. *(green 2026-11-15: 87.85%/86.01% workspace, 90.33%/93.70% screen-manager)*
- [x] AGENTS rewritten: root §4.3 KMS rules distilled; `services/screen-manager/AGENTS.md` fbdev/marquee/compose items replaced (WP7).
- [x] `docs/README.md` lists this plan.

---

## 9. Explicit Non-Goals

- No libseat / seatd / logind fixes (noseat makes them irrelevant).
- No touch calibration UI/code (identity mapping, as production already ran).
- No `simulator` feature changes.
- No Skia in this migration (§10).

---

## 10. Follow-up Plan: Skia Renderer (future, NOT this PR)

**Trigger:** if FemtoVG's vector quality/perf proves insufficient for the complex-graphics
roadmap, or Skia features (e.g. richer text/effects) become requirements.

**Effort:** half-day **contingent on one spike build**; code-level swap is a Cargo-feature
one-liner (`renderer-femtovg` → `renderer-skia-opengl`) with **zero** app/.slint/event-loop
changes — both features can even be compiled together for a runtime flip
(`SLINT_BACKEND=linuxkms-skia-opengl`).

**Constraints to respect when scheduled:**
- Pi 3 has **no Vulkan** (C9) → only `renderer-skia-opengl` (GLES2) is viable.
- rust-skia prebuilt `aarch64-unknown-linux-gnu` binaries (~16–24 MB) exist but the
  feature-set hash must match exactly, else cargo silently falls back to a source build
  (clang/depot_tools/`CLANGCC`/`SDKTARGETSYSROOT` — painful to cross-compile).
  **First task = spike:** `cargo build -p screen-manager --target aarch64-unknown-linux-gnu
  --release` with `renderer-skia-opengl`; confirm the prebuilt binary is fetched (network +
  `curl` needed in CI) before committing to the swap.
- Heavier binary + RAM on the 1 GB Pi 3; glibc ≥ 2.31 OK (Debian 13).
- Write as a separate doc (e.g. `docs/skia_renderer_followup.md`) when scheduled.
