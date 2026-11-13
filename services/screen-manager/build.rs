fn main() {
    #[cfg(feature = "ui")]
    {
        use std::collections::HashMap;
        use std::path::PathBuf;

        // Add Material Components 1.1.0 as a component library
        // with_library_paths expects a directory containing the library's entry point (material.slint)
        let material_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("material-1.1.0");

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
