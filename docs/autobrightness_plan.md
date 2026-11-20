# Auto-Brightness + Manual Brightness Control — Implementation Plan

Status: **planning only** — no implementation code has been written.
Branch/worktree: `feat/auto-brightness` at `4678ced` (= `origin/main`), worktree
`/root/pi-bluetooth-speaker-auto-brightness`.
Date: 2026-11-20.

**Confirmed decisions (2026-11-20):** (1) BH1750-only sensor backend — no IIO driver;
(2) the existing top-bar brightness button opens the brightness overlay;
(3) D-Bus single-writer daemon; (4) overlay contains slider + AUTO toggle + lux readout.

---

## 0. Scope and deliverable

Implement a phone-like auto-brightness daemon (`autobright`) that:

1. Computes a smoothed, hysteretic, asymmetric-debounced/slewed target brightness from
   ambient lux, with a phone-style **manual bias** (gamma) so that a user-set brightness
   shifts the whole auto curve instead of being overwritten on the next lux change.
2. Reads the appliance's ambient sensor and writes the display backlight through Linux
   sysfs, detecting external writes (DE slider, `brightnessctl`, hotkeys) as manual
   overrides.
3. Exposes a D-Bus control plane so the **brightness overlay**, opened from the brightness
   button in the Now Playing top bar, can set manual brightness and toggle auto mode
   without a second process fighting over the same sysfs node.

The deliverable is split into two PRs to contain blast radius:

| PR | Contents | Why separate |
|---|---|---|
| **PR 1 — core + daemon** | `speaker-hardware` drivers/algorithm, `services/autobright`, tests, deploy/CI/systemd wiring, optional diagnostic CLI | Verifiable headlessly and on the Pi without touching the UI |
| **PR 2 — UI overlay** | `brightness_hud.slint`, `appwindow.slint`, `status_bar.slint`, `AppShell` D-Bus client, `CoreConfig`, settings catalog, UI tests | Depends on PR 1's D-Bus contract; keeps screen-manager gate isolated |

---

## 1. Findings that shape the design (verified in this tree)

| Fact | Evidence | Consequence |
|---|---|---|
| Top bar already has a **brightness icon button** wired to `night_mode_clicked()`, whose Rust handler only logs | `services/screen-manager/ui/status_bar.slint:216-243`; `ui/appwindow.slint:242,337`; `src/ui/shell.rs:1553-1555` | Repurpose this button to open the brightness overlay; no new icon needed (`assets/icons/brightness.svg` exists) |
| `dim_active` is declared but never set | `status_bar.slint:50-51`; no `set_dim_active` repo-wide | Bind it to real state (auto/manual or night mode) |
| **Volume HUD** is the exact overlay precedent (visibility/opacity/timeout/focus/backdrop/mutual exclusion) | `ui/volume_hud.slint:9-49,72-84,145-220`; `ui/appwindow.slint:60-64,132-146,171-181,474-487` | Clone its structure for `BrightnessHudOverlay`; share the slider and HUD timer instead of copy-pasting |
| No brightness overlay exists | case-insensitive `brightness` search across `.slint`/`.rs` → only the icon hit | Greenfield component |
| Ambient sensor is a **BH1750 on I2C-1 @0x23**, already wrapped in `speaker_hardware::LightSensor` with `new_mocked()` | `crates/hardware/src/light.rs:1,33-42,768-935`; `README.md:91` | Production sensor path is rppal I2C, **not IIO** |
| Backlight is kernel sysfs `rpi_backlight`, range `0..=255` | `docs/rpi_audio_bluetooth_report.md:70,261`; `docs/ui_architecture_plan_rust.md:955-960`; `baseline/scripts/freenove/display_info.py:40-55`; `baseline/verify.sh:70` | `/sys/class/backlight/rpi_backlight/{brightness,max_brightness,actual_brightness,bl_power}` |
| **No Rust code writes brightness today**; no D-Bus brightness service | exhaustive `*.rs` search; `CoreConfig` has no brightness field | New driver + daemon are greenfield; no conflict to resolve |
| `screen-manager` renders on one thread and must never block on D-Bus | `services/screen-manager/AGENTS.md:113`; `src/ui/runtime.rs:112-121,310-334` | Brightness D-Bus I/O goes to a worker thread; results drained on the existing 50 ms sync timer |
| Tests must live under `tests/`, not `src/`; hardware needs `new_mocked() -> (Self, MockCtrl)` | root `AGENTS.md` §3/§4.3; `screen-manager/AGENTS.md` items 1, 7 | Sample code's inline `#[cfg(test)] mod tests` must be moved to `tests/`; add getters for assertions |
| Toolchain pinned to 1.99.1; host coverage uses `--target x86_64-unknown-linux-gnu` | `rust-toolchain.toml:6`; root `AGENTS.md` §4.4 | Use exact gate commands below |

### 1.1 Mandatory ecosystem-reuse gate — completed

Searched: `cargo search "auto brightness"`, `"backlight"`, `"illuminance"`,
`"ambient light sensor"`, `"exp moving average"`, `"debounce"`; inspected vendored sources of
`autolux 0.1.2`, `backlight 0.1.1`, `backlight_control_rs 1.0.0`,
`displays_physical_linux_sys 0.1.0`.

| Candidate | Decision | Concrete reason |
|---|---|---|
| `autolux 0.1.2` | **Reject as dependency; borrow ideas** | BlueOak-1.0.0 license (nonstandard for this Apache-2.0 repo); discovers sensors only through `/sys/bus/iio` (`src/sensor.rs`) which does not exist on this BH1750 board; uses logind `SetBrightness` (no seated session for a system service); pulls `zbus 6.1.0` (workspace pins 5.19.0) + `clap`/`miette`; its smoother has no manual gamma bias or asymmetric hysteresis/debounce. Its log-curve + dt-bounded fade + external-panel-read ideas are worth mirroring. |
| `backlight 0.1.1` | **Reject** | Hardcoded `intel_backlight` default, no injectable sysfs root, `try!`-era error style, returns `bool`, `Default` impl — violates mockability/canonical-error rules. |
| `backlight_control_rs 1.0.0` | **Reject** | CLI-oriented, no injectable backend/mock seam, no canonical error with `source()`. |
| `displays_physical_linux_sys 0.1.0` | **Reject for now** | Generic LED+backlight manager (0.1.0, new), no per-driver mock controller paired with its state, adds a broad device model for a single known device; our ~60-line sysfs adapter is simpler and fully testable. Revisit if a second backlight device appears. |

**Conclusion:** implement the policy in-house (it is a product-specific control policy supplied
by the requirement), but reuse the kernel sysfs interface (reuse hierarchy level 1) and the
existing `bh1750-embedded`-backed `LightSensor` (level 2).

---

## 2. Architecture

```
  ┌──────────────── screen-manager (UI thread) ────────────────┐
  │ Top bar brightness button → show_brightness_hud()          │
  │ BrightnessHudOverlay: slider, AUTO toggle, lux readout     │
  │ on_brightness_changed / on_auto_toggled → command queue    │
  └───────────────┬──────────────────────────▲─────────────────┘
                  │ zbus command (worker)    │ state deltas (channel → 50 ms timer)
                  ▼                          │
  ┌──────────────── autobright daemon ───────┴─────────────────┐
  │ D-Bus: org.speaker.Autobright1 /org/speaker/autobright     │
  │ AutoBrightness pure state machine (PR 1)                    │
  │ external-write detector (last_written vs hw brightness)    │
  │ 50 ms tick: read lux → update() → write duty×max           │
  └───────┬───────────────────────────────┬────────────────────┘
          │ read lux                      │ read/write sysfs
          ▼                               ▼
  speaker_hardware::LightSensor    speaker_hardware::Backlight
  (BH1750 @ I2C-1 0x23)            /sys/class/backlight/rpi_backlight
```

No IIO backend will be added: the requirement's `/sys/bus/iio/devices/...` path does not exist
on this appliance, and the confirmed decision is to use the existing BH1750 driver only.

**Single-writer invariant:** `autobright` is the only process that writes backlight
brightness in normal operation. The UI never writes sysfs directly; it sends D-Bus commands.
External writes are still detected (requirement 2) and logged, but are treated as a manual
override rather than a reason to start a second writer.

---

## 3. PR 1 — core algorithm, hardware adapters, daemon

### 3.1 `crates/hardware/src/brightness.rs` (new, pure algorithm)

Public API (production code only; no `#[cfg(test)]` in `src/`):

```rust
//! Phone-like ambient brightness policy with hysteresis and manual gamma bias.

pub const DEFAULT_CONFIG: Config = Config { /* values from requirement */ };

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub ema_alpha: f32,      // 0.2
    pub hyst_up: f32,        // 0.25
    pub hyst_down: f32,      // 0.30
    pub debounce_up: f32,    // 1.5 s
    pub debounce_down: f32,  // 5.0 s
    pub rate_up: f32,        // 0.40 /s
    pub rate_down: f32,      // 0.15 /s
    pub b_min: f32,          // 0.05
    pub gamma_min: f32,      // 0.30
    pub gamma_max: f32,      // 3.00
    pub pwm_gamma: f32,      // 2.20 (set 1.0 if panel already applies perceptual curve)
}

impl Config {
    pub const fn validate(self) -> Result<(), ConfigError>; // finite, 0<ema_alpha<=1,
    // 0<b_min<1, 0<gamma_min<=gamma_max, rates>0, debounce>0, pwm_gamma>0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingDir { None, Up, Down }

pub struct AutoBrightness { /* cfg, lux_ema, lux_anchor, b_current, b_target,
                               manual_gamma, pending_dir, pending_elapsed */ }

impl AutoBrightness {
    pub fn new(initial_lux: f32, cfg: Config) -> Self;
    pub fn base_curve(&self, lux: f32) -> f32;                 // 0.103*ln(lux+1)+b_min, clamped
    pub fn target_from_lux(&self, lux: f32) -> f32;            // base^manual_gamma, clamped
    pub fn on_manual_change(&mut self, user_b: f32);           // gamma = ln(user_b)/ln(base)
    pub fn reset_manual_bias(&mut self);
    pub fn update(&mut self, raw_lux: f32, dt_secs: f32) -> f32; // returns hardware duty
    // Read-only accessors for integration tests and telemetry:
    pub fn brightness(&self) -> f32;      // b_current (perceptual)
    pub fn target(&self) -> f32;          // b_target
    pub fn manual_gamma(&self) -> f32;
    pub fn lux_ema(&self) -> f32;
    pub fn lux_anchor(&self) -> f32;
    pub fn config(&self) -> Config;
}
```

Behavior details (from the requirement, tightened per repo rules):

- `dt = if dt_secs.is_finite() { dt_secs.clamp(0.0, 0.5) } else { 0.0 }`; `raw_lux` finite
  guard (`NaN`/∞ → treat as dark/unchanged, never poison `lux_ema`).
- `base_curve` clamps lux to `>= 0` and result to `[b_min, 1.0]`.
- Hysteresis reference is `lux_anchor.max(2.0)` so near-zero lux cannot chatter.
- Debounce: up/down thresholds from `hyst_up`/`hyst_down`; pending direction resets when
  lux returns to the deadband; on expiry commit `lux_anchor`, recompute `b_target`, clear pending.
- Ramp: `step = rate * dt`, `rate_up` when rising, `rate_down` when falling; `diff.clamp(-step, step)`.
- `on_manual_change`: clamp `user_b` to `[b_min, 1.0]`; `base = base_curve(lux_ema)`; if
  `base > 1e-6 && |base - 1.0| > 1e-6 && base.is_finite()` then
  `manual_gamma = (ln(user_b) / ln(base)).clamp(gamma_min, gamma_max)`, else leave/reset to `1.0`;
  set `b_current = b_target = user_b`, clear pending state immediately.
- `reset_manual_bias`: `manual_gamma = 1.0`, `b_target = target_from_lux(lux_ema)`
  (ramp remains smooth; do not snap `b_current`).
- `update` returns `b_current.powf(pwm_gamma) ∈ [b_min^pwm_gamma, 1.0]`.

`ConfigError` is a small canonical struct (kind + backtrace, `summary()` via
`impl_canonical_error_display!`), or a plain `thiserror` enum if no backtrace is warranted —
decide against the nearest sibling (`speaker_hardware::light::LightError`) for consistency.

### 3.2 `crates/hardware/src/backlight.rs` (new, sysfs adapter)

```rust
pub const BACKLIGHT_SYSFS_BASE: &str = "/sys/class/backlight";
pub const DEFAULT_BACKLIGHT_DEVICE: &str = "rpi_backlight";

pub struct Backlight { /* base: PathBuf, brightness: PathBuf, max_brightness: u32,
                          backend: ... */ }
pub struct BacklightMockCtrl { /* Arc<Mutex<MockState>>: raw, max, read_err, write_err, counters */ }

impl Backlight {
    pub fn new() -> Result<Self, BacklightError>;                       // default device
    pub fn with_device(device: &str) -> Result<Self, BacklightError>;
    pub fn with_root(root: impl AsRef<Path>, device: &str) -> Result<Self, BacklightError>;
    pub fn new_mocked() -> (Self, BacklightMockCtrl);
    pub fn max_brightness(&self) -> u32;
    pub fn read(&self) -> Result<u32, BacklightError>;
    pub fn write(&self, raw: u32) -> Result<(), BacklightError>;        // clamps to max
    pub async fn read_async(&self) -> Result<u32, BacklightError>;      // run_blocking
    pub async fn write_async(&self, raw: u32) -> Result<(), BacklightError>;
}
```

- Canonical `BacklightError` with typed `source: io::Error`, `Error::source()`, `summary()`,
  predicates (`is_not_found`, `is_permission_denied`, `is_invalid_data`, `is_mock_fault`).
- Mock mutation/inspection **only** on `BacklightMockCtrl` (never duplicate setters on the
  driver); preserve injected errors via `clone()`, not stringification.
- Parse `u32` with `map_err` into invalid-data kind; no `.unwrap()` on I/O paths.
- Re-export from `crates/hardware/src/lib.rs` beside `light`/`temp` with `#[doc(inline)] pub use`.

### 3.3 Ambient sensor — BH1750 only (decision confirmed)

Use `speaker_hardware::LightSensor` directly (`read_lux()` / `read_lux_async()`,
`new_mocked()` for tests, BH1750 at I2C-1 `0x23`, 180 ms datasheet settle already enforced
inside the driver). **No `light_iio.rs` module, no `--sensor` flag, no IIO sysfs path** — the
`in_illuminance_input` / `in_illuminance_raw * scale` code from the sample is intentionally
omitted because this board has no IIO device. The lux semantics from the requirement are
preserved at the `AutoBrightness` boundary (finite, non-negative lux in).

### 3.4 `tools/diagnostics/src/bin/backlight-info.rs` (new, optional-but-recommended)

Required by the three-fold-purpose rule for `speaker_hardware` drivers. Mirrors `light-read.rs`:
prints device, raw/max/percent, `bl_power`; `--json` NDJSON; CLI tests in
`tools/diagnostics/tests/cli_tests.rs`.

### 3.5 `services/autobright` (new crate)

Follow the `bt-manager` template: edition 2024, `version/authors/license.workspace = true`,
`publish = false`, explicit `[[bin]]` + `default-run`, `[features] test-util = []` with self
dev-dependency, `[lints] workspace = true`. Add to root `Cargo.toml` workspace members.

| File | Responsibility |
|---|---|
| `src/lib.rs` | Module root + single-item-path re-exports; `glibc_compat!()` only in `main.rs` |
| `src/main.rs` | Thin entry: `glibc_compat!()`, `init_daemon_logging()`, panic hook, delegate to `daemon::main()` |
| `src/config.rs` | CLI (`--backlight-device`, `--tick-ms`, `--idle-reset-secs`, `--config`, `--duration-secs` for bounded live probes), env `RUST_LOG`, config path via `speaker_common::diag`; log **full resolved config at `info!`** (§4.6) |
| `src/error.rs` | Canonical `AutobrightError` (kind + `source` + `Backtrace`), `summary()`, predicates |
| `src/daemon.rs` | Main loop, tick timing, external-write detector, manual command application, idle reset, `sd_notify` READY, `ctrlc` shutdown |
| `src/dbus.rs` | `#[zbus::interface(name = "org.speaker.Autobright1")]` object + proxy for tests |

D-Bus contract (name `org.speaker.Autobright`, path `/org/speaker/autobright`):

```
methods:  SetAutoEnabled(bool) -> ()
          SetManualPercent(f64) -> ()      // 0..=100; calls on_manual_change + immediate write
          ResetManualBias() -> ()
          GetState() -> (auto: bool, percent: f64, lux: f64, gamma: f64, mode: &'static str)
properties (PropertiesChanged): AutoEnabled, BrightnessPercent, AmbientLux, ManualGamma, Available
```

Main-loop pseudocode (production code; all blocking work on this daemon's own thread):

```
let mut ab = AutoBrightness::new(sensor.read_lux().unwrap_or(50.0), cfg);
let mut last_written = bl.read().unwrap_or(bl.max_brightness()/2);
let mut last_tick = Instant::now();
loop {
    sleep(tick_interval);                       // default 50 ms
    let now = Instant::now();
    let dt = now.duration_since(last_tick).as_secs_f32();
    last_tick = now;

    if let Ok(hw) = bl.read() {
        if hw.abs_diff(last_written) > 1 {        // external write
            let perceptual = ((hw as f32 / bl.max_brightness() as f32)
                              .powf(1.0 / cfg.pwm_gamma)).clamp(b_min, 1.0);
            ab.on_manual_change(perceptual);
            last_written = hw;
            info!(hw, percent, "external brightness change adopted as manual override");
            continue;                             // skip auto write this tick
        }
    }

    match sensor.read_lux() {
        Ok(lux) => {
            let duty = ab.update(lux.lux(), dt);
            let target_hw = (duty * bl.max_brightness() as f32).round() as u32;
            if target_hw != last_written {
                if let Err(e) = bl.write(target_hw) {
                    warn!(error = %e, "backlight write failed; retrying next tick");
                    // keep last_written unchanged so we retry, with backoff for EACCES
                } else { last_written = target_hw; }
            }
        }
        Err(e) => warn!(error = %e, "lux read failed; holding last brightness"),
    }
}
```

Additional requirements in the daemon:

- On `SetManualPercent`: convert percent → perceptual (`pct/100`), call
  `ab.on_manual_change`, write immediately (`duty = ab.brightness().powf(pwm_gamma)`),
  set `last_written`, and publish state. This is the UI's optimistic path; the D-Bus reply
  confirms convergence (§4.5 settle-window rule: if state has not converged after the
  settle window, re-publish).
- On `ResetManualBias`: call `ab.reset_manual_bias()`; do **not** snap brightness.
- Idle reset: after `--idle-reset-secs` with no manual interaction (timer reset by any
  manual command or external write), call `reset_manual_bias()` and log at `info!`.
  Screen-manager may also call `ResetManualBias` when it blanks the display (PR 2).
- Structured logs per §4.6; `warn!` on transient sensor/write faults (rate-limited),
  `error!` on fatal D-Bus export failure, full config at boot.
- Systemd unit `baseline/etc/systemd/system/autobright.service` (`Type=dbus`,
  `BusName=org.speaker.Autobright`, `ExecStart=/usr/local/bin/autobright`, root,
  `Restart=always`, `WantedBy=multi-user.target`), D-Bus policy
  `baseline/etc/dbus-1/system.d/org.speaker.Autobright.conf`, plus `baseline/install.sh` /
  `baseline/verify.sh` entries.

### 3.6 Build/deploy wiring (PR 1)

| File | Change |
|---|---|
| `Cargo.toml:3-13` | add `"services/autobright"` to `members` |
| `scripts/deploy_binaries.sh:27-37` | add `autobright` to `KNOWN_BINARIES` |
| `.github/workflows/core-ci.yml:133` | add `autobright` to architecture verification loop |
| `.github/workflows/core-ci.yml:143-152` | add release artifact path for `autobright` |
| `README.md:47-77`, `services/README.md:7-12` | document new service |

---

## 4. PR 1 — tests (all under `tests/`, host `x86_64-unknown-linux-gnu`)

| File | Cases |
|---|---|
| `crates/hardware/tests/brightness_integration.rs` | hysteresis deadband (no target change inside band); **zero-lux floor** (lux 0 → `b_min`, no chatter with anchor floor 2.0); **asymmetric debounce** (up commits before down for equal dt); **asymmetric ramp** (`rate_up` vs `rate_down` deltas); **dt clamp** (3600 s tick moves ≤ `rate_up*0.5`); **manual gamma + reset** (gamma within `[0.3,3.0]`, `b_current==b_target==user_b`, reset → gamma 1.0); base near 1.0 / near 0 fallback; NaN/∞/negative lux and NaN dt do not poison state; `Config::validate` boundaries; compile-time `Send + Sync` assertion placed before statements |
| `crates/hardware/tests/backlight_integration.rs` | fake sysfs tree via `ScopedTempDir`; read/write/round/max clamp; invalid `max_brightness` → typed error; missing node → `NotFound`; mock fault injection and call counters; `EACCES` mapping; async wrappers |
| `services/autobright/tests/daemon_integration.rs` | parameterised `run_tick(sensor, bl, ab, now, dt)` seam (production constant stays unconditional); external write detection/adoption; manual command → immediate write; auto write suppression after manual change; idle reset; sensor error holds last brightness; write error retry/backoff; exit on signal |
| `services/autobright/tests/dbus_integration.rs` | private `dbus-daemon` (fail loudly if missing), fake client, `SetManualPercent`/`SetAutoEnabled`/`ResetManualBias`/`GetState`, `PropertiesChanged`, distinct bus per test, kill in `Drop` |
| `services/autobright/tests/cli_tests.rs` | table-driven help/invalid args; assert no `Backtrace`/`panicked` in stderr; kill-on-hang timeout |
| `tools/diagnostics/tests/cli_tests.rs` | `backlight-info` rows (happy, `--json`, missing device, invalid value) |

---

## 5. PR 2 — UI overlay and manual control

### 5.1 `ui/brightness_hud.slint` (new)

Clone the `VolumeHudOverlay` shape:

```
export component BrightnessHudOverlay inherits Rectangle {
    in-out property <float> brightness_value: 100.0;   // percent 0..100
    in-out property <bool> auto_enabled: true;
    in property <float> ambient_lux: 0.0;
    in property <bool> overlay_backdrop_available: false;
    out property <bool> has-focus: focus_scope.has-focus;
    public function release_focus() { focus_scope.clear-focus(); }
    callback brightness_changed(float);
    callback auto_toggled(bool);
    callback dismiss_requested();
    callback interaction_activity();
}
```

- `OverlayAppBar` title **"Brightness"** + close → `dismiss_requested()` (`screen_app_bar.slint:103-106`).
- Full-viewport `backdrop_touch` Tap → dismiss; `FocusScope` (`focus-on-click: false`).
- **Extract a shared `LevelSlider`** from the inline volume slider (`volume_hud.slint:145-220`)
  into a reusable component (e.g. `ui/level_slider.slint`) and use it in both HUDs — avoids
  duplicated touch math (anti-bloat rule). Keep the NaN guard:
  `root.brightness_value >= 0.0 ? Math.min(1.0, root.brightness_value/100.0) : 0.0`.
- Row with an **AUTO toggle** chip (`auto_enabled`) and a subtle `Ambient: {lux} lx` readout;
  slider drag fires `brightness_changed(pct)` + `interaction_activity()`.
- All geometry via dynamic expressions/layouts (`M-SLINT-MATH-LAYOUT`), no hardcoded pixel offsets.

### 5.2 `ui/appwindow.slint`

- Add `brightness_hud_visible/opacity/timeout_ms/elapsed_ms/has_focus` mirroring volume HUD
  (`appwindow.slint:60-64,167`); `show_brightness_hud()`/`dismiss_brightness_hud()` public
  functions mirroring `132-146` with mutual exclusion against volume/codec HUDs and a single
  `prepare_overlay_backdrop()` call **only on the opening edge** (never re-capture the frozen
  backdrop after first render — AGENTS.md §4.5 rule).
- Refactor the three HUD timers into **one shared HUD tick** that advances all elapsed counters
  (anti-bloat; avoid a second 50 ms timer).
- Instance wiring mirroring `appwindow.slint:474-487`, binding `brightness_value <=> root.brightness_value`,
  `auto_enabled <=> root.auto_brightness_enabled`, `ambient_lux: root.ambient_lux`.
- `status_bar.night_mode_clicked => { root.show_brightness_hud(); root.night_mode_clicked(); }`.
  Set `status_bar.dim_active` from the real auto/manual state (or from a night-mode flag kept
  in the overlay).

### 5.3 `services/screen-manager` Rust

- New `src/brightness.rs` (or `src/ui/brightness.rs`): `BrightnessClient` — zbus proxy to
  `org.speaker.Autobright1`, a command queue (like `AmpVolumeCommandQueue` in `volume.rs`),
  and a worker thread that performs **all** D-Bus I/O and publishes `BrightnessState` deltas
  through an `mpsc` channel. Never call zbus from the render thread (`M-NO-DBUS-ON-RENDER-THREAD`).
- `AppShell` (`src/ui/shell.rs`): wire callbacks —
  `on_brightness_changed` (clamp finite/0..100, queue `SetManualPercent`, optimistic property
  update), `on_auto_toggled` (queue `SetAutoEnabled`), `on_night_mode_clicked` → show HUD
  (replace the log stub at `shell.rs:1553-1555`), `on_brightness_hud_*`.
  Convergence rule: if the daemon state has not echoed the optimistic value after the settle
  window, re-publish/query forced state (§4.5 settle-window forced publish).
- `src/ui/runtime.rs:310-334`: drain `BrightnessState` deltas on the existing 50 ms sync timer.
- Persistence: add `auto_brightness_enabled: bool` and `brightness_manual_percent: u8` to
  `CoreConfig` (`config.rs:21-84,488-506`), entries in `SETTINGS_CATALOG`
  (`settings_catalog.rs:138-201`), window getter/setter in `shell.rs:292-310,612-626`, and an
  "Auto brightness" toggle in the Settings view. Keep bounds single-sourced
  (`settings_bounds_drift_integration.rs`).

### 5.4 PR 2 tests

- `tests/top_bar_brightness_integration.rs`: tapping the brightness icon opens the overlay;
  `dim_active` reflects state; pixel regression like `top_bar_volume_integration.rs`.
- `tests/brightness_hud_integration.rs`: show/dismiss, mutual exclusion with other HUDs,
  slider drag → `brightness_changed`, AUTO toggle, NaN-safe percent, backdrop captured once.
- `tests/brightness_dbus_integration.rs`: private bus + fake `org.speaker.Autobright1`
  service; assert render-thread callbacks never perform D-Bus (worker + channel path),
  command coalescing, optimistic convergence/forced re-publish.
- Update `tests/ui_shell_integration.rs:229,617` expectations for the now-real brightness button.

---

## 6. Verification gates (run before each push, in order)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo llvm-cov --workspace --target x86_64-unknown-linux-gnu \
  --fail-under-lines 85 --fail-under-functions 85
cargo build --workspace --target aarch64-unknown-linux-gnu --release --bins
# screen-manager additionally: ≥90% lines/functions
cargo llvm-cov -p screen-manager --target x86_64-unknown-linux-gnu \
  --fail-under-lines 90 --fail-under-functions 90
```

- Capture real exit codes (`> log 2>&1; echo $?`); never read `$?` through a pipe to `tail`.
- After editing any `.slint`, purge stale codegen:
  `rm -rf target/<triple>/{debug,release}/build/screen-manager-* target/<triple>/{debug,release}/.fingerprint/screen-manager-*`.
- Verify workflow YAML locally if `core-ci.yml` is touched.
- Live Pi verification (non-destructive): `scripts/deploy_binaries.sh autobright` **only if
  deployment is explicitly requested**; otherwise copy to `/tmp/autobright`, run with
  `--help` and a short `--duration-secs` probe, read sysfs before/after, delete `/tmp` binary.
  Confirm boot log shows exactly one `Using FemtoVG OpenGL renderer` pair and no SIGILL
  (crypto flags already disabled in `.cargo/config.toml:24`).

---

## 7. Decisions — resolved

All four were confirmed with the user on 2026-11-20:

1. **Ambient source:** BH1750 only (existing `speaker_hardware::LightSensor`); no IIO driver.
2. **Top-bar button:** repurpose the existing DIM/brightness button to open the overlay.
3. **Control plane:** D-Bus single-writer daemon; UI never writes backlight sysfs directly.
4. **Overlay controls:** slider + AUTO toggle + ambient-lux readout.

---

## 8. Implementer hand-off prompt (adapted from the supplied block)

> Implement a Rust Linux auto-brightness subsystem for this appliance. In
> `crates/hardware/src/brightness.rs`, expose `AutoBrightness` and `Config` with: smoothed lux
> EMA; hysteresis around `lux_anchor.max(2.0)`; asymmetric debounce (up 1.5 s / down 5.0 s);
> asymmetric slew (up 0.40 / down 0.15 per second); `base_curve` clamped to `[b_min, 1.0]`;
> `dt_secs` clamped to `[0.0, 0.5]` with NaN/∞ guards; `on_manual_change(user_b)` computing
> `manual_gamma = (user_b.ln()/base.ln()).clamp(0.30, 3.00)` (fallback 1.0 only when
> `base <= 1e-6` or `base ≈ 1.0`) and snapping `b_current = b_target = user_b`;
> `reset_manual_bias()`; and `update()` returning `b_current.powf(pwm_gamma)`. Keep the module
> pure and I/O-free; expose read-only getters for tests. Put **all** tests under `tests/`.
>
> Add a mockable sysfs backlight driver (`crates/hardware/src/backlight.rs`) for
> `/sys/class/backlight/rpi_backlight`, reading `brightness`/`max_brightness`, clamping writes,
> with `new_mocked() -> (Self, MockCtrl)` and a canonical error preserving `io::Error` via
> `source()`. Read lux exclusively via the existing `speaker_hardware::LightSensor`
> (BH1750/I2C-1, `new_mocked()` in tests). Do **not** add an IIO sensor backend.
>
> Create `services/autobright` as a workspace service: 50 ms tick, `Instant`-derived dt,
> external-write detection via `last_written` (adopt as manual override, log, skip auto write),
> D-Bus `org.speaker.Autobright1` with `SetManualPercent`, `SetAutoEnabled`,
> `ResetManualBias`, `GetState`, `PropertiesChanged`, and idle manual-bias reset. Log the
> resolved config at boot and state transitions at `info!`, transient faults at `warn!`.
>
> In PR 2 add `BrightnessHudOverlay` (`ui/brightness_hud.slint`) opened by the top-bar
> brightness button, share the volume slider code, expose AUTO + lux readout, and drive it
> from `AppShell` through an off-render-thread D-Bus client. Persist auto/manual preference in
> `CoreConfig`/`SETTINGS_CATALOG`.
>
> Known side effects to document in the PR: concurrent writes by another backlight daemon can
> fight or be misclassified as manual overrides (mitigated by single-writer D-Bus rule and
> `warn!` logging); non-root sysfs writes fail with `EACCES` (service runs as root; baseline
> udev rule `99-backlight.rules` grants access for diagnostics).
>
> Verification: unit/integration tests for hysteresis deadband, zero-lux floor, asymmetric
> debounce/ramp timing, manual gamma override + reset, dt clamp, sysfs mock faults, D-Bus
> private-bus flows, and UI overlay behavior; then the full four-command gate above.
