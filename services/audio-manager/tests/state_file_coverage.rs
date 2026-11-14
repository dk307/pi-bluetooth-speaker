//! Additional integration tests for state_file module to increase coverage.

use audio_manager::{AmpState, StateFile};
use speaker_common::diag::ScopedTempDir;
use speaker_hardware::PERCENT_TO_RAW;

#[test]
fn state_file_load_missing_returns_default() {
    let dir = ScopedTempDir::new("state_file_missing");
    let path = dir.path().join("missing.json");
    let state_file = StateFile::new(&path);
    // A missing file returns the default state (the daemon falls back to
    // `AmpState::default()` at the call site in `load_persisted_state`).
    let state = state_file.load().unwrap();
    assert_eq!(state, AmpState::default());
}

#[tokio::test]
async fn state_file_save_and_load_roundtrip() {
    let dir = ScopedTempDir::new("state_file_roundtrip");
    let path = dir.path().join("state.json");
    let state_file = StateFile::new(&path);

    let original_state = AmpState::new(PERCENT_TO_RAW[75], true).unwrap();
    state_file.save(&original_state).await.unwrap();

    let loaded_state = state_file.load().unwrap();
    assert_eq!(loaded_state.level_raw, original_state.level_raw);
    assert_eq!(loaded_state.muted, original_state.muted);
}

#[tokio::test]
async fn state_file_save_creates_parent_directories() {
    let dir = ScopedTempDir::new("state_file_mkdir");
    let path = dir.path().join("subdir").join("state.json");
    let state_file = StateFile::new(&path);

    let state = AmpState::default();
    state_file.save(&state).await.unwrap();

    assert!(path.exists());
}

#[tokio::test]
async fn state_file_save_overwrites_existing() {
    let dir = ScopedTempDir::new("state_file_overwrite");
    let path = dir.path().join("state.json");
    let state_file = StateFile::new(&path);

    let state1 = AmpState::new(50, false).unwrap();
    state_file.save(&state1).await.unwrap();

    let state2 = AmpState::new(100, true).unwrap();
    state_file.save(&state2).await.unwrap();

    let loaded = state_file.load().unwrap();
    assert_eq!(loaded.level_raw, 100);
    assert!(loaded.muted);
}

#[tokio::test]
async fn state_file_save_atomic_write_leaves_no_temp_files() {
    let dir = ScopedTempDir::new("state_file_atomic");
    let path = dir.path().join("state.json");
    let state_file = StateFile::new(&path);

    let state = AmpState::default();
    state_file.save(&state).await.unwrap();

    // Verify no temp files left behind after the atomic rename.
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["state.json"]);
}

#[test]
fn state_file_load_corrupted_json_fails() {
    let dir = ScopedTempDir::new("state_file_corrupt");
    let path = dir.path().join("state.json");
    std::fs::write(&path, "{not valid json").unwrap();

    let state_file = StateFile::new(&path);
    let err = state_file.load().unwrap_err();
    assert!(err.to_string().contains("deserialize state"), "{err}");
}

#[test]
fn state_file_load_non_object_json_fails() {
    let dir = ScopedTempDir::new("state_file_wrongshape");
    let path = dir.path().join("state.json");
    std::fs::write(&path, r#""just a string""#).unwrap();

    let state_file = StateFile::new(&path);
    let err = state_file.load().unwrap_err();
    assert!(err.to_string().contains("deserialize state"), "{err}");
}

#[test]
fn state_file_load_directory_path_fails() {
    let dir = ScopedTempDir::new("state_file_dirpath");
    let state_file = StateFile::new(dir.path());
    // `File::open` on a directory succeeds on Linux; the read/deserialize
    // step is where the failure surfaces.
    let err = state_file.load().unwrap_err();
    assert!(
        err.to_string().contains("deserialize state"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn state_file_save_under_file_parent_fails() {
    let dir = ScopedTempDir::new("state_file_fileparent");
    // A path whose parent is an existing FILE cannot be created.
    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, b"i am a file").unwrap();
    let path = blocked.join("state.json");
    let state_file = StateFile::new(&path);
    let err = state_file.save(&AmpState::default()).await.unwrap_err();
    assert!(err.to_string().contains("create state directory"), "{err}");
}

#[tokio::test]
async fn state_file_multiple_save_load_cycles() {
    let dir = ScopedTempDir::new("state_file_cycles");
    let path = dir.path().join("state.json");
    let state_file = StateFile::new(&path);

    for i in 0..10u16 {
        let state = AmpState::new(i * 10, i % 2 == 0).unwrap();
        state_file.save(&state).await.unwrap();
        let loaded = state_file.load().unwrap();
        assert_eq!(loaded.level_raw, state.level_raw);
        assert_eq!(loaded.muted, state.muted);
    }
}

#[test]
fn state_file_debug_impl() {
    let dir = ScopedTempDir::new("state_file_debug");
    let path = dir.path().join("state.json");
    let state_file = StateFile::new(&path);
    let debug_str = format!("{:?}", state_file);
    assert!(debug_str.contains("StateFile"));
    assert!(debug_str.contains("state.json"));
}

#[test]
fn state_file_clone_shares_path() {
    let dir = ScopedTempDir::new("state_file_clone");
    let path = dir.path().join("state.json");
    let state_file = StateFile::new(&path);
    let cloned = state_file.clone();
    assert_eq!(
        format!("{:?}", state_file),
        format!("{:?}", cloned),
        "clone must carry the same path"
    );
}
