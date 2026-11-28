//! Integration tests for MPD stream metadata parsing and the cover-art waterfall.

use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;
use std::time::Duration;

use speaker_mpd::{
    ArtSource, MpdArtKey, MpdDevice, MpdTrack, StationManifest, StreamArtResolver, StreamArtResult,
    extract_embedded_tag_art, extract_icy_stream_metadata, parse_hls_target_url,
    parse_icy_metadata_block, resolve_relative_url,
};

fn build_id3v2_apic(mime: &str, image_data: &[u8]) -> Vec<u8> {
    let mut frame_body = Vec::new();
    frame_body.push(0x00); // ISO-8859-1 encoding
    frame_body.extend_from_slice(mime.as_bytes());
    frame_body.push(0x00);
    frame_body.push(0x03); // Cover (front)
    frame_body.push(0x00); // Empty description
    frame_body.extend_from_slice(image_data);

    let frame_size = u32::try_from(frame_body.len()).expect("small frame");
    let mut tag_body = Vec::new();
    tag_body.extend_from_slice(b"APIC");
    tag_body.extend_from_slice(&frame_size.to_be_bytes());
    tag_body.extend_from_slice(&[0x00, 0x00]); // flags
    tag_body.extend_from_slice(&frame_body);

    let tag_size = u32::try_from(tag_body.len()).expect("small tag");
    let syncsafe = [
        ((tag_size >> 21) & 0x7F) as u8,
        ((tag_size >> 14) & 0x7F) as u8,
        ((tag_size >> 7) & 0x7F) as u8,
        (tag_size & 0x7F) as u8,
    ];
    let mut out = Vec::new();
    out.extend_from_slice(b"ID3");
    out.extend_from_slice(&[0x03, 0x00, 0x00]); // ID3v2.3.0
    out.extend_from_slice(&syncsafe);
    out.extend_from_slice(&tag_body);
    // Minimal MPEG1 Layer3 frame (417 bytes at 128kbps / 44.1kHz) + second sync so lofty recognizes MP3
    out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    out.extend_from_slice(&[0x00; 413]);
    out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    out.extend_from_slice(&[0x00; 413]);
    out
}

#[test]
fn mpd_track_stream_metadata_and_codec_inference() {
    let icy = MpdTrack {
        file: "https://ice1.somafm.com/groovesalad-128-mp3".into(),
        title: Some("Boards of Canada - Roygbiv".into()),
        artist: None,
        album: None,
        station_name: Some("SomaFM Groove Salad".into()),
        duration: None,
    };
    assert!(icy.is_stream_url());
    assert_eq!(
        icy.resolved_artist_title(),
        (Some("Boards of Canada"), Some("Roygbiv"))
    );
    assert_eq!(icy.codec_label(), "MP3");
    assert_eq!(
        icy.codec_label_with_mime(Some("audio/aacp; charset=utf-8")),
        "AAC"
    );
    assert_eq!(icy.codec_label_with_mime(Some("audio/flac")), "FLAC");
    assert_eq!(icy.codec_label_with_mime(Some("audio/opus")), "OPUS");
    assert_eq!(icy.codec_label_with_mime(Some("application/ogg")), "OGG");
    assert_eq!(
        icy.codec_label_with_mime(Some("application/vnd.apple.mpegurl")),
        "HLS"
    );
    assert_eq!(icy.codec_label_with_mime(Some("audio/wav")), "WAV");

    let explicit_artist = MpdTrack {
        file: "http://stream.radioparadise.com/flac".into(),
        title: Some("Some - Song".into()),
        artist: Some("Explicit Artist".into()),
        ..MpdTrack::default()
    };
    assert_eq!(
        explicit_artist.resolved_artist_title(),
        (Some("Explicit Artist"), Some("Some - Song"))
    );
    assert_eq!(explicit_artist.codec_label(), "FLAC");

    let local = MpdTrack {
        file: "local/Artist - Song.unknown".into(),
        title: Some("Artist - Song".into()),
        ..MpdTrack::default()
    };
    assert!(!local.is_stream_url());
    assert_eq!(local.resolved_artist_title(), (None, Some("Artist - Song")));
    assert_eq!(local.codec_label(), "PCM");

    // Multi-byte UTF-8 paths must never panic in is_stream_url()
    let utf8_track = MpdTrack {
        file: "日本語の曲/トラック.flac".into(),
        ..MpdTrack::default()
    };
    assert!(!utf8_track.is_stream_url());
    assert_eq!(utf8_track.codec_label(), "FLAC");

    for (url, expected) in [
        ("http://example.com/live.m3u8", "HLS"),
        ("http://example.com/live/aacp", "AAC"),
        ("http://example.com/live/opus", "OPUS"),
        ("http://example.com/live/ogg", "OGG"),
        ("http://example.com/live/hls", "HLS"),
        ("http://example.com/stream", "PCM"),
    ] {
        let t = MpdTrack {
            file: url.into(),
            ..MpdTrack::default()
        };
        assert_eq!(t.codec_label(), expected, "url={url}");
    }
}

#[test]
fn icy_block_and_stream_metadata_parsing() {
    let raw = b"StreamTitle='Pink Floyd - Time';StreamUrl='https://art.example/dsotm.jpg';\0\0\0";
    let parsed = parse_icy_metadata_block(raw);
    assert_eq!(parsed.stream_title.as_deref(), Some("Pink Floyd - Time"));
    assert_eq!(
        parsed.stream_url.as_deref(),
        Some("https://art.example/dsotm.jpg")
    );

    // Build stream with metaint = 8:
    // Block 1: 8 audio bytes + len_byte=0 (empty metadata)
    // Block 2: 8 audio bytes + len_byte=3 (48 bytes metadata)
    let mut stream = Vec::new();
    stream.extend_from_slice(&[0xAA; 8]);
    stream.push(0);
    stream.extend_from_slice(&[0xBB; 8]);
    let mut meta = b"StreamTitle='Track';StreamUrl='http://x/a.png';".to_vec();
    while !meta.len().is_multiple_of(16) {
        meta.push(0);
    }
    stream.push(u8::try_from(meta.len() / 16).unwrap());
    stream.extend_from_slice(&meta);

    let mut cursor = Cursor::new(stream);
    let found = extract_icy_stream_metadata(&mut cursor, 8, 3).expect("icy meta");
    assert_eq!(found.stream_url.as_deref(), Some("http://x/a.png"));

    // Invalid metaint=0 returns None.
    assert!(extract_icy_stream_metadata(&mut Cursor::new(vec![1, 2]), 0, 3).is_none());
}

#[test]
fn hls_playlist_and_relative_url_resolution() {
    assert_eq!(
        resolve_relative_url("https://cdn.example.com/live/master.m3u8?t=1", "chunk.m3u8"),
        "https://cdn.example.com/live/chunk.m3u8"
    );
    assert_eq!(
        resolve_relative_url("https://cdn.example.com/live/master.m3u8", "/root/seg.aac"),
        "https://cdn.example.com/root/seg.aac"
    );
    assert_eq!(
        resolve_relative_url(
            "https://cdn.example.com/live/master.m3u8",
            "https://other/a.aac"
        ),
        "https://other/a.aac"
    );
    assert_eq!(
        resolve_relative_url("http://host:8000", "seg.m3u8"),
        "http://host:8000/seg.m3u8"
    );

    let master = b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000\naac/128.m3u8\n";
    assert_eq!(
        parse_hls_target_url(master, "https://cdn.example.com/master.m3u8"),
        Some(("https://cdn.example.com/aac/128.m3u8".to_string(), true))
    );

    let media =
        b"#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\nseg1.aac\n#EXTINF:6.0,\nseg2.aac\n";
    assert_eq!(
        parse_hls_target_url(media, "https://cdn.example.com/aac/128.m3u8"),
        Some(("https://cdn.example.com/aac/seg2.aac".to_string(), false))
    );
}

#[test]
fn embedded_id3_apic_extraction_with_lofty() {
    assert!(extract_embedded_tag_art(&[]).picture_bytes.is_none());
    assert!(
        extract_embedded_tag_art(b"not-audio")
            .picture_bytes
            .is_none()
    );

    let fake_jpeg = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0xFF, 0xD9,
    ];
    let mp3_bytes = build_id3v2_apic("image/jpeg", &fake_jpeg);
    let art = extract_embedded_tag_art(&mp3_bytes);
    assert_eq!(art.picture_bytes, Some(fake_jpeg));
}

#[test]
fn station_manifest_parsing_and_matching() {
    let toml_cfg = r#"
default_logo = "/var/lib/speaker/station-logos/default.png"
enable_track_art_lookup = false

[[station]]
name_match = "Radio Paradise"
url_match = "radioparadise\\.com"
logo = "/var/lib/speaker/station-logos/rp.png"

[[station]]
name_match = "SomaFM"
logo = "/var/lib/speaker/station-logos/somafm.png"
"#;
    let manifest = StationManifest::from_toml_str(toml_cfg).unwrap();
    assert!(!manifest.track_art_lookup_enabled());
    assert_eq!(
        manifest.default_logo(),
        Some("/var/lib/speaker/station-logos/default.png")
    );
    assert_eq!(
        manifest.match_station_logo("https://stream.radioparadise.com/flac", None),
        Some("/var/lib/speaker/station-logos/rp.png")
    );
    assert_eq!(
        manifest.match_station_logo("https://ice1.somafm.com/drone", Some("SomaFM Drone Zone")),
        Some("/var/lib/speaker/station-logos/somafm.png")
    );
    assert_eq!(
        manifest.match_station_logo("https://other.example/stream", Some("Other")),
        None
    );

    StationManifest::from_toml_str("not valid = [toml").unwrap_err();
    StationManifest::from_toml_str("[[station]]\nname_match = \"[unclosed\"\nlogo = \"a.png\"")
        .unwrap_err();
    assert!(
        StationManifest::load_or_default(Path::new("/nonexistent/stations.toml"))
            .default_logo()
            .is_none()
    );
}

#[tokio::test]
async fn mocked_waterfall_resolves_in_strict_priority_order() {
    let manifest = StationManifest::from_toml_str(
        r#"
default_logo = "/logos/default.png"
enable_track_art_lookup = true

[[station]]
url_match = "somafm\\.com"
logo = "/logos/somafm.png"
"#,
    )
    .unwrap();
    let (resolver, art_mock) = StreamArtResolver::new_mocked(manifest);
    let (mpd, mpd_mock) = MpdDevice::new_mocked();

    art_mock.set_local_file("/logos/somafm.png", b"SOMAFM_LOGO".to_vec());
    art_mock.set_local_file("/logos/default.png", b"DEFAULT_LOGO".to_vec());
    art_mock.set_track_lookup("Artist", "Song", b"ITUNES_ART".to_vec());
    mpd_mock.set_album_art("local/track.flac", Some(b"MPD_LOCAL".to_vec()));

    // 1. Local file -> MpdNative
    let local = MpdTrack {
        file: "local/track.flac".into(),
        ..MpdTrack::default()
    };
    let res1 = resolver.resolve_for_track(&local, &mpd).await;
    assert_eq!(res1.source, ArtSource::MpdNative);
    assert_eq!(res1.image_bytes.as_deref(), Some(b"MPD_LOCAL".as_slice()));

    // 2. Broadcast probe wins over TrackLookup and StationManifest
    let stream_url = "https://ice1.somafm.com/groovesalad-128-mp3";
    art_mock.set_stream_probe(
        stream_url,
        StreamArtResult {
            image_bytes: Some(b"BROADCAST_ART".to_vec()),
            probed_codec: Some("MP3"),
            source: ArtSource::BroadcastStreamUrl,
            ..StreamArtResult::default()
        },
    );
    let track_stream = MpdTrack {
        file: stream_url.into(),
        title: Some("Artist - Song".into()),
        station_name: Some("SomaFM".into()),
        ..MpdTrack::default()
    };
    let res2 = resolver.resolve_for_track(&track_stream, &mpd).await;
    assert_eq!(res2.source, ArtSource::BroadcastStreamUrl);
    assert_eq!(res2.probed_codec, Some("MP3"));
    assert_eq!(
        res2.image_bytes.as_deref(),
        Some(b"BROADCAST_ART".as_slice())
    );

    // 3. No broadcast art -> TrackLookup wins over StationManifest
    art_mock.set_stream_probe(
        stream_url,
        StreamArtResult {
            image_bytes: None,
            probed_codec: Some("MP3"),
            source: ArtSource::None,
            ..StreamArtResult::default()
        },
    );
    let res3 = resolver.resolve_for_track(&track_stream, &mpd).await;
    assert_eq!(res3.source, ArtSource::TrackLookup);
    assert_eq!(res3.image_bytes.as_deref(), Some(b"ITUNES_ART".as_slice()));

    // 4. Unknown track -> StationManifest wins over DefaultLogo
    let unknown_track = MpdTrack {
        file: stream_url.into(),
        title: Some("Unknown - Track".into()),
        ..MpdTrack::default()
    };
    let res4 = resolver.resolve_for_track(&unknown_track, &mpd).await;
    assert_eq!(res4.source, ArtSource::StationManifest);
    assert_eq!(res4.image_bytes.as_deref(), Some(b"SOMAFM_LOGO".as_slice()));

    // 5. Unmatched station -> DefaultLogo
    // 5. Unmatched station with station_name -> RadioBrowser wins over DefaultLogo
    art_mock.set_station_lookup("FIP", b"RADIO_BROWSER_LOGO".to_vec());
    let fip_stream = MpdTrack {
        file: "https://fip.example/live.m3u8".into(),
        station_name: Some("FIP (HLS)".into()),
        ..MpdTrack::default()
    };
    let res5a = resolver.resolve_for_track(&fip_stream, &mpd).await;
    assert_eq!(res5a.source, ArtSource::RadioBrowser);
    assert_eq!(
        res5a.image_bytes.as_deref(),
        Some(b"RADIO_BROWSER_LOGO".as_slice())
    );

    // 6. Unmatched station with no RadioBrowser hit -> DefaultLogo
    let other_stream = MpdTrack {
        file: "https://other.example/live".into(),
        ..MpdTrack::default()
    };
    let res5 = resolver.resolve_for_track(&other_stream, &mpd).await;
    assert_eq!(res5.source, ArtSource::DefaultLogo);
    assert_eq!(
        res5.image_bytes.as_deref(),
        Some(b"DEFAULT_LOGO".as_slice())
    );

    // MpdArtKey changes when ICY title changes on the same URL
    let k1 = MpdArtKey::from_track(&track_stream);
    let k2 = MpdArtKey::from_track(&unknown_track);
    assert_ne!(k1, k2);
}

#[tokio::test]
async fn live_http_icy_stream_and_image_fetch_over_loopback() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://{addr}");

    let server = thread::spawn(move || {
        // Serve 2 requests: 1) /stream (ICY), 2) /art.png (image/png)
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut req_buf = [0u8; 1024];
            let n = stream.read(&mut req_buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&req_buf[..n]);
            if req.starts_with("GET /stream") {
                let mut meta =
                    format!("StreamTitle='DJ - Live';StreamUrl='{base_url}/art.png';").into_bytes();
                while !meta.len().is_multiple_of(16) {
                    meta.push(0);
                }
                let hdr = "HTTP/1.1 200 OK\r\nContent-Type: audio/aacp\r\nicy-metaint: 8\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(&[0x11; 8]);
                let _ = stream.write_all(&[u8::try_from(meta.len() / 16).unwrap()]);
                let _ = stream.write_all(&meta);
            } else {
                let body = b"\x89PNG\r\n\x1a\nFAKEPNG";
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(body);
            }
        }
    });

    let resolver = StreamArtResolver::new(StationManifest::default());
    let (mpd, _) = MpdDevice::new_mocked();
    let track = MpdTrack {
        file: format!("http://{addr}/stream").into(),
        ..MpdTrack::default()
    };

    let res = resolver.resolve_for_track(&track, &mpd).await;
    assert_eq!(res.source, ArtSource::BroadcastStreamUrl);
    assert_eq!(res.probed_codec, Some("AAC"));
    assert_eq!(
        res.image_bytes.as_deref(),
        Some(b"\x89PNG\r\n\x1a\nFAKEPNG".as_slice())
    );

    server.join().expect("server thread");
}

#[tokio::test]
async fn live_http_hls_m3u8_segment_id3_apic_over_loopback() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let fake_jpeg = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0xFF, 0xD9,
    ];
    let seg_bytes = build_id3v2_apic("image/jpeg", &fake_jpeg);

    let server = thread::spawn(move || {
        // 1) master.m3u8 -> 2) media.m3u8 -> 3) seg0.mp3
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut req_buf = [0u8; 1024];
            let n = stream.read(&mut req_buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&req_buf[..n]);
            if req.starts_with("GET /master.m3u8") {
                let body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000\nmedia.m3u8\n";
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            } else if req.starts_with("GET /media.m3u8") {
                let body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg0.mp3\n";
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            } else {
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    seg_bytes.len()
                );
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(&seg_bytes);
            }
        }
    });

    let resolver = StreamArtResolver::new(StationManifest::default());
    let (mpd, _) = MpdDevice::new_mocked();
    let track = MpdTrack {
        file: format!("http://{addr}/master.m3u8").into(),
        ..MpdTrack::default()
    };

    let res = resolver.resolve_for_track(&track, &mpd).await;
    assert_eq!(res.source, ArtSource::BroadcastHlsId3);
    assert_eq!(res.probed_codec, Some("HLS"));
    assert_eq!(res.image_bytes, Some(fake_jpeg));

    server.join().expect("server thread");
}

#[tokio::test]
async fn live_http_icy_logo_header_and_cache_and_disk_manifest() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://{addr}");

    let server = thread::spawn(move || {
        // Serve /icy-logo-stream (twice, art.png once due to LRU cache!)
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut req_buf = [0u8; 1024];
            let n = stream.read(&mut req_buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&req_buf[..n]);
            if req.starts_with("GET /icy-logo-stream") {
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nicy-logo: {base_url}/logo.png\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(hdr.as_bytes());
            } else {
                let body = b"\x89PNG\r\n\x1a\nLOGO";
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(body);
            }
        }
    });

    // Real disk-backed manifest & logo file
    let tmp_dir = std::env::temp_dir().join(format!("mpd_art_test_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let logo_path = tmp_dir.join("station.png");
    std::fs::write(&logo_path, b"\x89PNG\r\n\x1a\nDISK").unwrap();
    let manifest_path = tmp_dir.join("stations.toml");
    std::fs::write(
        &manifest_path,
        format!(
            "enable_track_art_lookup = false\n[[station]]\nurl_match = \"disk-station\"\nlogo = \"{}\"\n",
            logo_path.display()
        ),
    )
    .unwrap();

    let manifest = StationManifest::load_or_default(&manifest_path);
    let resolver = StreamArtResolver::new(manifest);
    let (mpd, _) = MpdDevice::new_mocked();

    let track = MpdTrack {
        file: format!("http://{addr}/icy-logo-stream").into(),
        ..MpdTrack::default()
    };
    let r1 = resolver.resolve_for_track(&track, &mpd).await;
    assert_eq!(r1.source, ArtSource::BroadcastIcyHeader);
    assert_eq!(r1.probed_codec, Some("MP3"));
    // Second request hits LRU cache for logo.png
    let r2 = resolver.resolve_for_track(&track, &mpd).await;
    assert_eq!(r2.source, ArtSource::BroadcastIcyHeader);

    // Local disk manifest fallback on non-HTTP file when MPD has no native art
    let disk_track = MpdTrack {
        file: "disk-station/track.flac".into(),
        ..MpdTrack::default()
    };
    let r3 = resolver.resolve_for_track(&disk_track, &mpd).await;
    assert_eq!(r3.source, ArtSource::StationManifest);
    assert_eq!(
        r3.image_bytes.as_deref(),
        Some(b"\x89PNG\r\n\x1a\nDISK".as_slice())
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    server.join().expect("server thread");
}
