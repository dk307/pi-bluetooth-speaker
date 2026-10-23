# Agent Instructions: `core` Service & Hardware Utilities

This document defines the behavioral, architectural, and engineering rules for AI agents developing, refactoring, and testing the `core` service (`speaker-core`) and its hardware diagnostic utilities.

---

## 1. Target Platform & Compilation Environment

* **Target Hardware:** **Raspberry Pi 3 Model B** (Broadcom BCM2837, quad-core ARM Cortex-A53 `aarch64`, running DietPi / Debian 13 Trixie).
* **Primary Rust Target:** `aarch64-unknown-linux-gnu` with Cortex-A53 pipeline scheduling (`-C target-cpu=cortex-a53`) and hardware ARM NEON SIMD vectorization (`-C target-feature=+neon`), configured in [`.cargo/config.toml`](.cargo/config.toml).
* **Building for the Target Platform:**
  Always compile binaries for the Raspberry Pi 3 `aarch64-unknown-linux-gnu` target:
  ```bash
  cargo build --config services/core/.cargo/config.toml --manifest-path services/core/Cargo.toml --target aarch64-unknown-linux-gnu --release --bins
  ```
* **Live Hardware Testing via SSH (`root@192.168.2.110`):**
  * You can test compiled `aarch64` binaries directly on the live Raspberry Pi 3 over SSH at `root@192.168.2.110`.
  * **Strict Non-Destructive Rule:** **Do NOT modify any persistent files, configurations, or running services on the SSH server.** Copy test/utility binaries only to `/tmp/<binary>`, execute them, and remove `/tmp/<binary>` immediately afterward.

---

## 2. Primary Engineering Rule: Microsoft Pragmatic Rust Guidelines

All Rust code in this repository **MUST** strictly adhere to the **Microsoft Pragmatic Rust Guidelines** already downloaded in this repository:
* **Local Reference:** [`services/.guidelines/microsoft-rust-guidelines.md`](../.guidelines/microsoft-rust-guidelines.md)

Before designing types, writing modules, implementing error handling, or writing tests, consult [`microsoft-rust-guidelines.md`](../.guidelines/microsoft-rust-guidelines.md) and ensure all newly created or modified code complies with its rules.

### Key Guidelines Checklist
1. **Mockable I/O & Hardware (`M-MOCKABLE-SYSCALLS`, `M-DI-HIERARCHY`):**
   * Hardware drivers and external I/O types must never perform unmockable ad-hoc syscalls or offer `Default` implementations (`#[expect(clippy::new_without_default, reason = "...")]`).
   * Provide inherent mocking via `new_mocked() -> (Self, MockCtrl)` using an internal backend enum (`Sysfs`/`Hardware` vs. `Mock`) so tests can inject readings and faults deterministically without physical hardware.
2. **Three-Fold Purpose for Hardware Classes:**
   Every hardware peripheral abstraction in `speaker_core::hardware` must serve three purposes:
   * **Core Service:** Cheaply cloneable (`Arc`-backed per `M-SERVICES-CLONE`), `Send + Sync` (`M-TYPES-SEND`), with non-blocking `async` methods that isolate slow/blocking hardware I/O on `tokio::task::spawn_blocking`.
   * **Tests & Mocking:** Controllable via its companion mock handle (`new_mocked()`) and/or custom sysfs paths for unit and integration testing (`tests/`).
   * **Diagnostic CLI Utilities (`src/bin/*.rs`):** Usable directly in standalone Rust utilities that print hardware GPIO pinout metadata (`GpioPinInfo`: BCM pin, physical header pin, bus protocol, DT overlay) alongside live telemetry.
3. **Strong Typing & Invariant Guards (`M-STRONG-TYPES`, `M-STRONG-TYPES-GUARD`, `C-NEWTYPE`):**
   * Avoid primitive obsession. Use domain newtypes (`BcmPin`, `PhysicalPin`, `Temperature`, `SensorId`, `VolumePercent`, `AttenuationDb`) that guard their invariants at construction time via fallible constructors (`from_*`, `parse`, `TryFrom`, `FromStr`).
   * Use `Path` / `PathBuf` for filesystem paths (`M-STRONG-TYPES`) and `Box<str>` / `Box<[T]>` for immutable owned sequences (`M-BOX-DST`).
4. **Canonical Error Structs (`M-ERRORS-CANONICAL-STRUCTS`, `M-FROM-ERROR`, `M-PANIC-IS-STOP`):**
   * Subsystem errors must be canonical structs (e.g., `TempError`, `PinError`) wrapping a non-public `ErrorKind` and a captured `std::backtrace::Backtrace`, exposing `is_*()` query predicates and implementing `Debug`, `Display`, and `std::error::Error`.
   * Never use `.unwrap()` or `.expect()` on fallible runtime I/O paths.
5. **Single Item Path & Clean Exports (`M-SINGLE-ITEM-PATH`, `M-NO-GLOB-REEXPORTS`, `M-DOC-INLINE`):**
   * Each public item must be reachable through a single canonical path. Use `pub(crate) mod ...` with explicit `#[doc(inline)] pub use ...` re-exports rather than wildcard globs.
6. **Documentation, Lints & Static Verification (`M-MODULE-DOCS`, `M-CANONICAL-DOCS`, `M-FIRST-DOC-SENTENCE`, `M-LINT-OVERRIDE-EXPECT`, `M-STATIC-VERIFICATION`):**
   * Every module must begin with `//!` documentation; every public item must have `///` documentation whose first sentence is a single line of ~15 words or fewer, followed by `# Examples`, `# Errors`, and `# Panics` sections where applicable. Do not write `# Parameters` tables or meta design journals (`M-NO-META-DESIGN-DOCUMENTATION`).
   * Use `#[expect(..., reason = "...")]` instead of `#[allow(...)]` for lint overrides.
   * All code must format cleanly with `cargo fmt --check` and pass `cargo clippy --all-targets -- -D warnings` with zero warnings.
7. **Mandatory Code Coverage Verification (`M-TEST-COVERAGE`):**
   * **Always check code coverage:** Run `cargo llvm-cov --manifest-path services/core/Cargo.toml` before completing tasks or submitting changes.
   * **Target Standards:** Maintain **>85%–90%+** line and function coverage across the library, hardware modules, and diagnostic CLI binaries.
   * **Testable CLI Utilities:** Architect CLI diagnostic binaries with modular, testable entrypoints (`fn run_app(sensor: &Sensor, opts: &CliOptions) -> ExitCode`) to enable deterministic unit and integration testing of CLI flags, JSON outputs, session summaries, and error handling without physical hardware.
   * **Thorough Mock & Invariant Coverage:** Test domain invariant bounds, conversions, all mock state controls, and fault injections. Isolated physical hardware syscalls (e.g., `/dev/i2c-1`) must have their surrounding logic completely tested.
