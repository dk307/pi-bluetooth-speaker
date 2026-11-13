# MA12070P Defaults Parity Audit (vs. upstream `ma120x0p` kernel driver)

Reference: `sound/soc/codecs/ma120x0p.c` (raspberrypi/linux, rpi-6.6.y),
`ma120x0p_probe()` — verified line-by-line against our register map in
`crates/hardware/src/amp.rs` and the init sequence in
`services/audio-manager/src/amp_controller.rs::reinit_registers()`.

## Kernel `ma120x0p_probe()` step-by-step vs. our implementation

| # | Kernel probe | Ours | Parity |
|---|---|---|---|
| 1 | `clear_err`: pulse `0x2D` bit 2 (`0x04`) 0→1→0 | `clear_latched_errors_async()` (same pulse, same register/bit) | ✅ |
| 2 | `0x35 = 0x08` (std. I²S bits 2:0 = 000 + `audio_proc_enable` bit 3) | `write_i2s_format(StandardI2s)` (RMW bits 2:0) + `enable_audio_processor` (bit 3) — identical end state `0x08` modulo slew bits, which are then set explicitly | ✅ |
| 3 | `0x36 \|= 0x40` limiter enable | `enable_limiter_async()` (`LIMITER_ENABLE_MASK = 0x40`, RMW) | ✅ |
| 4 | attack mask `0xC0` → `0x80` (fast) | `configure_limiter_slew(Fast, …)` | ✅ |
| 5 | release mask `0x30` → `0x00` (slow) | `configure_limiter_slew(…, Slow)` | ✅ |
| 6 | `0x40 = 0x18` (0 dB, VLP `reg = 24 − dB`) | **Intentionally omitted.** We mute first at boot and write the *persisted* user level; the kernel's 0 dB default would blast full power into the speakers with no clock running. Our reset-default handling is strictly safer than kernel parity. | ✅ (superset, safer) |
| 7 | `0x47 = 0x27`, `0x48 = 0x27` (ch0/ch1 limiter −15 dB) | `write_limiter_thresholds(-15)` → `reg = 24 − (−15) = 39 = 0x27` for **all four channels** (kernel writes 2; we cover ch2/ch3 too, matching the PBTL/HAT-ZW app note) | ✅ superset |
| 8 | error-accumulation test bits on `0x6D` | live + latched error telemetry (`read_live_errors`/`read_latched_errors`, `AmpErrorFlags`) with critical-fault `/ENABLE` recovery | ✅ |

## Register address/encoding cross-check (kernel decimal → hex)

| Register | Kernel (`__a`) | Ours | Encoding match |
|---|---|---|---|
| `ma_i2s_format__a` | 53 = `0x35` | `REG_I2S_FORMAT = 0x35`, mask `0x07`, reset `0x01` (left-justified) | ✅ |
| `ma_audio_proc_limiterenable__a` | 54 = `0x36`, mask `0x40` | `REG_AUDIO_PROC = 0x36`, `LIMITER_ENABLE_MASK = 0x40` | ✅ |
| `ma_vol_db_master__a` | 64 = `0x40` | `REG_VOL_DB_MASTER = 0x40`, `reg = 24 − dB` (VLP) / `reg = 168 − raw` (user scale, `dB = raw − 144`) | ✅ (`raw = 144` ⇒ `0x18` = 0 dB, exactly the kernel constant) |
| `ma_thr_db_ch0__a` | 71 = `0x47` | `REG_THR_DB_CH0 = 0x47`, `reg = 24 − dbfs` | ✅ |
| `ma_thr_lsb_ch0__a` | 75 = `0x4B`, mask `0x03` | `REG_THR_LSB_CH = 0x4B`, zeroed alongside integer thresholds | ✅ |
| `ma_eh_clear__a` | 45 = `0x2D`, mask `0x04` | `REG_EH_CLEAR = 0x2D`, `eh_clear_pulse_sequence()` | ✅ |
| `ma_error_acc__a` | 109 = `0x6D` | `REG_ERROR_ACC = 0x6D` | ✅ |
| I²C address / bus | overlay `0x20` on BSC1 | `MA12070P_DEFAULT_I2C_ADDR = 0x20`, bus 1 | ✅ |

## Community-referenced startup values

* Squeezelite-ESP32 discussion: `0x35 = 0x08` (standard I²S) — matches our
  step 2 end state exactly.
* Infineon KIT_40W_AMP_HAT_ZW quick start: −15 dB limiter threshold for
  4 Ω speakers — matches our `write_limiter_thresholds(-15)`.
* Sonocotta `esphome-ma12070p`: `/ENABLE`/`/MUTE` GPIO sequencing —
  our `amp_gpio.rs` owns `/ENABLE` (GPIO 14) reset pulses and boot-time
  `/MUTE` (GPIO 15) wiring, mirroring the same pattern.

## Power mode / profile / input mode parity (0x00, 0x1D, 0x25, 0x27)

Audited against the upstream `R.Power Mode` / `Q.PM Prof` ALSA controls
(`ma120x0p.c`, `ma120x0p_pi3.c`) and the register-header reset values:

| Register / field | Upstream | Ours | Parity |
|---|---|---|---|
| `0x00` bit 6 `manualPM` (mask `0x40`, reset 0) | Dynamic when 0 | `PowerModeCtrl::Dynamic` clears bit 6 | ✅ |
| `0x00` bits 5:4 `PM_man` (mask `0x30`, reset 3) | `0x10`=Dynamic, `0x50`=PM1, `0x60`=PM2, `0x70`=PM3; `manualPM=1` + `PM_man=00` is never written upstream | Writer never emits that combination; PM1-PM3 encode `0x40 \| pm << 4` exactly as the upstream control values | ✅ (fixed — previously `Pm0` wrote `manualPM=1 \| PM_man=00`, undefined on silicon) |
| `0x1D` bits 2:0 `PMprofile` (mask `0x07`, reset 0) | presets 0-4 (`PMF0`-`PMF4`), 5 = custom | `PowerModeProfile` `Pmp0..Pmp4` + `Pmp5Custom`, RMW mask `0x07` | ✅ |
| `0x25` bits 7:5 `audio_in_mode` (mask `0xE0`, reset 0) | **Never written by the kernel or HAT driver** — left at reset/fuse default | Production runtime (`boot_init`/`reinit_registers`) never writes it either; only the `amp-info` CLI exposes an explicit operator override | ✅ |
| `0x27` bit 5 `audio_in_mode_ext` (mask `0x20`, reset 0) | reset 0 = fuse bank controls input mode | Same reset state at runtime; the CLI override correctly sets the ext gate when writing `0x25` (kernel parity: the driver exposes no such override at all) | ✅ |

Gain labels for `audio_in_mode` values 0-5 (digital 20/26/32/0 dB, analog
20/26 dB) come from the MA12070P datasheet register table; neither the
kernel driver nor the Infineon/Sonocotta implementations define alternate
labels, and the field/mask/reset encodings are identical.

**Boot behavior matches upstream exactly:** neither driver writes any of
these four registers during probe — power mode/profile/input mode stay at
their reset/fuse/MSEL-pin defaults unless an operator explicitly overrides
them via the diagnostic CLI.

## Deviations (all intentional, none accidental)

1. **Startup volume:** kernel probes at 0 dB; we boot muted with the
   persisted user level (speaker-safety, documented in `boot_init`).
2. **Limiter thresholds on all 4 channels** instead of the kernel's 2 —
   safe superset.
3. **Error-history clear is now part of the shared init sequence**
   (`reinit_registers`, used by both `boot_init` and fault recovery) —
   previously only fault/thermal paths pulsed `0x2D`; a boot with
   pre-latched faults would have shown stale `0x6D` flags as fresh.

No other defaults diverge from the upstream driver.
