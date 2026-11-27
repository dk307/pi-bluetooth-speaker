//! Integration tests for `audio_manager::sources` (registry, persistence, arbiter, controls, D-Bus, and manager).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audio_manager::rate_watch::LoopbackState;
use audio_manager::sources::state_file::PersistedSourceState;
use audio_manager::{
    AUDIO_SOURCES_INTERFACE, AUDIO_SOURCES_OBJECT_PATH, AudioSourcesInterface,
    AudioSourcesSnapshot, AutoSwitchPolicy, CaptureSpec, ControlSpec, SourceArbiter, SourceCommand,
    SourceCommandKind, SourceControl, SourceController, SourceDefinition, SourceId, SourceManager,
    SourceManagerConfig, SourceRegistry, SourceStateFile,
};
use camilladsp_schema::config::{CaptureDevice, Configuration};
use futures_util::StreamExt;
use speaker_common::diag::ScopedTempDir;
use speaker_dsp::{DspConfigInfo, DspDevice, ProcessingState};
use speaker_mpd::{MpdHost, MpdPort};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use zbus::{MatchRule, MessageStream, Proxy, interface};

const SAMPLE_CONFIG_YAML: &str = "\
devices:
  samplerate: 48000
  chunksize: 1024
  queuelimit: 4
  enable_rate_adjust: true
  stop_on_rate_change: true
  silence_threshold: -60
  silence_timeout_s: 3.0
  capture:
    type: Alsa
    channels: 2
    device: \"hw:Loopback,1,0\"
    format: S32_LE
    stop_on_inactive: true
  playback:
    type: Alsa
    channels: 2
    device: \"hw:sndrpihifiberry,0\"
    format: S32_LE
";

fn sid(raw: &str) -> SourceId {
    SourceId::parse(raw).unwrap()
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_private_bus() -> (KillOnDrop, String) {
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed to run D-Bus integration tests");
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let address = address.trim().to_owned();
    assert_ne!(address, "");
    (KillOnDrop(daemon), address)
}

#[derive(Debug, Default, Clone)]
struct MockBtIface {
    calls: Arc<Mutex<u32>>,
}

#[interface(name = "org.speaker.Bluetooth1")]
impl MockBtIface {
    fn pause_active(&self) {
        *self.calls.lock().unwrap() += 1;
    }
}

#[test]
fn registry_and_domain_types_validate_inputs_and_invariants() {
    assert_eq!(sid("bluetooth").as_str(), "bluetooth");
    assert_eq!(
        SourceId::try_from("airplay-1").unwrap().to_string(),
        "airplay-1"
    );
    for bad in [
        "",
        " ",
        "UPPER",
        "-lead",
        "_lead",
        "has space",
        &"a".repeat(33),
    ] {
        SourceId::parse(bad).unwrap_err();
    }

    assert_eq!(
        AutoSwitchPolicy::parse("last-play").unwrap(),
        AutoSwitchPolicy::LastPlayWins
    );
    assert_eq!(
        AutoSwitchPolicy::parse("first-play").unwrap(),
        AutoSwitchPolicy::FirstPlayWins
    );
    assert_eq!(
        AutoSwitchPolicy::parse("priority").unwrap(),
        AutoSwitchPolicy::Priority
    );
    assert_eq!(AutoSwitchPolicy::LastPlayWins.to_string(), "last-play");
    assert_eq!(AutoSwitchPolicy::FirstPlayWins.to_string(), "first-play");
    assert_eq!(AutoSwitchPolicy::Priority.to_string(), "priority");
    AutoSwitchPolicy::parse("random").unwrap_err();

    let parsed_def: SourceDefinition = serde_json::from_str(
        r#"{"id":"aux","display_name":"Aux","priority":5,"capture":{"kind":"alsa_direct","device":"hw:3,0"}}"#,
    )
    .unwrap();
    assert_eq!(parsed_def.capture.default_rate_hz(), 48_000);
    assert_eq!(parsed_def.control, ControlSpec::None);

    let cmd = SourceCommand::fire_and_forget(SourceCommandKind::SetAutoSwitchPolicy(
        AutoSwitchPolicy::LastPlayWins,
    ));
    assert!(cmd.reply.is_none());

    let loopback = CaptureSpec::Loopback { subdevice: 3 };
    assert_eq!(loopback.camilladsp_device().as_ref(), "hw:Loopback,1,3");
    assert_eq!(loopback.loopback_subdevice(), Some(3));
    assert_eq!(loopback.default_rate_hz(), 48_000);

    let direct = CaptureSpec::AlsaDirect {
        device: Box::from("hw:2,0"),
        default_rate_hz: 96_000,
    };
    assert_eq!(direct.camilladsp_device().as_ref(), "hw:2,0");
    assert_eq!(direct.loopback_subdevice(), None);
    assert_eq!(direct.default_rate_hz(), 96_000);

    let reg = SourceRegistry::default();
    assert!(reg.contains(&sid("bluetooth")));
    assert!(reg.contains(&sid("rmpd")));
    assert_eq!(reg.by_loopback_subdevice(0), Some(&sid("bluetooth")));
    assert_eq!(reg.by_loopback_subdevice(1), Some(&sid("rmpd")));
    assert_eq!(
        reg.loopback_subdevices(),
        vec![(0, sid("bluetooth")), (1, sid("rmpd"))]
    );

    // Rejections
    SourceRegistry::from_definitions(vec![]).unwrap_err();

    let mut bad_name = SourceRegistry::default_definitions();
    bad_name[0].display_name = Box::from("   ");
    SourceRegistry::from_definitions(bad_name).unwrap_err();

    let mut dup_id = SourceRegistry::default_definitions();
    dup_id[1].id = sid("bluetooth");
    SourceRegistry::from_definitions(dup_id).unwrap_err();

    let mut bad_sub = SourceRegistry::default_definitions();
    bad_sub[0].capture = CaptureSpec::Loopback { subdevice: 8 };
    SourceRegistry::from_definitions(bad_sub).unwrap_err();

    let mut dup_sub = SourceRegistry::default_definitions();
    dup_sub[1].capture = CaptureSpec::Loopback { subdevice: 0 };
    SourceRegistry::from_definitions(dup_sub).unwrap_err();

    let bad_direct = vec![SourceDefinition {
        id: sid("linein"),
        display_name: Box::from("Line In"),
        priority: 10,
        capture: CaptureSpec::AlsaDirect {
            device: Box::from(" "),
            default_rate_hz: 48_000,
        },
        control: ControlSpec::None,
    }];
    SourceRegistry::from_definitions(bad_direct).unwrap_err();

    let bad_direct_rate = vec![SourceDefinition {
        id: sid("linein"),
        display_name: Box::from("Line In"),
        priority: 10,
        capture: CaptureSpec::AlsaDirect {
            device: Box::from("hw:2,0"),
            default_rate_hz: 0,
        },
        control: ControlSpec::None,
    }];
    SourceRegistry::from_definitions(bad_direct_rate).unwrap_err();
}

#[tokio::test]
async fn source_state_file_persists_atomically_and_recovers_missing() {
    let dir = ScopedTempDir::new("source_state_file");
    let path = dir.path().join("source-state.json");
    let sf = SourceStateFile::new(&path);

    let missing = sf.load().unwrap();
    assert_eq!(missing, PersistedSourceState::default());

    let saved = PersistedSourceState {
        last_source: Some(sid("rmpd")),
        pinned_source: Some(sid("rmpd")),
        auto_switch_policy: AutoSwitchPolicy::Priority,
    };
    sf.save(&saved).await.unwrap();
    assert_eq!(sf.load().unwrap(), saved);

    std::fs::write(&path, b"{not valid json").unwrap();
    sf.load().unwrap_err();
}

#[test]
fn arbiter_handles_last_play_first_play_priority_pinning_and_release() {
    let bt = sid("bluetooth");
    let rmpd = sid("rmpd");
    let airplay = sid("airplay");

    let mut defs = SourceRegistry::default_definitions();
    defs.push(SourceDefinition {
        id: airplay.clone(),
        display_name: Box::from("AirPlay"),
        priority: 50, // equal priority to bluetooth
        capture: CaptureSpec::Loopback { subdevice: 2 },
        control: ControlSpec::None,
    });
    let reg = SourceRegistry::from_definitions(defs).unwrap();

    // Boot restores last_source = rmpd (ignores unknown pinned source)
    let mut arbiter = SourceArbiter::new(
        reg.clone(),
        PersistedSourceState {
            last_source: Some(rmpd.clone()),
            pinned_source: Some(sid("ghost")),
            auto_switch_policy: AutoSwitchPolicy::LastPlayWins,
        },
    );
    assert_eq!(arbiter.active(), Some(&rmpd));
    assert_eq!(arbiter.pinned(), None);

    // Seed with multiple streaming sources: prefers restored last_source (`rmpd`)
    let seed_out = arbiter.seed([
        (bt.clone(), true, 48_000),
        (rmpd.clone(), true, 44_100),
        (airplay.clone(), false, 0),
    ]);
    assert_eq!(arbiter.active(), Some(&rmpd));
    assert_eq!(seed_out.pause_sources, vec![bt.clone()]);
    assert_eq!(
        seed_out
            .dsp_target
            .as_ref()
            .unwrap()
            .capture_device
            .as_ref(),
        "hw:Loopback,1,1"
    );
    assert_eq!(seed_out.dsp_target.as_ref().unwrap().rate_hz, 44_100);

    // Lingering steady-state `streaming=true` on non-active `bt` must NOT steal focus
    let linger = arbiter.on_activity(&bt, true, 48_000);
    assert_eq!(linger.active_changed, None);
    assert!(linger.pause_sources.is_empty());

    // `bt` stops then restarts (false -> true edge): LastPlayWins switches to `bt` and pauses `rmpd`
    arbiter.on_activity(&bt, false, 0);
    let takeover = arbiter.on_activity(&bt, true, 48_000);
    assert_eq!(takeover.active_changed, Some(Some(bt.clone())));
    assert_eq!(takeover.pause_sources, vec![rmpd.clone()]);
    assert_eq!(
        takeover
            .dsp_target
            .as_ref()
            .unwrap()
            .capture_device
            .as_ref(),
        "hw:Loopback,1,0"
    );

    // Rate change on active source updates dsp_target without changing active
    let rate_up = arbiter.on_activity(&bt, true, 96_000);
    assert_eq!(rate_up.active_changed, None);
    assert_eq!(rate_up.dsp_target.as_ref().unwrap().rate_hz, 96_000);

    // Switch to FirstPlayWins: `airplay` starting while `bt` streams is paused
    let pol_out = arbiter.set_policy(AutoSwitchPolicy::FirstPlayWins);
    assert_eq!(
        pol_out.policy_changed,
        Some(AutoSwitchPolicy::FirstPlayWins)
    );
    let shut_out = arbiter.on_activity(&airplay, true, 44_100);
    assert_eq!(shut_out.active_changed, None);
    assert_eq!(shut_out.pause_sources, vec![airplay.clone()]);

    // Switch to Priority: `rmpd` (40) cannot preempt `bt` (50); `airplay` (50) equal cannot preempt `bt` (50)
    arbiter.on_activity(&rmpd, false, 0);
    arbiter.on_activity(&airplay, false, 0);
    arbiter.set_policy(AutoSwitchPolicy::Priority);
    let low_prio = arbiter.on_activity(&rmpd, true, 44_100);
    assert_eq!(low_prio.pause_sources, vec![rmpd.clone()]);
    let eq_prio = arbiter.on_activity(&airplay, true, 44_100);
    assert_eq!(eq_prio.pause_sources, vec![airplay.clone()]);

    // When `bt` stops, Priority promotes `airplay` (50 > rmpd 40)
    let promo = arbiter.on_activity(&bt, false, 0);
    assert_eq!(promo.active_changed, Some(Some(airplay.clone())));

    // Pinning to `rmpd` switches from streaming `airplay` -> `rmpd` and pauses `airplay`
    let pin_out = arbiter.pin_source(Some(&rmpd)).unwrap();
    assert_eq!(pin_out.pinned_changed, Some(Some(rmpd.clone())));
    assert_eq!(pin_out.active_changed, Some(Some(rmpd.clone())));
    assert_eq!(pin_out.pause_sources, vec![airplay.clone()]);

    // While pinned to `rmpd`, `bt` starting is blocked and paused
    let blocked = arbiter.on_activity(&bt, true, 48_000);
    assert_eq!(blocked.pause_sources, vec![bt.clone()]);
    assert_eq!(arbiter.active(), Some(&rmpd));

    // Selecting `bt` explicitly unpins and detaches if `bt` is idle
    arbiter.on_activity(&bt, false, 0);
    let sel_out = arbiter.select_source(&bt).unwrap();
    assert_eq!(sel_out.pinned_changed, Some(None));
    assert_eq!(sel_out.active_changed, Some(Some(bt.clone())));
    assert_eq!(sel_out.pause_sources, vec![rmpd.clone()]);
    assert!(sel_out.dsp_target.as_ref().unwrap().force_detach);

    // Releasing active streaming `rmpd` after switching back promotes `airplay` if streaming
    arbiter.on_activity(&rmpd, true, 44_100);
    arbiter.select_source(&rmpd).unwrap();
    let rel_out = arbiter.release_source(&rmpd).unwrap();
    assert_eq!(rel_out.pause_sources, vec![rmpd.clone()]);
    assert_eq!(rel_out.active_changed, Some(Some(airplay.clone())));

    // Error paths for unknown sources
    arbiter.select_source(&sid("unknown")).unwrap_err();
    arbiter.pin_source(Some(&sid("unknown"))).unwrap_err();
    arbiter.release_source(&sid("unknown")).unwrap_err();

    // Additional edge cases: seed with pin, seed priority tie-break, no-op select/pin/policy,
    // unknown on_activity, stop while pinned, stop with no fallback, and release with no fallback
    let mut arb2 = SourceArbiter::new(
        reg.clone(),
        PersistedSourceState {
            last_source: None,
            pinned_source: Some(rmpd.clone()),
            auto_switch_policy: AutoSwitchPolicy::LastPlayWins,
        },
    );
    let pinned_seed = arb2.seed([(bt.clone(), true, 48_000), (rmpd.clone(), false, 0)]);
    assert_eq!(arb2.active(), Some(&rmpd));
    assert_eq!(pinned_seed.pause_sources, vec![bt.clone()]);
    // Stopping while pinned stays on pinned
    arb2.on_activity(&rmpd, true, 44_100);
    let stop_pinned = arb2.on_activity(&rmpd, false, 0);
    assert_eq!(arb2.active(), Some(&rmpd));
    assert!(!stop_pinned.dsp_target.as_ref().unwrap().streaming);

    // Unpinned seed with no last_source picks highest priority (`bt` = 50 > `rmpd` = 40)
    let mut arb3 = SourceArbiter::new(reg, PersistedSourceState::default());
    let prio_seed = arb3.seed([(bt.clone(), true, 48_000), (rmpd.clone(), true, 44_100)]);
    assert_eq!(arb3.active(), Some(&bt));
    assert_eq!(prio_seed.pause_sources, vec![rmpd.clone()]);
    // No-op operations
    assert_eq!(arb3.select_source(&bt).unwrap().active_changed, None);
    assert_eq!(arb3.pin_source(None).unwrap().pinned_changed, None);
    assert_eq!(
        arb3.set_policy(AutoSwitchPolicy::LastPlayWins)
            .policy_changed,
        None
    );
    assert_eq!(
        arb3.on_activity(&sid("unknown"), true, 48_000)
            .active_changed,
        None
    );

    // LastPlayWins promotion & release with no fallback
    arb3.on_activity(&rmpd, false, 0);
    // Start active source when already active (false->true)
    arb3.on_activity(&bt, false, 0);
    let restart_same = arb3.on_activity(&bt, true, 48_000);
    assert!(restart_same.dsp_target.as_ref().unwrap().streaming);
    // Release active streaming `bt` when nothing else is streaming -> dispatches pause and marks desired_route non-streaming without DSP reload
    let rel_detach = arb3.release_source(&bt).unwrap();
    assert!(rel_detach.dsp_target.as_ref().is_some_and(|t| !t.streaming));
    assert_eq!(rel_detach.pause_sources, vec![bt.clone()]);
    // Release idle `rmpd` -> still dispatches pause without DSP target
    let rel_idle = arb3.release_source(&rmpd).unwrap();
    assert!(rel_idle.dsp_target.is_none());
    assert_eq!(rel_idle.pause_sources, vec![rmpd.clone()]);

    // FirstPlayWins promotion order when active stops
    arb3.set_policy(AutoSwitchPolicy::FirstPlayWins);
    arb3.on_activity(&rmpd, true, 44_100);
    arb3.on_activity(&airplay, true, 48_000);
    let fpw_promo = arb3.on_activity(&bt, false, 0);
    assert_eq!(fpw_promo.active_changed, Some(Some(rmpd.clone())));

    // LastPlayWins promotion order when active stops (airplay started after rmpd)
    arb3.set_policy(AutoSwitchPolicy::LastPlayWins);
    arb3.select_source(&bt).unwrap();
    arb3.on_activity(&bt, true, 48_000);
    arb3.on_activity(&rmpd, false, 0);
    arb3.on_activity(&airplay, false, 0);
    arb3.on_activity(&rmpd, true, 44_100);
    arb3.on_activity(&airplay, true, 48_000); // airplay most recent
    // Now force bt active via select_source and stop bt -> promotes airplay
    arb3.select_source(&bt).unwrap();
    arb3.on_activity(&bt, true, 48_000);
    arb3.on_activity(&rmpd, true, 44_100);
    let lpw_promo = arb3.on_activity(&bt, false, 0);
    assert_eq!(lpw_promo.active_changed, Some(Some(airplay.clone())));

    let views = arb3.status_views();
    assert_eq!(views.len(), 3);
    assert!(views.iter().any(|v| v.id == airplay && v.is_active));
}

#[tokio::test]
async fn source_controls_execute_mpd_tcp_bluetooth_dbus_noop_and_mock() {
    // 1. Mock controller
    let (mock_ctrl, handle) = SourceController::new_mocked();
    mock_ctrl.pause(&sid("rmpd")).await.unwrap();
    assert_eq!(handle.paused_sources(), vec![sid("rmpd")]);
    handle.set_fail_for(&sid("rmpd"), true);
    mock_ctrl.pause(&sid("rmpd")).await.unwrap_err();
    handle.set_fail_for(&sid("rmpd"), false);
    handle.set_fail_all(true);
    mock_ctrl.pause(&sid("bluetooth")).await.unwrap_err();
    handle.clear_paused();
    assert_eq!(handle.paused_sources(), []);

    // 2. Real MpdTcp + BluetoothDbus + Noop adapters
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = MpdPort::new(listener.local_addr().unwrap().port()).unwrap();
    let mpd_paused = Arc::new(Mutex::new(false));
    let mpd_paused_srv = Arc::clone(&mpd_paused);

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let (reader, mut writer) = stream.into_split();
            let _ = writer.write_all(b"OK MPD 0.24.0\n").await;
            let mut lines = TokioBufReader::new(reader).lines();
            let mut idling = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if line == "idle" {
                    idling = true;
                } else if line == "noidle" {
                    if idling {
                        idling = false;
                        let _ = writer.write_all(b"OK\n").await;
                    }
                } else {
                    if line == "pause 1" {
                        *mpd_paused_srv.lock().unwrap() = true;
                    }
                    let _ = writer.write_all(b"OK\n").await;
                }
            }
        }
    });

    let (kill, address) = spawn_private_bus();
    let bt_calls = Arc::new(Mutex::new(0u32));
    let server_conn = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.speaker.Bluetooth")
        .unwrap()
        .serve_at(
            "/org/speaker/bluetooth",
            MockBtIface {
                calls: Arc::clone(&bt_calls),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let client_conn = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();

    let reg = SourceRegistry::from_definitions(vec![
        SourceDefinition {
            id: sid("bluetooth"),
            display_name: Box::from("Bluetooth"),
            priority: 50,
            capture: CaptureSpec::Loopback { subdevice: 0 },
            control: ControlSpec::BluetoothDbus,
        },
        SourceDefinition {
            id: sid("rmpd"),
            display_name: Box::from("Music Player"),
            priority: 40,
            capture: CaptureSpec::Loopback { subdevice: 1 },
            control: ControlSpec::MpdTcp {
                host: MpdHost::default(),
                port,
            },
        },
        SourceDefinition {
            id: sid("linein"),
            display_name: Box::from("Line In"),
            priority: 10,
            capture: CaptureSpec::AlsaDirect {
                device: Box::from("hw:2,0"),
                default_rate_hz: 48_000,
            },
            control: ControlSpec::None,
        },
    ])
    .unwrap();

    let real = SourceController::from_registry(&reg, Some(client_conn));
    real.pause(&sid("rmpd")).await.unwrap();
    assert!(*mpd_paused.lock().unwrap());

    real.pause(&sid("bluetooth")).await.unwrap();
    assert_eq!(*bt_calls.lock().unwrap(), 1);

    real.pause(&sid("linein")).await.unwrap();
    real.pause(&sid("unregistered")).await.unwrap();

    // Missing D-Bus connection returns error cleanly for BluetoothDbus
    let no_bus = SourceController::from_registry(&reg, None);
    no_bus.pause(&sid("bluetooth")).await.unwrap_err();

    drop(server_conn);
    drop(kill);
}

#[tokio::test]
async fn source_manager_routes_dsp_persists_state_and_serves_dbus() {
    let tmp = ScopedTempDir::new("source_manager_e2e");
    let dsp_cfg_path = tmp.path().join("crossover.yml");
    let source_state_path = tmp.path().join("source-state.json");
    std::fs::write(&dsp_cfg_path, SAMPLE_CONFIG_YAML).unwrap();

    // Seed pre-existing last_source = rmpd on boot
    let sf = SourceStateFile::new(&source_state_path);
    sf.save(&PersistedSourceState {
        last_source: Some(sid("rmpd")),
        pinned_source: None,
        auto_switch_policy: AutoSwitchPolicy::LastPlayWins,
    })
    .await
    .unwrap();

    let (kill, address) = spawn_private_bus();
    let server_conn = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.speaker.AudioManager")
        .unwrap()
        .build()
        .await
        .unwrap();

    let reg = SourceRegistry::default_appliance();
    let snapshot = AudioSourcesSnapshot::new(AutoSwitchPolicy::LastPlayWins);
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let iface = AudioSourcesInterface::new(snapshot.clone(), cmd_tx);
    let emitter = audio_manager::export_sources_interface(&server_conn, iface)
        .await
        .unwrap();

    let (dsp, dsp_mock) = DspDevice::new_mocked();
    let (ctrl, ctrl_mock) = SourceController::new_mocked();

    let mgr = SourceManager::new(
        reg,
        SourceManagerConfig {
            camilladsp_config_path: dsp_cfg_path.clone(),
            source_state_path: source_state_path.clone(),
            default_policy: AutoSwitchPolicy::LastPlayWins,
        },
        dsp,
        Arc::new(ctrl),
        snapshot.clone(),
        Some((server_conn.clone(), emitter)),
    );
    assert_eq!(mgr.arbiter().active(), Some(&sid("rmpd")));

    let (loop_tx, loop_rx) = mpsc::channel(16);
    let mgr_handle = tokio::spawn(async move {
        let mut mgr = mgr;
        mgr.run(cmd_rx, loop_rx).await;
    });

    let client_conn = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let proxy = Proxy::new(
        &client_conn,
        "org.speaker.AudioManager",
        AUDIO_SOURCES_OBJECT_PATH,
        AUDIO_SOURCES_INTERFACE,
    )
    .await
    .unwrap();

    let rule = MatchRule::builder()
        .sender("org.speaker.AudioManager")
        .and_then(|b| b.path(AUDIO_SOURCES_OBJECT_PATH))
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .map(zbus::match_rule::Builder::build)
        .unwrap();
    let mut signals = MessageStream::for_match_rule(rule, &client_conn, Some(16))
        .await
        .unwrap();

    // Initial boot restored ActiveSource == "rmpd"
    let active_prop: String = proxy.get_property("ActiveSource").await.unwrap();
    assert_eq!(active_prop, "rmpd");

    // Seed both loopback subdevices idle -> stays on rmpd without pushing idle SetConfigJson
    loop_tx
        .send((
            0,
            LoopbackState {
                active: false,
                rate_hz: 48_000,
            },
            false,
        ))
        .await
        .unwrap();
    loop_tx
        .send((
            1,
            LoopbackState {
                active: false,
                rate_hz: 48_000,
            },
            false,
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(dsp_mock.last_set_config_json().is_none());

    // Start streaming on subdevice 1 (rmpd) @ 44100 Hz
    loop_tx
        .send((
            1,
            LoopbackState {
                active: true,
                rate_hz: 44_100,
            },
            true,
        ))
        .await
        .unwrap();
    let json_rmpd = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(j) = dsp_mock.last_set_config_json() {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("rmpd config push");
    let cfg_rmpd: Configuration = serde_json::from_str(&json_rmpd).unwrap();
    assert_eq!(cfg_rmpd.devices.samplerate.get(), 44_100);
    match cfg_rmpd.devices.capture {
        CaptureDevice::Alsa { device, .. } => assert_eq!(device, "hw:Loopback,1,1"),
        other => panic!("expected ALSA capture, got {other:?}"),
    }

    // Start streaming on subdevice 0 (bluetooth) @ 48000 Hz -> LastPlayWins switches + pauses rmpd
    loop_tx
        .send((
            0,
            LoopbackState {
                active: true,
                rate_hz: 48_000,
            },
            true,
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if snapshot.active_source_str() == "bluetooth"
                && !ctrl_mock.paused_sources().is_empty()
                && proxy
                    .get_property::<String>("ActiveSource")
                    .await
                    .ok()
                    .as_deref()
                    == Some("bluetooth")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bluetooth takeover + rmpd pause");

    assert_eq!(ctrl_mock.paused_sources(), vec![sid("rmpd")]);
    let active_after: String = proxy.get_property("ActiveSource").await.unwrap();
    assert_eq!(active_after, "bluetooth");
    assert_eq!(sf.load().unwrap().last_source, Some(sid("bluetooth")));

    // Verify at least one PropertiesChanged signal fired
    let sig = tokio::time::timeout(Duration::from_secs(2), signals.next())
        .await
        .expect("PropertiesChanged signal")
        .unwrap()
        .unwrap();
    let (iface_name, _changed, _): (
        String,
        HashMap<String, zbus::zvariant::OwnedValue>,
        Vec<String>,
    ) = sig.body().deserialize().unwrap();
    assert_eq!(iface_name, AUDIO_SOURCES_INTERFACE);

    // D-Bus control methods: PinSource, SetAutoSwitchPolicy, SelectSource, ReleaseSource
    proxy.call_method("PinSource", &("rmpd",)).await.unwrap();
    assert_eq!(
        proxy.get_property::<String>("PinnedSource").await.unwrap(),
        "rmpd"
    );
    assert_eq!(
        proxy.get_property::<String>("ActiveSource").await.unwrap(),
        "rmpd"
    );

    proxy.call_method("PinSource", &("",)).await.unwrap();
    assert_eq!(
        proxy.get_property::<String>("PinnedSource").await.unwrap(),
        ""
    );

    proxy
        .call_method("SetAutoSwitchPolicy", &("priority",))
        .await
        .unwrap();
    assert_eq!(
        proxy
            .get_property::<String>("AutoSwitchPolicy")
            .await
            .unwrap(),
        "priority"
    );

    proxy
        .call_method("SelectSource", &("bluetooth",))
        .await
        .unwrap();
    assert_eq!(
        proxy.get_property::<String>("ActiveSource").await.unwrap(),
        "bluetooth"
    );

    proxy
        .call_method("ReleaseSource", &("bluetooth",))
        .await
        .unwrap();

    let sources: Vec<(String, String, String, u8, u32, bool)> =
        proxy.get_property("Sources").await.unwrap();
    assert_eq!(sources.len(), 2);

    // Invalid D-Bus arguments rejected cleanly
    proxy
        .call_method("SelectSource", &("BAD!",))
        .await
        .unwrap_err();
    proxy
        .call_method("SelectSource", &("missing",))
        .await
        .unwrap_err();
    proxy
        .call_method("SetAutoSwitchPolicy", &("bogus",))
        .await
        .unwrap_err();

    // Verify reconcile recovers if CamillaDSP silently resets capture_device to subdevice 0 while rmpd is active
    dsp_mock.set_config(Some(DspConfigInfo {
        capture_device: Some("hw:Loopback,1,0".into()),
        samplerate: Some(48_000),
        ..DspConfigInfo::default()
    }));
    dsp_mock.set_state(ProcessingState::Running);
    proxy.call_method("SelectSource", &("rmpd",)).await.unwrap();
    loop_tx
        .send((
            1,
            LoopbackState {
                active: true,
                rate_hz: 96_000,
            },
            true,
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(j) = dsp_mock.last_set_config_json()
                && j.contains("96000")
                && j.contains("hw:Loopback,1,1")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("reconcile/route pushed 96k on subdevice 1");

    drop(loop_tx);
    mgr_handle.await.unwrap();
    drop(proxy);
    drop(server_conn);
    drop(kill);
}

#[tokio::test]
async fn source_manager_handles_corrupt_state_pause_errors_and_reconcile_timer() {
    let tmp = ScopedTempDir::new("source_manager_reconcile");
    let dsp_cfg_path = tmp.path().join("crossover.yml");
    let source_state_path = tmp.path().join("source-state.json");
    std::fs::write(&dsp_cfg_path, SAMPLE_CONFIG_YAML).unwrap();
    // Corrupt source-state.json falls back cleanly to default policy
    std::fs::write(&source_state_path, b"{corrupt").unwrap();

    let reg = SourceRegistry::default_appliance();
    let snapshot = AudioSourcesSnapshot::new(AutoSwitchPolicy::Priority);
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (dsp, dsp_mock) = DspDevice::new_mocked();
    let (ctrl, ctrl_mock) = SourceController::new_mocked();
    ctrl_mock.set_fail_all(true);

    let mut mgr = SourceManager::new(
        reg,
        SourceManagerConfig {
            camilladsp_config_path: dsp_cfg_path,
            source_state_path,
            default_policy: AutoSwitchPolicy::Priority,
        },
        dsp,
        Arc::new(ctrl),
        snapshot,
        None,
    );
    assert_eq!(mgr.arbiter().policy(), AutoSwitchPolicy::Priority);

    let (loop_tx, loop_rx) = mpsc::channel(8);
    let mgr_handle = tokio::spawn(async move {
        mgr.run(cmd_rx, loop_rx).await;
    });

    // Unknown subdevice ignored; seed subdevices 0 & 1
    loop_tx
        .send((
            7,
            LoopbackState {
                active: true,
                rate_hz: 48_000,
            },
            false,
        ))
        .await
        .unwrap();
    loop_tx
        .send((
            0,
            LoopbackState {
                active: true,
                rate_hz: 48_000,
            },
            false,
        ))
        .await
        .unwrap();
    loop_tx
        .send((
            1,
            LoopbackState {
                active: true,
                rate_hz: 44_100,
            },
            false,
        ))
        .await
        .unwrap();

    // Wait for initial DSP push on subdevice 0
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if dsp_mock.last_set_config_json().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // Simulate CamillaDSP restarting onto wrong capture_device and let the 2s reconcile timer fire
    let before_uploads = dsp_mock.set_config_json_calls();
    tokio::time::pause();
    dsp_mock.set_config(Some(DspConfigInfo {
        capture_device: Some("hw:Loopback,1,1".into()),
        samplerate: Some(44_100),
        ..DspConfigInfo::default()
    }));
    dsp_mock.set_state(ProcessingState::Running);
    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::time::resume();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if dsp_mock.set_config_json_calls() > before_uploads
                && let Some(j) = dsp_mock.last_set_config_json()
                && j.contains("hw:Loopback,1,0")
                && j.contains("48000")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // Closing cmd_rx exits cleanly
    drop(cmd_tx);
    mgr_handle.await.unwrap();
}

#[tokio::test]
async fn mpd_play_watcher_reports_play_intent_on_resume_edge() {
    use audio_manager::sources::manager::run_mpd_play_watcher;
    use speaker_mpd::{MpdDevice, MpdEvent, MpdPlaybackState, MpdSubsystem};

    let (mpd, mpd_mock) = MpdDevice::new_mocked();
    mpd_mock.set_state(MpdPlaybackState::Paused);
    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

    let watcher = tokio::spawn(run_mpd_play_watcher(sid("rmpd"), mpd, cmd_tx));

    // Wait for initial subscription, then transition Paused -> Playing
    tokio::time::sleep(Duration::from_millis(20)).await;
    mpd_mock.set_state(MpdPlaybackState::Playing);
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Player));

    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("ReportPlayIntent must arrive")
        .unwrap();
    assert_eq!(cmd.kind, SourceCommandKind::ReportPlayIntent(sid("rmpd")));

    // Second Player event while already Playing does NOT re-emit
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Player));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), cmd_rx.recv())
            .await
            .is_err()
    );

    drop(cmd_rx);
    watcher.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn source_manager_seed_deadline_unblocks_partial_loopbacks() {
    let reg = SourceRegistry::default_appliance();
    let dir = ScopedTempDir::new("src_mgr_seed_deadline");
    let dsp_cfg_path = dir.path().join("dsp.yml");
    let source_state_path = dir.path().join("source-state.json");
    std::fs::write(&dsp_cfg_path, SAMPLE_CONFIG_YAML).unwrap();

    let snapshot = AudioSourcesSnapshot::new(AutoSwitchPolicy::LastPlayWins);
    let (_cmd_tx, cmd_rx) = mpsc::channel(8);
    let (dsp, dsp_mock) = DspDevice::new_mocked();
    let (ctrl, _ctrl_mock) = SourceController::new_mocked();

    let mut mgr = SourceManager::new(
        reg,
        SourceManagerConfig {
            camilladsp_config_path: dsp_cfg_path,
            source_state_path,
            default_policy: AutoSwitchPolicy::LastPlayWins,
        },
        dsp,
        Arc::new(ctrl),
        snapshot.clone(),
        None,
    );

    let (loop_tx, loop_rx) = mpsc::channel(8);
    let mgr_handle = tokio::spawn(async move {
        mgr.run(cmd_rx, loop_rx).await;
    });

    // Only subdevice 0 reports; subdevice 1 never reports.
    loop_tx
        .send((
            0,
            LoopbackState {
                active: true,
                rate_hz: 48_000,
            },
            false,
        ))
        .await
        .unwrap();

    tokio::task::yield_now().await;
    assert!(dsp_mock.last_set_config_json().is_none());

    // Advance past the 2s seed deadline so partial seed activates bluetooth.
    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::time::resume();

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if dsp_mock.last_set_config_json().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("seed deadline should push route for observed subdevice");

    drop(loop_tx);
    mgr_handle.await.unwrap();
}

#[test]
fn arbiter_on_play_intent_handles_unknown_last_play_and_pin() {
    let reg = SourceRegistry::default_appliance();
    let mut arbiter = SourceArbiter::new(reg, PersistedSourceState::default());
    let seeded = arbiter.seed([
        (sid("bluetooth"), true, 48_000),
        (sid("rmpd"), false, 44_100),
    ]);
    assert_eq!(seeded.active_changed, Some(Some(sid("bluetooth"))));

    // Unknown source ID is ignored.
    let unknown = arbiter.on_play_intent(&sid("unknown"));
    assert!(unknown.dsp_target.is_none());
    assert!(unknown.pause_sources.is_empty());

    // Play intent from rmpd under LastPlayWins switches active source and pauses bluetooth.
    let take_over = arbiter.on_play_intent(&sid("rmpd"));
    assert_eq!(take_over.active_changed, Some(Some(sid("rmpd"))));
    assert_eq!(
        take_over
            .dsp_target
            .as_ref()
            .map(|r| r.capture_device.as_ref()),
        Some("hw:Loopback,1,1")
    );
    assert_eq!(take_over.pause_sources, vec![sid("bluetooth")]);

    // Pin bluetooth, then play intent on rmpd is rejected with pause_sources: [rmpd].
    let _ = arbiter.pin_source(Some(&sid("bluetooth"))).unwrap();
    let blocked = arbiter.on_play_intent(&sid("rmpd"));
    assert!(blocked.dsp_target.is_none());
    assert_eq!(blocked.pause_sources, vec![sid("rmpd")]);

    // Releasing the active source when no fallback is streaming clears dsp_target.streaming.
    let released = arbiter.release_source(&sid("bluetooth")).unwrap();
    assert_eq!(released.pause_sources, vec![sid("bluetooth")]);
    assert!(
        released
            .dsp_target
            .as_ref()
            .is_some_and(|t| !t.streaming && t.capture_device.as_ref() == "hw:Loopback,1,0")
    );
}

#[test]
fn mpd_host_and_port_serde_enforces_invariants() {
    assert!(serde_json::from_str::<MpdHost>("\"\"").is_err());
    assert!(serde_json::from_str::<MpdHost>("\"bad host\"").is_err());
    assert_eq!(
        serde_json::from_str::<MpdHost>("\"127.0.0.1\"").unwrap(),
        MpdHost::default()
    );
    assert!(serde_json::from_str::<MpdPort>("0").is_err());
    assert_eq!(
        serde_json::from_str::<MpdPort>("6600").unwrap(),
        MpdPort::default()
    );
}

#[tokio::test]
async fn mpd_play_watcher_handles_fault_non_player_and_disconnect() {
    use audio_manager::sources::manager::run_mpd_play_watcher;
    use speaker_mpd::{MpdDevice, MpdEvent, MpdPlaybackState, MpdSubsystem};

    let (mpd, mpd_mock) = MpdDevice::new_mocked();
    mpd_mock.set_state(MpdPlaybackState::Paused);
    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

    let watcher = tokio::spawn(run_mpd_play_watcher(sid("rmpd"), mpd, cmd_tx));
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Non-player subsystem event does not emit play intent.
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(cmd_rx.try_recv().is_err());

    // Fault during status check holds state without crashing, then clears.
    mpd_mock.inject_fault("transient mpd glitch");
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Player));
    tokio::time::sleep(Duration::from_millis(20)).await;
    mpd_mock.clear_fault();

    // Stopped -> Playing transition emits ReportPlayIntent.
    mpd_mock.set_state(MpdPlaybackState::Stopped);
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Player));
    tokio::time::sleep(Duration::from_millis(20)).await;
    mpd_mock.set_state(MpdPlaybackState::Playing);
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Player));

    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("ReportPlayIntent must arrive after Stopped -> Playing")
        .unwrap();
    assert_eq!(cmd.kind, SourceCommandKind::ReportPlayIntent(sid("rmpd")));

    // Disconnected event is logged and tolerated without panicking.
    mpd_mock.emit_event(MpdEvent::Disconnected("eof".into()));
    tokio::time::sleep(Duration::from_millis(20)).await;

    drop(cmd_rx);
    watcher.await.unwrap();
}

#[tokio::test]
async fn mpd_play_watcher_initial_playing_retry_and_manager_play_intent_cmd() {
    use audio_manager::sources::manager::run_mpd_play_watcher;
    use speaker_mpd::{MpdDevice, MpdPlaybackState};

    // 1. Closed cmd_tx exits watcher immediately.
    let (mpd_closed, _) = MpdDevice::new_mocked();
    let (tx_closed, rx_closed) = mpsc::channel(1);
    drop(rx_closed);
    run_mpd_play_watcher(sid("rmpd"), mpd_closed, tx_closed).await;

    // 2. Initial connect failure backs off, then initial Playing state emits ReportPlayIntent.
    let (mpd, mpd_mock) = MpdDevice::new_mocked();
    mpd_mock.set_state(MpdPlaybackState::Playing);
    mpd_mock.inject_fault("mpd not ready");
    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
    let watcher = tokio::spawn(run_mpd_play_watcher(sid("rmpd"), mpd, cmd_tx));
    tokio::time::sleep(Duration::from_millis(40)).await;
    mpd_mock.clear_fault();

    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("initial Playing state after retry must emit ReportPlayIntent")
        .unwrap();
    assert_eq!(cmd.kind, SourceCommandKind::ReportPlayIntent(sid("rmpd")));
    drop(cmd_rx);
    watcher.await.unwrap();

    // 3. SourceManager::handle_command processes ReportPlayIntent directly.
    let dir = ScopedTempDir::new("src_mgr_play_intent");
    let dsp_cfg_path = dir.path().join("dsp.yml");
    let state_path = dir.path().join("source-state.json");
    std::fs::write(&dsp_cfg_path, SAMPLE_CONFIG_YAML).unwrap();

    let reg = SourceRegistry::default_appliance();
    let snapshot = AudioSourcesSnapshot::new(AutoSwitchPolicy::LastPlayWins);
    let (dsp, _dsp_mock) = DspDevice::new_mocked();
    let (ctrl, _ctrl_mock) = SourceController::new_mocked();
    let mut mgr = SourceManager::new(
        reg,
        SourceManagerConfig {
            camilladsp_config_path: dsp_cfg_path,
            source_state_path: state_path,
            default_policy: AutoSwitchPolicy::LastPlayWins,
        },
        dsp,
        Arc::new(ctrl),
        snapshot.clone(),
        None,
    );
    mgr.handle_command(SourceCommandKind::ReportPlayIntent(sid("rmpd")))
        .await
        .unwrap();
    assert_eq!(snapshot.active_source_str(), "rmpd");
}

#[test]
fn source_manager_honors_default_policy_when_state_file_missing() {
    let dir = ScopedTempDir::new("src_mgr_default_policy");
    let dsp_cfg_path = dir.path().join("dsp.yml");
    let missing_state_path = dir.path().join("missing-source-state.json");
    std::fs::write(&dsp_cfg_path, SAMPLE_CONFIG_YAML).unwrap();

    let reg = SourceRegistry::default_appliance();
    let snapshot = AudioSourcesSnapshot::new(AutoSwitchPolicy::LastPlayWins);
    let (dsp, _dsp_mock) = DspDevice::new_mocked();
    let (ctrl, _ctrl_mock) = SourceController::new_mocked();
    let mgr = SourceManager::new(
        reg,
        SourceManagerConfig {
            camilladsp_config_path: dsp_cfg_path,
            source_state_path: missing_state_path,
            default_policy: AutoSwitchPolicy::Priority,
        },
        dsp,
        Arc::new(ctrl),
        snapshot.clone(),
        None,
    );
    assert_eq!(mgr.arbiter().policy(), AutoSwitchPolicy::Priority);
    assert_eq!(snapshot.policy_str(), "priority");
}
