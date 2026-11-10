//! Integration tests for config module.

use audio_manager::Config;
use std::path::PathBuf;

#[test]
fn default_config() {
    let cfg = Config::default();
    assert_eq!(cfg.alsa_card, "sndrpimerusamp");
    assert_eq!(
        cfg.state_path,
        PathBuf::from("/var/lib/speaker/amp-state.json")
    );
    assert_eq!(cfg.log_level, None);
}
