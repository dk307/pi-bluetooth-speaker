#![cfg(feature = "ui")]
//! Coverage tests for UI-shell branches the main `ui_shell_integration`
//! suite misses: NaN volume deltas, mute reconciliation on PCM pushes, and
//! absent-volume PCM snapshots.

use screen_manager::ui::{AppShell, Framebuffer, MainWindow};
use speaker_bluetooth::{BlueAlsaPcm, BtAddress};

fn shell_on_mocked_fb() -> AppShell {
    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("platform init");
    AppShell::new(MainWindow::new().expect("MainWindow"))
}

fn pcm(muted: bool, volume_percent: Option<u8>) -> BlueAlsaPcm {
    BlueAlsaPcm {
        pcm_path: "/org/bluealsa/pcm0".into(),
        device_address: BtAddress::new([1, 2, 3, 4, 5, 6]),
        resolved_name: "Phone".into(),
        transport: "A2DP-sink".into(),
        running: true,
        selected_codec: "".into(),
        available_codecs: Box::default(),
        sampling_rate: 48_000,
        channels: 2,
        format: "S16_LE".into(),
        volume: None,
        volume_percent,
        muted,
        delay: None,
    }
}

#[test]
fn apply_volume_delta_treats_nan_volume_as_zero() {
    let shell = shell_on_mocked_fb();
    // Bypass `AppShell::set_volume` (which sanitizes) and poison the window
    // property directly so the `is_finite` guard in `apply_volume_delta` runs.
    shell.window().set_volume_value(f32::NAN);
    assert!(!shell.volume().is_finite());

    // NaN current volume counts as 0: 1 detent at the default 2% step.
    let target = shell.apply_volume_delta(1);
    assert_eq!(target, 2);
    assert_eq!(shell.volume(), 2.0);
}

#[test]
fn apply_pcm_stream_reconciles_mute_and_absent_volume() {
    let shell = shell_on_mocked_fb();
    assert!(!shell.is_muted());

    // Muted snapshot against an unmuted shell must flip the mute property and
    // mark the volume/mute state as changed; `volume_percent = None` skips the
    // level-update branch entirely.
    shell.apply_pcm_stream(&pcm(true, None));
    assert!(shell.is_muted());
    assert_eq!(
        shell.volume(),
        75.0,
        "absent volume_percent must not touch the level"
    );
}

#[test]
fn volume_changed_callback_handles_nan_and_unmutes() {
    let shell = shell_on_mocked_fb();

    // NaN slider input falls back to 0%.
    shell.window().invoke_volume_changed(f32::NAN);
    assert_eq!(shell.volume(), 0.0);

    // A volume change while muted must unmute as a side effect.
    shell.set_muted(true);
    shell.window().invoke_volume_changed(50.0);
    assert!(!shell.is_muted());
    assert_eq!(shell.volume(), 50.0);
}
