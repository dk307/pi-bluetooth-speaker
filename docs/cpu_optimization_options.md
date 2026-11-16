# Screen-Manager CPU Optimization Options (KMS + FemtoVG)

Status: **analysis complete, implementation deferred** — recorded so the work can be
picked up later without re-deriving the measurements.

Target: Raspberry Pi 3 (BCM2837, Cortex-A53), 800×480 panel, Slint `=1.18.1` with
`backend-linuxkms` + `renderer-femtovg` (default) + `renderer-software` (fallback and
offscreen test harness).

---

## 1. Measured baseline (live Pi, `root@192.168.2.120`)

| Scenario | CPU |
|---|---|
| `screen-manager` idle (main) | ≈ 7.9 % |
| `screen-manager` idle (`bt-monitor` thread) | ≈ 2.9 % |
| `screen-manager` while phone playing | **44.1 % sustained (one window — decomposition unresolved)** |
| `kmscube` default | 6.8 % (steady ≈ 5 %) |
| `kmscube -x` | 5.6 % (steady ≈ 3.7 %) |

Supporting facts:

- Hardware GL confirmed: VC4 V3D 2.1, Mesa 26.2.6 — this is not a software-GL problem.
- `wchan` sampling: 29/30 threads in `do_epoll_wait` (no busy-spin).
- **Structural gap:** FemtoVG's `render()` repaints the **entire window** on every dirty
  frame — it has no region/dirty-rect parameter. (Software renderer has partial
  rendering; Skia has a shadow buffer; FemtoVG has neither. `PartialRenderer` in
  `i-slint-core` is referenced by no renderer.)
- Marquee is Option C (live scrolling `Text`, no offscreen strip rasterizer). One shared
  16.65 ms `Timer` gated on `title_marquee.is_overflow`; `step_frames = round(60 /
  marquee_fps)`; `marquee_fps` config already supports 30 (2 px per tick, same speed).

---

## 2. Options

### Option 1 — Pick the backend: `linuxkms-software` vs `linuxkms-femtovg`

**Key fact: this is a runtime switch, no rebuild.** Both renderers are already compiled
in. `SLINT_BACKEND` is parsed by `i-slint-backend-selector`:
`SLINT_BACKEND=linuxkms-software` → `with_renderer_name("software")` →
`SoftwareRendererAdapter` (verified: `calloop_backend.rs:94-107` factory match,
`parse_backend_env_var` splits on `-`).

| Claimed software-renderer side effect | Exposure in *our* UI (grep-verified) |
|---|---|
| `SLINT_KMS_ROTATION` disabled | Never used → no loss |
| `drop-shadow-*` disabled | Zero usages in any `.slint` (`draw_box_shadow` is `// TODO` upstream) → no loss |
| `border-radius` + `clip: true` disabled | **Partially wrong:** software renders rounded *rects* (`scene.rs` `RoundedRectangle` has radius), but `combine_clip()` **ignores the radius** — rounded *clipping* degrades to square. `MarqueeText` clip is square anyway; spot-check the other 3 `clip: true` files (`viewport`, `codec_hud`, `volume_hud`) before switching |
| Non-Western scripts | Inter only, western → no loss |
| Gradients | Supported (`SceneCommand::LinearGradient/Radial/Conic` all handled) |
| Full-screen transition spikes | Real: tab switches / HUD fades become CPU-rasterized 800×480 repaints |

**Win:** software supports **partial rendering** (official Slint docs), so a marquee tick
repaints only the strip rows instead of the whole window, and the Mesa GL overhead
(~5–6.8 %, per kmscube) disappears.

**Risk:** text-quality differences (FemtoVG glyph atlas vs software rasterizer) need a
visual diff of key screens.

**Verification:**
- A/B: stop service briefly, run binary under both `SLINT_BACKEND` values, sample `htop`.
- `SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` on both — idle FPS must be near-zero
  (proves no unnecessary repaints) and marquee-scroll FPS should match `marquee_fps`.

⚠️ Requires briefly stopping `screen-manager.service` (DRM master is exclusive) —
**needs user approval**.

**Assessment: highest-impact option; decide this first — it invalidates or enables
option 2.**

### Option 2 — Cache static subtrees (`cache-rendering-hint`, FemtoVG only)

- **Supported on FemtoVG:** implements `LayerRenderer`
  (`i-slint-renderer-femtovg-1.18.1/itemrenderer.rs:955`) — snapshots subtrees into GPU
  textures (FBO) that are blitted instead of re-tessellated each frame.
- **Not supported on software:** no `LayerRenderer` impl in
  `i-slint-renderer-software-1.18.1` → the hint is a no-op there (docs: "Not all
  rendering backends support this property").
- **Mutually exclusive with Option 1's payoff:** caching only helps under FemtoVG's
  full-redraw model; with software partial rendering it is irrelevant.

Targets: `status_bar`, dock, `settings_view`, VU/CD/DSP tabs — **never** the marquee
`Text`, progress bar, or `AmbientWaveIndicator` (frequent invalidation = constant FBO
re-snapshots).

Side effects: extra VideoCore IV CMA memory; the "Slint auto-downscales/blurs layers
> 2048×2048" claim could **not** be verified in the FemtoVG renderer source — treat as
unverified.

**Verification:** `SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` and the FemtoVG
`RenderingMetricsCollector` (`layers_created` counter, `itemrenderer.rs:983`) — cached
layer count must stay flat while the marquee string changes.

### Option 3 — Drop the marquee tick rate (`marquee_fps: 30`)

**Already implemented; only a Pi config change is pending.** `marquee_fps: 30` →
`step_frames = 2` → 2 px per 33.3 ms at identical on-screen speed. The 16.65 ms timer
keeps firing but only *dirties* every other tick, halving redraws → roughly halves
marquee-driven CPU under FemtoVG.

- Change: `"marquee_fps": 30` in the Pi appliance config. **Awaiting user approval**
  (persistent config change).
- Verification: `htop` while a long title scrolls; expect ~50 % drop in marquee-driven
  CPU (option's claimed range: 50–80 %).

### Option 4 — Decouple string updates from layout/shaping

**Not our bottleneck — not recommended.** Marquee strings change only on *track change*
(infrequent); per-frame cost is scroll-offset dirtiness, which buffering would not
reduce. The rasterize-to-`SharedPixelBuffer` variant is exactly what the Option C
migration deleted (`MarqueeStripRasterizer`); reintroducing it plus a
`fontdue`/`ab_glyph` dependency would undo the migration and add crates for no measured
gain. Buffering updates until the loop wraps would only add message latency.

### Option 5 — Opaque, flat, square-clipped marquee band

**Already satisfied.** `MarqueeText` (`now_playing.slint:12-93`) is `clip: true`, solid
`font_color`, no gradient/shadow/opacity inside the band. Nothing to do.

### Option 6 — Stop timers when idle + LTO build

**Already satisfied on both counts:**
- Marquee timer: `running: title_marquee.is_overflow` — stops completely when text fits
  (`now_playing.slint:228`).
- Wave indicator: `running: root.is_playing`.
- Build: workspace `[profile.release]` is `opt-level = 3`, `lto = "fat"`,
  `codegen-units = 1`, `strip = true` — already stronger than the suggested thin LTO
  (mandated by AGENTS.md).

---

## 3. Recommended order

1. **A/B the software backend (Option 1)** — decides everything else. Biggest structural
   lever: partial rendering vs FemtoVG's full-window redraw. Needs a service-stop
   approval.
2. **Approve `marquee_fps: 30` (Option 3)** — zero code, immediate ~50 % cut in
   marquee-driven redraws regardless of backend.
3. **Only if staying on FemtoVG:** Option 2 (`cache-rendering-hint` on static panels,
   measured via layer metrics).
4. **Resolve the 44.1 % playing-window anomaly** — leading candidates: marquee
   scrolling during playback (long titles overflow → 60 Hz full-window redraws),
   progress-bar position updates, `AmbientWaveIndicator` 180 ms ticks. Isolate with a
   pause/play A-B while sampling per-thread CPU.

Options 4–6 need no action (already done or not applicable).

## 4. Open items / approvals needed from the user

- [ ] Approval to briefly stop `screen-manager.service` for the Option 1 backend A/B.
- [ ] Approval for the `"marquee_fps": 30` appliance config change (Option 3).
- [ ] Play/pause A-B session to decompose the 44.1 % playing window.
