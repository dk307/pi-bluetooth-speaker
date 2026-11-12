//! Integration tests for config module.

use audio_manager::Config;
use std::path::PathBuf;

#[test]
fn default_config() {
    let cfg = Config::default();
    assert_eq!(cfg.alsa_card, "sndrpihifiberry");
    assert_eq!(
        cfg.state_path,
        PathBuf::from("/var/lib/speaker/amp-state.json")
    );
    assert_eq!(cfg.log_level, None);
}

/// `Config::load()` reads the fixed `/etc` path; on this host it does not
/// exist, so the defaults+environment path is exercised. Assertions use
/// `AMP_MGR_*` overrides, which win over any file that may exist on other
/// hosts, keeping the test hermetic in its assertions either way.
///
/// One sequential test function: `std::env` is process-global, so the
/// positive and rejection scenarios must not race across threads.
#[test]
fn load_env_overrides_validation_and_rejection() {
    unsafe { std::env::set_var("AMP_MGR_CONTROL_MODE", "alsa") };
    unsafe { std::env::set_var("AMP_MGR_ALSA_CARD", "customcard") };
    unsafe { std::env::set_var("AMP_MGR_STATE_PATH", "/tmp/amp-state-test.json") };
    unsafe { std::env::set_var("AMP_MGR_DSP_POLL_INTERVAL_MS", "700") };

    let cfg = Config::load().expect("valid overrides must load");
    assert_eq!(cfg.control_mode, "alsa");
    assert_eq!(cfg.alsa_card, "customcard");
    assert_eq!(cfg.state_path, PathBuf::from("/tmp/amp-state-test.json"));
    assert_eq!(cfg.dsp_poll_interval_ms, 700);
    // Defaults for untouched fields.
    assert_eq!(cfg.enable_gpio_pin, 14);
    assert_eq!(cfg.mute_gpio_pin, 15);
    assert!(cfg.safety_tick);

    // An invalid control_mode must bail with a clean, field-naming error.
    unsafe { std::env::set_var("AMP_MGR_CONTROL_MODE", "magic") };
    let err = Config::load().expect_err("invalid control_mode must bail");
    assert!(
        err.to_string().contains("Invalid control_mode"),
        "bail message must name the offending field: {err}"
    );

    unsafe { std::env::remove_var("AMP_MGR_CONTROL_MODE") };
    unsafe { std::env::remove_var("AMP_MGR_ALSA_CARD") };
    unsafe { std::env::remove_var("AMP_MGR_STATE_PATH") };
    unsafe { std::env::remove_var("AMP_MGR_DSP_POLL_INTERVAL_MS") };
}
