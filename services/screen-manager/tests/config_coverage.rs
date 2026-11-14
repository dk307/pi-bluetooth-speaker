#![cfg(feature = "ui")]
//! Coverage tests for configuration error paths that the happy-path suite
//! (roundtrips, validation, atomic replace) never reaches.
//!
//! All tests are hermetic: they only touch temp files created via
//! [`ScopedTempDir`] and never read or write system configuration paths.

use std::error::Error as _;
use std::io;

use screen_manager::config::{ConfigError, CoreConfig};
use speaker_common::diag::ScopedTempDir;

#[test]
fn load_non_directory_path_component_is_hard_io_error() {
    let dir = ScopedTempDir::new("cfg_not_a_dir");
    let file = dir.path().join("file.txt");
    std::fs::write(&file, b"regular file").expect("write regular file");

    // `metadata()` on `file.txt/child.json` fails with ENOTDIR, which is NOT
    // `ErrorKind::NotFound`, so the loader must surface a typed I/O error
    // instead of silently falling back to defaults.
    let child = file.join("nested.json");
    let err = CoreConfig::load(&child).expect_err("ENOTDIR path must fail");
    assert!(err.is_io(), "{err}");
    assert!(err.to_string().contains("failed to read config"), "{err}");
    assert!(err.source().is_some());
}

#[test]
fn save_into_missing_parent_directory_fails_before_write() {
    let dir = ScopedTempDir::new("cfg_missing_parent");
    let target = dir.path().join("missing-dir").join("config.json");
    let err = CoreConfig::default()
        .save(&target)
        .expect_err("missing parent dir must fail staging");
    assert!(err.is_io(), "{err}");
    assert!(err.to_string().contains("failed to create config"), "{err}");
    assert!(err.source().is_some());
}

#[test]
fn config_error_source_reflects_kind() {
    // I/O failures keep their underlying `io::Error` as the source.
    let io_err = ConfigError::io("ctx", io::Error::other("boom"));
    assert!(io_err.is_io());
    assert!(io_err.source().is_some());

    // Domain validation failures carry a message only — no foreign source.
    let invalid = ConfigError::invalid("bad value");
    assert!(invalid.is_invalid_value());
    assert!(invalid.source().is_none());
}
