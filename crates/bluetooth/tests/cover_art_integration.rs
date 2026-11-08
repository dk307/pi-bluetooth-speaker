//! Integration tests for Cover Art directory management, JPEG validation, and BIP handles.

use speaker_bluetooth::{
    COVER_ART_CACHE_DIR, ensure_cover_art_cache_dir_in, is_complete_jpeg, validate_bip_handle,
};
use speaker_common::diag::ScopedTempDir;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Minimal valid JPEG: >= 100 bytes, SOI header, EOI footer.
fn jpeg_bytes(total: usize, with_eoi: bool) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8];
    bytes.resize(total, 0x00);
    if with_eoi {
        let n = bytes.len();
        bytes[n - 2] = 0xFF;
        bytes[n - 1] = 0xD9;
    }
    bytes
}

#[test]
fn cache_dir_is_created_with_owner_only_permissions() {
    let dir = ScopedTempDir::new("cover_art_dir_ok");
    let target = dir.path().join("nested/covers");
    ensure_cover_art_cache_dir_in(&target).expect("dir created");
    let mode = std::fs::metadata(&target)
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "cache dir must be owner-only");
}

#[test]
fn cache_dir_creation_fails_when_parent_is_a_file() {
    let dir = ScopedTempDir::new("cover_art_dir_bad");
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").expect("blocker written");
    let err = ensure_cover_art_cache_dir_in(&blocker.join("sub")).expect_err("must fail");
    assert!(err.contains("cannot create"), "unexpected error: {err}");
}

#[test]
fn complete_jpeg_accepts_only_complete_files() {
    let dir = ScopedTempDir::new("cover_art_jpeg");
    let write = |name: &str, bytes: &[u8]| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("fixture");
        path
    };
    assert!(!is_complete_jpeg(Path::new("/nonexistent/cover.jpg")));
    let short = write("short.jpg", &jpeg_bytes(40, true));
    assert!(!is_complete_jpeg(&short), "below the 100-byte minimum");
    let corrupt_soi = write("bad_soi.jpg", &{
        let mut b = vec![0x00, 0x00];
        b.resize(120, 0x11);
        b
    });
    assert!(!is_complete_jpeg(&corrupt_soi), "missing SOI marker");
    let missing_eoi = write("bad_eoi.jpg", &jpeg_bytes(120, false));
    assert!(!is_complete_jpeg(&missing_eoi), "missing EOI marker");
    let ok = write("ok.jpg", &jpeg_bytes(120, true));
    assert!(is_complete_jpeg(&ok), "SOI + EOI >= 100 bytes is complete");
}

#[test]
fn bip_handles_are_exactly_seven_digits() {
    assert!(validate_bip_handle("5630701").is_some());
    assert!(validate_bip_handle("0000000").is_some());
    assert!(validate_bip_handle("563070").is_none(), "too short");
    assert!(validate_bip_handle("56307012").is_none(), "too long");
    assert!(validate_bip_handle("56307a1").is_none(), "non-digit");
    assert!(validate_bip_handle("../../x").is_none(), "path traversal");
    assert!(validate_bip_handle("").is_none());
}

#[test]
fn production_cache_dir_is_documented_constant() {
    assert_eq!(COVER_ART_CACHE_DIR, "/var/cache/speaker-covers");
}
