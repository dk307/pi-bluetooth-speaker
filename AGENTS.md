# Agent Instructions: Raspberry Pi Bluetooth Speaker & DSP Appliance

This document defines the behavioral, architectural, and engineering rules for AI agents working in this repository.

---

## 1. Target Platform & Compilation Environment

* **Target Hardware:** **Raspberry Pi 3 Model B** (Broadcom BCM2837, quad-core ARM Cortex-A53 `aarch64`, running DietPi / Debian 13 Trixie).
* **Primary Rust Target:** `aarch64-unknown-linux-gnu` with Cortex-A53 pipeline scheduling (`-C target-cpu=cortex-a53`) and hardware ARM NEON SIMD vectorization (`-C target-feature=+neon`), configured in [`services/core/.cargo/config.toml`](services/core/.cargo/config.toml).
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
* **Local Reference:** [`services/.guidelines/microsoft-rust-guidelines.md`](services/.guidelines/microsoft-rust-guidelines.md)
* **Service Instructions:** [`services/core/AGENTS.md`](services/core/AGENTS.md)

Before designing types, writing modules, implementing error handling, or writing tests, consult [`services/.guidelines/microsoft-rust-guidelines.md`](services/.guidelines/microsoft-rust-guidelines.md) and ensure all newly created or modified code complies with its rules.

---

## 3. Mandatory Code Coverage Verification

All code developed in this repository must maintain high, verifiable test coverage:
* **Tooling:** Use `cargo llvm-cov` to measure line, function, and region coverage across crates, modules, and CLI utilities:
  ```bash
  cargo llvm-cov --manifest-path services/core/Cargo.toml
  ```
* **Coverage Standards:**
  * Always check code coverage when writing, modifying, or refactoring code, and improve coverage to a reasonable extent (target **>85%–90%+** line and function coverage).
  * Every new driver, peripheral module, mock controller, and standalone CLI utility must include comprehensive unit tests and integration tests covering happy paths, edge cases, domain invariants, error conversions, and failure injection.
  * Code that depends on physical hardware (e.g., Broadcom I2C bus ioctl or 1-Wire sysfs) must isolate hardware-dependent calls behind mockable backends (`new_mocked()`) so all application logic, calculations, error paths, and diagnostic reporting are thoroughly exercised on any development host and CI.

---

## 4. Continuous Engineering Learnings & Self-Improvement Protocol

To eliminate repetitive code review churn across sessions, AI agents working in this repository **MUST** continuously capture and record engineering learnings into the instructions:

* **Mandatory Post-Review Distillation:** Whenever code review feedback (DiffGuard AI review, human reviewer comments, or CI failures) identifies architectural placement issues, anti-patterns, or subtle edge cases, the agent **MUST** distill the root cause and record actionable rules in the relevant `AGENTS.md` before finishing the task.
* **How to Apply Guidelines (Practical Anti-Churn Rules):**
  * **Keep Hardware and Async Runtime Separated:** Generic task join helpers and error-mapping glue (`map_join_result`, `FromJoinError`) belong in `crate::runtime` or `crate::util`, never mixed into peripheral hardware modules (`hardware::*`). Concrete trait implementations belong alongside the respective domain error.
  * **Do Not Create Dead Branches for Tokio JoinError:** In Tokio, `JoinError` is guaranteed to be either a panic or cancellation. Never add artificial trait bounds (such as forcing `From<io::Error>`) just to serve an unreachable branch; use `unreachable!()` with a descriptive message.
  * **Preserve Panic Unwinding on Bug:** Always re-raise thread panics using `std::panic::resume_unwind(err.into_panic())` rather than converting worker crashes into recoverable I/O errors (**M-PANIC-ON-BUG**).
  * **Contextual Cancellation Diagnostics:** Provide task context names in `map_join_result(res, "task context")` to avoid anonymous cancellation traces across concurrent drivers.
  * **Distinguish Task Cancellation from Hardware Faults:** Map aborted tasks to domain `Cancelled` errors containing `Box<str>` messages. Document clearly that callers/supervisors must treat cancellations during graceful shutdown as expected termination, not hardware failures.
  * **Table-Drive CLI Subprocess Tests:** Subprocess CLI tests must never silently pass on CI runners (fail loudly if native binary execution fails in CI). Avoid repetitive boilerplate by using table-driven test cases covering boundary values (`0`, `-1.0`, `nan`, infinity, `u64::MAX` overflow, missing arguments).
  * **Guard Float Types Against NaN:** Always guard float domain parsing and constructors with `val.is_finite()`, as `NaN` evaluates to `false` in range comparisons and bypasses min/max checks.
  * **Ensure Hermetic Tests:** Never depend on host-specific paths like `/dev/null` for negative sysfs tests; always create temporary files in `env::temp_dir()`.
  * **Lint Placement Hygiene:** Place compile-time `assert_send_sync` assertions before `mod tests` to prevent `clippy::items-after-test-module`. Apply `#[expect(...)]` to `let` statements or functions rather than raw expressions. In task tests, avoid post-panic code altogether by mapping the join handle (`task.await.map(...)`).

