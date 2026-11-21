//! GLIBC symbol compatibility helpers for target ARM Cortex-A53 / aarch64.
//!
//! Enforces linking against glibc 2.17 symbols for `acosf` and `atan2f` to prevent
//! requiring a newer GLIBC runtime when running on older Debian / `DietPi` distributions.

/// Enforces glibc 2.17 symbol versioning for math functions on aarch64-unknown-linux-gnu.
///
/// Defines `__wrap_acosf`, `__wrap_atan2f`, and `__wrap_remainder` (paired with
/// `-Wl,--wrap=acosf`, `-Wl,--wrap=atan2f`, `-Wl,--wrap=remainder` in
/// `.cargo/config.toml`) forwarding to the `@GLIBC_2.17` symbols in `libm.so.6`.
/// This redirects both Rust LTO references and precompiled C++ static archive
/// references (such as `libskia.a` in `skia-bindings`) away from `GLIBC_2.43`
/// symbols introduced on newer build hosts.
#[macro_export]
macro_rules! glibc_compat {
    () => {
        #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
        core::arch::global_asm!(
            ".symver __glibc217_acosf, acosf@GLIBC_2.17",
            ".global __wrap_acosf",
            ".type __wrap_acosf, %function",
            "__wrap_acosf:",
            "b __glibc217_acosf",
            ".symver __glibc217_atan2f, atan2f@GLIBC_2.17",
            ".global __wrap_atan2f",
            ".type __wrap_atan2f, %function",
            "__wrap_atan2f:",
            "b __glibc217_atan2f",
            ".symver __glibc217_remainder, remainder@GLIBC_2.17",
            ".global __wrap_remainder",
            ".type __wrap_remainder, %function",
            "__wrap_remainder:",
            "b __glibc217_remainder",
        );
    };
}

/// Enforces glibc 2.17 symbol versioning for `log10f` on aarch64-unknown-linux-gnu.
///
/// Slint 1.19 rendering code pulls in `log10f`, which recent compiler-rt
/// builds emit as a `GLIBC_2.43` reference; the Pi's glibc 2.41 only exports
/// the `GLIBC_2.17` version. The directive mirrors [`crate::glibc_compat!`]:
/// binaries that link Slint's renderer (which pulls `log10f` into the link)
/// must be built with the release profile (LTO) so the linker resolves the
/// math symbols against the `GLIBC_2.17` versions this directive requests;
/// the dev profile leaves unversioned references that bind to the host's
/// default (`GLIBC_2.43`) libm.
#[macro_export]
macro_rules! glibc_compat_log10 {
    () => {
        #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
        core::arch::global_asm!(".symver log10f, log10f@GLIBC_2.17");
    };
}
