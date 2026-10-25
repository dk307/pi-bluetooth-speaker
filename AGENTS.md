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

### 4.1 Zero-Retry Pre-Push Gate & Review Triage
* **Mandatory Local Verification Before Every Push:** Never push a commit without first passing all local gates in order:
  1. `cargo fmt --manifest-path services/core/Cargo.toml -- --check`
  2. `cargo clippy --manifest-path services/core/Cargo.toml --all-targets -- -D warnings`
  3. `cargo llvm-cov --manifest-path services/core/Cargo.toml --fail-under-lines 90 --fail-under-functions 90`
  4. `cargo build --config services/core/.cargo/config.toml --manifest-path services/core/Cargo.toml --target aarch64-unknown-linux-gnu --release --bins`
  5. If any CI script (`.github/scripts/*.py`) or workflow is touched, execute its unit tests (`python3 -m unittest discover -s .github/scripts -p "test_*.py" -v`) and verify prompt generation locally against real repository inputs before committing.
* **Strict PR Scope Boundaries (Blast Radius Containment):** A feature or bugfix PR must touch **only** the files within its declared scope. **Never refactor already-merged, working sibling drivers or CLI utilities** (e.g., modifying `read_temp` during a `read_light` PR). The rule *"sweep by bug class"* applies strictly to the new files and tests introduced in the PR.
* **Review Triage Classification (The 3-Tier Rule to Stop Review Churn):**
  Automated review bots (DiffGuard, CodeRabbit) will always generate critiques. Agents must classify feedback into three strict tiers:
  - **Tier 1 (Correctness & Safety):** Hardware protocol violations, data races, error-path state desyncs, and security flaws. **Must fix** immediately in a single batched commit.
  - **Tier 2 (Hard Gates):** Broken builds, clippy warnings, test failures, and coverage `< 90%`. **Must fix** immediately before pushing.
  - **Tier 3 (Advisory Simplifications & Scope Creep):** Subjective refactorings (e.g., *"extract `poll_loop`"*, *"unify main functions"*, *"change print formatting asymmetry in sibling CLI"*). **DO NOT TOUCH in the active feature PR.** Defer them to a separate dedicated tech-debt PR.
* **Critical Analysis of Code Review Comments (Only Fix if it Makes Sense):**
  Automated review bots (DiffGuard, CodeRabbit) generate extensive, multi-page critiques. Agents **must critically analyze all review comments before writing code or modifying files**. Never reflexively implement suggestions without verification:
  1. *Factual & Domain Soundness:* Review bots frequently hallucinate invariants, misinterpret async runtime semantics, or misunderstand physical hardware behavior. Verify against datasheets and kernel subsystem specs first.
  2. *Precedence of Codified Guidelines:* Never accept a review recommendation that contradicts an explicit rule in `AGENTS.md` (e.g., a bot suggesting an independent loop counter when the rules explicitly mandate cumulative stats termination). Codified repository rules **always** override automated review critiques.
  3. *Blast Radius & Non-Essential Changes:* If a comment suggests subjective style changes, speculative error variants, or refactoring stable sibling components, **reject it**. Do not fix issues unless they genuinely impact correctness, safety, or hard CI gates.
  4. *Simplicity Over Ceremony:* If a bot suggests adding layers of traits, indirection, or verbose wrapper boilerplate, evaluate whether it simplifies the code. If it adds net complexity without fixing a real bug, **decline it**.
* **Zero-Trust Verification of External AI Artifacts:** Never trust or copy-paste commit SHAs, package versions, or URLs suggested by an AI reviewer without verifying them against live registries or remotes first (e.g., running `git ls-remote <url> <tag>` or `cargo search <crate>`).
* **Transactional State Mutation:** In hardware drivers, internal state (`HwState: powered, mode, last_transition`) must be mutated **strictly after all fallible bus writes have completed successfully**. Never mutate driver state between or before fallible bus transactions.

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
  * **Ensure Hermetic Tests:** Never depend on host-specific paths like `/dev/null` for negative sysfs tests or invoke default hardware constructors (`LightSensor::new()`) that open real `/dev/i2c-*` buses on developer hosts; always use `ScopedTempDir`, invalid-bus error probes (`with_bus_and_address(99, ...)`), or `new_mocked()`.
  * **Consolidated Driver State Mutexes:** Group co-mutated driver state fields (`mode`, `powered`, `last_transition`) into a single `Mutex<HwState>` struct rather than separate per-field mutexes.
  * **Idempotent CI Patch Scripts:** Action bundle patchers must be idempotent for persistent/self-hosted runners (treating already-patched bundles that pass `node --check` as success) and raise errors on unterminated HTML comments during markdown prompt generation.
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
  * **Compile-Time Constant Proofs:** Use compile-time `const _: () = { ... }` invariant assertions to mathematically prove type limits (such as `(Illuminance::MAX_LUX * 1000.0) as u128 <= u32::MAX as u128`). When primitive `as` casts are required inside `const` blocks because `From::from` is not `const fn`, document that constraint alongside `#[expect(...)]`.
  * **Strict Hex Address Validation at the Domain Boundary:** Place string/hex parsers (`I2cAddress::parse_hex`, `SensorId::parse`) and `TryFrom` implementations directly on the domain newtype (`M-STRONG-TYPES-GUARD`) rather than in CLI binaries, and reject signed inputs (`+`, `-`) both before and after any `0x`/`0X` prefix (`"0x+23"`, `"+0x23"`) as well as non-hexadecimal bytes before passing to radix parsers.
  * **Backtrace-Free Machine Telemetry (`summary()` vs `Display`):** Canonical error `Display` implementations append multi-line backtraces when `RUST_BACKTRACE=1`. Never serialize `err.to_string()` into `NDJSON` streams; provide a single-line `err.summary()` method (sharing a private `fmt_kind` helper with `Display`) for machine-oriented JSON output while preserving full `Display` backtraces for human `stderr`.
  * **Make Invalid States Unrepresentable with Sum Types:** Never model mutually exclusive states with multiple `Option` fields or parallel values (e.g., `message: Option<Box<str>>` + `source: Option<Error>`, or `raw_word: u16` + `desired_lux: Option<f32>`). Use explicit enums (`I2cDetail::Message | I2cDetail::Source`, `SimSource::Lux | SimSource::Raw`) so empty or conflicting states cannot exist.
  * **Centralize CLI & Async Runtime Helpers (`crate::diag`, `crate::runtime`):** Extract shared CLI primitives (`CommonCliOptions::parse_common_flag`, `OutputFormat`, `NumericStats`, `resolve_cli_opts`, `next_arg_value`, `parse_count_arg`, `parse_interval_arg`, `fahrenheit`, `foot_candles`, `round_2dp`, `ScopedTempDir`) into `crate::diag` and blocking-task spawners (`run_blocking`, `abort_and_map`) into `crate::runtime`. Keep `crate::diag` dependency-free from `crate::hardware` (one-directional `hardware -> diag`), never store derived fields (like `foot_candles` on `LightDiagnostics`) or duplicate `min`/`max` fields alongside `NumericStats`, and guard `NumericStats::record_success` with `assert!(value.is_finite())`.
  * **Consistent Domain Parser Conventions:** Pair programmatic fallible constructors (`SensorId::parse`, `I2cAddress::new`, `TryFrom` returning canonical domain errors with `Backtrace`) with soft user-input string parsers (`SensorId::parse_str`, `I2cAddress::parse_hex` returning `Result<Self, String>`) sharing a zero-allocation boolean/slice validator on the domain type itself so CLI binaries neither capture wasted backtraces on user typos nor allocate discarded error strings.
  * **Host-Testable Hardware State Machines (`HwState`):** Encapsulate driver state transitions and settle-time math (`prepare_read`, `record_power_down`, `record_wake_or_reset`, `record_mode_change`) as pure methods on `HwState` so settle calculations and power-state transitions are 100% unit-testable on any host without physical I2C/sysfs hardware.
  * **Harden & Unit-Test CI Scripts:** Subprocess invocations in Python CI scripts (such as `node --check`) must specify an explicit `timeout` and catch `subprocess.TimeoutExpired`. Idempotent bundle patchers must reject half-patched files (`if patch_marker in content and target in content: sys.exit(1)`), CommonMark fence closers must verify empty trailing text (`line[m.end():].strip() == ""`), and all failure-injection paths must be covered by `.github/scripts/test_ci_scripts.py`.
  * **Continuous Mode Re-Arm on Reset (Datasheet Sequencing Contract):** Per the BH1750 datasheet, `CMD_POWER_ON` followed by `CMD_RESET` clears the data register and leaves the chip in "waiting for measurement instruction"; it does NOT restart conversion. When resetting while in continuous mode, drivers must immediately re-send the mode opcode to restart conversions. Model this contract cleanly in `HwState::record_wake_or_reset(&mut self, now: Instant) -> Option<u8>` returning the re-arm opcode so continuous re-arming is 100% host-verifiable without physical hardware.
  * **Eliminate Speculative Error Variants (`M-ERRORS-CANONICAL-STRUCTS`):** Never include dead or speculative error enum variants (such as `LightErrorKind::Io { source: io::Error }`) when the underlying peripheral driver (e.g. `rppal::i2c::I2c`) produces its own typed error that already wraps `io::Error`. Only represent error causes that the driver actually produces.
  * **Strong Physical Pin Typing on Peripheral Metadata (`M-STRONG-TYPES`):** Power and ground rails in peripheral metadata structures (`I2cHardwareInfo`) must use strongly typed header newtypes (`PhysicalPin::new(1)`, `PhysicalPin::new(9)`) rather than unstructured strings (`&'static str`).
  * **Panic-Free Test Utility Exports (`M-PANIC-IS-STOP`):** Shared diagnostic/test utilities that create resources (such as `ScopedTempDir`) must provide fallible constructors (`try_new(prefix) -> io::Result<Self>`) alongside infallible test conveniences (`new`) so the public crate interface provides panic-free alternatives.
  * **Deduplicate Canonical Error Display & Telemetry Formatting:** Use macro helpers (`impl_canonical_error_display!`) across domain errors to deduplicate single-line `summary()` formatting and `Display` backtrace rendering, preventing divergence across peripheral drivers.
  * **Action SHA Pinning & Step-Scoped Secrets:** Always SHA-pin third-party GitHub Actions (`actions/checkout`, `actions/setup-python`, `actions/upload-artifact`, `codecov/codecov-action`) with version comments in workflows, and never inject high-privilege credentials (like `OPENROUTER_API_KEY`) into whole jobs; scope secrets strictly to individual steps that require them.
  * **Harmonized Loop-Break Termination on Cumulative Stats:** Polling loops across diagnostic CLI utilities must terminate on cumulative stats (`stats.total_reads() >= max_count` / `stats.samples.total_reads() >= max_count`) rather than independent loop counter variables, ensuring summary reporting, error rate calculations, and process exit codes remain perfectly synchronized.
  * **Wrap `embedded-hal` Drivers in a Single Device Mutex:** When wrapping external `embedded-hal` drivers (such as `bh1750-embedded`) in `Backend::Hardware`, group driver and hardware state into a single `HardwareDevice` struct behind a single `Mutex<HardwareDevice>`. Avoid redundant nested mutexes (`Mutex<I2c>` inside `op_lock`) to eliminate deadlock potential and lock boilerplate.
  * **Centralize Periodic Polling Loops (`run_poll_loop`):** Scaffolding that executes periodic measurements, regulates intervals, and terminates on cumulative sample thresholds must be centralized in `crate::diag::run_poll_loop(interval, step)` where `step(&timestamp) -> bool` returns whether to break, eliminating duplicate timing boilerplate across diagnostic binaries.
  * **Enforce Mode Slug Inverses at the Domain Boundary:** `MeasurementMode::slug()` must be paired with `MeasurementMode::parse_slug(s) -> Result<Self, String>` sharing a single slug table on the domain type, preventing CLI flag parsers and telemetry serializations from diverging.
  * **Appliance Scope Containment over Speculative Generality:** Hardware peripheral drivers in appliance firmware must only expose operating modes and configurations actually needed by the appliance (e.g., continuous ambient light sampling) rather than general-purpose multi-mode abstractions (one-shot conversions, fine-grained resolution switching) that introduce state-machine races and settle-time complexity.
  * **Unconditional Failure on CLI Subprocess Probes:** Subprocess integration tests (`tests/cli_tests.rs`) must fail loudly with a panic if `--help` or basic binary execution fails, rather than silently skipping on non-CI hosts (`can_exec_binary`). Host-built test binaries are guaranteed runnable on the test machine.
  * **Unconditional Test Utility Visibility (`M-TEST-UTIL`):** Testing helpers that construct or map runtime task states (such as `abort_and_map`) must be exported unconditionally (`pub`) from library crates rather than gated behind `#[cfg(test)]`, ensuring integration test suites can utilize them.
  * **Assert Clean, Backtrace-Free CLI Rejections:** Table-driven CLI argument validation tests must assert not only the expected rejection substring but also explicitly verify `!stderr.contains("Backtrace") && !stderr.contains("panicked")` to enforce the user-facing error contract.
  * **Accurate Silicon Terminology in Diagnostics:** Diagnostic formatting and telemetry labels must accurately describe the physical silicon (e.g., DS18B20 configured measurement "Resolution", not "ADC Resolution" since the device is a direct-to-digital 1-Wire temperature sensor).


---

## 5. Ecosystem Reuse, Dependency Evaluation & User Approval Protocol

Before writing any new hardware driver, protocol parser, or peripheral module, agents **MUST** prioritize using existing, battle-tested solutions over writing from scratch.

### 5.1 Mandatory Reuse Hierarchy
1. **Linux Kernel Subsystems First:** Check if the Linux kernel already models the device (e.g., Industrial I/O `IIO` for ambient light/IMUs, `hwmon` for thermals/voltages, ALSA for audio DACs/codecs, `w1-gpio`/`w1_therm` for 1-Wire). Consuming a kernel sysfs/ioctl interface is preferred over bit-banging raw bus transactions in user space.
2. **Crates.io & `embedded-hal` Drivers Second:** If user-space bus communication is required (via `rppal`), search `crates.io` for existing drivers (e.g., `embedded-hal` drivers) where physical silicon quirks, timing, and register maps are already mastered.
3. **Raw Protocol Drivers as Last Resort:** Writing raw register/byte sequences (`i2c.write(&[0x01])`) from scratch is strictly forbidden unless no kernel driver and no maintained crate exists.

### 5.2 Research & User Choice Mandate (No Blind Additions)
* **Multiple Options Available:** If multiple crates or architectural approaches exist (e.g., Kernel IIO vs `embedded-hal` crate A vs crate B), agents **MUST NOT** choose blindly. Research each option (maintenance status, license, dependencies, `embedded-hal` version, async support, and suitability for Raspberry Pi 3) and present a structured trade-off comparison to the user to select one.
* **Single Option Available:** Even if there is only one viable crate or solution, agents **MUST explicitly inform and prompt the user** that a new external crate is being added (stating crate name, version, purpose, and license) before adding it to `Cargo.toml`.
* **Application Wrapping & Mocking:** Using an external crate does not bypass project architectural standards. The external driver must be wrapped inside the project's domain model (`new_mocked()`, canonical errors, strong newtypes) inside `Backend::Hardware`, preserving host-testability and CI coverage.

