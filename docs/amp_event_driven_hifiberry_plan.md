# Amp Manager Event-Driven Control Plan — hifiberry-dac Mode

> Status: **draft** (branch `feat/amp-manager-event-driven`, cut from `origin/main`
> at `447fdba`). Supersedes the merus-kernel-driver-dependent assumptions in
> [`audio_manager_plan.md`](audio_manager_plan.md) §1–§2 (the `A.Mstr Vol` ALSA
> mixer no longer exists in the new boot mode). Follows the event-driven design
> agreed 2026-11-10.

## 1. Context & Root Cause

The current appliance boots with `dtoverlay=merus-amp-custom` (stock overlay
with pinctrl stripped, see `baseline/boot/overlays/`). The upstream kernel
codec `ma120x0p.c` has three structural defects that make this mode unreliable
on the shifted Pi:

1. **Regmap cache desync.** `ma120x0p` exposes `/ENABLE` and `/MUTE` as
   user-togglable ALSA switches (`A.Enable`, `B.Mute`) but never calls
   `regcache_mark_dirty()`/`regcache_sync()`. Any `/ENABLE` toggle resets all
   chip registers to factory defaults while the kernel cache still believes the
   old values — subsequent writes of the "same" value are silently dropped.
2. **No PLL-lock gating.** `mute_stream` pulls `/MUTE` HIGH exactly at
   `SNDRV_PCM_TRIGGER_START` with no settle delay and no verification that the
   64×fs bit clock is present (`0x7C` bit 2 `PLL_UNLOCK`), so rate changes and
   unmute-without-clock cause PLL unlock pops.
3. **Rate restrictions as a patch.** RPi engineers limited sample rates in
   `rpi-simple-soundcard.c` (`drvdata_merus_amp`) to work around (2) rather
   than fixing the muting sequencing.

**Decision:** remove the kernel codec from the data path entirely.
`dtoverlay=hifiberry-dac` provides a clean, stateless I2S card
(`sndrpihifiberry`); `audio-manager` (amp-manager) becomes the **sole
supervisor** of the MA12070P over `/dev/i2c-1` @ `0x20` and (if wired on this
board) the control GPIOs. Volume ownership already lives in audio-manager
(`feat/audio-manager-volume-owner` → merged as #53); this plan moves the
*transport* under the same owner.

## 2. Design Principles

1. **Event-driven all the way.** No runtime polling as the primary control
   path. All state changes arrive as events (D-Bus calls, pipeline lifecycle
   events, GPIO line edges). The only blocking poll is a bounded one-shot
   PLL-lock handshake (~200 ms) inside the `AwaitingClock` state — a startup
   handshake, not a loop.
2. **Never toggle `/ENABLE` in normal operation.** The kernel driver's one
   correct instinct: leave the chip powered and switching continuously so
   volatile registers are never wiped. `/ENABLE` is boot-time + fault-recovery
   only, and any recovery cycle MUST perform a full register re-init afterwards.
3. **Serial mute as primary mute.** Register `0x36` bit 7 (`audio_proc_mute`)
   via I2C — already live-verified in `services/audio-manager/src/amp_backend.rs`
   (header docs). Instant, atomic RMW, independent of GPIO wiring, and cannot
   unlock the PLL (pure register write, no clocking interaction).
4. **Single writer.** With the `ma120x0p` kernel driver gone, audio-manager is
   the only user-space writer of the MA12070P register space besides reads.
   No ALSA mixer control exists anymore to fight with.
5. **Reuse, no parallel register tables.** Register constants, error bitflags
   (`AmpErrorFlags`), address newtypes (`AmpI2cAddress`) come from
   `crates/hardware::amp`; the volume curve comes from `crates/amp_curve`.
   **Note (audit):** `AmpDevice` currently exposes only read operations and 7
   monitor/handler register constants; the volume (`0x40`/`0x41`), serial-mute
   (`0x36`), and format (`0x35`) constants plus all write operations are added
   to the hardware crate in Phase 1.5 before Phase 2 consumes them. The
   externally-referenced register map in earlier discussion drafts contains
   errors (power-mode profile is register `0x60`, **not** `0x00`; master volume
   `0x40/0x41` raw = −dB, **not** the per-channel `24 − dB` formula) — the repo
   driver's constants are authoritative. **Datasheet cross-check (§16 register map):**
   - `0x00` = PM_CTRL (bit 6 `manualPM`, bits 5:4 `PM_man` 0-3) — **not** PMP_SELECT
   - `0x1D` = PM_PROFILE (bits 2:0 `PMprofile` 0-4) — separate from PM_CTRL
   - `0x35` = I2S format (bits 2:0)
   - `0x36` = Audio processor (bit 7 `audio_proc_mute`) — live-verified
   - `0x40`/`0x41` = Master volume (raw = −dB) — kernel + live-verified
   - `0x6D` = Error accumulated (latched), `0x7C` = Error live — repo constants
   - `0x60` = **unconfirmed** (repo's `REG_POWER_MODE`); Phase 1.5 verifies against full datasheet
   **Third-party reuse (§5.1–5.2):** No MA12070P crate exists on crates.io
   (`cargo search ma120|merus|infineon` → zero results). The repo's
   `bh1750-embedded` driver uses `embedded-hal`; Phase 1.5 refactors
   `HardwareDevice` to be generic over `embedded_hal::i2c::I2c` (with
   `rppal::hal::I2c` in production) so the amp driver follows the same
   ecosystem pattern and is testable with embedded-hal mocks — no reinventing
   I2C primitives.

## 3. Architecture

```
Event sources                          Core                          Sinks
──────────────                         ────                          ─────
D-Bus SetLevel/SetMute ──────┐
AVRCP volume (via bt-manager)│        ┌──────────────────────┐   I2C writes:
Pipeline lifecycle events ───┼──────► │ AmpController state  │──► 0x40/0x41 volume
 (CamillaDSP events or       │        │ machine (single      │──► 0x36 bit7 mute
  process-state watch)       │        │ Mutex, one event     │──► 0x2D eh_clear
GPIO error/clip line edges ──┘        │ loop)                │──► 0x60 power mode
  (async line-event watch)            └──────────────────────┘   GPIO (recovery only)
State file (boot reconcile) ─────────► /ENABLE: boot + fault-recovery only
Low-frequency safety tick ───────────► telemetry reads 0x6D/0x74/0x75/0x7C/0x7E
 (configurable; not the primary path)
```

### 3.1 State machine

```
              SetMute(true) / StreamStopping
  Playing ────────────────────────────────────► Muted
     ▲                                           │ StreamStarting
     │ PLL locked (bounded one-shot handshake)   ▼
     └──────────────────────────────────── AwaitingClock
                                                 │ handshake timeout
                                                 ▼
   Faulted ◄── GPIO error edge / I2C fault    Muted (warn!, stay muted)
     │  recovery: mute → optional /ENABLE cycle
     │  + full register re-init → Muted
```

Event semantics:

| Event | Action |
|---|---|
| `SetLevel(p)` | Write `0x40`/`0x41` (raw = −dB, 0.25 dB steps, `amp_curve` mapping) + persist state file. Valid in every state; never touches mute. |
| `SetMute(true)` | Serial mute `0x36` bit 7 immediately (atomic RMW). |
| `SetMute(false)` | If pipeline running: `AwaitingClock` handshake, then clear mute; else remembered-unmute on next `StreamStarting`. |
| `StreamStarting` | `AwaitingClock`: one bounded PLL handshake — poll `0x7C` bit 2 until `PLL_UNLOCK == 0` (≤200 ms in `spawn_blocking`), clear transient latch (`0x2D` bit 2), then clear serial mute → `Playing`. Timeout → stay `Muted`, single `warn!` with telemetry; next `StreamStarting` re-attempts. |
| `StreamStopping` | Serial mute immediately. |
| `RateChanged` | Modeled as `StreamStopping` + `StreamStarting` (mute across transition — no pop). |
| GPIO `error_gp` falling edge | One telemetry read (`0x6D`/`0x7C`); thermal warn → −6 dB throttle (`info!`); critical (OC/DC/flying-cap/OT-shutdown) → mute → fault recovery path → `Muted`. |
| Boot | Reconcile: read hardware mute bit + level, compare with state file, converge; leave `/ENABLE` LOW. |

### 3.2 Unmute handshake is the kernel `mute_stream` superset

The bounded one-shot handshake (not a persistent poll loop) replaces what the
kernel driver lacked: it verifies an active 64×fs bit clock before releasing
mute. Because there is no codec `mute_stream` callback in hifiberry mode, the
*event* (`StreamStarting`) comes from the pipeline integration chosen in §5
Phase 0; the handshake itself is self-contained and testable without a pipeline.

## 4. Boot & Configuration Changes

`baseline/boot/config.txt`:

```diff
-dtoverlay=merus-amp-custom
+dtoverlay=hifiberry-dac
 dtparam=audio=off
 dtparam=spi=off
 dtparam=i2c_arm=on
+enable_uart=0        # only if verified pinout uses GPIO 14/15 (UART0)
```

- Expected ALSA card: **`sndrpihifiberry`** (was `sndrpimerusamp`). Update
  asound.conf / CamillaDSP playback device. Keep **`S32_LE`** (keeps BCLK at
  64×fs; verified by the PLL handshake live).
- `baseline/alsa/asound.state`: no `A.Mstr Vol` anymore — volume/mute state is
  owned by audio-manager's state file, not `alsactl`.
- `bluealsa-aplay`: drop `-M hw:sndrpimerusamp --mixer-control="A.Mstr Vol"`;
  run `-S` full-scale (bit-perfect into CamillaDSP). amp-manager owns
  AVRCP→I2C volume end-to-end via D-Bus `SetLevel`.

## 5. Implementation Phases

### Phase 0 — Ground truth on the shifted Pi (read-only, /tmp binaries only)
1. `dmesg | grep -Ei 'ma120|asoc|hifiberry'`, `aplay -l`, device-tree model.
2. `i2cdetect -y 1` (expect `0x20`); register dump via existing
   `tools/diagnostics` `amp-info` (copy to `/tmp`).
3. Decompile the **on-disk** overlay file (it exists on the boot partition
   regardless of what is currently loaded in the live device tree):
   `dtc -I dtb -O dts /boot/firmware/overlays/merus-amp.dtbo`
   → **actual** `enable_gp`/`mute_gp`/`booster_gp`/`error_gp`/`clip_gp` pin
   numbers for this board revision. Verify electrical wiring on the shifted unit.
3b. **GPIO async strategy decision.** `rppal::gpio` edge detection is a
   blocking `poll()`; it has no async API. If GPIO line watching is needed,
   run it in a dedicated `spawn_blocking` task that sends events over an
   `mpsc` channel (matching the plan's event source model). Do **not** add a
   new crate (`gpiod`) unless the user explicitly approves it (§5.3).
4. Check `A.Enable`/`B.Mute` positions in current mixer state (a flipped
   `A.Enable` may be the immediate cause of the current failure) and whether
   the chip is currently in standby.
5. Determine the stream-lifecycle hook: CamillaDSP event/websocket interface in
   our deployment, else `systemd` unit-state watch of `camilladsp`/
   `bluealsa-aplay` via D-Bus (`GetUnit`; classify `NoSuchUnit` as absent unit,
   not transport fault).

**Exit criterion:** written pinout + card name + register dump + chosen
lifecycle hook, recorded in this document.

### Phase 1 — Boot config (small, verifiable PR)
Apply §4 to `baseline/boot/config.txt` + playback config; verify card appears,
no `ma120x0p` module, amp still answers at `0x20` after reboot.

### Phase 1.5 — Extend `crates/hardware` with write path + register constants
**Audit finding addressed:** `AmpDevice` today exposes only reads
(`read_status`, `read_live_errors`, `read_latched_errors`,
`clear_latched_errors`) and only 7 register constants (`0x1B`, `0x2D`, `0x60`,
`0x61`, `0x62`, `0x6D`, `0x7C`). The volume, serial-mute, and format register
constants and all write operations needed by Phase 2 must be added here first
so the plan's "single source of truth" claim (§2.5) holds.

**Third-party reuse (§5.1–5.2):** No MA12070P driver exists on crates.io
(verified `cargo search ma120`, `cargo search merus`, `cargo search infineon`
— zero results). The repo already uses `bh1750-embedded` (an `embedded-hal`
driver) for the light sensor. **Refactor `HardwareDevice` to use
`embedded-hal::i2c::I2c` trait** (with `rppal::hal::I2c` in production) so it
follows the same ecosystem pattern as `bh1750-embedded` and is composable with
embedded-hal test doubles — no reinventing I2C primitives.

Changes in `crates/hardware/src/amp.rs` (all following the existing
`HardwareDevice` + `Backend` + `AmpDeviceMock` patterns, but generic over
`embedded-hal::i2c::I2c`):

| Change | Detail |
|---|---|
| Register constants | Add `pub const` entries **from the datasheet register map** (Infineon MA12070P DS v1.1, §16): `REG_PM_CTRL = 0x00` (Power Mode Control: bit 6 `manualPM`, bits 5:4 `PM_man` 0-3), `REG_PM_PROFILE = 0x1D` (Power Mode Profile: bits 2:0 `PMprofile` 0-4), `REG_AUDIO_IN_MODE = 0x25` (bits 6:5 gain select), `REG_EH_CLEAR = 0x2D` (bit 2 `eh_clear` toggle), `REG_I2S_FORMAT = 0x35` (bits 2:0 format), `REG_AUDIO_PROC = 0x36` (bit 7 `audio_proc_mute`), `REG_VOL_DB_MASTER = 0x40`, `REG_VOL_LSB_MASTER = 0x41`, `REG_VOL_DB_CH0 = 0x42` (`0x42..=0x45`), `REG_VOL_LSB_CH = 0x46`, `REG_THR_DB_CH0 = 0x47` (`0x47..=0x4A`), `REG_THR_LSB_CH = 0x4B`. **Existing constants verified:** `REG_AUDIO_IN = 0x1B` (input format + clock ratio), `REG_POWER_MODE = 0x60` (needs full datasheet cross-check — Phase 1.5 will verify), `REG_DCU_MON0 = 0x61`, `REG_DCU_MON1 = 0x62`, `REG_ERROR_ACC = 0x6D`, `REG_ERROR_LIVE = 0x7C`. |
| `HardwareDevice<I2C>` | Make the struct generic over `I2C: embedded_hal::i2c::I2c` (blocking). Production instantiates with `rppal::hal::I2c` (enabled by `rppal` `hal` feature, already in workspace). All I/O methods (`read_reg`, `write_reg`, `read_regs`, `write_regs`) delegate to the trait. |
| `HardwareDevice::write_reg` / `write_regs` | Promote existing private `write_reg` to public trait-level; add `write_regs(&[(u8, u8)])` for the master vol int+frac pair (single transaction to avoid transient intermediate volumes). |
| `HardwareDevice::write_serial_mute` | RMW of `0x36` bit 7 **with verify-by-re-read** (semantics ported verbatim from `services/audio-manager/src/i2c_mute.rs::set_serial_mute_blocking` — the live-verified implementation). After the port, `i2c_mute.rs`'s bus functions are deleted and its pure helpers (`apply_mute_bit`, `is_muted_bit`) move to `crates/hardware` (single source of truth). |
| `HardwareDevice::write_master_volume` | Two-register write `0x40`/`0x41` (raw = −dB, 0..144, 0.25 dB fractional steps in `0x41` bits [1:0]). |
| `HardwareDevice::write_power_mode_ctrl` | RMW of `0x00`: set bit 6 `manualPM` = 1, bits 5:4 `PM_man` from `PowerMode` (0-3). **Not** `0x60` — the datasheet puts manual PM control at `0x00`. |
| `HardwareDevice::write_power_mode_profile` | Write `0x1D` bits 2:0 `PMprofile` (0-4). Distinct from PM_CTRL. |
| `PowerMode` / `PowerModeProfile` type split (Phase 1.5) | The datasheet has two distinct concepts: **PM_CTRL at 0x00** (`manualPM` + `PM_man` 0-3) and **PM_PROFILE at 0x1D** (`PMprofile` 0-4). The repo's `PowerModeProfile` enum (4 variants, 0-3, from 0x60 bits [1:0]) matches 0x00's PM_man. Phase 1.5 will: (a) verify 0x60 in full datasheet; (b) if 0x60 is a monitor register, keep `PowerModeProfile` as the 0-3 decode from 0x60; (c) add a separate `PowerModeProfileSelect` (or similar) for 0x1D's 0-4 PMprofile; (d) rename `write_power_mode_ctrl` to take a `PowerMode` (0-3) and `write_power_mode_profile` to take `PowerModeProfileSelect` (0-4). |
| `HardwareDevice::write_i2s_format` | RMW of `0x35` bits 2:0 (needed by the fault-recovery re-init path only). |
| `HardwareDevice::write_audio_in_mode` | RMW of `0x25` bits 6:5 (gain select) — used if needed by recovery. |
| `HardwareDevice::clear_error_handler` | Toggle `0x2D` bit 2 low→high→low (existing `AmpStatus::eh_clear_pulse_sequence`, already correct). |
| `AmpDevice` async wrappers | `write_master_volume_async`, `write_serial_mute_async`, `write_power_mode_ctrl_async`, `write_power_mode_profile_async`, `write_i2s_format_async`, `clear_error_handler_async` via `run_blocking` (same pattern as existing async reads). |
| `AmpDeviceMock` | Write-path support: `mock_master_volume_raw` / `mock_serial_muted` / `mock_pm_ctrl` / `mock_pm_profile` / `mock_i2s_format` state fields + `set_*` setters and fault injection (`set_fault` already exists) so `I2cAmpBackend` tests can drive and assert writes without hardware. |
| Datasheet note | Document in module docs: **Master volume `0x40`/`0x41` raw = −dB (0 = −144 dB mute, 144 = 0 dB)** — matches kernel `A.Mstr Vol` 0..144 and `amp_curve`. The `24 − dB` formula (Table 8-9) applies **only** to per-channel registers `0x42..=0x45` (+24..−120 dB boost/attenuation), which the appliance does not use (§6 non-goals). **Power Mode** has two registers: `0x00` (PM_CTRL: manual enable + PM 0-3 selection) and `0x1D` (PM_PROFILE: PMP 0-4 selection). The repo's `REG_POWER_MODE = 0x60` and `PowerModeProfile::from_raw(reading 0x60 bits [1:0])` will be verified against the full datasheet in Phase 1.5 — if `0x60` is a different monitor register, the enum/decoding stays but the register address changes. |

Unit tests for each new write method go under `crates/hardware/tests/`
(mock-driven, plus existing invalid-address/bus probes). No behavior change to
existing readers.

### Phase 2 — `I2cAmpBackend` + event loop in audio-manager
File-by-file (all under `services/audio-manager/src/`, tests under `tests/`):

| File | Change |
|---|---|
| `amp_backend.rs` | Add `I2cAmpBackend: AmpBackend` implementing the existing trait (`write_state`, `read_state`, `set_serial_mute`, `read_serial_mute`, `validate`) over I2C only; backed by the Phase 1.5 `AmpDevice` write methods (now generic over `embedded_hal::i2c::I2c`). Volume: `0x40`/`0x41`, raw = −dB (0..144), `amp_curve` mapping. **`card_name()` decision (audit):** the trait's `card_name()` is meaningless for an I2C backend — rename it to `backend_id() -> &str` in a breaking but internal-only trait change (audio-manager is the sole crate importing it). `I2cAmpBackend` returns `"i2c-ma12070p@0x20"`, `RealAmpBackend` returns its ALSA card name, `MockAmpBackend` returns `"mock"`. The I2S playback card (`sndrpihifiberry`) is **not** the amp backend and must not leak into this trait; its validation belongs to the pipeline config, not audio-manager. `MockAmpBackend` stays the mock; add mock ctrl for I2C write faults per `new_mocked() -> (Self, Ctrl)`. |
| `amp_state.rs` / `state_file.rs` | Unchanged semantics (level percent + muted). |
| `config.rs` | `alsa_card` → playback card for validation only; add optional GPIO line config (`enable_pin`, `error_pin`, …) from Phase 0 pinout; add `pll_lock_timeout_ms` (production constant unconditional; tests use parameterized variant per §5.3 rule) and safety-tick interval (default enabled, can disable). |
| `amp_controller.rs` (new) | Event-loop state machine §3.1. One `tokio::select!` over D-Bus commands, lifecycle events, GPIO edge channel, safety tick. GPIO edges consumed via async line-event watch task → `mpsc`; no polling. |
| `i2c_mute.rs` | **Deleted** after Phase 1.5 (bus RMW moves to `crates/hardware`); pure helpers `apply_mute_bit`/`is_muted_bit` move there too. Keep its module doc's live-verification notes by porting them into the new `write_serial_mute` doc comment. |
| `main.rs` / `dbus_api.rs` | Wire backend selection (`i2c` mode new default; `alsa` mode retained until cutover verified). Boot config fully logged at `info!` per logging discipline. |

Rules applied: all I2C/GPIO in `spawn_blocking` (`speaker_common::runtime`),
canonical errors with `source` preservation, `warn!` for transient faults /
`info!` for state transitions / `debug!` for steady ticks, no `thread::sleep`
in async paths, `/ENABLE` writes only in the recovery path, transactional state
mutation (mutate in-memory state strictly after successful bus writes).

### Phase 3 — BlueALSA playback path update
Update `baseline` install/config for `-S` full-scale bluealsa-aplay (§4).
Verify bt-manager `sync_volume` routes phone volume through D-Bus `SetLevel`
end-to-end (already the merged architecture in #53).

### Phase 4 — Tests & local gates
- **DI-based coverage strategy (per AGENTS.md §3, §5.3):** All hardware-dependent
  code is tested via the existing dependency-injection pattern:
  - `AmpBackend` trait → `I2cAmpBackend` (production) + `MockAmpBackend` (tests)
  - `AmpDevice` → `Backend::Hardware` (production) + `AmpDeviceMock` (tests)
  - Both mocks expose `new_mocked() -> (Self, MockCtrl)` for fault injection
    and call inspection (`MockAmpCtrl`, `AmpDeviceMock` setters) — **no
    `#[cfg(test)]` in production code**, mocks are unconditionally compiled
    (`M-TEST-UTIL` rule).
- `crates/hardware/tests/` additions (Phase 1.5): each new write method
  (master volume int+frac pair, serial-mute RMW with verify, power-mode RMW
  bit-preservation, I2S format RMW) driven through `AmpDeviceMock`, including
  fault injection on the write path; register-constant invariants.
- `services/audio-manager/tests/` additions: state-machine transitions (each
  table row of §3.1), volume encode boundaries (0, −144, 0.25 dB steps, clamp,
  NaN guard), serial mute RMW failure injection, PLL-handshake timeout, GPIO
  fault edge handling, boot reconcile convergence, `/ENABLE`-cycle recovery
  asserting full re-init, `backend_id()` per-backend values.
- **Coverage gates** (must pass locally before push, per AGENTS.md §4.1):
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --workspace --all-targets -- -D warnings`
  3. `cargo llvm-cov --workspace --target x86_64-unknown-linux-gnu --fail-under-lines 85 --fail-under-functions 85`
  4. `cargo build --target aarch64-unknown-linux-gnu --release --bins`

### Phase 5 — Live validation on the Pi
1. Reboot with new config → `sndrpihifiberry` card present, no `ma120x0p`,
   amp alive at `0x20`.
2. Play `S32_LE` stream → handshake unmutes only after PLL lock (`amp-info`
   telemetry confirms `0x7C` bit 2 = 0).
3. Rate-switch test: 44.1 k → 48 k → 192 k while muted → no pop; unmute gated.
4. D-Bus `SetLevel` sweep 0→100 → register `0x40` follows `amp_curve`; phone
   AVRCP volume maps end-to-end.
5. Mute/unmute, reboot persistence via state file, thermal throttle + fault
   recovery under load, journal log discipline check.

## 6. Explicit Non-Goals (scope containment)
- No general-purpose multi-mode MA12070P abstractions (one-shot conversions,
  resolution switching) — appliance needs only: volume, serial mute, PLL-gated
  unmute, telemetry, fault recovery.
- No source routing/handover (future `sources` module, see
  `audio_manager_plan.md` §7).
- No refactor of sibling drivers (`light.rs`, `temp.rs`, `encoder.rs`).

## 7. Open Questions (blocked on Phase 0)
1. Is `/ENABLE` wired to a BCM GPIO on this unit (which one), or hard-strapped
   enabled? Decides whether the recovery cycle is software-reachable.
2. Is `/MUTE` GPIO wired and needed, or is serial mute (`0x36` bit 7)
   sufficient? Recommendation: serial mute only unless live testing says
   otherwise.
3. CamillaDSP event interface availability (lifecycle hook choice).
4. **Resolved (design-time):** GPIO line watching, if needed, uses
   `rppal::gpio` (already a workspace dependency) inside a dedicated
   `spawn_blocking` task feeding an `mpsc` channel. `rppal`'s edge watch is a
   blocking `poll()`; no async support — hence the task/channel bridge. No new
   crate (`gpiod`) is added; per §5.3 that would require explicit user
   approval, which has not been given.
5. **Resolved (design-time):** `AmpBackend::card_name` → renamed
   `backend_id()`; `I2cAmpBackend` reports `"i2c-ma12070p@0x20"`. The I2S
   playback card name (`sndrpihifiberry`) is pipeline configuration, not an
   amp-backend identifier, and is not returned by this trait.

## 8. Audit Trail
2026-11-10 audit addressed in this revision:
- **Gap 1 (missing write path):** new Phase 1.5 adds write methods + mock
  support to `crates/hardware::AmpDevice` before audio-manager consumes them.
- **Gap 2 (missing register constants):** all volume/mute/format register
  constants added to `crates/hardware/src/amp.rs` in Phase 1.5; `i2c_mute.rs`
  is folded into the hardware crate (single source of truth).
- **Gap 3 (`card_name()` semantics):** trait renamed to `backend_id()` with
  per-backend values; I2S card name excluded from the amp backend trait.
- **Datasheet cross-check (§16 register map, Infineon MA12070P DS v1.1):**
  - Reference plan's `PMP_SELECT = 0x00` is **wrong** — datasheet `0x00` = PM_CTRL
    (manualPM bit 6, PM_man bits 5:4 = 0-3); `0x1D` = PM_PROFILE (PMprofile bits 2:0 = 0-4).
  - Repo's `REG_POWER_MODE = 0x60` + `PowerModeProfile` (0-3) is **unconfirmed** — Phase 1.5
    verifies against full datasheet; may split into `PowerMode` (0-3, for 0x00) and
    `PowerModeProfileSelect` (0-4, for 0x1D).
  - Master volume `0x40`/`0x41` raw = −dB confirmed by kernel driver + live test.
  - Serial mute `0x36` bit 7 confirmed live-verified.
  - Error registers `0x6D` (latched), `0x7C` (live) match repo constants.
- **Minor:** Phase 0 overlay-decompile wording corrected (file is on-disk,
  independent of the loaded dtb); GPIO async strategy pinned to
  `rppal` + `spawn_blocking` + `mpsc` (open question 4 resolved); test plan
  split into hardware-crate and service-level test files.
