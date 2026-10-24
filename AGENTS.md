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

## 4. First-Pass PR Quality, Code Simplification & Continuous Learning Protocol

To prevent multi-iteration PR review churn and excessive token/CI consumption, AI agents working in this repository **MUST** follow these engineering and review rules:

### 4.1 Zero-Retry Pre-Push Gate (Never Use CI as a Linter)
* **Mandatory Local Verification Before Every Push:** Never push a commit without first passing all local gates in order:
  1. `cargo fmt --manifest-path services/core/Cargo.toml -- --check`
  2. `cargo clippy --manifest-path services/core/Cargo.toml --all-targets -- -D warnings`
  3. `cargo llvm-cov --manifest-path services/core/Cargo.toml --fail-under-lines 90 --fail-under-functions 90`
  4. `cargo build --config services/core/.cargo/config.toml --manifest-path services/core/Cargo.toml --target aarch64-unknown-linux-gnu --release --bins`
  5. If any CI script (`.github/scripts/*.py`) or workflow is touched, execute and verify it locally against real repository inputs before committing.
* **Batch All Fixes & Sweep by Bug Class:** Never trickle-push partial fixes. When addressing code review feedback, read the *entire* review (Critical Issues, Compliance Findings, Simplification Opportunities, and Recommendations) and resolve 100% of items in a single commit. For every defect found in one place, sweep all sibling methods, binaries, and tests for the same class of bug before committing.
* **Upfront Hardware Datasheet & Concurrency Audit:** Before writing or modifying a hardware driver, verify:
  1. Maximum (not typical) datasheet conversion/integration times.
  2. Power-state wake-up sequences (e.g., `CMD_POWER_ON` before mode commands) and first-conversion settle times (`last_transition: Mutex<Instant>`).
  3. Transaction-level locking (`op_lock`) across all state-mutating and multi-step bus operations on cloned handles.
  4. Hardware controller pinout accuracy (assert valid bus numbers for fixed GPIO pin descriptors).

### 4.2 Code Simplification & Anti-Bloat Mandate (Authoring & Review)
* **Actively Minimize Code Volume & Duplication:** Every line of duplicated boilerplate doubles the surface area for bugs and review churn. Extract shared helpers (CLI argument/interval parsing, session statistics, timestamp formatting, test temp-dir cleanup guards) into shared modules instead of copy-pasting across binaries or test files.
* **Prefer Direct, Idiomatic Constructs:**
  * Use `?` via `From<ForeignError>` instead of verbose `.map_err(DomainError::from)`.
  * Use `Duration::checked_sub` / `saturating_*` instead of manual conditional subtraction.
  * Use `const` pin initializers (`BcmPin::new(...)`) and compile-time `const _: () = { assert!(...); }` proofs instead of runtime checks where possible.
  * Avoid speculative abstractions, dead trait bounds, or redundant wrapper layers.
* **Review Requirement — How Code Can Be Simplified:** Every self-review and automated PR review **MUST** explicitly evaluate how the code can be simplified: identifying duplicated logic to unify, over-engineered abstractions to flatten, verbose error/lock plumbing to streamline, and lines of code that can be removed while preserving safety and domain invariants.

### 4.3 Continuous Post-Review Distillation & Anti-Churn Rules
* **Mandatory Post-Review Distillation:** Whenever code review feedback (DiffGuard AI review, human reviewer comments, or CI failures) identifies architectural placement issues, anti-patterns, or subtle edge cases, distill the root cause and record actionable rules in [`AGENTS.md`](AGENTS.md) / [`services/core/AGENTS.md`](services/core/AGENTS.md) before finishing the task.
* **How to Apply Guidelines (Practical Anti-Churn Rules):**
  * **Keep Hardware and Async Runtime Separated:** Generic task join helpers and error-mapping glue (`map_join_result`, `FromJoinError`) belong in `crate::runtime` or `crate::util`, never mixed into peripheral hardware modules (`hardware::*`). Concrete trait implementations belong alongside the respective domain error.
  * **Do Not Create Dead Branches for Tokio JoinError:** In Tokio, `JoinError` is guaranteed to be either a panic or cancellation. Never add artificial trait bounds (such as forcing `From<io::Error>`) just to serve an unreachable branch; use `unreachable!()` with a descriptive message.
  * **Preserve Panic Unwinding on Bug:** Always re-raise thread panics using `std::panic::resume_unwind(err.into_panic())` rather than converting worker crashes into recoverable I/O errors (**M-PANIC-ON-BUG**).
  * **Contextual Cancellation Diagnostics:** Provide task context names in `map_join_result(res, "task context")` to avoid anonymous cancellation traces across concurrent drivers.
  * **Distinguish Task Cancellation from Hardware Faults:** Map aborted tasks to domain `Cancelled` errors containing `Box<str>` messages. Document clearly that callers/supervisors must treat cancellations during graceful shutdown as expected termination, not hardware failures.
  * **Table-Drive CLI Subprocess Tests:** Subprocess CLI tests must never silently pass on CI runners (fail loudly if native binary execution fails in CI). Avoid repetitive boilerplate by using table-driven test cases covering boundary values (`0`, `-1.0`, `nan`, infinity, `u64::MAX` overflow, missing arguments).
  * **Guard Float Types Against NaN:** Always guard float domain parsing and constructors with `val.is_finite()`, as `NaN` evaluates to `false` in range comparisons and bypasses min/max checks.
  * **Ensure Hermetic Tests:** Never depend on host-specific paths like `/dev/null` for negative sysfs tests; always create temporary files in `env::temp_dir()`.
  * **Lint Placement Hygiene:** Place compile-time `assert_send_sync` assertions before `mod tests` to prevent `clippy::items-after-test-module`, and place `const` items before statements in functions to prevent `clippy::items-after-statements`. Apply `#[expect(...)]` to `let` statements or functions rather than raw expressions. In task tests, avoid post-panic code altogether by mapping the join handle (`task.await.map(...)`).
  * **Domain String Invariant Precision:** Strong domain types (like 1-Wire `SensorId`) must enforce hardware-accurate character sets (`is_ascii_hexdigit()` vs `is_ascii_alphanumeric()`) matching the domain error message and hardware spec.
  * **Production Task Mapping Tests:** Don't test runtime traits solely with dummy errors; add tests verifying cancelled Tokio tasks mapped into production error types (`TempError`, `LightError`) asserting `is_cancelled()` predicates and contextual cancellation messages.
  * **Layered Bus Locking on Shared Clones:** Peripheral drivers supporting concurrent clones must serialize all bus transactions (including `power_down` and `reset`) behind transaction mutexes (`op_lock`) to prevent bus interleaving during multi-step measurement conversions.
  * **Fail-Loud CI Action Scripting:** CI scripts injecting runtime shims or patching actions must verify that patches were successfully applied and exit non-zero immediately on missing targets.
  * **Compile-Time Hardware Pin Initializers:** Use `const` initializers (`BcmPin::new(...)`, `PhysicalPin::new(...)`) for fixed platform pinouts to eliminate runtime panic paths from production metadata structures.
  * **Hardware Settle-Time Contract & Transition Tracking:** Hardware sensor drivers must track mode transitions (`last_transition: Mutex<Instant>`) and guarantee settle/integration times before returning conversion results, rather than relying on application or CLI layers to guess or sleep.
  * **Datasheet Conversion Time Bounds:** Always use the datasheet maximum conversion/integration time rather than typical integration times when waiting on one-shot or continuous conversions to prevent races against slower silicon.
  * **Clean Error Propagation with `?`:** When domain error types implement `From<ForeignError>`, always leverage the `?` operator directly instead of verbose `.map_err(DomainError::from)`.
  * **Soften User-Facing Input Errors:** Differentiate programmer backtraces from end-user CLI argument validation errors; input rejections should return clean, concise messages without stack backtraces.
  * **Dynamic Bus Device Paths via Box<str>:** Use heap-allocated `Box<str>` for dynamic system paths (such as `/dev/i2c-{bus}`) rather than rigid `match` arms returning `&'static str`.
  * **Subprocess Probe Timeout Discipline:** Subprocess probes (such as checking whether a binary is executable via `can_exec_binary`) must never use unbounded blocking calls (`Command::output()`). Always run inside a deadline loop with an explicit timeout (e.g., 10s) and kill the spawned child if it hangs, preventing infinite CI hangs if a binary regresses to a loop.
  * **Peripheral Bus Pinout Accuracy:** Peripheral metadata structures (`I2cHardwareInfo`, `GpioPinInfo`) must only claim pinout descriptors matching the physical hardware bus. Validate/assert that caller-provided bus numbers match the hardware controller whose pinout is modeled.
  * **Fence-Aware Markdown Transformations:** Markdown processing scripts must never apply regex substitutions or newline collapses globally across full document text (`flags=re.DOTALL`). All comment stripping and whitespace normalization must occur strictly on lines outside fenced code blocks, preserving code examples verbatim.
  * **Action Bundle Monkey-Patch Upstream Tracking:** CI patching scripts must use explicit conditional checks (`sys.exit(1)`) rather than Python `assert` (which is stripped under `-O`), and must record pinned commit hashes and upstream issue links.
  * **Compile-Time Constant Proofs:** Use compile-time `const _: () = { ... }` invariant assertions to mathematically prove type limits (such as `(Illuminance::MAX_LUX * 1000.0) as u128 <= u32::MAX as u128`).
  * **Strict Hex Address Validation:** Hex address parsers must reject signed inputs (`+`, `-`) before passing to radix parsers, as signs are non-standard in device address specifications.

