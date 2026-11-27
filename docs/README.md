# Architecture Documentation & Technical Specifications

This directory contains the system audit reports, architectural specifications, and implementation plans for the **Raspberry Pi Audio Bluetooth & DSP Appliance**.

---

## Documents

| Document | Format | Description |
| :--- | :--- | :--- |
| [rpi_audio_bluetooth_report.md](rpi_audio_bluetooth_report.md) | Markdown | Comprehensive hardware, audio pipeline, and system audit report for the Raspberry Pi 3 running Debian 13 (Trixie) / DietPi. Includes the AVRCP absolute-volume sync architecture, the BlueZ gate analysis (`[AVRCP] VolumeCategory` 5.88 default, info-file `Volume=` seed, `ControllerMode`), and the verified transport→Merus volume mapping. |
| [volume_sync_plan.md](volume_sync_plan.md) | Markdown | Volume-sync plan — **implemented**: phone AVRCP → bluez-alsa native volume → bt-manager `sync_volume` (deduped) → `audio-manager` D-Bus `SetLevel` → Merus amp. Root cause was BlueZ 5.88 `[AVRCP] VolumeCategory = true` rejecting Pixel's AVRCP TG; fixed via `main.conf` in `baseline/etc/bluetooth/main.conf`. |
| [single_active_bt_source.md](single_active_bt_source.md) | Markdown | Single active Bluetooth audio source architecture — `bluealsa-aplay --single-audio` audio-path gate + `bt-manager` `org.speaker.Bluetooth` (`ActiveDevice`) arbiter, real `avrcp/player0` AVRCP pause, and active-device scoping across `volume_sync`, `cover_provider`, and `screen-manager`. |
| [audio_manager_plan.md](audio_manager_plan.md) | Markdown | **Implemented** `services/audio-manager`: single event-driven hardware owner of the Merus amp (`A.Mstr Vol`) fixing the mute-is-a-no-op finding; D-Bus `SetLevel`/`SetMute`, zero polling, unit ordering fixes. |
| [amp_defaults_parity_audit.md](amp_defaults_parity_audit.md) | Markdown | Audit of Merus MA12070P default register/mixer parity between ALSA state and `audio-manager`. |
| [amp_event_driven_hifiberry_plan.md](amp_event_driven_hifiberry_plan.md) | Markdown | Event-driven `hifiberry-dac` (`sndrpihifiberry`) + I2C Merus MA12070P control plan (`rate_watch` and `dsp_watch` PLL-gated unmute). |
| [camilladsp_5_upgrade.md](camilladsp_5_upgrade.md) | Markdown | **Implemented** CamillaDSP `5.0.0-beta.2` upgrade, `camilladsp-schema` adoption, vanilla passthrough, dynamic sample-rate switching (`rate_watch` + `snd-aloop pcm_notify=1`), and `SubscribeState` push `dsp_watch`. |
| [private_bluealsa_plan.md](private_bluealsa_plan.md) | Markdown | Private Cortex-A53-tuned BlueALSA v5.0.0 stack under `/usr/local` (`scripts/build_bluealsa_pi.sh`). |
| [mpd_plan.md](mpd_plan.md) | Markdown | **Implemented:** Cortex-A53 cross-compiled `MPD v0.25` streaming/radio daemon (`/usr/local/bin/mpd`), Trixie sysroot pins, `baseline/etc/mpd.conf`, and `mpd.service`. |
| [ui_architecture_plan_rust.md](ui_architecture_plan_rust.md) | Markdown | **Primary Architecture Plan:** Native Rust 2024 & Slint UI touch console, Tokio actor model, CamillaDSP WebSocket, BlueZ D-Bus, and PeppyMeter integration. |
| [ui_architecture_plan_python.md](ui_architecture_plan_python.md) | Markdown | Alternative / reference architecture plan utilizing Python and `pygame-ce` directly over the Linux framebuffer (`/dev/fb0`). |
| [skia_opengl_migration_plan.md](skia_opengl_migration_plan.md) | Markdown | **Implemented:** Replace `renderer-femtovg` with `renderer-skia-opengl` (Skia Ganesh OpenGL ES 2.0 on VideoCore IV `VC4 V3D 2.1`), vc4-only Mesa build, GPU FBO layer caching, and cover-art cache sizing (`TARGET_COVER_SIZE = 360`). |
| [slint-material-migration.md](slint-material-migration.md) | Markdown | Material 3 UI design system migration plan for Slint components and screens. |
| [m3_auto_theming_plan.md](m3_auto_theming_plan.md) | Markdown | **Implemented:** Material 3 dynamic cover-art auto-theming (`mcu-material-color` palette extraction driving UI and visualizer colors). |
| [visualizer_architecture_plan.md](visualizer_architecture_plan.md) | Markdown | **Implemented:** Full-screen (`800×480`) 48-bar bilateral Radial FFT + 24-band Glasberg–Moore ERB linear spectrum driven by CamillaDSP 5.0 `SubscribeSpectrum` WebSocket streaming and Visualizer Settings sub-screen. |
| [autobrightness_plan.md](autobrightness_plan.md) | Markdown | Ambient auto-brightness loop design (BH1750 lux sensor -> backlight policy inside `screen-manager`). |
| [bt-devices-screen-plan.md](bt-devices-screen-plan.md) | Markdown | Bluetooth devices management screen plan in `screen-manager`. |
| [bt-info-phone-volume-plan.md](bt-info-phone-volume-plan.md) | Markdown | `bt-info` diagnostic probe and phone AVRCP volume telemetry plan. |
| [smooth_scrolling_investigation.md](smooth_scrolling_investigation.md) | Markdown | Investigation into smooth scrolling and touch responsiveness in Slint on Pi 3. |
| [cpu_optimization_options.md](cpu_optimization_options.md) | Markdown | CPU optimization options for Cortex-A53 (`screen-manager`, DSP, and rendering). |
| [bluetooh_disconnect_issues.md](bluetooh_disconnect_issues.md) | Markdown | **Bluetooth Disconnect Investigation:** Root-cause analysis of RTL8761BU `command tx timeout` wedges under 60 fps visualizer load on 64-bit Pi 3 (`dwc_otg_sim-fiq` split-transaction contention) and verification of `coherent_pool=4M dwc_otg.fiq_enable=0 dwc_otg.fiq_fsm_enable=0`. |
| [releasing.md](releasing.md) | Markdown | Versioning, conventional commits, `release-plz` automation, and tag release workflow. |
| [pr20_leftovers_refactor_plan.md](pr20_leftovers_refactor_plan.md) | Markdown | Follow-up refactoring notes from PR #20. |
| [pr21_next_pr_findings.md](pr21_next_pr_findings.md) | Markdown | Review findings and action items carried forward from PR #21. |
| [requirements-python-ui.txt](requirements-python-ui.txt) | Text | Pinned Python dependencies for the Python UI reference implementation and diagnostic utilities. |
