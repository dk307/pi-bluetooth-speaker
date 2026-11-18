# Changelog

All notable changes to the Raspberry Pi Bluetooth Speaker appliance are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases use
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.1.0](https://github.com/dk307/pi-bluetooth-speaker/releases/tag/v2.1.0) - 2026-11-16

### Added

- *(bt-info)* report phone AVRCP absolute volume alongside hardware volume ([#56](https://github.com/dk307/pi-bluetooth-speaker/pull/56))
- *(audio)* implement audio-manager hardware volume/mute owner (Phase 1) ([#53](https://github.com/dk307/pi-bluetooth-speaker/pull/53))
- hybrid Material 1.1.0 adoption + MaterialPalette theme migration (P0–P5) ([#50](https://github.com/dk307/pi-bluetooth-speaker/pull/50))
- *(screen-manager)* implement Bluetooth connection details screen ([#46](https://github.com/dk307/pi-bluetooth-speaker/pull/46))
- *(screen-manager)* implement UI volume HUD screen and rotary encoder volume control ([#44](https://github.com/dk307/pi-bluetooth-speaker/pull/44))
- *(screen-manager)* touch input, read-only seek bar, and evdev DI ([#41](https://github.com/dk307/pi-bluetooth-speaker/pull/41))
- *(screen-manager)* evdev touch input, interactive viewport controls & read-only progress bar ([#40](https://github.com/dk307/pi-bluetooth-speaker/pull/40))

### Fixed

- move Bluetooth connect off the render thread; prune dead code (#69 follow-up) ([#72](https://github.com/dk307/pi-bluetooth-speaker/pull/72))
- *(screen-manager)* fixed-size top bar codec badge and volume readout ([#45](https://github.com/dk307/pi-bluetooth-speaker/pull/45))

### Other

- *(volume_hud)* fix percentage readout position and reduce font size ([#71](https://github.com/dk307/pi-bluetooth-speaker/pull/71))
- migrate to Slint backend-linuxkms + FemtoVG, live-text marquee  ([#69](https://github.com/dk307/pi-bluetooth-speaker/pull/69))
- fix Now Playing top bar codec badge, codec overlay and device name overlap ([#62](https://github.com/dk307/pi-bluetooth-speaker/pull/62))
- remove 2s marquee pause for continuous seamless scrolling ([#60](https://github.com/dk307/pi-bluetooth-speaker/pull/60))
- Strong typed amp enums + speaker-to-phone volume sync ([#58](https://github.com/dk307/pi-bluetooth-speaker/pull/58))
- fix marquee zero-height geometry crash ([#59](https://github.com/dk307/pi-bluetooth-speaker/pull/59))
- fix marquee zero-height geometry crash
- standardize Material 3 app bars across all screens and overlays ([#54](https://github.com/dk307/pi-bluetooth-speaker/pull/54))
- Event-driven MA12070P amp supervisor: hifiberry-dac migration + DSP stream-lifecycle watcher ([#55](https://github.com/dk307/pi-bluetooth-speaker/pull/55))
- *(build)* optimize compile times, decouple test suites to tests/, and audit agent guidelines ([#47](https://github.com/dk307/pi-bluetooth-speaker/pull/47))
- wip ([#48](https://github.com/dk307/pi-bluetooth-speaker/pull/48))
- restructure workspace into multi-crate services, crates, and tools ([#38](https://github.com/dk307/pi-bluetooth-speaker/pull/38))
