//! BIP cover-art discovery, caching, and validation helpers.

use std::collections::HashMap;
use std::io::Read;

use zbus::zvariant::OwnedValue;

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

/// Maximum accepted OBEX native image size in bytes.
///
/// Larger descriptors risk Android `TransactionTooLargeException` crashes on the
/// phone side when the bitmaps are scaled during the Binder IPC reply.
pub const BIP_MAX_NATIVE_BYTES: u64 = 280_000;

/// Default per-device resolution ceiling in pixels (800x800).
pub const BIP_DEFAULT_CAP_PX: u32 = 640_000;

/// Minimum pixel area for a Tier-1 candidate; 200x200 is reserved for the thumbnail tier.
pub const BIP_MIN_HIGHRES_PX: u32 = 40_000;

/// Poll interval for OBEX `Transfer1` progress watchdogs, in milliseconds.
pub const BIP_POLL_INTERVAL_MS: u64 = 40;

/// Stall watchdog: abort a transfer only after this many milliseconds with zero byte progress.
pub const BIP_STALL_TIMEOUT_MS: u64 = 2_500;

/// Total watchdog for the zero-descriptor native pull, in milliseconds.
pub const BIP_NATIVE_TIMEOUT_MS: u64 = 8_000;

/// Total watchdog for an explicit high-res variant transfer, in milliseconds.
pub const BIP_VARIANT_TIMEOUT_MS: u64 = 4_000;

/// Total watchdog for the baseline `Image1.GetThumbnail` transfer, in milliseconds.
pub const BIP_THUMBNAIL_TIMEOUT_MS: u64 = 2_500;

/// Settle wait after `TrackChanged` before any OBEX traffic, in milliseconds.
///
/// Keep short: live Pi measurements show OBEX Get requests are accepted ~90 ms
/// after fetch start, so a long fixed sleep only adds dead latency to every
/// track change.
pub const BIP_TRACK_SETTLE_MS: u64 = 80;

/// Default per-device maximum safe native image size in bytes for the zero-descriptor pull.
pub const BIP_DEFAULT_NATIVE_BYTES_CAP: u64 = 1_500_000;

/// ACL link supervision timeout in Bluetooth clock slots (32000 slots = 20 s).
pub const HCI_SUPERVISION_TIMEOUT_SLOTS: u32 = 32_000;

/// Raised kernel socket receive buffer ceiling (`net.core.rmem_max`), in bytes.
pub const BT_SOCKET_RMEM_MAX: u32 = 2_097_152;

/// Raised kernel socket receive buffer default (`net.core.rmem_default`), in bytes.
pub const BT_SOCKET_RMEM_DEFAULT: u32 = 1_048_576;

/// Synthetic explicit targets used when a BIP descriptor advertises an open range.
pub const BIP_SYNTHETIC_TARGETS: [(u32, u32); 3] = [(800, 800), (600, 600), (400, 400)];

/// A concrete BIP image variant selected for an `Image1.Get` transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BipImageCandidate {
    /// BIP `Pixel` descriptor string, e.g. `600*600`.
    pub pixel: String,
    /// BIP `Encoding` string, e.g. `JPEG`.
    pub encoding: String,
    /// Pixel area `width * height` used for step-down ordering.
    pub area: u32,
}

/// Parses a BIP `Pixel` descriptor like `600*600` or a range `0*0-65535*65535`.
#[must_use]
pub fn parse_bip_pixel(s: &str) -> Option<(u32, u32)> {
    let first = s.split('-').next()?;
    let (w, h) = first.split_once('*')?;
    let w = w.trim().parse::<u32>().ok()?;
    let h = h.trim().parse::<u32>().ok()?;
    Some((w, h))
}

/// Whether a BIP `Pixel` descriptor is an open-ended range (min `0*0`).
#[must_use]
pub fn is_bip_pixel_range(s: &str) -> bool {
    s.split('-').next() == Some("0*0")
}

/// Returns the maximum advertised native image `Size` in bytes across all descriptors.
///
/// Compares the `Size` key case-insensitively (BIP descriptors are case-insensitive in
/// practice) and returns 0 when no descriptor advertises a parseable byte count, meaning
/// the native size is unknown and a zero-descriptor pull is safe to attempt.
#[must_use]
pub fn native_image_size<S: ::std::hash::BuildHasher>(
    descriptors: &[HashMap<String, OwnedValue, S>],
) -> u64 {
    descriptors
        .iter()
        .filter_map(|descriptor| {
            descriptor
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("Size"))
                .and_then(|(_, v)| v.downcast_ref::<u64>().ok())
        })
        .max()
        .unwrap_or(0)
}

/// Selects BIP `Get` candidates from `Image1.Properties` descriptors, descending by pixel area.
///
/// Descriptors advertising `Size` above [`BIP_MAX_NATIVE_BYTES`] are discarded, as are
/// candidates above the per-device pixel ceiling or at/below [`BIP_MIN_HIGHRES_PX`]
/// (the 200x200 baseline is reserved for the thumbnail fallback tier). Open range
/// descriptors are expanded into the explicit [`BIP_SYNTHETIC_TARGETS`] that fit the ceiling.
#[must_use]
pub fn select_bip_candidates(
    descriptors: &[HashMap<String, OwnedValue>],
    max_cap_px: u32,
) -> Vec<BipImageCandidate> {
    let mut out: Vec<BipImageCandidate> = Vec::new();
    for descriptor in descriptors {
        // BIP descriptors are case-insensitive in practice (obexd emits lowercase keys).
        let get_value = |key: &str| -> Option<&OwnedValue> {
            descriptor
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v)
        };
        let get_str = |key: &str| -> Option<String> {
            get_value(key)
                .and_then(|v| v.downcast_ref::<&str>().ok())
                .map(str::to_owned)
        };
        let (Some(pixel), Some(encoding)) = (get_str("Pixel"), get_str("Encoding")) else {
            continue;
        };
        // `Size` is in bytes; BIP `MaxSize` is in kilobytes.
        let size_bytes = get_value("Size")
            .and_then(|v| v.downcast_ref::<u64>().ok())
            .unwrap_or(0);
        let max_size_kb = get_value("MaxSize")
            .and_then(|v| v.downcast_ref::<u64>().ok())
            .unwrap_or(0);
        if size_bytes > BIP_MAX_NATIVE_BYTES || max_size_kb > BIP_MAX_NATIVE_BYTES / 1024 {
            continue;
        }
        if is_bip_pixel_range(&pixel) {
            for (w, h) in BIP_SYNTHETIC_TARGETS {
                let Some(area) = w.checked_mul(h) else {
                    continue;
                };
                if area <= max_cap_px && area > BIP_MIN_HIGHRES_PX {
                    out.push(BipImageCandidate {
                        pixel: format!("{w}*{h}"),
                        encoding: encoding.clone(),
                        area,
                    });
                }
            }
            continue;
        }
        let Some((w, h)) = parse_bip_pixel(&pixel) else {
            continue;
        };
        // Descriptor dimensions come from the remote phone's `Image1.Properties`
        // (peer-controlled OBEX data); reject any pair whose product overflows `u32`
        // instead of wrapping around and letting a huge candidate pass the cap filter.
        let Some(area) = w.checked_mul(h) else {
            continue;
        };
        if area > max_cap_px || area <= BIP_MIN_HIGHRES_PX {
            continue;
        }
        out.push(BipImageCandidate {
            pixel,
            encoding,
            area,
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.area));
    out.dedup_by(|a, b| a.pixel == b.pixel && a.encoding == b.encoding);
    out
}

/// Per-device resolution ceilings keyed by sanitized MAC address.
pub type DeviceCaps = HashMap<String, u32>;

/// Name of the persistent per-device BIP resolution ceiling file.
pub const DEVICE_CAPS_FILENAME: &str = "device_caps.json";

/// Loads per-device BIP pixel ceilings from a directory (hermetic variant).
#[must_use]
pub fn load_device_caps_in(dir: &std::path::Path) -> DeviceCaps {
    std::fs::read_to_string(dir.join(DEVICE_CAPS_FILENAME))
        .ok()
        .and_then(|raw| serde_json::from_str::<DeviceCaps>(&raw).ok())
        .unwrap_or_default()
}

/// Persists a single device ceiling into a caps file (read-modify-write).
///
/// # Errors
/// Returns an error string when the file cannot be written.
pub fn store_device_cap_in(
    dir: &std::path::Path,
    sanitized_mac: &str,
    max_px: u32,
) -> Result<(), Box<str>> {
    let mut caps = load_device_caps_in(dir);
    caps.insert(sanitized_mac.to_owned(), max_px);
    let raw =
        serde_json::to_string(&caps).map_err(|e| Box::<str>::from(format!("caps encode: {e}")))?;
    std::fs::write(dir.join(DEVICE_CAPS_FILENAME), raw)
        .map_err(|e| Box::<str>::from(format!("cannot write {DEVICE_CAPS_FILENAME}: {e}")))
}

/// Per-device native-pull byte ceilings keyed by sanitized MAC address.
pub type NativeCaps = HashMap<String, u64>;

/// Name of the persistent per-device native byte ceiling file.
pub const NATIVE_CAPS_FILENAME: &str = "native_caps.json";

/// Loads per-device native byte ceilings from a directory (hermetic variant).
#[must_use]
pub fn load_native_caps_in(dir: &std::path::Path) -> NativeCaps {
    std::fs::read_to_string(dir.join(NATIVE_CAPS_FILENAME))
        .ok()
        .and_then(|raw| serde_json::from_str::<NativeCaps>(&raw).ok())
        .unwrap_or_default()
}

/// Persists a single native byte ceiling into a caps file (read-modify-write).
///
/// # Errors
/// Returns an error string when the file cannot be written.
pub fn store_native_cap_in(
    dir: &std::path::Path,
    sanitized_mac: &str,
    max_bytes: u64,
) -> Result<(), Box<str>> {
    let mut caps = load_native_caps_in(dir);
    caps.insert(sanitized_mac.to_owned(), max_bytes);
    let raw = serde_json::to_string(&caps)
        .map_err(|e| Box::<str>::from(format!("native caps encode: {e}")))?;
    std::fs::write(dir.join(NATIVE_CAPS_FILENAME), raw)
        .map_err(|e| Box::<str>::from(format!("cannot write {NATIVE_CAPS_FILENAME}: {e}")))
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
pub(super) fn ensure_cover_art_cache_dir_in(dir: &std::path::Path) -> Result<(), Box<str>> {
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

/// Sanitizes a BIP image handle into a filesystem-safe cache filename component.
///
/// Keeps ASCII alphanumerics plus `_` and `-`; returns `None` when nothing remains.
#[must_use]
pub fn sanitize_bip_handle(handle: &str) -> Option<String> {
    let sanitized: String = handle
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
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
    let sanitized_handle = sanitize_bip_handle(handle)?;
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
            if let Ok(file_name) = entry.file_name().into_string() {
                if file_name.starts_with(&prefix)
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
