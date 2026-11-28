#![cfg(feature = "ui")]
//! Integration tests for source-agnostic Now Playing abstraction, MPD
//! capabilities (seek, repeat, random, album art), and `AudioSources1` switching.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use screen_manager::daemon::{BtSharedUpdate, apply_shared_bt_state};
use screen_manager::ui::sources_worker::{
    join_worker, spawn_audio_sources_worker, spawn_mpd_monitor_with_device,
    spawn_mpd_monitor_with_resolver,
};
use screen_manager::ui::{
    AppShell, AudioSourceEntry, AudioSourcesState, Framebuffer, MainWindow, MediaSourceKind,
    MpdCommand, MpdCommandQueue, NowPlayingSnapshot, SOURCE_ID_BLUETOOTH, SOURCE_ID_MPD,
    SourceCommand, SourceCommandQueue,
};
use speaker_bluetooth::{BtPlaybackState, TransportCommand, TransportCommandQueue};
use speaker_mpd::{
    ArtSource, MpdDevice, MpdEvent, MpdMockCommand, MpdPlaybackState, MpdSubsystem, MpdTrack,
    StationManifest, StreamArtResolver, StreamArtResult,
};

fn make_shell() -> (AppShell, Framebuffer) {
    let fb = Framebuffer::new_mocked().0;
    let _platform = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    (shell, fb)
}

fn minimal_png_bytes() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([220, 40, 80, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

#[test]
fn now_playing_snapshot_maps_bluetooth_and_mpd_capabilities() {
    let bt = BtPlaybackState {
        title: "Blue Song".into(),
        artist: "Blue Artist".into(),
        album: "Blue Album".into(),
        status: "playing".into(),
        position_ms: 15_000,
        duration_ms: 120_000,
        device_name: "Pixel 8".into(),
        ..BtPlaybackState::default()
    };
    let bt_snap = NowPlayingSnapshot::from_bluetooth(&bt);
    assert_eq!(bt_snap.source, MediaSourceKind::Bluetooth);
    assert_eq!(bt_snap.source_label.as_ref(), "Pixel 8");
    assert!(bt_snap.capabilities.can_play_pause);
    assert!(!bt_snap.capabilities.can_seek);
    assert!(!bt_snap.capabilities.can_repeat);
    assert!(!bt_snap.capabilities.can_random);

    let (mpd, mock) = MpdDevice::new_mocked();
    mock.set_state(MpdPlaybackState::Playing);
    mock.set_repeat(true);
    mock.set_random(true);
    mock.set_audio_format(Some((44_100, 16, 2)));
    mock.set_track(
        Some(MpdTrack {
            file: "jazz/autumn.flac".into(),
            title: None,
            artist: Some("Bill Evans".into()),
            album: Some("Portrait in Jazz".into()),
            station_name: None,
            duration: Some(Duration::from_secs(240)),
        }),
        Some(Duration::from_secs(60)),
        Some(Duration::from_secs(240)),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let status = rt.block_on(mpd.read_status()).unwrap();
    let mpd_snap = NowPlayingSnapshot::from_mpd(&status);
    assert_eq!(mpd_snap.source, MediaSourceKind::Mpd);
    assert_eq!(mpd_snap.title.as_ref(), "autumn.flac");
    assert_eq!(mpd_snap.artist.as_ref(), "Bill Evans");
    assert!(mpd_snap.capabilities.can_play_pause);
    assert!(mpd_snap.capabilities.can_seek);
    assert!(mpd_snap.capabilities.can_repeat);
    assert!(mpd_snap.capabilities.can_random);
    assert_eq!(
        mpd_snap.stream_info.as_ref().map(|s| s.codec.as_ref()),
        Some("FLAC")
    );
    let stream_track = MpdTrack {
        file: "https://stream.example.com/jazz.aac?quality=high".into(),
        title: Some("Jazz FM".into()),
        ..MpdTrack::default()
    };
    assert_eq!(stream_track.codec_label(), "AAC");
}

#[expect(clippy::too_many_lines, reason = "end-to-end source switch test")]
#[test]
fn shell_routes_transport_and_preserves_per_source_state_across_switches() {
    let (shell, _fb) = make_shell();
    let bt_queue = TransportCommandQueue::new();
    let mpd_queue = MpdCommandQueue::new();
    let source_queue = SourceCommandQueue::new();

    shell.wire_bluetooth_transport(&bt_queue);
    shell.wire_mpd_transport(&mpd_queue);
    shell.wire_source_commands(&source_queue);

    // Seed Bluetooth playback and cover art.
    shell.apply_bluetooth_playback(&BtPlaybackState {
        title: "BT Track".into(),
        artist: "BT Artist".into(),
        status: "playing".into(),
        position_ms: 10_000,
        duration_ms: 100_000,
        device_name: "Phone".into(),
        ..BtPlaybackState::default()
    });
    let bt_buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(4, 4);
    shell.set_cover_image(slint::Image::from_rgba8(bt_buf));
    assert!(shell.window().get_has_cover_art());
    assert!(!shell.window().get_seekable());

    // Transport action while Bluetooth is active goes to Bluetooth queue.
    shell.window().invoke_next_track();
    assert_eq!(bt_queue.drain(), vec![TransportCommand::NextTrack]);
    assert_eq!(mpd_queue.drain(), Vec::new());

    // Seed MPD status while Bluetooth is still active (must not overwrite BT UI).
    let (mpd, mock) = MpdDevice::new_mocked();
    mock.set_state(MpdPlaybackState::Paused);
    mock.set_repeat(true);
    mock.set_audio_format(Some((44_100, 16, 2)));
    mock.set_track(
        Some(MpdTrack {
            file: "rock/song.flac".into(),
            title: Some("MPD Song".into()),
            artist: Some("MPD Artist".into()),
            album: Some("MPD Album".into()),
            station_name: None,
            duration: Some(Duration::from_secs(200)),
        }),
        Some(Duration::from_secs(50)),
        Some(Duration::from_secs(200)),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mpd_status = rt.block_on(mpd.read_status()).unwrap();
    shell.apply_mpd_playback(&mpd_status);
    assert_eq!(shell.window().get_track_title().as_str(), "BT Track");

    // Switch active source to MPD via AudioSourcesState.
    shell.apply_audio_sources(&AudioSourcesState {
        active_source: Some(SOURCE_ID_MPD.into()),
        pinned_source: None,
        sources: vec![
            AudioSourceEntry {
                id: SOURCE_ID_BLUETOOTH.into(),
                display_name: "Bluetooth".into(),
                state: "idle".into(),
                sample_rate_hz: 0,
                is_active: false,
            },
            AudioSourceEntry {
                id: SOURCE_ID_MPD.into(),
                display_name: "Music Player".into(),
                state: "streaming".into(),
                sample_rate_hz: 44_100,
                is_active: true,
            },
        ],
    });

    assert_eq!(
        shell.window().get_active_source_id().as_str(),
        SOURCE_ID_MPD
    );
    assert_eq!(shell.window().get_active_source().as_str(), "Music Player");
    assert_eq!(shell.window().get_track_or_device().as_str(), "");
    assert_eq!(shell.window().get_track_title().as_str(), "MPD Song");
    assert!(shell.window().get_seekable());
    assert_eq!(shell.window().get_stream_codec().as_str(), "FLAC");
    assert_eq!(shell.window().get_stream_url().as_str(), "rock/song.flac");
    // A Bluetooth PCM update while MPD is active must NOT overwrite MPD's codec badge.
    shell.apply_pcm_stream(&speaker_bluetooth::BlueAlsaPcm {
        pcm_path: "/org/bluealsa/pcm0".into(),
        device_address: speaker_bluetooth::BtAddress::new([1, 2, 3, 4, 5, 6]),
        resolved_name: "Phone".into(),
        transport: "A2DP-sink".into(),
        running: true,
        selected_codec: "LDAC".into(),
        available_codecs: vec!["LDAC".into()].into_boxed_slice(),
        sampling_rate: 96_000,
        channels: 2,
        format: "S24_LE".into(),
        volume: None,
        volume_percent: None,
        muted: false,
        delay: None,
    });
    assert_eq!(shell.window().get_stream_codec().as_str(), "FLAC");
    // Tapping the codec badge opens the Codec HUD even when MPD is active.
    shell.window().invoke_show_codec_hud();
    assert!(shell.window().get_codec_hud_visible());
    shell.window().invoke_dismiss_codec_hud();
    // MPD has no cover art yet -> Bluetooth cover art must NOT leak onto MPD.
    assert!(!shell.window().get_has_cover_art());

    // Seek and next while MPD is active go to MPD queue.
    shell.window().invoke_seek_to(0.5);
    shell.window().invoke_next_track();
    assert_eq!(
        mpd_queue.drain(),
        vec![MpdCommand::Seek(Duration::from_secs(100)), MpdCommand::Next,]
    );
    assert_eq!(bt_queue.drain(), Vec::new());

    // Requesting a source switch from UI pushes to SourceCommandQueue.
    shell
        .window()
        .invoke_select_audio_source(SOURCE_ID_BLUETOOTH.into());
    assert_eq!(
        source_queue.drain(),
        vec![SourceCommand::SelectSource(SOURCE_ID_BLUETOOTH.into())]
    );

    // Switching back to Bluetooth restores BT track & BT cover art.
    shell.apply_audio_sources(&AudioSourcesState {
        active_source: Some(SOURCE_ID_BLUETOOTH.into()),
        pinned_source: None,
        sources: vec![],
    });
    assert_eq!(shell.window().get_track_title().as_str(), "BT Track");
    assert!(shell.window().get_has_cover_art());
    assert!(!shell.window().get_seekable());
    // Bluetooth's cached stream info is restored and MPD's URL is cleared.
    assert_eq!(shell.window().get_stream_codec().as_str(), "LDAC");
    assert_eq!(shell.window().get_stream_url().as_str(), "");

    // Switching to a source with no stream metadata clears all stream fields.
    shell.apply_audio_sources(&AudioSourcesState {
        active_source: Some("line-in".into()),
        pinned_source: None,
        sources: vec![],
    });
    assert_eq!(shell.window().get_stream_codec().as_str(), "");
    assert_eq!(shell.window().get_stream_url().as_str(), "");
    assert_eq!(shell.window().get_bt_stream_format().as_str(), "—");
}

#[test]
fn mpd_and_audio_sources_workers_publish_and_execute_commands() {
    let (shell, _fb) = make_shell();
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(true));

    let source_cmds = SourceCommandQueue::new();
    let mpd_cmds = MpdCommandQueue::new();
    let (mpd_dev, mpd_mock) = MpdDevice::new_mocked();
    mpd_mock.set_state(MpdPlaybackState::Playing);
    mpd_mock.set_track(
        Some(MpdTrack {
            file: "albums/cover_test.flac".into(),
            title: Some("Worker Track".into()),
            artist: Some("Worker Artist".into()),
            album: Some("Worker Album".into()),
            station_name: None,
            duration: Some(Duration::from_secs(180)),
        }),
        Some(Duration::from_secs(30)),
        Some(Duration::from_secs(180)),
    );
    mpd_mock.set_album_art("albums/cover_test.flac", Some(minimal_png_bytes()));

    let src_handle = spawn_audio_sources_worker(&shared, &running, &source_cmds, true);
    let mpd_handle = spawn_mpd_monitor_with_device(&shared, &running, &mpd_cmds, mpd_dev);

    // Switch to MPD via mock audio-sources worker and trigger MPD commands.
    source_cmds.push(SourceCommand::SelectSource(SOURCE_ID_MPD.into()));
    mpd_cmds.push(MpdCommand::Seek(Duration::from_secs(90)));
    mpd_cmds.push(MpdCommand::SetRepeat(true));
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Player));

    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(25));
        apply_shared_bt_state(&shell, &shared);
        if shell.window().get_active_source_id().as_str() == SOURCE_ID_MPD
            && shell.window().get_track_title().as_str() == "Worker Track"
            && shell.window().get_has_cover_art()
        {
            break;
        }
    }

    assert_eq!(
        shell.window().get_active_source_id().as_str(),
        SOURCE_ID_MPD
    );
    assert_eq!(shell.window().get_track_title().as_str(), "Worker Track");
    assert!(shell.window().get_has_cover_art());
    assert!(
        mpd_mock
            .commands()
            .contains(&MpdMockCommand::Seek(Duration::from_secs(90)))
    );
    assert!(
        mpd_mock
            .commands()
            .contains(&MpdMockCommand::SetRepeat(true))
    );

    running.store(false, Ordering::Relaxed);
    join_worker(src_handle, "audio-sources");
    join_worker(mpd_handle, "mpd-monitor");
}

#[test]
fn mpd_stream_reconciles_station_name_icy_title_probed_codec_and_cover_changes() {
    let (shell, _fb) = make_shell();
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(true));
    let mpd_cmds = MpdCommandQueue::new();
    let source_cmds = SourceCommandQueue::new();

    let (mpd_dev, mpd_mock) = MpdDevice::new_mocked();
    let (resolver, art_mock) = StreamArtResolver::new_mocked(StationManifest::default());
    let stream_url = "https://ice1.somafm.com/groovesalad-stream";

    art_mock.set_stream_probe(
        stream_url,
        StreamArtResult {
            image_bytes: None,
            probed_codec: Some("AAC"),
            source: ArtSource::None,
            ..StreamArtResult::default()
        },
    );
    art_mock.set_track_lookup("Boards of Canada", "Roygbiv", minimal_png_bytes());

    mpd_mock.set_state(MpdPlaybackState::Playing);
    mpd_mock.set_track(
        Some(MpdTrack {
            file: stream_url.into(),
            title: Some("Boards of Canada - Roygbiv".into()),
            artist: None,
            album: None,
            station_name: Some("SomaFM Groove Salad".into()),
            duration: None,
        }),
        Some(Duration::from_secs(12)),
        None,
    );

    let src_handle = spawn_audio_sources_worker(&shared, &running, &source_cmds, true);
    let mpd_handle =
        spawn_mpd_monitor_with_resolver(&shared, &running, &mpd_cmds, mpd_dev, resolver);
    source_cmds.push(SourceCommand::SelectSource(SOURCE_ID_MPD.into()));
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Player));

    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(25));
        apply_shared_bt_state(&shell, &shared);
        if shell.window().get_active_source_id().as_str() == SOURCE_ID_MPD
            && shell.window().get_track_title().as_str() == "Roygbiv"
            && shell.window().get_has_cover_art()
            && shell.window().get_stream_codec().as_str() == "AAC"
        {
            break;
        }
    }

    assert_eq!(
        shell.window().get_track_or_device().as_str(),
        "SomaFM Groove Salad"
    );
    assert_eq!(
        shell.window().get_track_artist().as_str(),
        "Boards of Canada"
    );
    assert_eq!(shell.window().get_track_title().as_str(), "Roygbiv");
    assert_eq!(shell.window().get_stream_codec().as_str(), "AAC");
    assert!(!shell.window().get_seekable());
    assert!(shell.window().get_has_cover_art());

    running.store(false, Ordering::Relaxed);
    join_worker(src_handle, "audio-sources");
    join_worker(mpd_handle, "mpd-monitor");
}
