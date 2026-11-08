//! Integration tests for Bluetooth transport reconciler state machine,
//! snapshot classification, and D-Bus mock recovery.

use std::collections::HashMap;
use std::time::Duration;

use speaker_bluetooth::connection::ManagedObjects;
use speaker_bluetooth::reconciler::{
    BluezDeviceSnapshot, DeviceWatchState, DisconnectReason, MAX_HARD_RESETS_PER_SESSION,
    MockReconcilerDbus, ReconcileAction, ReconcilerDbus, TickOutcome, UUID_A2DP_SOURCE,
    build_snapshots, collect_snapshots, execute_reconcile_actions_with_settle, extract_bool,
    extract_object_path, extract_str, extract_string_array, is_unknown_object_or_method,
    reconcile_snapshots, reconciler_tick,
};
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{Array, OwnedObjectPath, OwnedValue, Value};

fn to_val<T: Into<Value<'static>>>(v: T) -> OwnedValue {
    OwnedValue::try_from(v.into()).unwrap()
}

fn owned_iface(s: &str) -> OwnedInterfaceName {
    OwnedInterfaceName::try_from(s).expect("valid iface")
}

fn owned_path(s: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(s).expect("valid path")
}

#[test]
fn extract_bool_handles_various_types() {
    let mut props = HashMap::new();
    props.insert("Connected".to_string(), to_val(true));
    props.insert("Paired".to_string(), to_val(false));
    props.insert("NonBool".to_string(), to_val("hello"));

    assert_eq!(extract_bool(&props, "Connected"), Some(true));
    assert_eq!(extract_bool(&props, "Paired"), Some(false));
    assert_eq!(extract_bool(&props, "NonBool"), None);
    assert_eq!(extract_bool(&props, "Missing"), None);
}

#[test]
fn extract_str_and_object_path_coverage() {
    let mut props = HashMap::new();
    props.insert("Name".to_string(), to_val("Pixel"));
    props.insert("DevPath".to_string(), to_val(owned_path("/dev/test")));
    props.insert("NonStr".to_string(), to_val(42i32));

    assert_eq!(extract_str(&props, "Name"), Some("Pixel"));
    assert_eq!(extract_str(&props, "NonStr"), None);
    assert_eq!(extract_str(&props, "Missing"), None);

    assert_eq!(
        extract_object_path(&props, "DevPath"),
        Some("/dev/test".to_string())
    );
    assert_eq!(
        extract_object_path(&props, "Name"),
        Some("Pixel".to_string())
    );
    assert_eq!(extract_object_path(&props, "NonStr"), None);
    assert_eq!(extract_object_path(&props, "Missing"), None);
}

#[test]
fn extract_string_array_handles_vec_and_array() {
    let mut props = HashMap::new();
    let list = vec!["item1".to_string(), "item2".to_string()];
    props.insert("List1".to_string(), to_val(list.clone()));

    let extracted = extract_string_array(&props, "List1");
    assert_eq!(extracted, list);

    let array_val = Value::Array(Array::from(vec![Value::from("alpha"), Value::from("beta")]));
    props.insert("List2".to_string(), to_val(array_val));
    let extracted2 = extract_string_array(&props, "List2");
    assert_eq!(extracted2, vec!["alpha".to_string(), "beta".to_string()]);

    assert_eq!(
        extract_string_array(&props, "Missing"),
        Vec::<String>::new()
    );
}

#[test]
fn build_snapshots_classifies_healthy_session() {
    let dev_path = "/org/bluez/hci1/dev_30_E0_44_67_38_A2";
    let mut bluez_objs = ManagedObjects::new();

    let mut dev_props = HashMap::new();
    dev_props.insert("Connected".to_string(), to_val(true));
    dev_props.insert("Paired".to_string(), to_val(true));
    dev_props.insert("Bonded".to_string(), to_val(true));
    dev_props.insert("ServicesResolved".to_string(), to_val(true));
    dev_props.insert("UUIDs".to_string(), to_val(vec![UUID_A2DP_SOURCE]));

    let mut dev_ifaces = HashMap::new();
    dev_ifaces.insert(owned_iface("org.bluez.Device1"), dev_props);
    bluez_objs.insert(owned_path(dev_path), dev_ifaces);

    // Add MediaTransport1 object
    let fd_path = format!("{dev_path}/fd1");
    let mut transport_ifaces = HashMap::new();
    transport_ifaces.insert(owned_iface("org.bluez.MediaTransport1"), HashMap::new());
    bluez_objs.insert(owned_path(&fd_path), transport_ifaces);

    // Add bluealsa PCM object
    let mut bluealsa_objs = ManagedObjects::new();
    let pcm_path = "/org/bluealsa/hci1/dev_30_E0_44_67_38_A2/a2dpsnk/source";
    let mut pcm_props = HashMap::new();
    pcm_props.insert("Transport".to_string(), to_val("A2DP-sink"));
    pcm_props.insert("Device".to_string(), to_val(owned_path(dev_path)));
    let mut pcm_ifaces = HashMap::new();
    pcm_ifaces.insert(owned_iface("org.bluealsa.PCM1"), pcm_props);
    bluealsa_objs.insert(owned_path(pcm_path), pcm_ifaces);

    let snapshots = build_snapshots(&bluez_objs, &bluealsa_objs);
    assert_eq!(snapshots.len(), 1);
    let s = &snapshots[0];
    assert_eq!(s.path.as_ref(), dev_path);
    assert!(s.connected);
    assert!(s.paired);
    assert!(s.bonded);
    assert!(s.services_resolved);
    assert!(s.supports_a2dp_source);
    assert!(s.has_bluez_transport);
    assert!(s.has_bluealsa_pcm);
    assert!(!s.has_voice_call_pcm);
}

#[test]
fn reconcile_snapshots_case_a_orphaned_transport() {
    let dev_path: Box<str> = "/org/bluez/hci1/dev_orphan".into();
    let mut tracked = HashMap::new();

    let snapshot = BluezDeviceSnapshot {
        path: dev_path.clone(),
        connected: true,
        paired: true,
        bonded: true,
        services_resolved: true,
        supports_a2dp_source: true,
        has_bluez_transport: true,
        has_bluealsa_pcm: false, // orphaned fd1 in BlueZ
        has_voice_call_pcm: false,
    };

    // Tick 1: observed divergence, tick counter advances to 1, no action yet (debounce)
    let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
    assert_eq!(actions, []);
    assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 1);

    // Tick 2: divergence persists, triggers CycleConnection
    let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
    assert_eq!(actions.len(), 1);
    assert_eq!(
        actions[0],
        ReconcileAction::CycleConnection {
            device_path: dev_path.clone(),
            reason: DisconnectReason::OrphanedBluezTransport,
        }
    );
    assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 0);
    // hard_resets_attempted is incremented only after execution succeeds
    assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 0);
}

#[test]
fn reconcile_snapshots_case_b_acl_half_reconnect() {
    let dev_path: Box<str> = "/org/bluez/hci1/dev_half_conn".into();
    let mut tracked = HashMap::new();

    let snapshot = BluezDeviceSnapshot {
        path: dev_path.clone(),
        connected: true,
        paired: true,
        bonded: true,
        services_resolved: true,
        supports_a2dp_source: true,
        has_bluez_transport: false, // no transport
        has_bluealsa_pcm: false,    // no PCM
        has_voice_call_pcm: false,
    };

    // Tick 1 (3s): debounce
    let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
    assert_eq!(actions, []);

    // Tick 2 (6s): Stage 1 - non-disruptive ConnectProfile(A2DP_SOURCE)
    let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
    assert_eq!(actions.len(), 1);
    assert_eq!(
        actions[0],
        ReconcileAction::ConnectA2dpProfile {
            device_path: dev_path.clone(),
        }
    );
    assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 2);

    // Tick 3 (9s): debounce wait for transport to appear
    let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
    assert_eq!(actions, []);
    assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 3);

    // Tick 4 (12s): Stage 2 - escalation to Disconnect -> Connect cycle
    let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
    assert_eq!(actions.len(), 1);
    assert_eq!(
        actions[0],
        ReconcileAction::CycleConnection {
            device_path: dev_path.clone(),
            reason: DisconnectReason::AclHalfReconnect,
        }
    );
    assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 0);
    assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 0);
}

#[test]
fn reconcile_snapshots_ignores_unpaired_or_non_a2dp_devices() {
    let mut tracked = HashMap::new();

    let unpaired = BluezDeviceSnapshot {
        path: "/org/bluez/hci1/dev_unpaired".into(),
        connected: true,
        paired: false,
        bonded: false,
        services_resolved: true,
        supports_a2dp_source: true,
        has_bluez_transport: false,
        has_bluealsa_pcm: false,
        has_voice_call_pcm: false,
    };

    let non_a2dp = BluezDeviceSnapshot {
        path: "/org/bluez/hci1/dev_keyboard".into(),
        connected: true,
        paired: true,
        bonded: true,
        services_resolved: true,
        supports_a2dp_source: false,
        has_bluez_transport: false,
        has_bluealsa_pcm: false,
        has_voice_call_pcm: false,
    };

    let actions = reconcile_snapshots(&mut tracked, &[unpaired, non_a2dp]);
    assert_eq!(actions, []);
    assert!(tracked.is_empty());
}

#[test]
fn reconcile_snapshots_preserves_cap_across_transient_disconnect() {
    let dev_path: Box<str> = "/org/bluez/hci1/dev_disconnect".into();
    let mut tracked = HashMap::new();
    tracked.insert(
        dev_path.clone(),
        DeviceWatchState {
            divergence_ticks: 2,
            hard_resets_attempted: 1,
        },
    );

    let disconnected_snapshot = BluezDeviceSnapshot {
        path: dev_path.clone(),
        connected: false,
        paired: true,
        bonded: true,
        services_resolved: true,
        supports_a2dp_source: true,
        has_bluez_transport: false,
        has_bluealsa_pcm: false,
        has_voice_call_pcm: false,
    };

    let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&disconnected_snapshot));
    assert_eq!(actions, []);
    let state = tracked
        .get(&dev_path)
        .expect("tracked state preserved across disconnect");
    assert_eq!(state.divergence_ticks, 0);
    assert_eq!(state.hard_resets_attempted, 1);
}

#[test]
fn reconcile_snapshots_voice_call_protection() {
    let dev_path: Box<str> = "/org/bluez/hci1/dev_call".into();
    let mut tracked = HashMap::new();

    let in_call = BluezDeviceSnapshot {
        path: dev_path.clone(),
        connected: true,
        paired: true,
        bonded: true,
        services_resolved: true,
        supports_a2dp_source: true,
        has_bluez_transport: false,
        has_bluealsa_pcm: false,
        has_voice_call_pcm: true, // phone call active!
    };

    let actions = reconcile_snapshots(&mut tracked, &[in_call]);
    assert_eq!(actions, []);
    // Voice call must not accumulate divergence ticks
    assert_eq!(tracked.get(&dev_path).map_or(0, |s| s.divergence_ticks), 0);
}

#[test]
fn reconcile_snapshots_backs_off_after_max_resets() {
    let dev_path: Box<str> = "/org/bluez/hci1/dev_stubborn".into();
    let mut tracked = HashMap::new();
    tracked.insert(
        dev_path.clone(),
        DeviceWatchState {
            divergence_ticks: 3,
            hard_resets_attempted: MAX_HARD_RESETS_PER_SESSION,
        },
    );

    let snapshot = BluezDeviceSnapshot {
        path: dev_path.clone(),
        connected: true,
        paired: true,
        bonded: true,
        services_resolved: true,
        supports_a2dp_source: true,
        has_bluez_transport: true,
        has_bluealsa_pcm: false,
        has_voice_call_pcm: false,
    };

    let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
    assert_eq!(actions, []);

    // Stage 1 ConnectProfile in Case B is still permitted even when hard resets are capped,
    // whereas Stage 2 CycleConnection is blocked by the cap.
    tracked.insert(
        dev_path.clone(),
        DeviceWatchState {
            divergence_ticks: 1,
            hard_resets_attempted: MAX_HARD_RESETS_PER_SESSION,
        },
    );
    let case_b_snapshot = BluezDeviceSnapshot {
        has_bluez_transport: false,
        ..snapshot
    };
    let stage1_actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&case_b_snapshot));
    assert_eq!(
        stage1_actions,
        vec![ReconcileAction::ConnectA2dpProfile {
            device_path: dev_path,
        }]
    );
    let _ = reconcile_snapshots(&mut tracked, std::slice::from_ref(&case_b_snapshot));
    let stage2_actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&case_b_snapshot));
    assert_eq!(stage2_actions, []);
}

#[test]
fn is_unknown_object_or_method_matches_names() {
    let err1 = zbus::fdo::Error::UnknownObject("object not found".into());
    let err2 = zbus::fdo::Error::UnknownInterface("iface not found".into());
    let err3 = zbus::fdo::Error::UnknownMethod("method not found".into());
    let err4 = zbus::fdo::Error::AccessDenied("rejected".into());

    assert!(is_unknown_object_or_method(&err1));
    assert!(is_unknown_object_or_method(&err2));
    assert!(is_unknown_object_or_method(&err3));
    assert!(!is_unknown_object_or_method(&err4));
}

#[tokio::test]
async fn mock_dbus_collect_snapshots_fast_path_and_errors() {
    let (mock, ctrl) = MockReconcilerDbus::new_mocked();

    // 1. BlueZ query fails with D-Bus error
    ctrl.set_bluez_error(zbus::Error::Address("unreachable bus".into()));
    let res = collect_snapshots(&mock).await;
    let _ = res.unwrap_err();

    // 2. Fast-path: BlueZ has only disconnected devices -> skips BlueALSA query
    let mut bluez_objs = ManagedObjects::new();
    let mut dev_props = HashMap::new();
    dev_props.insert("Connected".to_string(), to_val(false));
    dev_props.insert("Paired".to_string(), to_val(true));
    let mut dev_ifaces = HashMap::new();
    dev_ifaces.insert(owned_iface("org.bluez.Device1"), dev_props);
    bluez_objs.insert(owned_path("/org/bluez/hci0/dev_1"), dev_ifaces);

    ctrl.set_bluez_objects(bluez_objs.clone());
    ctrl.set_bluealsa_error(zbus::Error::Failure("must not be called".into()));
    let snapshots = collect_snapshots(&mock).await.expect("fast path succeeds");
    assert_eq!(snapshots.len(), 1);
    assert!(!snapshots[0].connected);

    // 3. Connected device triggers BlueALSA query; BlueALSA failure aborts tick
    let mut connected_dev_props = HashMap::new();
    connected_dev_props.insert("Connected".to_string(), to_val(true));
    connected_dev_props.insert("Paired".to_string(), to_val(true));
    let mut connected_ifaces = HashMap::new();
    connected_ifaces.insert(owned_iface("org.bluez.Device1"), connected_dev_props);
    let mut connected_bluez = ManagedObjects::new();
    connected_bluez.insert(owned_path("/org/bluez/hci0/dev_2"), connected_ifaces);

    ctrl.set_bluez_objects(connected_bluez);
    ctrl.set_bluealsa_error(zbus::Error::Failure("bluealsa down".into()));
    let err_res = collect_snapshots(&mock).await;
    let _ = err_res.unwrap_err();
}

#[tokio::test]
async fn mock_dbus_execute_actions_and_cycle_connection() {
    let (mock, ctrl) = MockReconcilerDbus::new_mocked();
    let dev_path: Box<str> = "/org/bluez/hci0/dev_action_test".into();
    let mut tracked = HashMap::new();

    // Successful ConnectProfile
    let action1 = ReconcileAction::ConnectA2dpProfile {
        device_path: dev_path.clone(),
    };
    execute_reconcile_actions_with_settle(
        &mut tracked,
        &mock,
        &[action1],
        Duration::from_millis(1),
    )
    .await;
    assert_eq!(
        ctrl.connect_profile_calls(),
        vec![(dev_path.clone(), UUID_A2DP_SOURCE.into())]
    );

    // Failing ConnectProfile (handles error branch cleanly)
    ctrl.set_connect_profile_result(Err(zbus::Error::Failure("profile rejected".into())));
    let action2 = ReconcileAction::ConnectA2dpProfile {
        device_path: dev_path.clone(),
    };
    execute_reconcile_actions_with_settle(
        &mut tracked,
        &mock,
        &[action2],
        Duration::from_millis(1),
    )
    .await;

    // Successful connection cycle (Disconnect -> Settle -> Connect) increments hard_resets_attempted
    let action3 = ReconcileAction::CycleConnection {
        device_path: dev_path.clone(),
        reason: DisconnectReason::OrphanedBluezTransport,
    };
    execute_reconcile_actions_with_settle(
        &mut tracked,
        &mock,
        std::slice::from_ref(&action3),
        Duration::from_millis(1),
    )
    .await;
    assert_eq!(ctrl.disconnect_calls(), vec![dev_path.clone()]);
    assert_eq!(ctrl.connect_calls(), vec![dev_path.clone()]);
    assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 1);

    // Failing Disconnect/Connect also consumes the hard resets budget to prevent infinite thrashing loops
    ctrl.set_disconnect_result(Err(zbus::Error::Failure("disconnect failed".into())));
    ctrl.set_connect_result(Err(zbus::Error::Failure("connect failed".into())));
    execute_reconcile_actions_with_settle(
        &mut tracked,
        &mock,
        &[action3],
        Duration::from_millis(1),
    )
    .await;
    assert_eq!(ctrl.disconnect_calls().len(), 2);
    assert_eq!(ctrl.connect_calls().len(), 2);
    assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 2);
}

#[tokio::test]
async fn mock_dbus_reconciler_tick_outcomes() {
    let (mock, ctrl) = MockReconcilerDbus::new_mocked();
    let mut tracked = HashMap::new();

    // 1. Query error outcome
    ctrl.set_bluez_error(zbus::Error::Failure("service absent".into()));
    let outcome1 = reconciler_tick(&mock, &mut tracked, Duration::from_millis(1)).await;
    assert!(matches!(outcome1, TickOutcome::SkippedQueryError(_)));

    // 2. Normal tick with no actions needed (empty devices)
    ctrl.set_bluez_objects(ManagedObjects::new());
    let outcome2 = reconciler_tick(&mock, &mut tracked, Duration::from_millis(1)).await;
    assert_eq!(outcome2, TickOutcome::ActionsExecuted(0));

    // 3. Normal tick with recovery action executed
    let dev_path = "/org/bluez/hci0/dev_tick_test";
    let mut dev_props = HashMap::new();
    dev_props.insert("Connected".to_string(), to_val(true));
    dev_props.insert("Paired".to_string(), to_val(true));
    dev_props.insert("Bonded".to_string(), to_val(true));
    dev_props.insert("ServicesResolved".to_string(), to_val(true));
    dev_props.insert("UUIDs".to_string(), to_val(vec![UUID_A2DP_SOURCE]));
    let mut dev_ifaces = HashMap::new();
    dev_ifaces.insert(owned_iface("org.bluez.Device1"), dev_props);
    let mut bluez_objs = ManagedObjects::new();
    bluez_objs.insert(owned_path(dev_path), dev_ifaces);

    // Orphaned transport: BlueZ has /fd0, BlueALSA has no PCMs
    let mut fd_ifaces = HashMap::new();
    fd_ifaces.insert(owned_iface("org.bluez.MediaTransport1"), HashMap::new());
    bluez_objs.insert(owned_path(&format!("{dev_path}/fd0")), fd_ifaces);

    ctrl.set_bluez_objects(bluez_objs.clone());
    ctrl.set_bluealsa_objects(ManagedObjects::new());

    // Tick 1 (debounce): 0 actions executed
    let outcome3 = reconciler_tick(&mock, &mut tracked, Duration::from_millis(1)).await;
    assert_eq!(outcome3, TickOutcome::ActionsExecuted(0));
    assert_eq!(tracked.get(dev_path).unwrap().divergence_ticks, 1);

    // Tick 2 (escalate): 1 action executed (CycleConnection)
    ctrl.set_bluez_objects(bluez_objs);
    ctrl.set_bluealsa_objects(ManagedObjects::new());
    let outcome4 = reconciler_tick(&mock, &mut tracked, Duration::from_millis(1)).await;
    assert_eq!(outcome4, TickOutcome::ActionsExecuted(1));
    assert_eq!(tracked.get(dev_path).unwrap().divergence_ticks, 0);
    assert_eq!(tracked.get(dev_path).unwrap().hard_resets_attempted, 1);
}

#[tokio::test]
async fn test_mock_reconciler_new_mocked_and_controller() {
    let (mock, ctrl) = MockReconcilerDbus::new_mocked();

    // Test controller setters and mock calls, verifying exact error variant preservation
    ctrl.set_bluez_error(zbus::Error::Address("bluez failure".into()));
    let bluez_err = mock.fetch_bluez_objects().await;
    assert!(matches!(bluez_err.unwrap_err(), zbus::Error::Address(_)));

    ctrl.set_bluealsa_error(zbus::Error::Failure("bluealsa failure".into()));
    let bluealsa_err = mock.fetch_bluealsa_objects().await;
    assert!(matches!(bluealsa_err.unwrap_err(), zbus::Error::Failure(_)));

    ctrl.set_connect_profile_result(Err(zbus::Error::Address("profile err".into())));
    let cp_res = mock.connect_profile("/dev/test", "uuid").await;
    assert!(matches!(cp_res.unwrap_err(), zbus::Error::Address(_)));

    ctrl.set_disconnect_result(Err(zbus::Error::Address("disc err".into())));
    let disc_res = mock.disconnect("/dev/test").await;
    assert!(matches!(disc_res.unwrap_err(), zbus::Error::Address(_)));

    ctrl.set_connect_result(Err(zbus::Error::Address("conn err".into())));
    let conn_res = mock.connect("/dev/test").await;
    assert!(matches!(conn_res.unwrap_err(), zbus::Error::Address(_)));

    // Verify calls recorded through controller
    assert_eq!(
        ctrl.connect_profile_calls(),
        vec![("/dev/test".into(), "uuid".into())]
    );
    assert_eq!(ctrl.disconnect_calls(), vec!["/dev/test".into()]);
    assert_eq!(ctrl.connect_calls(), vec!["/dev/test".into()]);

    // Test &T blanket impl of ReconcilerDbus
    let mock_ref = &mock;
    let empty_bluez = mock_ref.fetch_bluez_objects().await.unwrap();
    assert!(empty_bluez.is_empty());
    let empty_bluealsa = mock_ref.fetch_bluealsa_objects().await.unwrap();
    assert!(empty_bluealsa.is_empty());

    ctrl.set_connect_profile_result(Ok(()));
    ctrl.set_disconnect_result(Ok(()));
    ctrl.set_connect_result(Ok(()));

    mock_ref.connect_profile("/dev/ref", "uuid2").await.unwrap();
    mock_ref.disconnect("/dev/ref").await.unwrap();
    mock_ref.connect("/dev/ref").await.unwrap();

    assert_eq!(ctrl.connect_profile_calls().len(), 2);
    assert_eq!(ctrl.disconnect_calls().len(), 2);
    assert_eq!(ctrl.connect_calls().len(), 2);
}
