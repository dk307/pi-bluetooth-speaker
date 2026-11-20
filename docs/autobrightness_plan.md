# Auto-Brightness — Implementation Status

Status: **implemented in `screen-manager`** (no separate daemon).

The original plan called for a standalone `autobright` D-Bus daemon. That design was
replaced: the brightness policy loop now runs as a background thread inside the
`screen-manager` service, so there is no second process, no `autobright` binary, and
no `org.speaker.Autobright` D-Bus service or systemd unit.

## Final architecture

```text
  ┌──────────────── screen-manager process ────────────────────────┐
  │ UI thread: brightness overlay (slider, AUTO toggle, lux readout)│
  │ background brightness thread: periodic policy loop              │
  │   read lux → AutoBrightness::update() → write backlight sysfs   │
  └──────────────┬────────────────────────────┬────────────────────┘
                 │ read lux                   │ read/write sysfs
                 ▼                            ▼
   speaker_hardware::LightSensor   speaker_hardware::Backlight
   (BH1750 @ I2C-1 0x23)           /sys/class/backlight/rpi_backlight
```

- The control policy lives in `speaker_hardware::AutoBrightness` (smoothed lux EMA,
  hysteresis, asymmetric debounce/ramp, and manual gamma bias).
- Ambient light is read through `speaker_hardware::LightSensor` (BH1750 on I2C-1 `@0x23`);
  there is no IIO sensor backend.
- Backlight access goes through the mockable `speaker_hardware::Backlight` sysfs adapter —
  `screen-manager` is the single writer of `/sys/class/backlight/rpi_backlight`.
- The UI shares brightness state with the background thread in-process; no cross-process
  D-Bus control plane is involved.

## Removed as part of the consolidation

- `services/autobright/` crate and the `autobright` workspace member / deploy / CI entries.
- `baseline/etc/systemd/system/autobright.service` and its `install.sh` / `verify.sh` hooks.
- `baseline/etc/dbus-1/system.d/org.speaker.Autobright.conf` D-Bus policy.

The diagnostic utilities `light-read` and the hardware drivers remain in their original
crates; only the standalone daemon and its service/D-Bus wiring were eliminated.
