# MPD Audio Stream Metadata & Cover-Art Resolution Plan

> **Status:** **Implemented & verified on Pi (`192.168.2.120`)** (2026-11-27).
>
> **Scope:** Reconcile Bluetooth and MPD (`v0.25`) metadata & cover-art pipelines in this repository (`speaker-mpd` + `screen-manager`), and add full metadata + cover-art resolution for MPD internet radio and audio streams (`http://`, `https://`, `.m3u8` HLS, ICY/SHOUTcast, Ogg/Opus/FLAC streams).
>
> **Architecture note:** This appliance runs upstream C++ **MPD v0.25** (`/usr/local/bin/mpd`, `database=false`; see [`docs/mpd_plan.md`](mpd_plan.md)) — *not* `rmpd`. The `"rmpd"` / `src_rmpd` identifiers in `audio-manager` and `/etc/asound.conf` are preserved only as internal source/ALSA routing IDs. Unlike a standalone `mpd-art-daemon` writing to `/dev/fb0` or `/dev/shm/cover.png`, artwork and metadata belong inside `speaker-mpd` and `screen-manager`'s existing `mpd-monitor` worker + Slint UI (`NowPlayingSnapshot` + M3 `theme_engine`).

---

## 1. Current State & Reconciliation: Bluetooth vs. MPD

Both audio sources already converge on `NowPlayingSnapshot` ([`services/screen-manager/src/ui/now_playing.rs`](../services/screen-manager/src/ui/now_playing.rs)) and per-source cover slots (`bt_cover_image`/`mpd_cover_image`, `bt_cover_seed`/`mpd_cover_seed`) in `AppShell` ([`services/screen-manager/src/ui/shell.rs`](../services/screen-manager/src/ui/shell.rs)). However, MPD streams currently have several parity gaps compared to Bluetooth:

| Dimension | Bluetooth (`speaker-bluetooth` + `bt-manager`) | MPD Current (`speaker-mpd` + `sources_worker.rs`) | Reconciled Target (MPD Streams) |
| :--- | :--- | :--- | :--- |
| **`source_label`** (top bar `track_or_device`) | `pb.device_name` (e.g., `"iPhone 15 Pro"`) | Hardcoded `""` | Station `Name` tag (from MPD `CurrentSong`), falling back to `"Music Player"` |
| **`title` / `artist` / `album`** | `MediaPlayer1.Track` (`Title`, `Artist`, `Album`) | `s.title()`, `s.artists().first()`, `s.album()` | Parse `Name` tag; when `artist` is empty on an HTTP stream and `title` is `"Artist - Title"`, split into `(artist, title)` |
| **Codec badge** (`StreamMetadata.codec`) | `BlueAlsaPcm.selected_codec` (`"AAC"`, `"LDAC"`, `"SBC"`) | File extension of `t.file` only (`"PCM"` for extensionless URLs) | Infer from URL path/extension, probed HTTP `Content-Type`, or stream container (`MP3`, `AAC`, `FLAC`, `OPUS`, `OGG`, `HLS`) |
| **Stream format & detail** | BlueALSA `sampling_rate`, `bit_depth`, `channels`, `delay` | MPD `audio` line + `bitrate_kbps` (`"128 kbps"`) | Keep MPD `audio` line + `bitrate_kbps`; ensure live streams (`duration = None`) report `duration_known = false` |
| **Cover art trigger key** | `(device_address, image_handle, bip_epoch)` via `org.speaker.CoverArt1` | `current_track.file` string only | `MpdArtKey { file, title, artist, station_name }` so ICY track changes on a constant stream URL re-trigger resolution |
| **Cover art transport** | `bt-manager` BIP OBEX -> `/var/cache/speaker-covers/*.jpg` -> `decode_cover_buffer` | `MpdDevice::read_album_art(file)` (`albumart` -> `readpicture`) -> `decode_cover_bytes` | Multi-tier waterfall (§2) feeding the same `decode_cover_bytes` -> `(CoverPixelBuffer, Option<u32>)` pipeline |

---

## 2. Artwork & Stream Metadata Resolution Waterfall

On every MPD track, station, or ICY title change (`MpdArtKey` change), `mpd-monitor` resolves artwork in strict priority order and stops at the first valid decoded image:

1. **Local / Non-HTTP `song.file`:**
   * Query MPD's native `albumart` and `readpicture` commands via `MpdDevice::read_album_art(file)` ([`crates/mpd/src/client.rs`](../crates/mpd/src/client.rs)).
   * *(Note: MPD v0.25 is currently built with `database=false`, so this primarily serves direct local URIs or future database-enabled builds.)*
2. **Source Broadcast Extraction (HTTP/HTTPS `song.file`):**
   * **HLS (`.m3u8`):** Parse media playlist via `m3u8-rs`, fetch the latest segment (bounded read), and extract ID3v2 `APIC` image bytes or `WXXX` artwork URLs via `lofty`.
   * **ICY / Direct HTTP Streams:** Open a single bounded `GET` request with `Icy-MetaData: 1`:
     * Check `icy-logo` response header first.
     * If `icy-metaint` is present, skip up to 3 audio+metadata blocks (bounded total bytes & short timeout) to parse `StreamUrl='...'` (and `StreamTitle='...'` if MPD hasn't surfaced it yet).
     * For Ogg/Opus/FLAC streams (`Content-Type` `application/ogg`, `audio/ogg`, `audio/flac`), read initial stream pages and extract `METADATA_BLOCK_PICTURE` via `lofty`.
     * Drop the HTTP stream connection immediately after header/metadata/picture extraction.
     * When an artwork URL (`icy-logo` or `StreamUrl`) is found, fetch it with `Content-Type: image/*` and max-size validation (e.g. 4 MiB).
3. **Track Metadata Artwork Lookup (Optional / Configurable):**
   * Many radio streams send `StreamTitle` (`Artist - Title`) without `StreamUrl` or `icy-logo`.
   * When enabled in config and `(artist, title)` are non-empty (and not a station-ID/ad string), query an artwork endpoint (e.g. iTunes Search API) with a short timeout and cache by normalized `(artist, title)`.
4. **Station Fallback Manifest (`/etc/speaker/stations.toml`):**
   * Match `song.file` (`url_match` substring/regex) or `station_name` (`name_match` case-insensitive substring/regex) against local station logos (e.g. `/var/lib/speaker/station-logos/`).
5. **Radio-Browser Station Logo Lookup:**
   * If no local manifest rule matches and `station_name` is present, query `https://de1.api.radio-browser.info/json/stations/search` (cached in LRU by cleaned station name) so HLS and ICY stations without `StreamUrl`/`icy-logo` still resolve official station artwork.
6. **Default Fallback:**
   * If `default_logo` is configured in `stations.toml` and exists on disk, decode and display it; otherwise publish `None` (`has_cover_art = false`), letting `now_playing.slint` render the built-in procedural vinyl / default artwork.

---

## 3. Codebase Integration & Module Layout

Instead of a separate `mpd-art-daemon` binary, integrate into the existing workspace crates and `screen-manager` service:

### 3.1 `crates/mpd` (`speaker-mpd`)
* **`crates/mpd/src/client.rs`:**
  * Extend `MpdTrack` with `pub station_name: Option<Box<str>>` populated from MPD `Song` `Name` tag (`Tag::Name`).
  * Add `MpdTrack::parsed_artist_title(&self) -> (Option<&str>, Option<&str>)` (or normalize in `NowPlayingSnapshot::from_mpd`) to split `"Artist - Title"` on HTTP streams when `artist` is `None`.
  * Improve `MpdTrack::codec_label(&self)` (and add `codec_label_with_mime(content_type: Option<&str>)`) so HTTP streams without file extensions (`.../stream`, `.../mp3`, `.../aacp`) resolve accurately.
* **`crates/mpd/src/stream_art.rs`** *(new module, or in `speaker-mpd` behind a `stream-art` feature if `audio-manager` doesn't need HTTP deps)*:
  * Strong domain types: `StreamUrl`, `MpdArtKey`, `StreamProbeResult { codec, icy_logo, stream_url_art, embedded_picture }`, `StationManifest`, `StationRule`.
  * `StreamArtResolver` with inherent mockable backend (`StreamArtResolver::new_mocked()`, matching `MpdDevice::new_mocked()`) so all ICY byte-skipping, HLS parsing, `lofty` picture extraction, LRU caching, and `stations.toml` matching are 100% unit/integration-testable on `x86_64` with zero network I/O.

### 3.2 `services/screen-manager`
* **`services/screen-manager/src/ui/now_playing.rs`:**
  * Update `NowPlayingSnapshot::from_mpd` to populate `source_label` from `station_name` (fallback `"Music Player"`), apply `"Artist - Title"` splitting for HTTP streams, and use probed stream codec when available.
* **`services/screen-manager/src/ui/sources_worker.rs`:**
  * Replace `last_art_file: Option<Box<str>>` with `last_art_key: Option<MpdArtKey>` (`file`, `title`, `artist`, `station_name`).
  * Run HTTP stream probes asynchronously / via `spawn_blocking` with a generation/cancellation guard so rapid station switching drops stale probes and never stalls `run_mpd_worker`'s `tokio::select!` loop.
  * Feed resolved image bytes into existing `decode_cover_bytes(&bytes)` -> `publish_mpd_cover(shared, Some(buf), seed)`.
  * Optionally propagate probed stream codec (`MP3`/`AAC`/`FLAC`/`OPUS`) into `BtSharedUpdate` / `StreamMetadata` so `CodecHud` displays the true stream codec instead of `"PCM"`.

---

## 4. Workspace Dependencies & Cross-Compilation Considerations

Target hardware is **Raspberry Pi 3 Model B (`aarch64-unknown-linux-gnu`, Cortex-A53, 1 GiB RAM)** with strict workspace lints (`edition = "2024"`, `clippy::pedantic`).

* **`image` (`=0.25.10`):** Already in workspace `Cargo.toml` with `default-features = false, features = ["png", "jpeg"]`. Add `"webp"` if station/API artwork returns WebP.
* **HTTP client (`ureq` `3.x` vs. `reqwest`):**
  * `ureq` (`default-features = false, features = ["rustls"]`) is lean and avoids extra async runtime/hyper bloat; run inside `tokio::task::spawn_blocking` with explicit connect/read timeouts (e.g. `3s` connect, `5s` total probe budget).
  * **TLS caveat:** Legacy SHOUTcast/Icecast servers occasionally require older TLS/ciphers; test against target stations and evaluate `native-tls` only if `rustls` fails on required streams (note `native-tls` requires OpenSSL in the `aarch64` sysroot).
* **Playlist & Tag parsing:**
  * `m3u8-rs` (`6.0.1`) for `.m3u8` HLS playlists.
  * `lofty` (`0.25.5`, `default-features = false`) for ID3v2 `APIC`/`WXXX` and Ogg/Opus/FLAC `METADATA_BLOCK_PICTURE`. (Verify `lofty` MSRV against repo's pinned `rust-toolchain.toml` before adding.)
* **Config & Cache:**
  * `toml` + `serde` (already in workspace) for `/etc/speaker/stations.toml`.
  * `lru` (or small bounded `VecDeque`/`HashMap` like `BoundedSeedCache` in `theme_engine.rs`) for `(stream_url, art_url) -> bytes` and `(artist, title) -> bytes` caches.
* **Single-listener stream caveat:** Probing an HTTP stream alongside MPD briefly opens a second connection from the Pi's IP; strict single-listener streams will reject the probe, which must fail fast and fall through to Track Lookup / Station Manifest without affecting MPD playback.

---

## 5. Configuration Schema (`/etc/speaker/stations.toml`)

```toml
# Optional local fallback image; omit to use the built-in Slint vinyl/default art.
# default_logo = "/var/lib/speaker/station-logos/default.png"

enable_track_art_lookup = true

[[station]]
name_match = "Radio Paradise"
url_match = "radioparadise.com"
logo = "/var/lib/speaker/station-logos/radio-paradise.png"

[[station]]
name_match = "SomaFM"
url_match = "somafm.com"
logo = "/var/lib/speaker/station-logos/somafm.png"
```

---

## 6. Implementation Phases & Verification Gates

1. **Phase 1 — Metadata Reconciliation (`speaker-mpd` + `now_playing.rs`):**
   * Extract `station_name` (`Name` tag) in `MpdTrack`, split `"Artist - Title"` on HTTP streams, populate `source_label`, and improve `codec_label` for stream URLs.
   * Update `sources_worker.rs` art-refresh key to `MpdArtKey` (`file`, `title`, `artist`, `station_name`).
2. **Phase 2 — Stream Prober & Waterfall (`speaker-mpd` `stream_art` + `sources_worker.rs`):**
   * Implement ICY header/metadata-block parser, HLS segment ID3 extractor, Ogg/Opus/FLAC `METADATA_BLOCK_PICTURE` extractor, `Content-Type: image/*` guard, bounded LRU cache, and `stations.toml` manifest loader.
   * Wire into `refresh_mpd_cover` with non-blocking execution and cancellation on track/station switch.
3. **Phase 3 — Testing & Coverage (`AGENTS.md` §4.1):**
   * Integration tests under `crates/mpd/tests/` and `services/screen-manager/tests/` using `MpdDevice::new_mocked()` and `StreamArtResolver::new_mocked()` (synthetic ICY frames, HLS/ID3 fixtures, Ogg pictures, malformed headers, timeouts, `stations.toml` matching).
   * Pass all gates:
     1. `cargo fmt --all -- --check`
     2. `cargo clippy --workspace --all-targets -- -D warnings`
     3. `cargo llvm-cov --workspace --target x86_64-unknown-linux-gnu --fail-under-lines 85 --fail-under-functions 85`
     4. `cargo build --target aarch64-unknown-linux-gnu --release --bins`
