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
    }
}
