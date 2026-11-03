//! Re-exports the internal submodules of the Bluetooth hardware driver.
//!
//! The original monolithic `hardware::bluetooth` module was split into
//! logical submodules: [`connection`] (D-Bus/device state, address helpers),
//! [`transport`] (playback state and transport commands), and [`cover_art`]
//! (BIP cover-art discovery and caching).

pub(crate) mod connection;
pub(crate) mod cover_art;
pub(crate) mod transport;

#[doc(inline)]
pub use connection::{
    BCM43438_FIRMWARE_PATH, BCM43438_UART_BAUD_RATE, BT_ADDRESS_OCTETS, BluetoothControllerInfo,
    BluetoothDevice, BluetoothDeviceMock, BluetoothHardwareInfo, BluetoothServices,
    BluetoothStatus, BluezSnapshot, BtAdapter, BtAddress, BtDevice, BtError, DEFAULT_BT_ADAPTER,
    DbusClient, ServiceState, is_no_such_unit_error,
};
#[doc(inline)]
pub use cover_art::{
    COVER_ART_CACHE_DIR, clean_cover_art_in_dir, clean_old_cover_art, clean_old_cover_art_in,
    cover_art_cached_path, cover_art_cached_path_in, cover_art_path_for, cover_art_path_for_in,
    is_complete_jpeg, load_native_caps_in, native_image_size, store_native_cap_in,
    validate_bip_handle,
};
#[doc(inline)]
pub use transport::{
    BlueAlsaPcm, BtPlaybackState, TransportCommand, TransportCommandQueue, decode_pcm_format,
    format_duration_ms, playback_progress_ratio,
};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use connection::test_helpers;
