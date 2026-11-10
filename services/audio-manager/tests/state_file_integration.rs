//! Integration tests for state_file module.

use audio_manager::{StateFile, AmpState};
use tempfile::tempdir;

#[tokio::test]
async fn save_and_load_roundtrip() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("amp-state.json");
    let sf = StateFile::new(&path);

    let state = AmpState { level_raw: 100, muted: false };
    sf.save(&state).await.unwrap();

    let loaded = sf.load().unwrap();
    assert_eq!(loaded.level_raw, 100);
    assert_eq!(loaded.muted, false);
}

#[tokio::test]
async fn save_atomic_on_crash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("amp-state.json");
    let sf = StateFile::new(&path);

    // Write initial state
    let state1 = AmpState { level_raw: 50, muted: false };
    sf.save(&state1).await.unwrap();

    // Simulate a partial write by corrupting the file directly
    // (not via StateFile, to test recovery)
    std::fs::write(&path, b"corrupted").unwrap();

    // Next save should succeed and replace the file
    let state2 = AmpState { level_raw: 75, muted: true };
    sf.save(&state2).await.unwrap();

    let loaded = sf.load().unwrap();
    assert_eq!(loaded.level_raw, 75);
    assert!(loaded.muted);
}

#[tokio::test]
async fn load_missing_file_errors() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("nonexistent.json");
    let sf = StateFile::new(&path);

    let err = sf.load().unwrap_err();
    assert!(err.to_string().contains("open state file"));
}

#[tokio::test]
async fn load_corrupt_json_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.json");
    std::fs::write(&path, b"not json").unwrap();
    let sf = StateFile::new(&path);

    let err = sf.load().unwrap_err();
    assert!(err.to_string().contains("deserialize"));
}