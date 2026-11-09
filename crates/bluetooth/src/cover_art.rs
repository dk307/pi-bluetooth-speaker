//! BIP cover-art discovery, caching, and validation helpers.

use std::io::Read;

use super::connection::BtAddress;

/// Ensures both the SOI marker (`0xFF, 0xD8`) and the EOI marker (`0xFF, 0xD9`)
/// are present so Slint never attempts to decode a partially written transfer buffer.
#[must_use]
pub fn is_complete_jpeg(path: &std::path::Path) -> bool {
    use std::io::{Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return false;
    };
    if len < 100 {
        return false;
    }
    let mut header = [0u8; 2];
    if f.read_exact(&mut header).is_err() || header != [0xFF, 0xD8] {
        return false;
    }
    if f.seek(SeekFrom::End(-2)).is_err() {
        return false;
    }
    let mut footer = [0u8; 2];
    if f.read_exact(&mut footer).is_err() {
        return false;
    }
    footer == [0xFF, 0xD9]
}

/// Root-only spool directory for OBEX cover art cache files.
///
/// A predictable path under the world-writable `/tmp` would let a local process
/// win a TOCTOU symlink race against the privileged `obexd` transfer write; a
/// root-owned `0700` directory closes that window.
pub const COVER_ART_CACHE_DIR: &str = "/var/cache/speaker-covers";

/// Creates the cover art cache directory (defaulting to [`COVER_ART_CACHE_DIR`]) with
/// `0700` permissions if needed.
///
/// # Errors
/// Returns an error string when the directory cannot be created or locked down.
pub fn ensure_cover_art_cache_dir_in(dir: &std::path::Path) -> Result<(), Box<str>> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)
        .map_err(|e| Box::<str>::from(format!("cannot create {}: {e}", dir.display())))?;
    let mut perms = std::fs::metadata(dir)
        .map_err(|e| Box::<str>::from(format!("cannot stat {}: {e}", dir.display())))?
        .permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(dir, perms)
        .map_err(|e| Box::<str>::from(format!("cannot chmod {}: {e}", dir.display())))?;
    Ok(())
}

/// Validates a BIP image handle per the AOSP `is_valid_img_handle` contract.
///
/// A valid handle is exactly 7 ASCII digits; anything else is rejected so a
/// malformed handle can never reach an OBEX `Get` round-trip.
#[must_use]
pub fn validate_bip_handle(handle: &str) -> Option<String> {
    let valid = handle.len() == 7 && handle.bytes().all(|b| b.is_ascii_digit());
    valid.then(|| handle.to_string())
}

/// Returns the deterministic cache path and sanitized MAC/handle for an OBEX cover art handle.
#[must_use]
pub fn cover_art_path_for(addr: BtAddress, handle: &str) -> Option<(String, String, String)> {
    cover_art_path_for_in(std::path::Path::new(COVER_ART_CACHE_DIR), addr, handle)
}

/// Directory-parametrized variant of [`cover_art_path_for`] (testable without the
/// production cache directory).
#[must_use]
pub fn cover_art_path_for_in(
    dir: &std::path::Path,
    addr: BtAddress,
    handle: &str,
) -> Option<(String, String, String)> {
    let sanitized_mac = addr.to_string().replace([':', '-'], "_");
    let sanitized_handle = validate_bip_handle(handle)?;
    let cache_path = format!(
        "{}/speaker-cover-{sanitized_mac}-{sanitized_handle}.jpg",
        dir.display()
    );
    Some((cache_path, sanitized_mac, sanitized_handle))
}

/// Checks if cover art is already cached on disk and cleans obsolete images.
#[must_use]
pub fn cover_art_cached_path(addr: BtAddress, handle: &str) -> Option<Box<str>> {
    cover_art_cached_path_in(std::path::Path::new(COVER_ART_CACHE_DIR), addr, handle)
}

/// Directory-parametrized variant of [`cover_art_cached_path`] (testable without the
/// production cache directory).
#[must_use]
pub fn cover_art_cached_path_in(
    dir: &std::path::Path,
    addr: BtAddress,
    handle: &str,
) -> Option<Box<str>> {
    let (cache_path, sanitized_mac, sanitized_handle) = cover_art_path_for_in(dir, addr, handle)?;
    cover_art_cached_path_in_dir(dir, &cache_path, &sanitized_mac, &sanitized_handle)
}

/// Path-parametrized cache validity check (internal helper of [`cover_art_cached_path_in`]).
pub(super) fn cover_art_cached_path_in_dir(
    dir: &std::path::Path,
    cache_path: &str,
    sanitized_mac: &str,
    sanitized_handle: &str,
) -> Option<Box<str>> {
    let cache_path_buf = std::path::PathBuf::from(cache_path);
    if is_complete_jpeg(&cache_path_buf) {
        clean_cover_art_in_dir(dir, sanitized_mac, sanitized_handle);
        return Some(Box::<str>::from(cache_path));
    }
    None
}

/// Cleans old cover art files in the target directory for a device address except the active handle.
pub fn clean_cover_art_in_dir(dir: &std::path::Path, sanitized_mac: &str, current_handle: &str) {
    let prefix = format!("speaker-cover-{sanitized_mac}-");
    let current_filename = format!("speaker-cover-{sanitized_mac}-{current_handle}.jpg");
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(file_name) = entry.file_name().into_string()
                && file_name.starts_with(&prefix)
                && std::path::Path::new(&file_name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
                && file_name != current_filename
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Cleans old cover art files in the cache directory for a device address except the active handle.
pub fn clean_old_cover_art(sanitized_mac: &str, current_handle: &str) {
    clean_cover_art_in_dir(
        std::path::Path::new(COVER_ART_CACHE_DIR),
        sanitized_mac,
        current_handle,
    );
}

/// Directory-parametrized variant of [`clean_old_cover_art`] (testable without the
/// production cache directory).
pub fn clean_old_cover_art_in(dir: &std::path::Path, sanitized_mac: &str, current_handle: &str) {
    clean_cover_art_in_dir(dir, sanitized_mac, current_handle);
}
