//! Central speaker daemon and hardware orchestration service for Raspberry Pi 3.
//!
//! # Binary Target Naming
//!
//! The executable binary target is named `core` in `Cargo.toml` (`[[bin]] name = "core"`)
//! matching the `DietPi` systemd unit `/etc/systemd/system/core.service`. To prevent shadowing
//! the standard library's `core` crate in external prelude and dependency graphs, the library
//! crate is named `speaker_core`.

use std::env;
#[cfg(feature = "ui")]
use std::path::Path;
use std::process::ExitCode;
#[cfg(feature = "ui")]
use std::time::{Duration, Instant};

use speaker_core::hardware::TempSensor;

#[cfg(feature = "ui")]
use speaker_core::ui::{ComponentHandle, Framebuffer, SplashScreen};

fn print_help() {
    println!("DietPi Bluetooth Speaker & DSP Core Appliance Service");
    println!();
    println!("Usage: core [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -p, --preview <SECS>  Render UI and animate for SECS seconds");
    println!("  -l, --loop            Run UI animation loop continuously");
    println!("  -V, --version         Print version and exit");
    println!("  -h, --help            Print this help text and exit");
}

#[cfg(feature = "ui")]
fn run_ui_boot_splash(preview_secs: Option<u64>, run_loop: bool) {
    let fb_path = Path::new(Framebuffer::DEFAULT_FB_PATH);
    if !fb_path.exists() {
        tracing::info!("Headless/simulated environment (no /dev/fb0 detected)");
        println!("  Display: Headless/simulated environment (no /dev/fb0 detected).");
        return;
    }

    tracing::info!("Initializing Framebuffer /dev/fb0 (800x480 32bpp BGRX)");
    println!("  Display: Initializing Framebuffer /dev/fb0 (800x480 32bpp BGRX)...");
    let mut fb = match Framebuffer::new() {
        Ok(fb) => fb,
        Err(e) => {
            tracing::warn!("Could not open /dev/fb0: {e}");
            eprintln!("  Display Warning: Could not open /dev/fb0: {e}");
            return;
        }
    };

    if let Err(e) = fb.unblank() {
        tracing::warn!("Failed to unblank display: {e}");
        eprintln!("  Display Warning: Failed to unblank display: {e}");
    }

    let window = match fb.init_platform() {
        Ok(win) => win,
        Err(e) => {
            tracing::error!("Failed to initialize platform: {e}");
            eprintln!("  Display Error: Failed to initialize platform: {e}");
            return;
        }
    };

    let splash = match SplashScreen::new() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to initialize SplashScreen: {e}");
            eprintln!("  Display Error: Failed to initialize SplashScreen: {e}");
            return;
        }
    };

    if let Err(e) = splash.show() {
        tracing::warn!("Failed to show SplashScreen: {e}");
        eprintln!("  Display Warning: Failed to show SplashScreen: {e}");
        return;
    }

    match fb.render_frame(&window) {
        Ok(true) => {
            tracing::info!("Boot splash screen rendered to /dev/fb0 successfully");
            println!("  Display: Boot splash screen rendered to /dev/fb0 successfully!");
        }
        Ok(false) => {
            tracing::debug!("Screen buffer already current");
            println!("  Display: Screen buffer already current.");
        }
        Err(e) => {
            tracing::error!("Failed to render frame: {e}");
            eprintln!("  Display Error: Failed to render frame: {e}");
        }
    }

    if run_loop || preview_secs.is_some() {
        let start = Instant::now();
        let duration = preview_secs.map(Duration::from_secs);
        tracing::info!("Running UI render loop");
        println!("  Display: Running UI render loop...");

        while run_loop || duration.is_some_and(|d| start.elapsed() < d) {
            slint::platform::update_timers_and_animations();
            if let Err(e) = fb.render_frame(&window) {
                tracing::error!("Render loop frame failed: {e}");
                eprintln!("  Display Error: render loop frame failed: {e}");
                break;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        tracing::info!("Preview complete");
        println!("  Display: Preview complete.");
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mut preview_secs: Option<u64> = None;
    let mut run_loop = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("core {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "-l" | "--loop" => {
                run_loop = true;
                i += 1;
            }
            "-p" | "--preview" => {
                if i + 1 < args.len() {
                    if let Ok(secs) = args[i + 1].parse::<u64>() {
                        preview_secs = Some(secs);
                        i += 2;
                    } else {
                        eprintln!("Error: --preview requires a valid positive integer in seconds");
                        return ExitCode::FAILURE;
                    }
                } else {
                    eprintln!("Error: --preview requires a seconds argument");
                    return ExitCode::FAILURE;
                }
            }
            unknown => {
                eprintln!("Error: unrecognized argument '{unknown}'");
                eprintln!("Try 'core --help' for more information.");
                return ExitCode::FAILURE;
            }
        }
    }

    let temp_sensor = TempSensor::new();

    println!("============================================================");
    println!("  Speaker Core Service is running on Raspberry Pi 3!");
    println!("  Target: ARM Cortex-A53 (aarch64)");
    println!("  Thermal Sensor: {}", temp_sensor.gpio_info());

    #[cfg(feature = "ui")]
    run_ui_boot_splash(preview_secs, run_loop);

    #[cfg(not(feature = "ui"))]
    let _ = (preview_secs, run_loop);
    #[cfg(not(feature = "ui"))]
    println!("  Display: UI feature disabled at compile-time.");

    println!("============================================================");
    ExitCode::SUCCESS
}
