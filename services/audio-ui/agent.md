# Agent Instructions: `audio-ui` Service

This document defines the behavioral and engineering rules for AI agents developing, refactoring, and maintaining the `audio-ui` service.

---

## 1. Primary Rule: Microsoft Pragmatic Rust Guidelines

All Rust code in this service **MUST** strictly adhere to the **Microsoft Pragmatic Rust Guidelines**:
* **Source:** [microsoft-rust-guidelines.md](file:///home/deepak/server/services/.guidelines/microsoft-rust-guidelines.md)

Before designing APIs, writing modules, implementing error handling, or writing tests, consult the corresponding sections of `microsoft-rust-guidelines.md`.

---

## 2. Core Directives for `audio-ui`

### 2.1. Design with AI & Testability in Mind (`M-DESIGN-FOR-AI`, `M-TESTABLE-API`)
* **Trait Abstraction:** Hardware interfaces (GPIO, I2C, BlueZ D-Bus, CamillaDSP WebSocket) must be abstracted behind asynchronous traits (e.g., `PeripheralController`, `DspClient`, `BluetoothManager`).
* **Test Fixtures & Mocks:** Maintain standalone mock implementations (e.g., in-memory mock BlueZ, simulated DSP WebSocket) so that all state machines, volume bounds, and reconnection logic can be thoroughly unit tested on non-ARM host environments without requiring physical Raspberry Pi hardware.
* **Workstation Simulator:** Keep the desktop simulation feature (`--features simulator`) functioning cleanly with Slint's winit/femtovg backend.

### 2.2. Strong Typing & Domain Modeling (`C-NEWTYPE`)
* **Avoid Primitive Obsession:** Do not pass raw integers or floats for domain-specific audio and hardware quantities.
* Use newtypes and typed enums:
  * Volume: `VolumePercent(u8)` (clamped `0..=100`), `AttenuationDb(f32)`
  * Frequencies: `FrequencyHz(u32)`
  * Hardware pins & states: `EncoderDirection`, `AmpFaultState`, `PlayState`
* Implement standard conversion traits: `From`, `TryFrom`, `Display`, `AsRef`.

### 2.3. Resilient Error Handling (`M-ERROR-DESIGN`, `C-NO-PANIC`)
* **No Panicking in Runtime Paths:** Never use `.unwrap()` or `.expect()` in runtime event dispatch loops, D-Bus handlers, or WebSocket streams.
* **Explicit Domain Errors:** Use typed error enums derived with `thiserror` for each subsystem (e.g., `BluetoothError`, `DspError`, `HardwareError`).
* **Non-Blocking Recovery:** If a peripheral or external daemon disconnects (e.g., CamillaDSP restart or BlueZ reload), transition into a graceful reconnect state with exponential backoff rather than terminating the process.

### 2.4. Tokio Concurrency & Actor Discipline
* Follow Tokio best practices:
  * Never hold `std::sync::Mutex` guards across an `.await` point.
  * Use bounded channels (`tokio::sync::mpsc`) with sensible capacities for actor events to prevent unbounded queue growth.
  * Use `tokio::sync::broadcast` or `tokio::sync::watch` for fan-out telemetry (such as 60 FPS Peak/RMS dBFS meter updates to Slint).
* Isolate blocking hardware operations (e.g. 1-Wire DS18B20 750 ms reads) using `tokio::task::spawn_blocking`.

### 2.5. Documentation & Code Hygiene (`M-MODULE-DOCS`, `C-CRATE-DOC`)
* Every module must begin with a top-level `//!` module docstring explaining its architectural responsibility and threading model.
* All public structs, enums, traits, and methods must have docstrings (`///`) including `# Errors` and `# Panics` sections where applicable.
* Maintain clean formatting via `cargo fmt` and zero warnings under `cargo clippy`.
