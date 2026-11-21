fn main() {
    // Redirect math symbols that host glibc >= 2.42 versions as @GLIBC_2.43
    // (`acosf`, `atan2f`, `remainder`) to `speaker_common`'s `@GLIBC_2.17`
    // `__wrap_*` trampolines so both Rust LTO and `libskia.a` link cleanly for
    // Debian 13 Trixie (glibc 2.41) without affecting host build scripts.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
    {
        println!("cargo:rustc-link-arg=-Wl,--wrap=acosf");
        println!("cargo:rustc-link-arg=-Wl,--wrap=atan2f");
        println!("cargo:rustc-link-arg=-Wl,--wrap=remainder");
    }

    #[cfg(feature = "ui")]
    {
        use std::collections::HashMap;
        use std::path::PathBuf;

        // Add vendored Material 3 Components as the @material component library.
        // with_library_paths expects a directory containing the library's entry point (material.slint).
        let material_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/material");

        let config = slint_build::CompilerConfiguration::new()
            .with_library_paths(HashMap::from([("material".to_string(), material_path)]));

        slint_build::compile_with_config("ui/appwindow.slint", config)
            .expect("Failed to compile Slint UI definitions");

        // Watch the whole ui/ directory: slint-build's own emitted rerun-if-changed
        // paths are unreliable across stale fingerprints (AGENTS.md §4.13), and a
        // directory watch also covers .slint files added or renamed later — a
        // hardcoded per-file list silently misses new imports.
        println!("cargo:rerun-if-changed=ui/appwindow.slint");
        println!("cargo:rerun-if-changed=ui");
    }
}
