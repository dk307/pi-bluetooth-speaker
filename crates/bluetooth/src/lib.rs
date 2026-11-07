//! Bluetooth & `BlueALSA` peripheral drivers, D-Bus connection management,
//! AVRCP transport, and Cover Art client contract.

pub mod connection;
pub mod cover_art;
pub mod cover_service;
pub mod reconciler;
pub mod transport;

#[doc(inline)]
pub use connection::{
    BCM43438_FIRMWARE_PATH, BCM43438_UART_BAUD_RATE, BT_ADDRESS_OCTETS, BluetoothControllerInfo,
    BluetoothDevice, BluetoothDeviceMock, BluetoothHardwareInfo, BluetoothServices,
    BluetoothStatus, BluezSnapshot, BtAdapter, BtAddress, BtDevice, BtError, DEFAULT_BT_ADAPTER,
    resolve_cover_art_with,
};

#[doc(inline)]
pub use reconciler::{
    BluezDeviceSnapshot, DBUS_CALL_TIMEOUT, DeviceWatchState, DisconnectReason,
    MAX_HARD_RESETS_PER_SESSION, MockReconcilerController, MockReconcilerDbus, POLL_INTERVAL,
    RECONNECT_SETTLE_DELAY, ReconcileAction, ReconcilerDbus, TickOutcome, UUID_A2DP_SOURCE,
    build_snapshots, collect_snapshots, cycle_device_connection,
    cycle_device_connection_with_settle, execute_reconcile_actions,
    execute_reconcile_actions_with_settle, fetch_bluealsa_objects, reconcile_snapshots,
    reconciler_tick, run_reconciler_loop, run_reconciler_loop_with, spawn_bt_transport_reconciler,
    spawn_bt_transport_reconciler_with,
};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use connection::test_helpers as bluetooth_test_helpers;

#[doc(inline)]
pub use transport::{
    BT_NO_CONNECTION_TITLE, BlueAlsaPcm, BtPlaybackState, TransportCommand, TransportCommandQueue,
    decode_pcm_format, format_duration_ms, format_volume_bytes, playback_progress_ratio,
    seek_offset_us,
};

#[doc(inline)]
pub use cover_art::{
    COVER_ART_CACHE_DIR, clean_cover_art_in_dir, clean_old_cover_art, clean_old_cover_art_in,
    cover_art_cached_path, cover_art_cached_path_in, cover_art_path_for, cover_art_path_for_in,
    ensure_cover_art_cache_dir_in, is_complete_jpeg, validate_bip_handle,
};

#[doc(inline)]
pub use cover_service::{
    COVER_ART_DBUS_NAME, COVER_ART_INTERFACE, COVER_ART_OBJECT_PATH, CoverArtSnapshot,
    CoverSnapshot, query_snapshot, query_snapshot_state,
};
