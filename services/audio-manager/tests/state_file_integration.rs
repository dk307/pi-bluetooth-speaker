//! Integration tests for state_file module.

use audio_manager::{AmpState, StateFile};
use tempfile::tempdir;

#[tokio::test]
async fn save_and_load_roundtrip() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("amp-state.json");
    let sf = StateFile::new(&path);

    let state = AmpState {
        level_raw: 100,
        muted: false,
    };
    sf.save(&state).await.unwrap();

    let loaded = sf.load().unwrap();
    assert_eq!(loaded.level_raw, 100);
    assert!(!loaded.muted);
}

#[tokio::test]
async fn save_atomic_on_crash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("amp-state.json");
    let sf = StateFile::new(&path);

    // Write initial state
    let state1 = AmpState {
        level_raw: 50,
        muted: false,
    };
    sf.save(&state1).await.unwrap();

    // Simulate a partial write by corrupting the file directly
    // (not via StateFile, to test recovery)
    std::fs::write(&path, b"corrupted").unwrap();

    // Next save should succeed and replace the file
    let state2 = AmpState {
        level_raw: 75,
        muted: true,
    };
    sf.save(&state2).await.unwrap();

    let loaded = sf.load().unwrap();
    assert_eq!(loaded.level_raw, 75);
    assert!(loaded.muted);
}

#[tokio::test]
async fn load_missing_file_returns_default() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("nonexistent.json");
    let sf = StateFile::new(&path);

    let state = sf.load().unwrap();
    assert_eq!(state.level_raw, 116); // default level_raw
    assert!(!state.muted); // default muted
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

#[tokio::test]
async fn save_multiple_times() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("amp-state.json");
    let sf = StateFile::new(&path);

    sf.save(&AmpState {
        level_raw: 10,
        muted: false,
    })
    .await
    .unwrap();
    sf.save(&AmpState {
        level_raw: 20,
        muted: true,
    })
    .await
    .unwrap();
    sf.save(&AmpState {
        level_raw: 30,
        muted: false,
    })
    .await
    .unwrap();
    sf.save(&AmpState {
        level_raw: 40,
        muted: true,
    })
    .await
    .unwrap();

    let loaded = sf.load().unwrap();
    assert_eq!(loaded.level_raw, 40);
    assert!(loaded.muted);
}

#[tokio::test]
async fn load_empty_file_errors() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty.json");
    std::fs::write(&path, b"").unwrap();
    let sf = StateFile::new(&path);

    let err = sf.load().unwrap_err();
    assert!(err.to_string().contains("deserialize"));
}

#[tokio::test]
async fn load_wrong_structure_errors() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wrong.json");
    std::fs::write(&path, b"{}").unwrap();
    let sf = StateFile::new(&path);

    let err = sf.load().unwrap_err();
    eprintln!("Actual error: {}", err);
    eprintln!("Error chain: {:?}", err);
    eprintln!("Error source chain:");
    let mut source = std::error::Error::source(&err);
    while let Some(s) = source {
        eprintln!("  Source: {}", s);
        source = std::error::Error::source(s);
    }
    // Check if any error in the chain contains "missing field"
    let mut found = false;
    let mut current: &dyn std::error::Error = &err;
    loop {
        if current.to_string().contains("missing field") {
            found = true;
            break;
        }
        if let Some(src) = std::error::Error::source(current) {
            current = src;
        } else {
            break;
        }
    }
    assert!(
        found,
        "No 'missing field' in error chain. Error was: {}",
        err
    );
}

#[tokio::test]
async fn save_preserves_json_format() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("format.json");
    let sf = StateFile::new(&path);

    sf.save(&AmpState {
        level_raw: 123,
        muted: true,
    })
    .await
    .unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("level_raw"));
    assert!(content.contains("123"));
    assert!(content.contains("muted"));
    assert!(content.contains("true"));
}

#[tokio::test]
async fn load_then_save_then_load() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cycle.json");
    let sf = StateFile::new(&path);

    // Initial load creates default
    let s1 = sf.load().unwrap();
    assert_eq!(s1.level_raw, 116);

    // Save new state
    sf.save(&AmpState {
        level_raw: 99,
        muted: true,
    })
    .await
    .unwrap();

    // Load again
    let s2 = sf.load().unwrap();
    assert_eq!(s2.level_raw, 99);
    assert!(s2.muted);
}

#[tokio::test]
async fn concurrent_save_load() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("concurrent.json");
    let sf = StateFile::new(&path);

    // Save initial
    sf.save(&AmpState {
        level_raw: 10,
        muted: false,
    })
    .await
    .unwrap();

    // Multiple concurrent saves
    let mut handles = vec![];
    for i in 0..10 {
        let sf_clone = StateFile::new(&path);
        handles.push(tokio::spawn(async move {
            sf_clone
                .save(&AmpState {
                    level_raw: i * 10,
                    muted: i % 2 == 0,
                })
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }

    // Final state should be valid
    let loaded = sf.load().unwrap();
    assert!(loaded.level_raw <= 90);
}
