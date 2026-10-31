# Agent Instructions: `core` Service & Hardware Utilities

This document defines the behavioral, architectural, and engineering rules for AI agents developing, refactoring, and testing the `core` service (`speaker-core`) and its hardware diagnostic utilities.

---

## 1. Target Platform & Compilation Environment

* **Target Hardware:** **Raspberry Pi 3 Model B** (Broadcom BCM2837, quad-core ARM Cortex-A53 `aarch64`, running DietPi / Debian 13 Trixie).
* **Primary Rust Target:** `aarch64-unknown-linux-gnu` with Cortex-A53 pipeline scheduling (`-C target-cpu=cortex-a53`) and hardware ARM NEON SIMD vectorization with crypto disabled (`-C target-feature=+neon,-aes,-sha2`), configured in [`.cargo/config.toml`](.cargo/config.toml).
* **Building for the Target Platform:**
  Always compile binaries for the Raspberry Pi 3 `aarch64-unknown-linux-gnu` target:
  ```bash
  cargo build --config services/core/.cargo/config.toml --manifest-path services/core/Cargo.toml --target aarch64-unknown-linux-gnu --release --bins
  ```
* **Live Hardware Testing via SSH (`root@192.168.2.110`):**
  * You can test compiled `aarch64` binaries directly on the live Raspberry Pi 3 over SSH at `root@192.168.2.110`.
  * **Strict Non-Destructive Rule:** **Do NOT modify any persistent files, configurations, or running services on the SSH server.** Copy test/utility binaries only to `/tmp/<binary>`, execute them, and remove `/tmp/<binary>` immediately afterward. **Exception:** when the user explicitly asks to deploy/install release binaries, copy them to `/usr/local/bin` (never touch configs or services, and do not restart services unless asked).

---

## 2. Primary Engineering Rule: Microsoft Pragmatic Rust Guidelines

All Rust code in this repository **MUST** strictly adhere to the **Microsoft Pragmatic Rust Guidelines** already downloaded in this repository:
* **Local Reference:** [`services/.guidelines/microsoft-rust-guidelines.md`](../.guidelines/microsoft-rust-guidelines.md)

Before designing types, writing modules, implementing error handling, or writing tests, consult [`microsoft-rust-guidelines.md`](../.guidelines/microsoft-rust-guidelines.md) and ensure all newly created or modified code complies with its rules.

### Key Guidelines Checklist
0. **Mandatory Ecosystem Reuse over Reinvention (Kernel & Crates First):**
   * Never implement raw register/byte-level hardware protocols from scratch without evaluating existing solutions:
     1. **Linux Kernel Subsystems First:** Check if Linux has a kernel driver (e.g., Industrial I/O `IIO` for ambient light/IMUs, `hwmon` for thermals, ALSA for audio DACs, `w1-gpio`/`w1_therm` for 1-Wire). Consuming sysfs/devfs is preferred over bit-banging raw bus transactions in user space.
     2. **Crates.io & `embedded-hal` Drivers Second:** If user-space I2C/SPI is required (via `rppal`), search `crates.io` for existing drivers.
   * **Crates.io Search Is Mandatory (Pre-Implementation Gate):** Before writing ANY driver, state machine, protocol parser, decoder, or debouncer, run concrete `cargo search` queries covering the functionality (e.g., "quadrature", "debounce", "rgb led") and read the vendored source of top candidates in `~/.cargo/registry/src/`. Hand-rolling logic a published crate already implements is a process violation even if trivially correct; document rejection reasons (license, maintenance, hardware mismatch) before hand-rolling any remainder.
   * **Interactive User Decision Mandate:**
     - When multiple options exist, research each (maintenance, license, dependencies, `embedded-hal` compatibility, async support) and present a structured trade-off comparison to the user to choose.
     - When a single option exists, explicitly inform and prompt the user before adding a new crate to `Cargo.toml`.
   * **Domain Wrapping & Mocking:** Wrap the external driver or kernel interface inside `Backend::Hardware` behind the project's domain models (`new_mocked()`, canonical errors, strong newtypes) so host-testability and CI coverage are preserved.
1. **Mockable I/O & Hardware (`M-MOCKABLE-SYSCALLS`, `M-DI-HIERARCHY`):**
   * Hardware drivers and external I/O types must never perform unmockable ad-hoc syscalls or offer `Default` implementations (`#[expect(clippy::new_without_default, reason = "...")]`).
   * Provide inherent mocking via `new_mocked() -> (Self, MockCtrl)` using an internal backend enum (`Sysfs`/`Hardware` vs. `Mock`) so tests can inject readings and faults deterministically without physical hardware.
   * *Project Convention (`M-TEST-UTIL`):* Diagnostic and peripheral drivers intentionally provide inherent `new_mocked()` companion controllers unconditionally in production builds (and `speaker_core::diag::ScopedTempDir` is similarly exported unconditionally) to guarantee that CI coverage verification, binary unit tests, integration tests, and standalone diagnostic utilities execute host-agnostically without physical hardware or separate test-only feature flags.
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
7. **Mandatory Code Coverage Verification (`M-INTEGRATION-TESTS`, `M-TEST-UTIL` & Repository Coverage Mandate):**
   * **Always check code coverage:** Run `cargo llvm-cov --manifest-path services/core/Cargo.toml --target x86_64-unknown-linux-gnu` before completing tasks or submitting changes.
   * **Target Standards:** Maintain **>85%–90%+** line and function coverage across the library, hardware modules, and diagnostic CLI binaries.
   * **Testable CLI Utilities:** Architect CLI diagnostic binaries with modular, testable entrypoints (`fn run_app(sensor: &Sensor, opts: &CliOptions) -> ExitCode`) to enable deterministic unit and integration testing of CLI flags, JSON outputs, session summaries, and error handling without physical hardware.
   * **Thorough Mock & Invariant Coverage:** Test domain invariant bounds, conversions, all mock state controls, and fault injections. Isolated physical hardware syscalls (e.g., `/dev/i2c-1`) must have their surrounding logic completely tested.
8. **Code Simplification & Deduplication (`speaker-core` Architecture):**
   * **Shared Diagnostic & Runtime Infrastructure (`crate::diag`, `crate::runtime`):** Do not copy-paste CLI scaffolding (`CommonCliOptions::parse_common_flag`, `parse_args_with_common`, `OutputFormat`, `NumericStats`, `resolve_cli_opts`, `format_timestamp`, `next_arg_value`, `parse_count_arg`, `parse_interval_arg`, `fahrenheit`, `foot_candles`, `round_2dp`, `ScopedTempDir`) across `src/bin/read_*.rs` utilities or `tests/*.rs`. Use `crate::diag` for shared CLI/test helpers (keeping `diag` independent of `hardware` so the dependency edge is strictly `hardware -> diag`) and `crate::runtime::run_blocking` / `abort_and_map` for `spawn_blocking` task execution and cancellation testing. Shared utilities like `ScopedTempDir` must offer a fallible constructor (`try_new`) alongside infallible test conveniences. Diagnostic loop termination must break consistently on cumulative sample stats (`stats.total_reads() >= limit` / `stats.samples.total_reads() >= limit`) to ensure summary metrics and exit codes stay synchronized.
   * **Make Invalid States Unrepresentable & Avoid Derived Fields:** Use sum types (`I2cDetail::Message | I2cDetail::Source`, `SimSource::Lux | SimSource::Raw`) instead of parallel `Option` fields in error kinds and mock state structs. Eliminate speculative/dead error variants (e.g. `Io` variants when the driver already produces wrapped I2C errors). Do not store derived fields (such as `foot_candles` on `LightDiagnostics`) or duplicate `min`/`max` tracking alongside `NumericStats`. Use strongly-typed newtypes (`PhysicalPin::new(1)`, `PhysicalPin::new(9)`) rather than raw string slices for hardware metadata pin descriptors.
   * **Enforce Parsing & Backtrace-Free Machine Summaries at the Domain Boundary (`M-STRONG-TYPES-GUARD`):** Implement both programmatic constructors (`SensorId::parse`, `I2cAddress::new`, `TryFrom`) and soft user-facing string parsers (`SensorId::parse_str`, `I2cAddress::parse_hex` returning `Result<Self, String>`) sharing a zero-allocation validator on the domain type in `hardware::*`, rejecting signed inputs (`+`, `-`) both before and after any `0x`/`0X` prefix. Expose a single-line `err.summary()` method on canonical errors (`LightError`, `TempError`) via `impl_canonical_error_display!` for `NDJSON` telemetry so `RUST_BACKTRACE=1` never injects multi-line stack traces into machine streams.
   * **Minimal Surface Area & Host-Testable State Machines (`HwState`):** Encapsulate driver state transitions and settle-time calculations on `HwState` (`prepare_read`, `record_power_down`, `record_wake_or_reset`, `record_mode_change`) so they are unit-testable on any host. Prefer `#[derive(Copy)]` on small value-type option structs, bare `?` error propagation via `From`, `Duration::checked_sub` for settle calculations, and struct-update syntax (`..CliOptions::default()`) in tests.
   * **Upfront Datasheet & Concurrency Checklist for `hardware::*`:**
     1. Use datasheet **maximum** conversion times (180 ms), never typical times, when calculating settle delays.
     2. Standardize on Continuous High-Resolution Mode for ambient lighting and display brightness; avoid speculative one-shot or alternate-resolution modes that introduce state-machine races and settle complexity.
     3. Track reset/transition times (`last_transition: Instant`) and explicit sleep state (`powered: bool`) inside `HwState` so callers never have to guess settle times and reads while powered down return a typed `is_powered_down()` error.
     4. Serialize all bus transactions (`power_down`, `reset`, `read_lux`) inside a single `Mutex<HardwareDevice>` wrapping the `embedded-hal` driver; do not nest redundant per-bus mutexes.
     5. Assert that bus numbers passed to platform-specific pinout constructors (`I2cHardwareInfo::rpi3_with_bus_and_address`) match the physical hardware controller modeled.
     6. Re-arm continuous mode conversions on reset per datasheet so readings do not stall.
     7. Mutate internal driver state (`HwState: powered, last_transition`) strictly **after** all fallible bus transactions succeed.
   * **Direct Linux Framebuffer (`/dev/fb0`) Software Rendering (`crate::ui::framebuffer`):**
     1. Broadcom BCM2837 video core exposes `/dev/fb0` with little-endian 32-bit BGRX (`rgba 8/16,8/8,8/0,8/24`). Define `Bgrx8888Pixel` (`#[repr(C)]` layout: blue, green, red, alpha) implementing Slint's `TargetPixel` directly so Slint's `SoftwareRenderer` renders straight into mmapped `/dev/fb0` video RAM with zero intermediate color-conversion passes.
     2. Enable `software-renderer-systemfonts` on `slint` in `Cargo.toml` so Slint software rendering has dynamic font fallback support.
     3. Provide `Framebuffer::new_mocked()` for 100% hermetic host testing without `/dev/fb0` (the mock always models the fixed appliance panel geometry; do not take dynamic width/height parameters — all geometry is compile-time constant and proven via `const _` invariant blocks).
     4. When cross-compiling on newer development hosts targeting Debian/DietPi baseline (`aarch64-unknown-linux-gnu`), pin newer math symbols explicitly to Debian baseline via `#[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))] core::arch::global_asm!(".symver acosf, acosf@GLIBC_2.17");`.
     5. Retain underlying foreign errors (`io::Error`) as typed error fields (`source: io::Error` in `FramebufferErrorKind::Io`) and implement `std::error::Error::source(&self)` per `M-ERRORS-STD-SOURCE`.
     6. Never re-export foreign dependency traits (such as `pub use slint::ComponentHandle;`) per `M-FOREIGN-REEXPORTS`.
     7. Guard appliance service display initialization with `SPEAKER_CORE_HEADLESS=1` in subprocess tests to prevent non-hermetic writes to host workstation displays.
     8. Guard Slint float layout properties against `NaN` using explicit boundary checks (`val >= 0.0 ? ... : 0`) to prevent widget layout collapse.
     9. **Dynamic Math Expressions over Hardcoded Pixel Offsets (`M-SLINT-MATH-LAYOUT`):** In Slint UI markup, component layouts, and geometry calculations, always use dynamic mathematical expressions (`(parent.height - self.height) / 2`, `(parent.width - self.width) / 2`, layout centering containers `VerticalLayout { alignment: center; ... }`, or relative percentage/stretch properties) to compute positions, alignments, and sizes. Never manually pre-calculate pixel offsets and hardcode literal numbers into `.slint` files (e.g. `y: 6px`, `y: -5px`, `y: 24px`, or manual top-alignment offsets). Hardcoded literal numbers break whenever parent bounds, padding, themes, or font scales change, introducing subtle visual regressions. Let Slint compute the geometry dynamically.

---

## 3. Zero-Retry Pre-Push Gate & Continuous Learning

AI agents working on `speaker-core` **MUST** follow the repository-wide **First-Pass PR Quality, Code Simplification & Continuous Learning Protocol** defined canonically in [root `AGENTS.md` (§4)](../../AGENTS.md).

* **Service-Specific Gate Sequence:** Before every push, execute in order:
  1. `cargo fmt --manifest-path services/core/Cargo.toml -- --check`
  2. `cargo clippy --manifest-path services/core/Cargo.toml --all-targets -- -D warnings`
  3. `cargo llvm-cov --manifest-path services/core/Cargo.toml --target x86_64-unknown-linux-gnu --fail-under-lines 90 --fail-under-functions 90`
  4. `cargo build --config services/core/.cargo/config.toml --manifest-path services/core/Cargo.toml --target aarch64-unknown-linux-gnu --release --bins`
* **Protocol & Review Rules:** Consult [root `AGENTS.md` (§4.1–§4.3)](../../AGENTS.md) for the 3-Tier review triage classification, mandatory critical analysis of code review comments before fixing (only fix what genuinely makes sense), strict PR blast radius containment, code anti-bloat mandate, and continuous post-review distillation requirements.
