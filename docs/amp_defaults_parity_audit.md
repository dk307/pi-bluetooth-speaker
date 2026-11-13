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
