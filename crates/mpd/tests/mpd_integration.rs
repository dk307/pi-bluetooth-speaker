//! Integration tests for `speaker-mpd` domain types, mock backend, and live TCP `mpd_client` protocol.

use std::time::Duration;

use speaker_mpd::{
    DEFAULT_MPD_HOST, DEFAULT_MPD_PORT, MpdDevice, MpdEvent, MpdHost, MpdMockCommand,
    MpdPlaybackState, MpdPort, MpdSubsystem, MpdTrack,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[test]
fn host_and_port_validation_enforces_domain_invariants() {
    let default_host = MpdHost::default();
    assert_eq!(default_host.as_str(), DEFAULT_MPD_HOST);
    assert_eq!(default_host.to_string(), DEFAULT_MPD_HOST);
    assert!(default_host.is_loopback());

    assert!(MpdHost::new("localhost").unwrap().is_loopback());
    assert!(MpdHost::new("::1").unwrap().is_loopback());
    assert!(!MpdHost::new("192.168.1.50").unwrap().is_loopback());
    MpdHost::try_from("music.local").unwrap();

    for invalid in [
        "",
        "   ",
        "host name",
        "bad\nhost",
        ".leading",
        "trailing.",
        "-dash",
        "dash-",
        "bad_host",
    ] {
        let err = MpdHost::new(invalid).unwrap_err();
        assert!(
            err.is_invalid_state(),
            "expected invalid_state for {invalid:?}"
        );
        assert_ne!(err.summary(), "");
    }

    let default_port = MpdPort::default();
    assert_eq!(default_port.get(), DEFAULT_MPD_PORT);
    assert_eq!(default_port.to_string(), "6600");
    assert_eq!(MpdPort::parse_str(" 6601 ").unwrap().get(), 6601);
    MpdPort::try_from(6600).unwrap();

    assert!(MpdPort::new(0).unwrap_err().is_invalid_state());
    MpdPort::parse_str("0").unwrap_err();
    assert!(
        MpdPort::parse_str("70000")
            .unwrap_err()
            .contains("between 1 and 65535")
    );
    MpdPort::parse_str("not-a-port").unwrap_err();
    MpdPort::parse_str("+6600").unwrap_err();
    MpdPort::parse_str("-1").unwrap_err();
    MpdPort::parse_str("").unwrap_err();
}

#[test]
fn playback_state_predicates_and_display() {
    assert!(!MpdPlaybackState::Stopped.is_playing());
    assert!(!MpdPlaybackState::Stopped.is_active());
    assert!(!MpdPlaybackState::Paused.is_playing());
    assert!(MpdPlaybackState::Paused.is_active());
    assert!(MpdPlaybackState::Playing.is_playing());
    assert!(MpdPlaybackState::Playing.is_active());

    assert_eq!(MpdPlaybackState::Stopped.to_string(), "stopped");
    assert_eq!(MpdPlaybackState::Paused.to_string(), "paused");
    assert_eq!(MpdPlaybackState::Playing.to_string(), "playing");
}

#[tokio::test]
async fn mocked_device_records_commands_updates_state_and_emits_events() {
    let (mpd, mock) = MpdDevice::new_mocked();
    assert!(mpd.is_mocked());
    assert_eq!(mpd.host(), MpdHost::default());
    assert_eq!(mpd.port(), MpdPort::default());
    assert_eq!(
        mpd.with_timeout(Duration::from_millis(500)).port(),
        MpdPort::default()
    );

    mock.set_state(MpdPlaybackState::Playing);
    mock.set_volume(Some(65));
    mock.set_track(
        Some(MpdTrack {
            file: "radio/fip.mp3".into(),
            title: Some("FIP".into()),
            artist: Some("Radio France".into()),
            album: None,
            duration: Some(Duration::from_secs(240)),
        }),
        Some(Duration::from_secs(12)),
        Some(Duration::from_secs(240)),
    );

    let status = mpd.read_status().await.unwrap();
    assert_eq!(status.state, MpdPlaybackState::Playing);
    assert_eq!(status.volume, Some(65));
    assert_eq!(status.elapsed, Some(Duration::from_secs(12)));
    assert_eq!(
        status
            .current_track
            .as_ref()
            .and_then(|t| t.title.as_deref()),
        Some("FIP")
    );

    let mut events = mpd.subscribe_events().await.unwrap();
    mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer));
    assert_eq!(
        events.recv().await,
        Some(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer))
    );

    mpd.pause().await.unwrap();
    assert_eq!(
        mpd.read_status().await.unwrap().state,
        MpdPlaybackState::Paused
    );
    mpd.resume().await.unwrap();
    assert_eq!(
        mpd.read_status().await.unwrap().state,
        MpdPlaybackState::Playing
    );
    mpd.next().await.unwrap();
    mpd.previous().await.unwrap();
    mpd.set_volume(42).await.unwrap();
    assert_eq!(mpd.read_status().await.unwrap().volume, Some(42));
    mpd.stop().await.unwrap();
    assert_eq!(
        mpd.read_status().await.unwrap().state,
        MpdPlaybackState::Stopped
    );
    mpd.play().await.unwrap();
    assert_eq!(
        mpd.read_status().await.unwrap().state,
        MpdPlaybackState::Playing
    );

    assert_eq!(
        mock.commands(),
        vec![
            MpdMockCommand::Pause,
            MpdMockCommand::Resume,
            MpdMockCommand::Next,
            MpdMockCommand::Previous,
            MpdMockCommand::SetVolume(42),
            MpdMockCommand::Stop,
            MpdMockCommand::Play,
        ]
    );
    mock.clear_commands();
    assert_eq!(mock.commands(), []);

    // Out-of-range volume rejected before touching backend
    let range_err = mpd.set_volume(101).await.unwrap_err();
    assert!(range_err.is_invalid_state());

    // Fault injection covers command, status, and event subscription paths
    mock.inject_fault("injected failure");
    assert!(mpd.pause().await.unwrap_err().is_protocol_error());
    assert!(mpd.read_status().await.unwrap_err().is_protocol_error());
    assert!(
        mpd.subscribe_events()
            .await
            .unwrap_err()
            .is_protocol_error()
    );
    mock.clear_fault();
    mpd.pause().await.unwrap();
}

fn spawn_fake_mpd_server_on(
    listener: TcpListener,
    handler: impl Fn(String) -> String + Send + Sync + 'static,
) -> MpdPort {
    let port = listener.local_addr().unwrap().port();
    let handler = std::sync::Arc::new(handler);

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = std::sync::Arc::clone(&handler);
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                if writer.write_all(b"OK MPD 0.24.0\n").await.is_err() {
                    return;
                }
                let mut lines = BufReader::new(reader).lines();
                let mut idling = false;
                let mut in_list = false;
                let mut list_buf = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line == "idle" {
                        idling = true;
                        continue;
                    }
                    if line == "noidle" {
                        if idling {
                            idling = false;
                            if writer.write_all(b"OK\n").await.is_err() {
                                break;
                            }
                        }
                        continue;
                    }
                    if line == "command_list_ok_begin" {
                        in_list = true;
                        list_buf.clear();
                        continue;
                    }
                    if line == "command_list_end" {
                        in_list = false;
                        list_buf.push_str("OK\n");
                        if writer.write_all(list_buf.as_bytes()).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    let reply = handler(line);
                    if in_list {
                        list_buf.push_str(&reply);
                        list_buf.push_str("list_OK\n");
                    } else {
                        let out = if reply.ends_with("OK\n") || reply.starts_with("ACK ") {
                            reply
                        } else {
                            format!("{reply}OK\n")
                        };
                        if writer.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                }
            });
        }
    });

    MpdPort::new(port).unwrap()
}

async fn spawn_fake_mpd_server(
    handler: impl Fn(String) -> String + Send + Sync + 'static,
) -> MpdPort {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    spawn_fake_mpd_server_on(listener, handler)
}

#[tokio::test]
async fn tcp_backend_executes_mpd_protocol_commands_and_handles_errors() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_srv = std::sync::Arc::clone(&seen);

    let port = spawn_fake_mpd_server(move |cmd| {
        seen_srv.lock().unwrap().push(cmd.clone());
        match cmd.as_str() {
            "status" => concat!(
                "repeat: 0\n",
                "random: 0\n",
                "single: 0\n",
                "consume: 0\n",
                "volume: 72\n",
                "state: play\n",
                "elapsed: 15.5\n",
                "duration: 180.0\n",
                "song: 0\n",
                "songid: 1\n",
            )
            .to_string(),
            "currentsong" => concat!(
                "file: radio/stream.flac\n",
                "Title: Live Stream\n",
                "Artist: Test Artist\n",
                "Album: Test Album\n",
                "duration: 180.0\n",
                "Pos: 0\n",
                "Id: 1\n",
            )
            .to_string(),
            "setvol 50" => "ACK [52@0] {setvol} problems setting volume\n".to_string(),
            _ => String::new(),
        }
    })
    .await;

    let mpd =
        MpdDevice::with_endpoint(MpdHost::default(), port).with_timeout(Duration::from_secs(2));
    assert!(!mpd.is_mocked());

    mpd.pause().await.unwrap();
    mpd.resume().await.unwrap();
    mpd.play().await.unwrap();
    mpd.stop().await.unwrap();
    mpd.next().await.unwrap();
    mpd.previous().await.unwrap();

    let status = mpd.read_status().await.unwrap();
    assert_eq!(status.protocol_version.as_ref(), "0.24.0");
    assert_eq!(status.state, MpdPlaybackState::Playing);
    assert_eq!(status.volume, Some(72));
    assert_eq!(
        status
            .current_track
            .as_ref()
            .and_then(|t| t.title.as_deref()),
        Some("Live Stream")
    );
    assert_eq!(
        status
            .current_track
            .as_ref()
            .and_then(|t| t.artist.as_deref()),
        Some("Test Artist")
    );

    let vol_err = mpd.set_volume(50).await.unwrap_err();
    assert!(vol_err.is_command_failed());

    let recorded = seen.lock().unwrap().clone();
    assert!(recorded.contains(&"pause 1".to_string()));
    assert!(recorded.contains(&"pause 0".to_string()));
    assert!(recorded.contains(&"play".to_string()));
    assert!(recorded.contains(&"stop".to_string()));
    assert!(recorded.contains(&"next".to_string()));
    assert!(recorded.contains(&"previous".to_string()));
}

#[tokio::test]
async fn tcp_backend_subscribes_idle_events_and_handles_connect_failures() {
    // 1. Live TCP idle event stream
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = MpdPort::new(listener.local_addr().unwrap().port()).unwrap();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        writer.write_all(b"OK MPD 0.24.0\n").await.unwrap();
        let mut lines = BufReader::new(reader).lines();
        if let Ok(Some(line)) = lines.next_line().await
            && line.starts_with("idle")
        {
            writer.write_all(b"changed: mixer\nOK\n").await.unwrap();
        }
        // Hold briefly, then close cleanly so the client observes disconnect.
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    let mpd = MpdDevice::with_endpoint(MpdHost::default(), port);
    let mut stream = mpd.subscribe_events().await.unwrap();
    stream.try_recv().unwrap_err();
    assert_eq!(
        stream.recv().await,
        Some(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer))
    );
    assert!(matches!(
        stream.recv().await,
        Some(MpdEvent::Disconnected(_))
    ));

    // 2. Connection refused -> ConnectionFailed
    let closed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = MpdPort::new(closed_listener.local_addr().unwrap().port()).unwrap();
    drop(closed_listener);

    let unreachable = MpdDevice::with_endpoint(MpdHost::default(), closed_port);
    let err = unreachable.pause().await.unwrap_err();
    assert!(err.is_connection_failed());
    assert!(std::error::Error::source(&err).is_some());
    assert_ne!(err.summary(), "");

    // 3. Greeting timeout -> Timeout
    let silent_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_port = MpdPort::new(silent_listener.local_addr().unwrap().port()).unwrap();
    let _hold_task = tokio::spawn(async move {
        let (_sock, _) = silent_listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let slow = MpdDevice::with_endpoint(MpdHost::default(), silent_port)
        .with_timeout(Duration::from_millis(50));
    let timeout_err = slow.pause().await.unwrap_err();
    assert!(timeout_err.is_timeout());

    // 4. IPv6 loopback connects cleanly via tuple ToSocketAddrs.
    if let Ok(v6_listener) = TcpListener::bind("[::1]:0").await {
        let v6_port = spawn_fake_mpd_server_on(v6_listener, |cmd| match cmd.as_str() {
            "status" => "repeat: 0\nrandom: 0\nsingle: 0\nconsume: 0\nstate: pause\n".to_string(),
            _ => String::new(),
        });
        let v6_mpd = MpdDevice::with_endpoint(MpdHost::new("::1").unwrap(), v6_port);
        assert_eq!(
            v6_mpd.read_status().await.unwrap().state,
            MpdPlaybackState::Paused
        );
    }
}

#[test]
fn mpd_subsystem_from_maps_all_variants() {
    use mpd_client::client::Subsystem;
    assert_eq!(MpdSubsystem::from(Subsystem::Player), MpdSubsystem::Player);
    assert_eq!(MpdSubsystem::from(Subsystem::Mixer), MpdSubsystem::Mixer);
    assert_eq!(MpdSubsystem::from(Subsystem::Queue), MpdSubsystem::Queue);
    assert_eq!(
        MpdSubsystem::from(Subsystem::Options),
        MpdSubsystem::Options
    );
    assert_eq!(MpdSubsystem::from(Subsystem::Output), MpdSubsystem::Output);
    assert_eq!(
        MpdSubsystem::from(Subsystem::Database),
        MpdSubsystem::Other("database".into())
    );
}

#[tokio::test]
async fn mpd_read_volume_mocked_and_tcp() {
    let (mock_mpd, mock_ctrl) = MpdDevice::new_mocked();
    assert_eq!(mock_mpd.read_volume().await.unwrap(), None);
    mock_ctrl.set_volume(Some(42));
    assert_eq!(mock_mpd.read_volume().await.unwrap(), Some(42));
    mock_ctrl.inject_fault("boom");
    assert!(
        mock_mpd
            .read_volume()
            .await
            .unwrap_err()
            .is_protocol_error()
    );

    let port = spawn_fake_mpd_server(|cmd| match cmd.as_str() {
        "status" => "volume: 63\nrepeat: 0\nrandom: 0\nsingle: 0\nconsume: 0\nstate: play\n".into(),
        _ => String::new(),
    })
    .await;
    let tcp_mpd = MpdDevice::with_endpoint(MpdHost::default(), port);
    assert_eq!(tcp_mpd.read_volume().await.unwrap(), Some(63));

    // Absent volume field (mixer_type = "none") must yield None, not Some(0).
    let no_mixer_port = spawn_fake_mpd_server(|cmd| match cmd.as_str() {
        "status" => "repeat: 0\nrandom: 0\nsingle: 0\nconsume: 0\nstate: play\n".into(),
        _ => String::new(),
    })
    .await;
    let no_mixer_mpd = MpdDevice::with_endpoint(MpdHost::default(), no_mixer_port);
    assert_eq!(no_mixer_mpd.read_volume().await.unwrap(), None);
    assert_eq!(no_mixer_mpd.read_status().await.unwrap().volume, None);

    // Invalid hostname labels (empty / leading hyphen in label) must be rejected.
    MpdHost::new("music..local").unwrap_err();
    MpdHost::new("music.-local").unwrap_err();
    MpdHost::new("music-.local").unwrap_err();
}

#[tokio::test]
async fn tcp_backend_recovers_connection_closed_and_clears_slot_on_timeout() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = MpdPort::new(listener.local_addr().unwrap().port()).unwrap();
    let conn_num = std::sync::Arc::new(AtomicUsize::new(0));
    let conn_num_srv = std::sync::Arc::clone(&conn_num);

    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let n = conn_num_srv.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let (reader, mut writer) = socket.into_split();
                if writer.write_all(b"OK MPD 0.24.0\n").await.is_err() {
                    return;
                }
                let mut lines = BufReader::new(reader).lines();
                let mut in_list = false;
                let mut list_body = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    let cmd = line.trim();
                    if cmd == "idle" || cmd == "noidle" {
                        if cmd == "noidle" {
                            let _ = writer.write_all(b"OK\n").await;
                        }
                        continue;
                    }
                    // Connection 0: drop immediately on first command to exercise ConnectionClosed retry.
                    if n == 0 {
                        return;
                    }
                    // Connection 1: hang on "next" to exercise timeout + client_slot clearing + no-retry.
                    if n == 1 && cmd == "next" {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        return;
                    }
                    if cmd == "command_list_ok_begin" || cmd == "command_list_begin" {
                        in_list = true;
                        list_body.clear();
                        continue;
                    }
                    if cmd == "command_list_end" {
                        in_list = false;
                        if writer
                            .write_all(format!("{list_body}OK\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                    let body = match cmd {
                        "status" => {
                            "volume: 55\nrepeat: 0\nrandom: 0\nsingle: 0\nconsume: 0\nstate: play\n"
                        }
                        _ => "",
                    };
                    if in_list {
                        list_body.push_str(body);
                        list_body.push_str("list_OK\n");
                    } else if writer
                        .write_all(format!("{body}OK\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });

    let mpd =
        MpdDevice::with_endpoint(MpdHost::default(), port).with_timeout(Duration::from_millis(80));
    // First call hits ConnectionClosed on conn 0 and transparently retries on conn 1.
    assert_eq!(mpd.read_volume().await.unwrap(), Some(55));
    // Next call times out on conn 1, clearing the cached client_slot.
    assert!(mpd.next().await.unwrap_err().is_timeout());
    // Subsequent call reconnects to conn 2 and succeeds cleanly.
    assert_eq!(mpd.read_status().await.unwrap().volume, Some(55));
}
