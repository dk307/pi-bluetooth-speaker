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

        // Emit rerun-if-changed for all UI files so cargo rebuilds when they change
        println!("cargo:rerun-if-changed=ui/appwindow.slint");
        println!("cargo:rerun-if-changed=ui/now_playing.slint");
        println!("cargo:rerun-if-changed=ui/marquee_state.slint");
        println!("cargo:rerun-if-changed=ui/marquee_rasterizer.slint");
        println!("cargo:rerun-if-changed=ui/status_bar.slint");
        println!("cargo:rerun-if-changed=ui/volume_hud.slint");
        println!("cargo:rerun-if-changed=ui/screen_app_bar.slint");
        println!("cargo:rerun-if-changed=ui/splash.slint");
        println!("cargo:rerun-if-changed=ui/viewport.slint");
        println!("cargo:rerun-if-changed=ui/settings_view.slint");
        println!("cargo:rerun-if-changed=ui/app_theme.slint");
    }
}
