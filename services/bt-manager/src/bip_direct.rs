//! Direct OBEX cover-art retrieval over Bluetooth RFCOMM (an `obexd` replacement).
//!
//! Performs BIP (Bluetooth Imaging Profile) GET transactions against the
//! AVRCP 1.6 cover-art OBEX target directly over an RFCOMM socket:
//!
//! 1. the RFCOMM server channel of the remote BIP Image Pull service is
//!    resolved through a pure-Rust SDP query over L2CAP ([`bluesdp`]),
//! 2. a `BlueZ` RFCOMM stream is opened ([`bluer`]),
//! 3. an OBEX CONNECT is issued with the AVRCP cover-art Target UUID,
//! 4. the image is pulled with an OBEX GET carrying the `Type` and BIP
//!    `Img-Handle` (0x30) headers and NO `Img-Descriptor` (the AOSP native
//!    path — obexd-style empty descriptors are rejected with 0x4C),
//! 5. the OBEX session is closed with a best-effort DISCONNECT.
//!
//! The profile-independent OBEX core ([`pull_image`]) is generic over the
//! transport and therefore fully host-testable against an in-process OBEX
//! server; only [`RfcommBipChannel`] touches physical Bluetooth hardware.

use std::backtrace::Backtrace;
use std::fmt::{self, Formatter};
use std::os::unix::io::AsRawFd;
use std::time::Instant;

use obex_protocol::Error as ObexError;
use obex_protocol::header::{Header, HeaderValue};
use obex_protocol::response::ResponseCode;
use obex_protocol::tokio::Client;
use tokio::io::{AsyncRead, AsyncWrite};

use speaker_bluetooth::BtAddress;

/// AVRCP 1.6 cover-art OBEX Target UUID
/// (`7163DD54-4A7E-11E2-B47C-0050C2490048`, big-endian bytes).
pub const AVRCP_COVER_ART_UUID: [u8; 16] = [
    0x71, 0x63, 0xDD, 0x54, 0x4A, 0x7E, 0x11, 0xE2, 0xB4, 0x7C, 0x00, 0x50, 0xC2, 0x49, 0x00, 0x48,
];

/// Bluetooth SIG assigned number of the BIP Image Pull service (UUID16 `0x111B`).
pub const BIP_IMAGE_PULL_UUID16: u16 = 0x111B;

// GETs omit the `Img-Descriptor` header entirely. `BlueZ` obexd always
// attaches a descriptor with an empty `<image>` element, but AOSP
// (`BipPixel.determinePixelType`) throws `ParseException` on the empty
// `pixel=""` attribute, which its responder surfaces as Precondition Failed
// (0x4C). A missing descriptor selects the responder's native-image path
// (`AvrcpBipObexServer.handleGetImage`: `descriptor == null`), which is the
// only universally accepted native pull.

/// OBEX header identifier of the BIP `Img-Handle` header (`obexd/client/bip.c`
/// `IMG_HANDLE_TAG`); the AOSP responder reads the image handle exclusively
/// from this header, not from the generic OBEX Name header.
pub const IMG_HANDLE_HEADER_ID: u8 = 0x30;

/// OBEX header identifier of the BIP `Img-Descriptor` header (0x71).
pub const IMG_DESCRIPTOR_HEADER_ID: u8 = 0x71;

/// OBEX MIME type for querying image properties XML.
pub const MIME_IMAGE_PROPERTIES: &str = "x-bt/img-properties";

/// Target minimum dimension for screen cover art display (matching 360x360 UI container).
pub const TARGET_COVER_MIN_DIMENSION: u32 = 360;

/// Maximum dimension to request, preventing excessive Bluetooth transfer latency and memory use.
pub const MAX_COVER_DIMENSION: u32 = 1280;

/// Largest accepted image-handle string (e.g. `0000001`).
pub const IMG_HANDLE_MAX_LEN: usize = 64;

const _: () = {
    assert!(AVRCP_COVER_ART_UUID.len() == 16);
};

/// The kind of BIP image representation to request from the responder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BipImageType {
    /// Full-resolution native image (`x-bt/img-img`).
    Native,
    /// Small thumbnail variant (`x-bt/img-thm`).
    Thumbnail,
}

impl BipImageType {
    /// The OBEX `Type` (MIME) header value for this image representation.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Native => "x-bt/img-img",
            Self::Thumbnail => "x-bt/img-thm",
        }
    }
}

/// Subsystem error type for direct BIP cover-art retrieval.
///
/// The optional `source` field preserves the typed underlying error (e.g. the
/// original [`bluesdp::SdpError`], [`std::io::Error`] or [`ObexError`]) so the
/// cause chain survives `?` conversions, per the M-ERRORS-STD-SOURCE guideline.
#[derive(Debug)]
pub struct BipError {
    kind: BipErrorKind,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    backtrace: Backtrace,
}

#[derive(Debug)]
enum BipErrorKind {
    SdpLookup {
        details: Box<str>,
    },
    ServiceNotFound {
        address: Box<str>,
    },
    RfcommConnect {
        details: Box<str>,
    },
    L2capConnect {
        details: Box<str>,
    },
    InvalidHandle {
        details: Box<str>,
    },
    Obex {
        details: Box<str>,
    },
    ImageRejected {
        operation: &'static str,
        response_code: u8,
    },
}

impl BipError {
    #[must_use]
    fn new(kind: BipErrorKind) -> Self {
        Self {
            kind,
            source: None,
            backtrace: Backtrace::capture(),
        }
    }

    /// Attaches an underlying error as this error's [`std::error::Error::source`].
    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    #[must_use]
    pub fn sdp_lookup(source: bluesdp::SdpError) -> Self {
        Self::new(BipErrorKind::SdpLookup {
            details: source.to_string().into(),
        })
        .with_source(source)
    }

    #[must_use]
    pub fn service_not_found(address: impl Into<Box<str>>) -> Self {
        Self::new(BipErrorKind::ServiceNotFound {
            address: address.into(),
        })
    }

    #[must_use]
    pub fn rfcomm_connect(source: std::io::Error) -> Self {
        Self::new(BipErrorKind::RfcommConnect {
            details: source.to_string().into(),
        })
        .with_source(source)
    }

    #[must_use]
    pub fn l2cap_connect(source: std::io::Error) -> Self {
        Self::new(BipErrorKind::L2capConnect {
            details: source.to_string().into(),
        })
        .with_source(source)
    }

    #[must_use]
    pub fn invalid_handle(details: impl Into<Box<str>>) -> Self {
        Self::new(BipErrorKind::InvalidHandle {
            details: details.into(),
        })
    }

    #[must_use]
    pub fn obex(source: ObexError) -> Self {
        Self::new(BipErrorKind::Obex {
            details: source.to_string().into(),
        })
        .with_source(source)
    }

    #[must_use]
    pub fn image_rejected(operation: &'static str, response_code: ResponseCode) -> Self {
        Self::new(BipErrorKind::ImageRejected {
            operation,
            response_code: response_code as u8,
        })
    }

    #[must_use]
    pub fn image_rejected_raw(operation: &'static str, response_code: u8) -> Self {
        Self::new(BipErrorKind::ImageRejected {
            operation,
            response_code,
        })
    }

    /// Returns `true` if the SDP service lookup failed.
    #[must_use]
    pub const fn is_sdp_lookup(&self) -> bool {
        matches!(self.kind, BipErrorKind::SdpLookup { .. })
    }

    /// Returns `true` if the remote device publishes no BIP Image Pull service.
    #[must_use]
    pub const fn is_service_not_found(&self) -> bool {
        matches!(self.kind, BipErrorKind::ServiceNotFound { .. })
    }

    /// Returns `true` if the RFCOMM connection could not be established.
    #[must_use]
    pub const fn is_rfcomm_connect(&self) -> bool {
        matches!(self.kind, BipErrorKind::RfcommConnect { .. })
    }

    /// Returns `true` if the L2CAP connection could not be established.
    #[must_use]
    pub const fn is_l2cap_connect(&self) -> bool {
        matches!(self.kind, BipErrorKind::L2capConnect { .. })
    }

    /// Returns `true` if the requested image handle failed validation.
    #[must_use]
    pub const fn is_invalid_handle(&self) -> bool {
        matches!(self.kind, BipErrorKind::InvalidHandle { .. })
    }

    /// Returns `true` if the OBEX exchange itself failed (transport or protocol).
    #[must_use]
    pub const fn is_obex(&self) -> bool {
        matches!(self.kind, BipErrorKind::Obex { .. })
    }

    /// Returns `true` if the responder answered an operation with a failure response code.
    #[must_use]
    pub const fn is_image_rejected(&self) -> bool {
        matches!(self.kind, BipErrorKind::ImageRejected { .. })
    }

    /// Returns the OBEX response code the responder answered with, when the
    /// failure was an explicit rejection.
    #[must_use]
    pub const fn response_code(&self) -> Option<u8> {
        match self.kind {
            BipErrorKind::ImageRejected { response_code, .. } => Some(response_code),
            _ => None,
        }
    }

    /// Returns `true` if this error represents an underlying transport failure (broken connection,
    /// timeout, EOF) rather than an application-level rejection or parse issue.
    #[must_use]
    pub fn is_transport_fault(&self) -> bool {
        if let Some(obex_err) = self
            .source
            .as_ref()
            .and_then(|s| s.downcast_ref::<ObexError>())
        {
            return obex_err.response_code().is_none();
        }
        matches!(
            self.kind,
            BipErrorKind::RfcommConnect { .. }
                | BipErrorKind::L2capConnect { .. }
                | BipErrorKind::SdpLookup { .. }
        )
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BipErrorKind::SdpLookup { details } => {
                write!(f, "SDP service lookup failed: {details}")
            }
            BipErrorKind::ServiceNotFound { address } => {
                write!(f, "no BIP Image Pull service record on device {address}")
            }
            BipErrorKind::RfcommConnect { details } => {
                write!(f, "RFCOMM connect failed: {details}")
            }
            BipErrorKind::L2capConnect { details } => {
                write!(f, "L2CAP connect failed: {details}")
            }
            BipErrorKind::InvalidHandle { details } => {
                write!(f, "invalid image handle: {details}")
            }
            BipErrorKind::Obex { details } => {
                write!(f, "OBEX protocol error: {details}")
            }
            BipErrorKind::ImageRejected {
                operation,
                response_code,
            } => {
                write!(
                    f,
                    "{operation} rejected with response code 0x{response_code:02X}"
                )
            }
        }
    }
}

speaker_common::impl_canonical_error_display!(BipError);

impl std::error::Error for BipError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|boxed| {
            let erased: &(dyn std::error::Error + 'static) = &**boxed;
            erased
        })
    }
}

impl From<bluesdp::SdpError> for BipError {
    fn from(source: bluesdp::SdpError) -> Self {
        Self::sdp_lookup(source)
    }
}

/// An opened raw OBEX transport to a remote BIP Image Pull service.
pub trait BipIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> BipIo for T {}

pub type BipStream = Box<dyn BipIo>;

/// Opens a raw OBEX transport to a remote device's BIP Image Pull service.
///
/// Implementations resolve the BIP RFCOMM channel themselves (SDP in the
/// production backend) and return a connected stream ready for
/// [`pull_image`].
#[async_trait::async_trait]
pub trait BipChannel: Send + Sync {
    /// Resolves the BIP Image Pull service channel and connects to it.
    ///
    /// # Errors
    /// Returns [`BipError`] when SDP lookup, channel resolution or the
    /// transport connection fails.
    #[expect(
        clippy::double_must_use,
        reason = "async_trait adds #[must_use] to the boxed-future signature while the Result output is already must_use"
    )]
    async fn open(&self, address: &BtAddress) -> Result<BipStream, BipError>;
}

/// Production [`BipChannel`]: pure-Rust SDP lookup plus a `BlueZ` RFCOMM socket.
///
/// [`RfcommBipChannel::with_known_channel`] skips the SDP lookup when the
/// caller already knows the channel number (e.g. `BlueZ` published an RFCOMM
/// channel in `MediaPlayer1.ObexPort`).
/// Physical-transport seam behind the [`BipChannel`] implementations.
///
/// Production always uses [`BluerSocketCore`] (real bluer sockets and real
/// SDP); host tests inject a scripted core so the channel assembly logic —
/// SDP-vs-known-channel selection, address construction, error mapping — is
/// exercised without any Bluetooth hardware.
mod bip_socket_core {
    use super::{BipError, BipStream, BtAddress};

    #[async_trait::async_trait]
    pub trait BipSocketCore: Send + Sync {
        /// SDP lookup of the BIP Image Pull RFCOMM channel (`None` when the
        /// device publishes no such service record).
        #[expect(
            clippy::double_must_use,
            reason = "async_trait adds #[must_use] to the boxed-future signature while the Result output is already must_use"
        )]
        async fn sdp_bip_rfcomm_channel(&self, address: &str) -> Result<Option<u8>, BipError>;

        /// Connects an RFCOMM stream to the given server channel.
        #[expect(
            clippy::double_must_use,
            reason = "async_trait adds #[must_use] to the boxed-future signature while the Result output is already must_use"
        )]
        async fn connect_rfcomm(
            &self,
            address: &BtAddress,
            channel: u8,
        ) -> Result<BipStream, BipError>;

        /// Connects an L2CAP (ERTM) stream to the given PSM.
        #[expect(
            clippy::double_must_use,
            reason = "async_trait adds #[must_use] to the boxed-future signature while the Result output is already must_use"
        )]
        async fn connect_l2cap(&self, address: &BtAddress, psm: u16)
        -> Result<BipStream, BipError>;
    }
}

// Internal transport seam: `pub(crate)` in production builds; `pub` only when
// `test-util` is enabled so the moved `tests/bip_direct_integration.rs` suite
// can inject scripted cores without widening the public API surface.
#[cfg(not(any(test, feature = "test-util")))]
pub(crate) use bip_socket_core::BipSocketCore;
#[cfg(any(test, feature = "test-util"))]
pub use bip_socket_core::BipSocketCore;

/// Production [`BipSocketCore`]: real bluer sockets and real SDP queries.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BluerSocketCore;

#[async_trait::async_trait]
impl BipSocketCore for BluerSocketCore {
    async fn sdp_bip_rfcomm_channel(&self, address: &str) -> Result<Option<u8>, BipError> {
        bluesdp::query_rfcomm_channel(address, bluesdp::Uuid16(BIP_IMAGE_PULL_UUID16))
            .await
            .map_err(BipError::sdp_lookup)
    }

    async fn connect_rfcomm(
        &self,
        address: &BtAddress,
        channel: u8,
    ) -> Result<BipStream, BipError> {
        let octets = *address.as_bytes();
        let socket_addr = bip_socket_address(octets, channel);
        tracing::debug!(socket = %socket_addr, "opening RFCOMM connection");
        let stream = bluer::rfcomm::Stream::connect(socket_addr)
            .await
            .map_err(BipError::rfcomm_connect)?;
        tracing::debug!(socket = %socket_addr, "RFCOMM connection established");
        Ok(Box::new(stream) as BipStream)
    }

    async fn connect_l2cap(&self, address: &BtAddress, psm: u16) -> Result<BipStream, BipError> {
        let canonical = address.to_string();
        let octets = *address.as_bytes();
        let started = Instant::now();
        tracing::debug!(address = %canonical, psm, "opening L2CAP (ERTM) connection");
        let socket = open_configured_l2cap_socket()?;
        let socket_addr =
            bluer::l2cap::SocketAddr::new(bluer::Address(octets), bluer::AddressType::BrEdr, psm);
        let stream = socket
            .connect(socket_addr)
            .await
            .map_err(l2cap_step_err("connect"))?;
        tracing::debug!(
            address = %canonical,
            psm,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "L2CAP connection established"
        );
        Ok(Box::new(stream) as BipStream)
    }
}

fn open_configured_l2cap_socket() -> Result<bluer::l2cap::Socket<bluer::l2cap::Stream>, BipError> {
    let socket = bluer::l2cap::Socket::<bluer::l2cap::Stream>::new_stream()
        .map_err(l2cap_step_err("create socket"))?;
    socket
        .bind(bluer::l2cap::SocketAddr::any_br_edr())
        .map_err(l2cap_step_err("bind local address"))?;
    // SOCK_STREAM Classic channels default to ERTM (l2cap_sock.c
    // `l2cap_sock_init`); `L2CAP_OPTIONS` is the correct knob for Classic
    // BR/EDR channels and must be set before `connect()` rejects it. It
    // advertises the large receive MTU the phone needs for few-packet
    // image transfers; the BT_MODE sockopt (`set_flow_control`) rejects
    // ERTM with EINVAL and cannot set an MTU at all.
    request_l2cap_receive_mtu(&socket).map_err(l2cap_step_err("configure L2CAP receive MTU"))?;
    Ok(socket)
}

pub struct RfcommBipChannel {
    known_channel: Option<u8>,
    /// Physical-transport core (real bluer sockets in production).
    core: std::sync::Arc<dyn BipSocketCore>,
}

impl Default for RfcommBipChannel {
    fn default() -> Self {
        Self {
            known_channel: None,
            core: std::sync::Arc::new(BluerSocketCore),
        }
    }
}

impl std::fmt::Debug for RfcommBipChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RfcommBipChannel")
            .field("known_channel", &self.known_channel)
            .finish_non_exhaustive()
    }
}

impl RfcommBipChannel {
    /// Returns the statically configured RFCOMM channel, if any.
    #[must_use]
    pub const fn known_channel(&self) -> Option<u8> {
        self.known_channel
    }
    /// Creates a channel that skips the SDP lookup and connects directly to
    /// the given RFCOMM channel number.
    ///
    /// Not `const`: the default transport core is a heap-allocated `Arc`.
    #[must_use]
    pub fn with_known_channel(channel: u8) -> Self {
        Self {
            known_channel: Some(channel),
            core: std::sync::Arc::new(BluerSocketCore),
        }
    }

    /// Creates a channel over an injected transport core (host tests).
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_core(known_channel: Option<u8>, core: std::sync::Arc<dyn BipSocketCore>) -> Self {
        Self {
            known_channel,
            core,
        }
    }
}

#[async_trait::async_trait]
impl BipChannel for RfcommBipChannel {
    async fn open(&self, address: &BtAddress) -> Result<BipStream, BipError> {
        let canonical = address.to_string();
        let started = Instant::now();
        let channel = if let Some(channel) = self.known_channel {
            tracing::debug!(
                address = %canonical,
                channel,
                "using known BIP RFCOMM channel (BlueZ ObexPort)"
            );
            channel
        } else {
            tracing::debug!(address = %canonical, "SDP: querying BIP Image Pull service record");
            let sdp_result = self.core.sdp_bip_rfcomm_channel(&canonical).await?;
            resolve_sdp_channel(&canonical, started.elapsed().as_millis() as u64, sdp_result)?
        };
        self.core.connect_rfcomm(address, channel).await
    }
}

/// Production [`BipChannel`] for devices that serve cover art over L2CAP.
///
/// Android 8+ Pixels advertise an L2CAP PSM (e.g. 4097) in
/// `org.bluez.MediaPlayer1.ObexPort` instead of an RFCOMM channel; this
/// backend connects with Enhanced Retransmission Mode, which the AVRCP
/// cover-art profile requires for flow-controlled L2CAP transports.
pub struct L2capBipChannel {
    /// L2CAP Protocol/Service Multiplexor published by the remote device.
    psm: u16,
    /// Physical-transport core (real bluer sockets in production).
    core: std::sync::Arc<dyn BipSocketCore>,
}

impl Default for L2capBipChannel {
    fn default() -> Self {
        Self {
            psm: 0,
            core: std::sync::Arc::new(BluerSocketCore),
        }
    }
}

impl std::fmt::Debug for L2capBipChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L2capBipChannel")
            .field("psm", &self.psm)
            .finish_non_exhaustive()
    }
}

impl L2capBipChannel {
    /// Returns the L2CAP PSM.
    #[must_use]
    pub const fn psm(&self) -> u16 {
        self.psm
    }
    /// Creates a channel targeting a specific L2CAP PSM.
    #[must_use]
    pub fn new(psm: u16) -> Self {
        Self {
            psm,
            core: std::sync::Arc::new(BluerSocketCore),
        }
    }

    /// Creates a channel over an injected transport core (host tests).
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_core(psm: u16, core: std::sync::Arc<dyn BipSocketCore>) -> Self {
        Self { psm, core }
    }
}

fn l2cap_step_err(step: &'static str) -> impl Fn(std::io::Error) -> BipError {
    move |e| {
        tracing::debug!(step, error = %e, "L2CAP socket operation failed");
        BipError::l2cap_connect(e)
    }
}

/// Kernel `SOL_L2CAP` socket level (include/net/bluetooth/l2cap.h).
const SOL_L2CAP: libc::c_int = 6;
/// Kernel `L2CAP_OPTIONS` socket option number.
const L2CAP_OPTIONS_OPT: libc::c_int = 0x01;
/// Kernel `L2CAP_MODE_ERTM`.
const L2CAP_MODE_ERTM: u8 = 0x03;
/// Classic BR/EDR receive MTU advertised to the phone (0x7FFF).
///
/// AOSP clamps its outbound OBEX packet size to the peer's advertised L2CAP
/// receive MTU; without configuration the kernel negotiates the 672-byte
/// default (`L2CAP_DEFAULT_MTU`) and fragments every image into dozens of
/// stop-and-wait OBEX packets. 32767 (not 65535) also stays clear of signed
/// 16-bit length handling in Android's javax.obex stack.
pub const L2CAP_AOSP_RECEIVE_MTU: u16 = 32767;

/// Kernel `struct l2cap_options` layout (include/net/bluetooth/l2cap.h):
/// three `u16` fields, three `u8` fields, then a `u16` with one byte of
/// trailing padding.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct L2capOptions {
    omtu: u16,
    imtu: u16,
    flush_to: u16,
    mode: u8,
    fcs: u8,
    max_tx: u8,
    txwin_size: u16,
}
const _: () = {
    assert!(std::mem::size_of::<L2capOptions>() == 12);
    // 48 is the kernel `L2CAP_DEFAULT_MIN_MTU` floor for `L2CAP_OPTIONS`.
    assert!(L2CAP_AOSP_RECEIVE_MTU >= 48);
};

/// Advertises a 32767-byte receive MTU on a Classic BR/EDR L2CAP socket.
///
/// Must run after `bind()` and before `connect()`: the kernel rejects
/// `L2CAP_OPTIONS` once the channel is connected (`l2cap_sock.c`). All other
/// fields are preserved from the kernel defaults (ERTM mode and a 63-frame
/// transmit window are already the defaults for `SOCK_STREAM` channels), so
/// only the receive MTU is modified.
///
/// # Errors
/// Propagates the raw `getsockopt`/`setsockopt` failures as `io::Error`.
fn request_l2cap_receive_mtu<F: AsRawFd>(socket: &F) -> std::io::Result<()> {
    let fd = socket.as_raw_fd();
    let mut opts = L2capOptions::default();
    let mut optlen = std::mem::size_of::<L2capOptions>() as libc::socklen_t;
    // SAFETY: `opts` is a valid `struct l2cap_options` out-parameter with
    // `optlen` reporting its length, per the getsockopt(2) contract.
    if unsafe {
        libc::getsockopt(
            fd,
            SOL_L2CAP,
            L2CAP_OPTIONS_OPT,
            std::ptr::from_mut(&mut opts).cast(),
            std::ptr::from_mut(&mut optlen),
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    opts.imtu = L2CAP_AOSP_RECEIVE_MTU;
    opts.mode = L2CAP_MODE_ERTM;
    // SAFETY: `opts` is a valid `struct l2cap_options` in-parameter of the
    // exact size the kernel expects for `L2CAP_OPTIONS`.
    if unsafe {
        libc::setsockopt(
            fd,
            SOL_L2CAP,
            L2CAP_OPTIONS_OPT,
            std::ptr::from_ref(&opts).cast(),
            std::mem::size_of::<L2capOptions>() as libc::socklen_t,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    tracing::debug!(
        imtu = u32::from(L2CAP_AOSP_RECEIVE_MTU),
        mode_ertm = true,
        "L2CAP receive MTU configured"
    );
    Ok(())
}

/// OBEX `MaxPacketLength` advertised in CONNECT (0x7FFF).
///
/// Matches the configured L2CAP receive MTU and stays clear of signed 16-bit
/// length handling in Android's javax.obex stack.
pub const OBEX_AOSP_MAX_PACKET_LENGTH: u16 = 32767;
const _: () = {
    assert!(OBEX_AOSP_MAX_PACKET_LENGTH >= obex_protocol::packet::MINIMUM_PACKET_LENGTH);
};

/// Deadline for the OBEX CONNECT handshake. obexd's pipeline used an
/// explicit watchdog; without one a phone that accepts the socket and then
/// stops responding would wedge the provider worker forever (the ACL link
/// stays alive while the A2DP stream plays, so the link layer never fires).
///
/// The three OBEX deadlines are unconditional production values: host tests
/// inject their own short deadlines directly into `with_obex_timeout`, so no
/// build (including `--features test-util`) ever sees shrunk production
/// behavior.
const OBEX_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Deadline for a complete OBEX GET (CONNECT-for-GET through final body).
const OBEX_GET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Deadline for an OBEX GET image properties round-trip.
const OBEX_PROPERTIES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Deadline for the keepalive SETPATH round-trip.
const OBEX_KEEPALIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bounds an OBEX request/response cycle with an application-level deadline.
///
/// On expiry the peer is treated as a transport fault: the mapped
/// [`ObexError::Io`] with `ErrorKind::TimedOut` releases the held session so
/// the next `ensure` re-establishes from scratch.
///
/// Maps a deadline expiry into an OBEX transport fault.
fn obex_timeout(operation: &'static str, deadline: std::time::Duration) -> ObexError {
    tracing::warn!(
        operation,
        timeout_ms = deadline.as_millis() as u64,
        "OBEX operation timed out"
    );
    ObexError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("{operation} timed out after {deadline:?}"),
    ))
}

/// Wraps an OBEX future with a timeout deadline.
///
/// # Errors
///
/// Returns [`ObexError::Io`] with [`std::io::ErrorKind::TimedOut`] if the operation
/// does not complete within `deadline`, or passes through the underlying error from `fut`.
pub async fn with_obex_timeout<F, T>(
    operation: &'static str,
    deadline: std::time::Duration,
    fut: F,
) -> Result<T, ObexError>
where
    F: std::future::Future<Output = Result<T, ObexError>>,
{
    match tokio::time::timeout(deadline, fut).await {
        Ok(result) => result,
        Err(_) => Err(obex_timeout(operation, deadline)),
    }
}

/// Builds an OBEX client tuned for the phone's AVRCP cover-art server.
///
/// Enables Single Response Mode (GOEP 2.0) so multi-packet GET responses
/// stream back-to-back instead of stopping after every 0x90 Continue frame
/// to wait for a client continuation round-trip; the obex-protocol engine
/// falls back automatically when the responder does not latch SRM.
#[must_use]
pub fn cover_art_client<T: AsyncRead + AsyncWrite + Unpin>(stream: T) -> Client<T> {
    Client::new(stream)
        .with_max_packet_length(OBEX_AOSP_MAX_PACKET_LENGTH)
        .with_single_response_mode(true)
}

#[async_trait::async_trait]
impl BipChannel for L2capBipChannel {
    async fn open(&self, address: &BtAddress) -> Result<BipStream, BipError> {
        self.core.connect_l2cap(address, self.psm).await
    }
}

/// Runs a complete BIP GET transaction over an already connected `stream`.
///
/// Sends an OBEX CONNECT targeting the AVRCP 1.6 cover-art service, then pulls
/// the requested image with a GET carrying `Type` and `Img-Handle` headers
/// (no OBEX DISCONNECT — see [`get_image_on`]).
///
/// Dropping the `Client` at the end of the call closes the transport.
///
/// # Errors
/// Returns [`BipError`] for invalid handles, OBEX transport/protocol failures
/// and responder rejections (see [`BipError::response_code`]).
pub async fn pull_image<T>(
    stream: T,
    image_type: BipImageType,
    img_handle: &str,
) -> Result<Vec<u8>, BipError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    validate_img_handle(img_handle)?;

    let started = Instant::now();

    let mut client = cover_art_client(stream);
    client
        .connect([Header::Target(AVRCP_COVER_ART_UUID.to_vec())])
        .await
        .map_err(|e| map_obex_failure("OBEX CONNECT", e))?;
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "OBEX CONNECT accepted"
    );

    get_image_on(&mut client, image_type, img_handle).await
}

/// Pixel dimensions for a BIP image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BipPixel {
    /// Fixed pixel dimensions `(width, height)`.
    Fixed(u32, u32),
    /// Pixel range with minimum and maximum bounds `(min_w, min_h, max_w, max_h)`.
    Range {
        min_w: u32,
        min_h: u32,
        max_w: u32,
        max_h: u32,
    },
}

impl BipPixel {
    /// Parses a BIP pixel string like `"640*480"`, `"200*200-800*800"`, or `"200**-800*800"`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some((min_str, max_str)) = s.split_once('-') {
            let (max_w, max_h) = parse_wh(max_str)?;
            if max_w == 0 || max_h == 0 {
                return None;
            }
            let (min_w, min_h) = if let Some(min_w_str) = min_str.trim().strip_suffix("**") {
                let w = min_w_str.trim().parse::<u32>().ok()?;
                if w == 0 {
                    return None;
                }
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "proportional height calculation fits within u32"
                )]
                let h = ((u64::from(w) * u64::from(max_h)) / u64::from(max_w)) as u32;
                (w, h.max(1))
            } else {
                parse_wh(min_str)?
            };
            Some(Self::Range {
                min_w,
                min_h,
                max_w,
                max_h,
            })
        } else {
            let (w, h) = parse_wh(s)?;
            Some(Self::Fixed(w, h))
        }
    }

    /// Returns the maximum dimensions `(width, height)` represented by this pixel specification.
    #[must_use]
    pub const fn max_dimensions(&self) -> (u32, u32) {
        match *self {
            Self::Fixed(w, h) => (w, h),
            Self::Range { max_w, max_h, .. } => (max_w, max_h),
        }
    }
}

fn parse_wh(s: &str) -> Option<(u32, u32)> {
    let (w_str, h_str) = s.trim().split_once('*')?;
    let w = w_str.trim().parse::<u32>().ok()?;
    let h = h_str.trim().parse::<u32>().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// One format entry in a BIP `image-properties` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BipImageFormat {
    /// Encoding format name (e.g. `"JPEG"`, `"PNG"`).
    pub encoding: Box<str>,
    /// Pixel dimensions.
    pub pixel: BipPixel,
    /// Optional file size in bytes, if advertised by responder.
    pub size: Option<u32>,
    /// Whether this is the native image format (`true`) or a variant (`false`).
    pub is_native: bool,
}

/// Parsed BIP `image-properties` document describing available artwork formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BipImageProperties {
    /// Image handle the properties describe.
    pub handle: Box<str>,
    /// Formats offered by the responder.
    pub formats: Vec<BipImageFormat>,
}

fn find_attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let mut search = tag;
    while let Some(idx) = search.find(attr) {
        let is_boundary = idx == 0
            || search.as_bytes()[idx - 1].is_ascii_whitespace()
            || matches!(search.as_bytes()[idx - 1], b'<' | b'?');
        if is_boundary {
            let after = &search[idx + attr.len()..];
            let trimmed = after.trim_start();
            if let Some(rest) = trimmed.strip_prefix('=') {
                let val_trimmed = rest.trim_start();
                if let Some(quote) = val_trimmed.chars().next()
                    && (quote == '"' || quote == '\'')
                {
                    let content = &val_trimmed[quote.len_utf8()..];
                    if let Some(end_idx) = content.find(quote) {
                        return Some(&content[..end_idx]);
                    }
                }
            }
        }
        search = &search[idx + attr.len()..];
    }
    None
}

impl BipImageProperties {
    /// Parses a BIP `<image-properties ...>` XML string.
    #[must_use]
    pub fn parse(xml: &str) -> Option<Self> {
        let mut handle = None;
        let mut formats = Vec::new();

        let mut pos = 0;
        while let Some(start) = xml[pos..].find('<') {
            let abs_start = pos + start;
            let Some(end) = xml[abs_start..].find('>') else {
                break;
            };
            let abs_end = abs_start + end;
            let tag = &xml[abs_start..=abs_end];
            pos = abs_end + 1;

            let tag_trimmed = tag.trim_start_matches('<').trim_start();
            if tag_trimmed.starts_with("image-properties") {
                if let Some(h) = find_attr(tag, "handle") {
                    handle = Some(Box::<str>::from(h));
                }
            } else if tag_trimmed.starts_with("native") || tag_trimmed.starts_with("variant") {
                let is_native = tag_trimmed.starts_with("native");
                let (Some(encoding), Some(pixel_str)) =
                    (find_attr(tag, "encoding"), find_attr(tag, "pixel"))
                else {
                    continue;
                };
                let Some(pixel) = BipPixel::parse(pixel_str) else {
                    continue;
                };
                let size = find_attr(tag, "size").and_then(|s| s.parse::<u32>().ok());
                formats.push(BipImageFormat {
                    encoding: Box::<str>::from(encoding),
                    pixel,
                    size,
                    is_native,
                });
            }
        }

        let handle = handle?;
        Some(Self { handle, formats })
    }
}

/// Negotiated format selection for cover art download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatSelection {
    /// Request native image with NO `Img-Descriptor` header.
    NativeDefault,
    /// Request with `Img-Descriptor` specifying pixel dimensions `(width, height)`.
    Descriptor { width: u32, height: u32 },
}

/// Evaluates available formats from `properties` and selects the best resolution.
///
/// If a higher resolution variant is offered (exceeding native and suitable for display),
/// returns [`FormatSelection::Descriptor`]. If the native format is already high-resolution
/// or no better format is offered, returns [`FormatSelection::NativeDefault`].
#[must_use]
pub fn select_best_format(properties: &BipImageProperties, target_min_dim: u32) -> FormatSelection {
    let mut native_dim = (0u32, 0u32);
    for f in &properties.formats {
        if f.is_native {
            let (w, h) = f.pixel.max_dimensions();
            if w > native_dim.0 {
                native_dim = (w, h);
            }
        }
    }

    if native_dim.0 >= target_min_dim && native_dim.1 >= target_min_dim {
        return FormatSelection::NativeDefault;
    }

    let mut best_variant: Option<(u32, u32)> = None;
    for f in &properties.formats {
        if !f.is_native && f.encoding.eq_ignore_ascii_case("JPEG") {
            let (target_w, target_h) = match f.pixel {
                BipPixel::Fixed(w, h) => {
                    let max_dim = w.max(h);
                    if max_dim > MAX_COVER_DIMENSION {
                        let scale = f64::from(MAX_COVER_DIMENSION) / f64::from(max_dim);
                        if scale.is_finite() && scale > 0.0 {
                            #[expect(
                                clippy::cast_possible_truncation,
                                clippy::cast_sign_loss,
                                reason = "scaled dimensions fit within MAX_COVER_DIMENSION bounds"
                            )]
                            let sw = (f64::from(w) * scale).round() as u32;
                            #[expect(
                                clippy::cast_possible_truncation,
                                clippy::cast_sign_loss,
                                reason = "scaled dimensions fit within MAX_COVER_DIMENSION bounds"
                            )]
                            let sh = (f64::from(h) * scale).round() as u32;
                            (sw.max(1), sh.max(1))
                        } else {
                            (w, h)
                        }
                    } else {
                        (w, h)
                    }
                }
                BipPixel::Range { max_w, max_h, .. } => {
                    let w = max_w.min(MAX_COVER_DIMENSION);
                    let h = if max_w > 0 {
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "range proportion calculation fits within u32 bounds"
                        )]
                        let calculated_h =
                            ((u64::from(w) * u64::from(max_h)) / u64::from(max_w)) as u32;
                        calculated_h
                    } else {
                        max_h
                    };
                    (w, h.max(1))
                }
            };

            if target_w > native_dim.0 {
                if let Some((curr_w, curr_h)) = best_variant {
                    if (u64::from(target_w) * u64::from(target_h))
                        > (u64::from(curr_w) * u64::from(curr_h))
                    {
                        best_variant = Some((target_w, target_h));
                    }
                } else {
                    best_variant = Some((target_w, target_h));
                }
            }
        }
    }

    if let Some((width, height)) = best_variant {
        FormatSelection::Descriptor { width, height }
    } else {
        FormatSelection::NativeDefault
    }
}

/// Builds the UTF-8 XML byte payload for an OBEX `Img-Descriptor` header (0x71).
#[must_use]
pub fn build_image_descriptor_xml(width: u32, height: u32) -> Vec<u8> {
    format!(
        "<image-descriptor version=\"1.0\"><image encoding=\"JPEG\" pixel=\"{width}*{height}\"/></image-descriptor>"
    )
    .into_bytes()
}

/// Queries the remote responder for BIP image properties XML (`x-bt/img-properties`).
///
/// # Errors
/// Returns [`BipError`] if the request fails, times out, or the response XML cannot be parsed.
pub async fn get_image_properties_on<T>(
    client: &mut Client<T>,
    img_handle: &str,
) -> Result<BipImageProperties, BipError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    validate_img_handle(img_handle)?;
    let started = Instant::now();
    tracing::debug!(handle = img_handle, "BIP image properties query starting");

    let get = with_obex_timeout(
        "OBEX GET PROPERTIES",
        OBEX_PROPERTIES_TIMEOUT,
        client.get([
            Header::Type(MIME_IMAGE_PROPERTIES.to_string()),
            Header::Other {
                id: IMG_HANDLE_HEADER_ID,
                value: HeaderValue::Unicode(img_handle.to_string()),
            },
        ]),
    )
    .await;

    match get {
        Ok(response) => {
            let xml = std::str::from_utf8(&response.body).map_err(|e| {
                BipError::new(BipErrorKind::Obex {
                    details: format!("invalid UTF-8 in image properties: {e}").into_boxed_str(),
                })
            })?;
            tracing::debug!(
                handle = img_handle,
                elapsed_ms = started.elapsed().as_millis() as u64,
                xml_len = xml.len(),
                "BIP image properties received"
            );
            BipImageProperties::parse(xml).ok_or_else(|| {
                BipError::new(BipErrorKind::Obex {
                    details: "failed to parse image-properties XML".into(),
                })
            })
        }
        Err(e) => Err(map_obex_failure("OBEX GET PROPERTIES", e)),
    }
}

/// Runs the BIP GET transaction on an already-connected OBEX `client`, optionally
/// specifying an `Img-Descriptor` byte payload (header 0x71).
///
/// # Errors
/// Returns [`BipError`] for invalid handles, OBEX transport/protocol failures
/// and responder rejections (see [`BipError::response_code`]).
pub async fn get_image_with_descriptor_on<T>(
    client: &mut Client<T>,
    image_type: BipImageType,
    img_handle: &str,
    descriptor: Option<&[u8]>,
) -> Result<Vec<u8>, BipError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    validate_img_handle(img_handle)?;

    let started = Instant::now();
    tracing::info!(
        mime = image_type.mime(),
        handle = img_handle,
        has_descriptor = descriptor.is_some(),
        "BIP image pull starting"
    );

    let mut headers = vec![
        Header::Type(image_type.mime().to_string()),
        Header::Other {
            id: IMG_HANDLE_HEADER_ID,
            value: HeaderValue::Unicode(img_handle.to_string()),
        },
    ];
    if let Some(desc_bytes) = descriptor {
        headers.push(Header::Other {
            id: IMG_DESCRIPTOR_HEADER_ID,
            value: HeaderValue::Bytes(desc_bytes.to_vec()),
        });
    }

    let get = with_obex_timeout("OBEX GET", OBEX_GET_TIMEOUT, client.get(headers)).await;

    match get {
        Ok(response) => {
            tracing::info!(
                bytes = response.body.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                has_descriptor = descriptor.is_some(),
                "BIP image received"
            );
            Ok(response.body)
        }
        Err(e) => Err(map_obex_failure("OBEX GET", e)),
    }
}

/// Runs the BIP GET transaction (Type + Img-Handle headers) on an
/// already-connected OBEX `client`.
///
/// No OBEX DISCONNECT is sent afterwards: the persistent cover-art session
/// must stay connected, because AOSP stops advertising `ImgHandle` in
/// `MediaPlayer1.Track` once the client disconnects (verified live), while
/// `obexd` likewise keeps its BIP session open across transfers. Dropping the
/// `Client` (closing the transport) is the caller's shutdown path.
///
/// The header set mirrors the AOSP native path: Type, Img-Handle (0x30,
/// Unicode — AOSP ignores the generic OBEX Name header) and NO
/// Img-Descriptor, selecting the responder's native-image format.
///
/// # Errors
/// Returns [`BipError`] for invalid handles, OBEX transport/protocol failures
/// and responder rejections (see [`BipError::response_code`]).
pub async fn get_image_on<T>(
    client: &mut Client<T>,
    image_type: BipImageType,
    img_handle: &str,
) -> Result<Vec<u8>, BipError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    get_image_with_descriptor_on(client, image_type, img_handle, None).await
}

/// Validates the image handle against the BIP constraints.
fn validate_img_handle(img_handle: &str) -> Result<(), BipError> {
    if img_handle.is_empty()
        || img_handle.len() > IMG_HANDLE_MAX_LEN
        || !img_handle.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(BipError::invalid_handle(
            "expected a non-empty image handle of at most 64 printable ASCII characters",
        ));
    }
    Ok(())
}

/// OBEX transport selected for a direct BIP cover-art fetch.
///
/// `BlueZ` publishes the remote player's `MediaPlayer1.ObexPort`: RFCOMM
/// channel numbers are at most 30, while Android 8+ Pixels advertise an
/// L2CAP PSM (e.g. 4097). The classification picks the matching transport
/// and the [`BipChannel`] implementation that can open it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BipTransport {
    /// Unknown port: resolve the BIP RFCOMM channel via SDP.
    SdpRfcomm,
    /// Known RFCOMM channel: skip the SDP lookup and connect directly.
    RfcommChannel(u8),
    /// Known L2CAP PSM: connect with ERTM flow control (AVRCP 1.6 Cover Art).
    L2capPsm(u16),
}

/// Classifies a remote `MediaPlayer1.ObexPort` value into a [`BipTransport`].
///
/// RFCOMM channels are 1..=30, so anything larger must be an L2CAP PSM
/// (e.g. Pixel phones publish PSM 4097 for AVRCP 1.6 Cover Art).
#[must_use]
pub fn classify_obex_port(obex_port: Option<u16>) -> BipTransport {
    match obex_port {
        None => BipTransport::SdpRfcomm,
        Some(p) if p <= 30 => BipTransport::RfcommChannel(p as u8),
        Some(p) => BipTransport::L2capPsm(p),
    }
}

/// Builds the [`BipChannel`] implementation matching a [`BipTransport`].
#[must_use]
pub fn bip_channel_for(transport: BipTransport) -> Box<dyn BipChannel> {
    match transport {
        BipTransport::SdpRfcomm => Box::new(RfcommBipChannel::default()),
        BipTransport::RfcommChannel(channel) => {
            Box::new(RfcommBipChannel::with_known_channel(channel))
        }
        BipTransport::L2capPsm(psm) => Box::new(L2capBipChannel::new(psm)),
    }
}

/// Keeps the persistent BIP OBEX session for one device alive.
///
/// On AOSP the cover-art flow only advertises `ImgHandle` values in
/// `MediaPlayer1.Track` while a BIP client session exists, so the appliance
/// must hold the session open even between fetches (the role obexd played).
#[async_trait::async_trait]
pub trait BipSessionKeeper: Send {
    /// Ensures an OBEX session is established to `address` over `transport`.
    ///
    /// Calling this repeatedly with the same address is a cheap no-op; a
    /// different address first releases the previous session.
    ///
    /// # Errors
    /// Returns [`BipError`] when the transport cannot be opened or the OBEX
    /// CONNECT is rejected. A failed ensure leaves the previous session
    /// released, so the next call re-establishes from scratch.
    #[expect(
        clippy::double_must_use,
        reason = "async-trait injects #[must_use] on the desugared future while Result is already must_use"
    )]
    async fn ensure(
        &mut self,
        address: &BtAddress,
        transport: &BipTransport,
    ) -> Result<(), BipError>;

    /// Fetches an image over the held session, re-establishing it if needed.
    ///
    /// The GET reuses the same transport connection as the session
    /// keepalive, mirroring obexd, which serves its single BIP session for
    /// both liveness and transfers. On a fetch failure the held session is
    /// released so the next call re-establishes from scratch.
    ///
    /// # Errors
    /// Returns [`BipError`] when the session cannot be ensured, the handle is
    /// invalid, or the responder rejects/fails the GET.
    #[expect(
        clippy::double_must_use,
        reason = "async-trait injects #[must_use] on the desugared future while Result is already must_use"
    )]
    async fn fetch_image(
        &mut self,
        address: &BtAddress,
        transport: &BipTransport,
        image_type: BipImageType,
        img_handle: &str,
    ) -> Result<Vec<u8>, BipError>;

    /// Pings the held session with a minimal OBEX operation.
    ///
    /// AOSP idles out the BIP session quickly when no traffic flows, after
    /// which the phone stops advertising `ImgHandle` in `MediaPlayer1.Track`
    /// entirely (the publisher-restart deadlock). The keepalive keeps the
    /// OBEX connection warm between fetches. Any complete response counts as
    /// liveness; only a transport failure is an error, which releases the
    /// held session so the next ensure re-establishes it.
    ///
    /// # Errors
    /// Returns [`BipError`] when the transport write/read fails; the session
    /// is released so the next ensure re-establishes from scratch.
    #[expect(
        clippy::double_must_use,
        reason = "async-trait injects #[must_use] on the desugared future while Result is already must_use"
    )]
    async fn keepalive(&mut self) -> Result<(), BipError>;

    /// Drops the held session (if any), closing the transport connection.
    fn release(&mut self);

    /// Monotonic generation of the held session.
    ///
    /// Changes whenever the session is dropped or re-established. Consumers
    /// holding session-scoped state (e.g. BIP image-handle caches — handles
    /// are only unique for the duration of one OBEX session) purge that
    /// state whenever this value differs from the one they cached.
    #[must_use]
    fn epoch(&self) -> u64;
}

/// Production [`BipSessionKeeper`] holding one live OBEX session.
///
/// The held [`Client`] owns the transport stream; dropping it closes the
/// L2CAP/RFCOMM link and releases the remote cover-art session.
pub struct PersistentBipSession {
    session: Option<Client<BipStream>>,
    address: Option<BtAddress>,
    transport: Option<BipTransport>,
    /// Session generation, bumped on every release/re-establish; see
    /// [`BipSessionKeeper::epoch`].
    epoch: u64,
    /// Builds the physical channel for a transport. Production uses
    /// [`bip_channel_for`] (real bluer sockets); host tests inject a factory
    /// serving duplex OBEX streams.
    channel_factory: ChannelFactory,
}

/// Builds a [`BipChannel`] for a transport (dependency-injection seam for
/// [`PersistentBipSession`]).
type ChannelFactory = Box<dyn Fn(&BipTransport) -> Box<dyn BipChannel> + Send>;

impl fmt::Debug for PersistentBipSession {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistentBipSession")
            .field("address", &self.address)
            .field("transport", &self.transport)
            .field("session_held", &self.session.is_some())
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl Default for PersistentBipSession {
    fn default() -> Self {
        Self {
            session: None,
            address: None,
            transport: None,
            epoch: 0,
            channel_factory: Box::new(|transport| bip_channel_for(*transport)),
        }
    }
}

impl PersistentBipSession {
    /// Creates a session over an injected channel factory (host tests); each
    /// `ensure` call builds its channel through the factory.
    #[must_use]
    pub fn with_channel_factory(
        factory: impl Fn(&BipTransport) -> Box<dyn BipChannel> + Send + 'static,
    ) -> Self {
        Self {
            session: None,
            address: None,
            transport: None,
            epoch: 0,
            channel_factory: Box::new(factory),
        }
    }
}

#[async_trait::async_trait]
impl BipSessionKeeper for PersistentBipSession {
    async fn ensure(
        &mut self,
        address: &BtAddress,
        transport: &BipTransport,
    ) -> Result<(), BipError> {
        if self.address.as_ref() == Some(address)
            && self.transport.as_ref() == Some(transport)
            && self.session.is_some()
        {
            tracing::trace!(address = %address.to_string(), "BIP session already established");
            return Ok(());
        }
        // Address AND transport must match: a phone that re-registers BIP on
        // a different PSM/channel (profile re-registration) invalidates the
        // old link, and the session generation bump below purges every
        // session-scoped cached handle with it.
        self.release();
        let started = Instant::now();
        let channel = (self.channel_factory)(transport);
        tracing::debug!(
            address = %address.to_string(),
            ?transport,
            "establishing persistent BIP session"
        );
        let stream = channel.open(address).await?;
        let mut client = cover_art_client(stream);
        with_obex_timeout("OBEX CONNECT", OBEX_CONNECT_TIMEOUT, async {
            client
                .connect([Header::Target(AVRCP_COVER_ART_UUID.to_vec())])
                .await
        })
        .await
        .map_err(|e| map_obex_failure("OBEX CONNECT", e))?;
        self.epoch = self.epoch.wrapping_add(1);
        tracing::info!(
            address = %address.to_string(),
            ?transport,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "persistent BIP session established"
        );
        self.session = Some(client);
        self.address = Some(*address);
        self.transport = Some(*transport);
        Ok(())
    }

    async fn fetch_image(
        &mut self,
        address: &BtAddress,
        transport: &BipTransport,
        image_type: BipImageType,
        img_handle: &str,
    ) -> Result<Vec<u8>, BipError> {
        self.ensure(address, transport).await?;
        let Some(client) = self.session.as_mut() else {
            unreachable!("ensure() guarantees a held session on Ok");
        };

        if image_type == BipImageType::Native {
            match get_image_properties_on(client, img_handle).await {
                Ok(properties) => {
                    if let FormatSelection::Descriptor { width, height } =
                        select_best_format(&properties, TARGET_COVER_MIN_DIMENSION)
                    {
                        tracing::info!(
                            handle = img_handle,
                            width,
                            height,
                            "requesting higher-resolution cover art variant"
                        );
                        let descriptor_xml = build_image_descriptor_xml(width, height);
                        match get_image_with_descriptor_on(
                            client,
                            image_type,
                            img_handle,
                            Some(&descriptor_xml),
                        )
                        .await
                        {
                            Ok(body) => return Ok(body),
                            Err(e) => {
                                tracing::warn!(
                                    error = %e.summary(),
                                    handle = img_handle,
                                    "higher-resolution variant GET rejected; falling back to native"
                                );
                                if e.is_transport_fault() {
                                    self.release();
                                    return Err(e);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e.summary(),
                        handle = img_handle,
                        "BIP image properties query failed; falling back to native format"
                    );
                    if e.is_transport_fault() {
                        self.release();
                        return Err(e);
                    }
                }
            }
        }

        match get_image_on(client, image_type, img_handle).await {
            Ok(body) => Ok(body),
            Err(e) => {
                tracing::warn!(
                    error = %e.summary(),
                    handle = img_handle,
                    "persistent BIP session fetch failed; releasing session for re-establishment"
                );
                self.release();
                Err(e)
            }
        }
    }

    async fn keepalive(&mut self) -> Result<(), BipError> {
        let Some(client) = self.session.as_mut() else {
            tracing::trace!("BIP keepalive skipped: no session held");
            return Ok(());
        };
        match with_obex_timeout("OBEX SETPATH", OBEX_KEEPALIVE_TIMEOUT, async {
            client.set_path_root().await
        })
        .await
        {
            Ok(_) => {
                tracing::debug!("BIP keepalive OK");
                Ok(())
            }
            // AOSP answers SETPATH with a complete OBEX response (Not
            // Implemented), which is itself proof of transport liveness: the
            // phone-side BIP server session is still connected. Only a
            // transport failure (reset/timeout) releases the session.
            Err(e) if e.response_code().is_some() => {
                tracing::debug!("BIP keepalive answered (responder ignores SETPATH): {e}");
                Ok(())
            }
            Err(e) => {
                tracing::warn!("BIP keepalive failed; releasing session for re-establishment: {e}");
                self.release();
                Err(map_obex_failure("OBEX SETPATH", e))
            }
        }
    }

    fn release(&mut self) {
        if let Some(address) = self.address.take() {
            tracing::debug!(address = %address.to_string(), "releasing persistent BIP session");
        }
        if self.session.take().is_some() {
            self.epoch = self.epoch.wrapping_add(1);
        }
        self.transport = None;
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// Builds the RFCOMM socket address for the resolved BIP channel.
#[must_use]
pub fn bip_socket_address(octets: [u8; 6], channel: u8) -> bluer::rfcomm::SocketAddr {
    bluer::rfcomm::SocketAddr::new(bluer::Address(octets), channel)
}

/// Logs the SDP lookup outcome and maps it to a channel number or error.
///
/// Pure except for its tracing side effect, so both outcomes (channel found,
/// service record absent) are unit-testable on any host while the physical
/// L2CAP/RFCOMM transport stays isolated in [`RfcommBipChannel::open`].
///
/// # Errors
///
/// Returns [`BipError`] with [`BipErrorKind::ServiceNotFound`] if `channel` is `None`.
pub fn resolve_sdp_channel(
    address: &str,
    elapsed_ms: u64,
    channel: Option<u8>,
) -> Result<u8, BipError> {
    if let Some(channel) = channel {
        tracing::info!(
            address,
            channel,
            elapsed_ms,
            "SDP resolved BIP Image Pull RFCOMM channel"
        );
        Ok(channel)
    } else {
        tracing::warn!(
            address,
            elapsed_ms,
            "no BIP Image Pull service record on device"
        );
        Err(BipError::service_not_found(address))
    }
}

/// Maps an OBEX failure into a [`BipError`], distinguishing explicit
/// responder rejections from transport and protocol faults.
fn map_obex_failure(operation: &'static str, error: ObexError) -> BipError {
    if let Some(code) = error.response_code() {
        tracing::warn!(
            operation,
            response_code = code as u8,
            "BIP operation rejected by responder"
        );
        BipError::image_rejected(operation, code)
    } else {
        tracing::warn!(operation, error = %error, "BIP operation failed");
        BipError::obex(error)
    }
}
