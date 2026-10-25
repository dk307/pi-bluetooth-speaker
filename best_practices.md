# Repository Best Practices: Microsoft Pragmatic Rust & Appliance Guidelines

This document defines the coding standards and review rules for all Rust code in this repository, derived from the **Microsoft Pragmatic Rust Guidelines** (`services/.guidelines/microsoft-rust-guidelines.md`) and appliance architecture rules (`AGENTS.md`).

---

## 1. Mockable Hardware & System Calls (`M-MOCKABLE-SYSCALLS`, `M-DI-HIERARCHY`)
- **DO** isolate all hardware bus transactions (I2C, SPI, 1-Wire sysfs) behind internal backend enums (`Backend::Hardware` vs. `Backend::Mock`).
- **DO** provide inherent companion mock controllers via `new_mocked() -> (Self, MockCtrl)` so all drivers, CLI utilities, and integration tests can execute deterministically on any development host and CI runner without physical Raspberry Pi hardware.
- **DON'T** perform ad-hoc direct syscalls or open `/dev/i2c-*` / `/sys/bus/w1/*` inside application logic without mock fallback.
- **DON'T** provide `Default` trait implementations for hardware peripheral types that attempt to open physical hardware buses (`#[expect(clippy::new_without_default, reason = "...")]`).

## 2. Strong Typing & Invariant Guards (`M-STRONG-TYPES`, `M-STRONG-TYPES-GUARD`, `C-NEWTYPE`)
- **DO** avoid primitive obsession by wrapping domain values in dedicated newtypes (`BcmPin`, `PhysicalPin`, `SensorId`, `I2cAddress`, `Temperature`, `Illuminance`).
- **DO** validate domain invariants strictly at construction time via fallible constructors (`parse`, `TryFrom`, `from_*`).
- **DO** reject invalid, signed (`+`, `-`), or out-of-range inputs at the domain boundary before parsing radix or storing values.
- **DO** use `Path` and `PathBuf` for filesystem paths, not `String`.
- **DON'T** use raw primitive floats (`f32`, `f64`) without guarding against `NaN` and `Infinity` via `.is_finite()`.

## 3. Canonical Error Structs & Clean Propagation (`M-ERRORS-CANONICAL-STRUCTS`, `M-FROM-ERROR`)
- **DO** represent subsystem errors as canonical structs (`TempError`, `LightError`) wrapping a non-public `ErrorKind` and captured `std::backtrace::Backtrace`.
- **DO** implement `is_*()` query predicates on error structs (e.g. `is_not_found()`, `is_cancelled()`, `is_hardware()`).
- **DO** implement `From<ForeignError>` and propagate errors using the idiomatic `?` operator.
- **DON'T** use manual `.map_err(DomainError::from)` when `From` can be implemented.
- **DON'T** use `.unwrap()` or `.expect()` on fallible runtime I/O paths.
- **DON'T** include speculative or dead error enum variants for errors the driver never produces.

## 4. Panic Discipline (`M-PANIC-IS-STOP`, `M-PANIC-ON-BUG`)
- **DO** treat panics as unrecoverable program stops or programmer bugs, never for expected runtime or hardware I/O failures.
- **DO** provide clear, helpful panic messages when an invariant or precondition check fails (`M-PANIC-MESSAGE`).
- **DO** re-raise thread/task worker panics using `std::panic::resume_unwind(err.into_panic())` rather than converting worker crashes into recoverable I/O errors.

## 5. Memory & Performance (`M-BOX-DST`, `M-AVOID-WRAPPERS`, `M-SIMPLE-ABSTRACTIONS`)
- **DO** use `Box<str>` and `Box<[T]>` for immutable owned sequences and dynamic system paths rather than `String` or `Vec<T>`.
- **DO** flatten nested abstractions, redundant smart pointers, and multi-layered locks.
- **DO** consolidate co-mutated driver state into a single `Mutex<HwState>` or `Mutex<HardwareDevice>` rather than multiple fragmented mutexes.
- **DON'T** store derived or redundant fields (e.g. tracking `min`/`max` separately alongside `NumericStats`, or storing `foot_candles` alongside `lux`).

## 6. Public Surface & Documentation (`M-SINGLE-ITEM-PATH`, `M-NO-GLOB-REEXPORTS`, `M-MODULE-DOCS`, `M-DOC-INLINE`)
- **DO** ensure each public item is reachable through a single canonical path.
- **DO** mark `pub use` items with `#[doc(inline)]`.
- **DO** begin every module with `//!` documentation.
- **DO** provide `///` documentation on all public items whose first sentence is a concise summary (~15 words or fewer), followed by `# Examples`, `# Errors`, and `# Panics` where applicable.
- **DON'T** use wildcard glob re-exports (`pub use foo::*`).
- **DON'T** write `# Parameters` tables or meta design documentation.

## 7. Static Verification & Lint Hygiene (`M-STATIC-VERIFICATION`, `M-LINT-OVERRIDE-EXPECT`)
- **DO** use `#[expect(..., reason = "...")]` instead of `#[allow(...)]` for lint overrides, stating the technical justification.
- **DO** ensure code compiles cleanly with `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.
- **DO** use compile-time `const _: () = { assert!(...); }` proofs for mathematical type invariants where possible.

## 8. Concurrency & Service Architecture (`M-SERVICES-CLONE`, `M-TYPES-SEND`)
- **DO** design core service and peripheral types to be cheaply cloneable (`Arc`-backed) and `Send + Sync`.
- **DO** offload blocking hardware I/O calls to `tokio::task::spawn_blocking` via `crate::runtime::run_blocking`.
- **DO** map aborted tasks to domain `Cancelled` errors containing task context names so cancellations during graceful shutdown are distinguishable from hardware faults.
- **DON'T** add dead error conversion branches for `tokio::task::JoinError`; `JoinError` is only cancellation or panic.
