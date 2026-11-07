//! Bluetooth & `BlueALSA` peripheral drivers, D-Bus connection management,
//! AVRCP transport, and Cover Art client contract.

pub mod connection;
pub mod cover_art;
pub mod cover_service;
pub mod transport;

#[doc(inline)]
pub use connection::{
    BCM43438_FIRMWARE_PATH, BCM43438_UART_BAUD_RATE, BT_ADDRESS_OCTETS, BluetoothControllerInfo,
    BluetoothDevice, BluetoothDeviceMock, BluetoothHardwareInfo, BluetoothServices,
    BluetoothStatus, BluezSnapshot, BtAdapter, BtAddress, BtDevice, BtError, DEFAULT_BT_ADAPTER,
    resolve_cover_art_with,
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
