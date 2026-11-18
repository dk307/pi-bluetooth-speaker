//! Integration tests for the single-active-Bluetooth-source arbiter
//! (`ActiveSourceState` + `org.speaker.Bluetooth1` D-Bus service).

use bt_manager::active_source::{
    ActiveSourceState, ArbiterOutcome, SharedActiveSource, serve_active_source_on,
};
use speaker_bluetooth::{
    ACTIVE_DEVICE_PROPERTY, ACTIVE_SOURCE_DBUS_NAME, ACTIVE_SOURCE_INTERFACE,
    ACTIVE_SOURCE_OBJECT_PATH, BluetoothDevice, BtAddress, device_address_from_path,
    format_active_device, parse_active_device,
};

const PCM_A: &str = "/org/bluealsa/hci0/dev_C4_03_A8_BD_EF_74/a2dpsnk/source";
const PCM_B: &str = "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/a2dpsnk/source";
const PCM_HFP: &str = "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/hfphf/source";

fn addr_a() -> BtAddress {
    BtAddress::parse_str("C4:03:A8:BD:EF:74").unwrap()
}

fn addr_b() -> BtAddress {
    BtAddress::parse_str("30:E0:44:67:38:A2").unwrap()
}

#[test]
fn first_play_wins_and_pauses_shutout_once_per_transition() {
    let mut state = ActiveSourceState::new();
    assert_eq!(state.active(), None);

    // Device A starts streaming -> acquires slot, no pause.
    let out = state.on_pcm_running_changed(PCM_A, true);
    assert_eq!(
        out,
        ArbiterOutcome {
            active_changed: Some(Some(addr_a())),
            pause_device: None,
        }
    );
    assert_eq!(state.active(), Some(addr_a()));

    // Duplicate Running=true on A is a no-op.
    assert_eq!(
        state.on_pcm_running_changed(PCM_A, true),
        ArbiterOutcome::default()
    );

    // Device B starts streaming while A is active -> B is shut out, paused once.
    let out = state.on_pcm_running_changed(PCM_B, true);
    assert_eq!(
        out,
        ArbiterOutcome {
            active_changed: None,
            pause_device: Some(addr_b()),
        }
    );
    assert_eq!(state.active(), Some(addr_a()));

    // Repeated Running=true or re-add for B while still running does not re-pause.
    assert_eq!(state.on_pcm_added(PCM_B, true), ArbiterOutcome::default());

    // B stops, then tries again -> paused once more.
    assert_eq!(
        state.on_pcm_running_changed(PCM_B, false),
        ArbiterOutcome::default()
    );
    assert_eq!(
        state.on_pcm_running_changed(PCM_B, true),
        ArbiterOutcome {
            active_changed: None,
            pause_device: Some(addr_b()),
        }
    );
}

#[test]
fn release_promotes_still_running_device_or_goes_idle() {
    let mut state = ActiveSourceState::new();
    state.on_pcm_running_changed(PCM_A, true);
    state.on_pcm_running_changed(PCM_B, true);

    // A stops while B is still streaming -> B is promoted immediately.
    let out = state.on_pcm_running_changed(PCM_A, false);
    assert_eq!(
        out,
        ArbiterOutcome {
            active_changed: Some(Some(addr_b())),
            pause_device: None,
        }
    );
    assert_eq!(state.active(), Some(addr_b()));

    // B is removed (disconnects) -> speaker goes idle.
    let out = state.on_pcm_removed(PCM_B);
    assert_eq!(
        out,
        ArbiterOutcome {
            active_changed: Some(None),
            pause_device: None,
        }
    );
    assert_eq!(state.active(), None);
}

#[test]
fn promotion_prefers_recent_client_delay_and_ignores_hfp() {
    let mut state = ActiveSourceState::new();
    // Non-A2DP PCM is ignored.
    assert_eq!(state.on_pcm_added(PCM_HFP, true), ArbiterOutcome::default());
    assert_eq!(state.active(), None);

    state.on_pcm_added(PCM_A, true);
    state.on_pcm_added(PCM_B, true);
    state.on_pcm_client_delay(PCM_A);

    // Remove B? Wait: A is active; if A drops, B is promoted.
    assert_eq!(
        state.on_pcm_removed(PCM_A),
        ArbiterOutcome {
            active_changed: Some(Some(addr_b())),
            pause_device: None,
        }
    );
}

#[test]
fn seed_handles_zero_one_and_two_running_pcms() {
    let mut state = ActiveSourceState::new();
    assert_eq!(
        state.seed([(PCM_A, false), (PCM_B, false)]),
        ArbiterOutcome::default()
    );
    assert_eq!(state.active(), None);

    let out = state.seed([(PCM_A, true), (PCM_B, false)]);
    assert_eq!(
        out,
        ArbiterOutcome {
            active_changed: Some(Some(addr_a())),
            pause_device: None,
        }
    );

    // Restart ambiguity: both running; lexicographically first path (PCM_B) wins,
    // and PCM_A is paused.
    let mut restarted = ActiveSourceState::new();
    let out = restarted.seed([(PCM_A, true), (PCM_B, true), (PCM_HFP, true)]);
    assert_eq!(out.active_changed, Some(Some(addr_b())));
    assert_eq!(out.pause_device, Some(addr_a()));
}

#[test]
fn active_source_wire_helpers_round_trip() {
    assert_eq!(parse_active_device(""), None);
    assert_eq!(parse_active_device("not-a-mac"), None);
    assert_eq!(parse_active_device("C4:03:A8:BD:EF:74"), Some(addr_a()));
    assert_eq!(format_active_device(None), "");
    assert_eq!(format_active_device(Some(addr_a())), "C4:03:A8:BD:EF:74");
    assert_eq!(device_address_from_path(PCM_A), Some(addr_a()));
    assert_eq!(
        device_address_from_path("/org/bluez/hci0/dev_30_E0_44_67_38_A2/avrcp/player0"),
        Some(addr_b())
    );
}

#[test]
fn mock_device_tracks_active_source_and_targets_volume() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    assert_eq!(bt.active_device(), None);
    mock.set_active_device(Some(addr_a()));
    assert_eq!(bt.active_device(), Some(addr_a()));
    bt.set_active_source(Some(addr_b()));
    assert_eq!(bt.active_device(), Some(addr_b()));
}

struct PrivateDbusDaemon {
    child: std::process::Child,
    address: String,
}

impl PrivateDbusDaemon {
    fn spawn() -> Self {
        let mut child = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("dbus-daemon must be installed for D-Bus integration tests");
        let stdout = child.stdout.take().unwrap();
        let mut reader = std::io::BufReader::new(stdout);
        let mut address = String::new();
        std::io::BufRead::read_line(&mut reader, &mut address).unwrap();
        Self {
            child,
            address: address.trim().to_string(),
        }
    }
}

impl Drop for PrivateDbusDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn dbus_active_device_property_round_trips_on_private_bus() {
    let daemon = PrivateDbusDaemon::spawn();
    let addr = daemon.address.clone();

    let server_conn = zbus::connection::Builder::address(addr.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let shared = SharedActiveSource::new();
    shared.set(Some(addr_a()));
    let _emitter = serve_active_source_on(&server_conn, shared.clone())
        .await
        .unwrap();

    let client_conn = zbus::connection::Builder::address(addr.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(&client_conn)
        .destination(ACTIVE_SOURCE_DBUS_NAME)
        .unwrap()
        .path(ACTIVE_SOURCE_OBJECT_PATH)
        .unwrap()
        .interface(ACTIVE_SOURCE_INTERFACE)
        .unwrap()
        .build()
        .await
        .unwrap();
    let prop: String = proxy.get_property(ACTIVE_DEVICE_PROPERTY).await.unwrap();
    assert_eq!(parse_active_device(&prop), Some(addr_a()));

    // Blocking client reads the same property.
    let read_back = tokio::task::spawn_blocking(move || {
        let blocking = zbus::blocking::connection::Builder::address(addr.as_str())
            .unwrap()
            .build()
            .unwrap();
        let bt = BluetoothDevice::with_connection(blocking);
        bt.active_device()
    })
    .await
    .unwrap();
    assert_eq!(read_back, Some(addr_a()));
}
