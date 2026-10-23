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

