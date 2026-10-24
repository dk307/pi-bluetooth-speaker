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
   * *Project Convention (`M-TEST-UTIL`):* Diagnostic and peripheral drivers intentionally provide inherent `new_mocked()` companion controllers unconditionally in production builds to guarantee that CI coverage verification, integration tests, and standalone diagnostic utilities execute host-agnostically without physical hardware.
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
   * **Always check code coverage:** Run `cargo llvm-cov --manifest-path services/core/Cargo.toml` before completing tasks or submitting changes.
   * **Target Standards:** Maintain **>85%–90%+** line and function coverage across the library, hardware modules, and diagnostic CLI binaries.
   * **Testable CLI Utilities:** Architect CLI diagnostic binaries with modular, testable entrypoints (`fn run_app(sensor: &Sensor, opts: &CliOptions) -> ExitCode`) to enable deterministic unit and integration testing of CLI flags, JSON outputs, session summaries, and error handling without physical hardware.
   * **Thorough Mock & Invariant Coverage:** Test domain invariant bounds, conversions, all mock state controls, and fault injections. Isolated physical hardware syscalls (e.g., `/dev/i2c-1`) must have their surrounding logic completely tested.
8. **Applied Engineering Hardening & Anti-Churn Patterns:**
   * **Module Boundaries (Runtime vs Hardware):**
     * Generic async runtime helpers (`map_join_result`, `FromJoinError`) belong in `crate::runtime` with dedicated unit tests. Do not place Tokio runtime glue in `crate::hardware`.
     * Concrete implementations (`impl FromJoinError for <Error>`) must live next to the respective error type in its own module.
   * **Tokio Task Join Semantics:**
     * `tokio::task::JoinError` represents either worker panics (`is_panic()`) or task cancellations (`is_cancelled()`).
     * Always propagate panics with `std::panic::resume_unwind(join_err.into_panic())`. Never convert thread crashes into recoverable I/O errors (**M-PANIC-ON-BUG**).
     * Provide diagnostic context in `map_join_result(res, "task context")` so concurrent cancellations produce identifiable messages (e.g. `"temperature read task was cancelled"`).
     * Do not attach `From<io::Error>` or other unrelated supertraits to `FromJoinError` to serve an unreachable branch; mark dead fallbacks with `unreachable!()`.
     * In task panic tests, avoid dead code and expression attributes after `panic!()`: reshape the result with `task.await.map(Ok::<_, ErrorType>)`.
   * **Float Validation & NaN Protection:**
     * All float domain constructors and CLI interval parsers must check `val.is_finite()` before range comparison, as `NaN` evaluates to `false` for all inequality checks and would otherwise bypass bounds.
   * **Cancellation Domain Semantics:**
     * Map cancelled tasks to domain `Cancelled { message: Box<str> }` variants per **M-BOX-DST**.
     * Document in `is_cancelled()` predicates that cancellations represent expected termination during graceful daemon shutdown and must not be retried or logged as hardware faults by callers or supervisor loops.
   * **Hermetic Tests & Subprocess Assertions:**
     * Never depend on `/dev/null` or ambient system paths; generate temporary files/directories inside `std::env::temp_dir()`.
     * In `cli_tests.rs`, helper `can_exec_binary` must assert that binaries successfully run under CI (i.e., native Linux runners must never silently skip CLI tests).
     * Use table-driven tests `(args, expected_stderr_fragment)` to cover negative values (`-1.0`), infinities (`1e400`), zero rejection, large number overflows, and missing arguments without repetitive boilerplate.
   * **Clippy & Lint Placement Hygiene:**
     * In stable Rust, `#[expect(...)]` cannot annotate raw expressions; attach to statements (`let`), items, or functions.
     * Place compile-time `const _: () = { assert_send_sync::<T>(); };` checks before `#[cfg(test)] mod tests` to avoid `clippy::items-after-test-module`.
   * **Domain Invariant Validation & Hex Character Sets:**
     * When validating sensor hardware IDs (like DS18B20 `28-*`), enforce the precise hardware character set (`is_ascii_hexdigit()`, not `is_ascii_alphanumeric()`) matching the domain error message and 1-Wire hardware spec.
   * **Production Runtime Cancellation Tests:**
     * Ensure production error types (`TempError`, `LightError`) implementing `FromJoinError` have dedicated tests verifying cancelled Tokio tasks mapped through `map_join_result` yield `is_cancelled() == true` with the expected contextual message.
   * **Layered Bus Locking & Clone Concurrency:**
     * Any hardware driver allowing cloneable handles must acquire transaction serialization locks (`op_lock`) for all bus state mutations (`power_down`, `reset`, `set_mode`, `read_raw_word`) to prevent command collisions across concurrent tasks.
   * **Compile-Time Pin Initializers:**
     * Use `const` initializers (`BcmPin::new(2)`, `PhysicalPin::new(3)`) for static hardware pin mappings rather than runtime fallible calls (`from_u8(...).expect(...)`).

---

## 3. Continuous Learning & Self-Improvement Protocol

AI agents working on this codebase **MUST** maintain and update these instructions:
* After any code review (whether from DiffGuard AI review, human reviewer comments, or CI compiler/lint errors), review what caused churn or rework.
* If a pattern was corrected or an architectural principle clarified, **immediately update this document** with the concrete rule and practical example before marking the task complete.
* Ensure future agents inherit the learnings so that recurring review comments and anti-patterns are permanently avoided.
