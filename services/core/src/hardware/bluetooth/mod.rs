//! Re-exports the internal submodules of the Bluetooth hardware driver.
//!
//! The original monolithic `hardware::bluetooth` module was split into
//! logical submodules: [`connection`] (D-Bus/device state, address helpers),
//! [`transport`] (playback state and transport commands), and [`cover_art`]
//! (BIP cover-art discovery and caching), and [`cover_service`] (the read
//! side of the D-Bus cover-art publisher hosted by `bt-manager`).

/// Not yet wired into the connection driver; consumed via integration in a
/// follow-up change.
#[allow(
    dead_code,
    reason = "wired into the connection driver in a follow-up change"
)]
pub(crate) mod bip_direct;
pub(crate) mod connection;
pub(crate) mod cover_art;
pub mod cover_service;
pub(crate) mod transport;

#[doc(inline)]
pub use connection::{
    BCM43438_FIRMWARE_PATH, BCM43438_UART_BAUD_RATE, BT_ADDRESS_OCTETS, BluetoothControllerInfo,
    BluetoothDevice, BluetoothDeviceMock, BluetoothHardwareInfo, BluetoothServices,
    BluetoothStatus, BluezSnapshot, BtAdapter, BtAddress, BtDevice, BtError, DEFAULT_BT_ADAPTER,
    DbusClient, ServiceState, is_no_such_unit_error, resolve_cover_art_with,
};
#[doc(inline)]
pub use cover_art::{
    COVER_ART_CACHE_DIR, clean_cover_art_in_dir, clean_old_cover_art, clean_old_cover_art_in,
    cover_art_cached_path, cover_art_cached_path_in, cover_art_path_for, cover_art_path_for_in,
    ensure_cover_art_cache_dir_in, is_complete_jpeg, validate_bip_handle,
};
#[doc(inline)]
pub use transport::{
    BlueAlsaPcm, BtPlaybackState, TransportCommand, TransportCommandQueue, decode_pcm_format,
    format_duration_ms, playback_progress_ratio, seek_offset_us,
};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use connection::test_helpers;
