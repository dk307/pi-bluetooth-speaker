// ==============================================================================
// Speaker Core — Central Speaker Logic, Appliance UI & Hardware Console
// ==============================================================================

slint::include_modules!();

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting Speaker Core Service...");

    let main_window = MainWindow::new()?;

    // Setup volume callbacks
    let window_handle = main_window.as_weak();
    main_window.on_volume_changed(move |val| {
        tracing::info!("Volume adjusted to: {:.1}%", val);
        if let Some(ui) = window_handle.upgrade() {
            ui.set_volume_value(val);
        }
    });

    let window_handle = main_window.as_weak();
    main_window.on_toggle_play_pause(move || {
        if let Some(ui) = window_handle.upgrade() {
            let playing = !ui.get_is_playing();
            ui.set_is_playing(playing);
            tracing::info!("Play/pause toggled: playing={}", playing);
        }
    });

    tracing::info!("Running Slint event loop on native framebuffer/KMS...");
    main_window.run()?;

    Ok(())
}
