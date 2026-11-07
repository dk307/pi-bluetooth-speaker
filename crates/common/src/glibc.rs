//! GLIBC symbol compatibility helpers for target ARM Cortex-A53 / aarch64.
//!
//! Enforces linking against glibc 2.17 symbols for `acosf` and `atan2f` to prevent
//! requiring a newer GLIBC runtime when running on older Debian / `DietPi` distributions.

/// Enforces glibc 2.17 symbol versioning for math functions on aarch64-unknown-linux-gnu.
#[macro_export]
macro_rules! glibc_compat {
    () => {
        #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
        core::arch::global_asm!(
            ".symver acosf, acosf@GLIBC_2.17",
            ".symver atan2f, atan2f@GLIBC_2.17"
        );
    };
}
