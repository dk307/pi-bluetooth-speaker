//! Direct OBEX cover-art retrieval over Bluetooth RFCOMM (an `obexd` replacement).
//!
//! Performs BIP (Bluetooth Imaging Profile) GET transactions against the
//! AVRCP 1.6 cover-art OBEX target directly over an RFCOMM socket:
//!
//! 1. the RFCOMM server channel of the remote BIP Image Pull service is
//!    resolved through a pure-Rust SDP query over L2CAP ([`bluesdp`]),
//! 2. a BlueZ RFCOMM stream is opened ([`bluer`]),
//! 3. an OBEX CONNECT is issued with the AVRCP cover-art Target UUID,
//! 4. the image is pulled with an OBEX GET carrying the `Type`, `Name`
//!    (image handle) and `Img-Descriptor` (0x71) headers,
//! 5. the OBEX session is closed with a best-effort DISCONNECT.
//!
//! The profile-independent OBEX core ([`pull_image`]) is generic over the
//! transport and therefore fully host-testable against an in-process OBEX
//! server; only [`RfcommBipChannel`] touches physical Bluetooth hardware.

use std::backtrace::Backtrace;
use std::fmt::{self, Formatter};
use std::time::Instant;

use obex_protocol::Error as ObexError;
use obex_protocol::header::{Header, HeaderValue};
use obex_protocol::response::ResponseCode;
use obex_protocol::tokio::Client;
use tokio::io::{AsyncRead, AsyncWrite};

use super::connection::BtAddress;

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
const IMG_HANDLE_HEADER_ID: u8 = 0x30;

/// Largest accepted image-handle string (e.g. `0000001`).
const IMG_HANDLE_MAX_LEN: usize = 64;

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
    fn new(kind: BipErrorKind) -> Self {
        Self {
            kind,
            source: None,
            backtrace: Backtrace::capture(),
        }
    }

    /// Attaches an underlying error as this error's [`std::error::Error::source`].
    fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub(crate) fn sdp_lookup(source: bluesdp::SdpError) -> Self {
        Self::new(BipErrorKind::SdpLookup {
            details: source.to_string().into(),
        })
        .with_source(source)
    }

    pub(crate) fn service_not_found(address: impl Into<Box<str>>) -> Self {
        Self::new(BipErrorKind::ServiceNotFound {
            address: address.into(),
        })
    }

    pub(crate) fn rfcomm_connect(source: std::io::Error) -> Self {
        Self::new(BipErrorKind::RfcommConnect {
            details: source.to_string().into(),
        })
        .with_source(source)
    }

    pub(crate) fn l2cap_connect(source: std::io::Error) -> Self {
        Self::new(BipErrorKind::L2capConnect {
            details: source.to_string().into(),
        })
        .with_source(source)
    }

    pub(crate) fn invalid_handle(details: impl Into<Box<str>>) -> Self {
        Self::new(BipErrorKind::InvalidHandle {
            details: details.into(),
        })
    }

    pub(crate) fn obex(source: ObexError) -> Self {
        Self::new(BipErrorKind::Obex {
            details: source.to_string().into(),
        })
        .with_source(source)
    }

    pub(crate) fn image_rejected(operation: &'static str, response_code: ResponseCode) -> Self {
        Self::new(BipErrorKind::ImageRejected {
            operation,
            response_code: response_code as u8,
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

crate::diag::impl_canonical_error_display!(BipError);

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

impl From<std::io::Error> for BipError {
    fn from(source: std::io::Error) -> Self {
        Self::l2cap_connect(source)
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
    #[allow(
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
#[derive(Debug, Clone, Copy, Default)]
pub struct RfcommBipChannel {
    known_channel: Option<u8>,
}

impl RfcommBipChannel {
    /// Creates a channel that skips the SDP lookup and connects directly to
    /// the given RFCOMM channel number.
    #[must_use]
    pub const fn with_known_channel(channel: u8) -> Self {
        Self {
            known_channel: Some(channel),
        }
    }
}

#[async_trait::async_trait]
impl BipChannel for RfcommBipChannel {
    async fn open(&self, address: &BtAddress) -> Result<BipStream, BipError> {
        let canonical = address.to_string();
        let octets = *address.as_bytes();
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
            let sdp_result =
                bluesdp::query_rfcomm_channel(&canonical, bluesdp::Uuid16(BIP_IMAGE_PULL_UUID16))
                    .await?;
            resolve_sdp_channel(&canonical, started.elapsed().as_millis() as u64, sdp_result)?
        };

        let socket_addr = bip_socket_address(octets, channel);
        tracing::debug!(socket = %socket_addr, "opening RFCOMM connection");
        let stream = bluer::rfcomm::Stream::connect(socket_addr)
            .await
            .map_err(BipError::rfcomm_connect)?;
        tracing::debug!(
            socket = %socket_addr,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "RFCOMM connection established"
        );

        Ok(Box::new(stream) as BipStream)
    }
}

/// Production [`BipChannel`] for devices that serve cover art over L2CAP.
///
/// Android 8+ Pixels advertise an L2CAP PSM (e.g. 4097) in
/// `org.bluez.MediaPlayer1.ObexPort` instead of an RFCOMM channel; this
/// backend connects with Enhanced Retransmission Mode, which the AVRCP
/// cover-art profile requires for flow-controlled L2CAP transports.
#[derive(Debug, Clone, Copy)]
pub struct L2capBipChannel {
    /// L2CAP Protocol/Service Multiplexor published by the remote device.
    psm: u16,
}

impl L2capBipChannel {
    /// Creates a channel targeting a specific L2CAP PSM.
    #[must_use]
    pub const fn new(psm: u16) -> Self {
        Self { psm }
    }
}

fn l2cap_step_err(step: &'static str) -> impl Fn(std::io::Error) -> BipError {
    move |e| {
        tracing::debug!(step, error = %e, "L2CAP socket operation failed");
        BipError::l2cap_connect(e)
    }
}

#[async_trait::async_trait]
impl BipChannel for L2capBipChannel {
    async fn open(&self, address: &BtAddress) -> Result<BipStream, BipError> {
        let canonical = address.to_string();
        let octets = *address.as_bytes();
        let started = Instant::now();
        tracing::debug!(address = %canonical, psm = self.psm, "opening L2CAP (ERTM) connection");
        let socket = bluer::l2cap::Socket::<bluer::l2cap::Stream>::new_stream()
            .map_err(l2cap_step_err("create socket"))?;
        socket
            .bind(bluer::l2cap::SocketAddr::any_br_edr())
            .map_err(l2cap_step_err("bind local address"))?;
        // SOCK_STREAM L2CAP channels already default to ERTM when the kernel
        // has ERTM enabled (net/bluetooth/l2cap_sock.c `l2cap_sock_init`), and
        // in that configuration the BT_MODE sockopt rejects ERTM with EINVAL
        // (`l2cap_set_mode`). It only accepts ERTM when the module-wide
        // `disable_ertm=1` fallback left the socket in Basic mode, so treat
        // EINVAL as "ERTM is already the default" and fail on anything else.
        if let Err(e) = socket.set_flow_control(bluer::l2cap::FlowControl::Ertm) {
            if e.kind() != std::io::ErrorKind::InvalidInput {
                return Err(l2cap_step_err("set ERTM flow control")(e));
            }
            tracing::debug!(address = %canonical, "L2CAP stream socket defaults to ERTM (BT_MODE rejected with EINVAL)");
        }
        let socket_addr = bluer::l2cap::SocketAddr::new(
            bluer::Address(octets),
            bluer::AddressType::BrEdr,
            self.psm,
        );
        let stream = socket
            .connect(socket_addr)
            .await
            .map_err(l2cap_step_err("connect"))?;
        tracing::debug!(
            address = %canonical,
            psm = self.psm,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "L2CAP connection established"
        );

        Ok(Box::new(stream) as BipStream)
    }
}

/// Fetches cover-art bytes from a remote device through `channel`.
///
/// # Errors
/// Returns [`BipError`] when the transport cannot be opened or the OBEX
/// exchange fails.
pub async fn fetch_cover_art_via(
    channel: &dyn BipChannel,
    address: &BtAddress,
    img_handle: &str,
    image_type: BipImageType,
) -> Result<Vec<u8>, BipError> {
    let stream = channel.open(address).await?;
    pull_image(stream, image_type, img_handle).await
}

/// Runs a complete BIP GET transaction over an already connected `stream`.
///
/// Sends an OBEX CONNECT targeting the AVRCP 1.6 cover-art service, pulls the
/// requested image with a GET carrying `Type`, `Name` and `Img-Descriptor`
/// headers, then closes the OBEX session with a best-effort DISCONNECT.
///
/// A failing DISCONNECT after a successful GET never discards the already
/// reassembled body; a failing GET is always propagated first.
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

    let mut client = Client::new(stream);
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

/// Runs the BIP GET transaction (Type + Img-Handle headers) on an
/// already-connected OBEX `client`, then closes the OBEX session with a
/// best-effort DISCONNECT.
///
/// The header set mirrors the AOSP native path: Type, Img-Handle (0x30,
/// Unicode — AOSP ignores the generic OBEX Name header) and NO
/// Img-Descriptor, selecting the responder's native-image format.
///
/// A failing DISCONNECT after a successful GET never discards the already
/// reassembled body; a failing GET is always propagated first.
///
/// # Errors
/// Returns [`BipError`] for invalid handles, OBEX transport/protocol failures
/// and responder rejections (see [`BipError::response_code`]).
async fn get_image_on<T>(
    client: &mut Client<T>,
    image_type: BipImageType,
    img_handle: &str,
) -> Result<Vec<u8>, BipError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    validate_img_handle(img_handle)?;

    let started = Instant::now();
    tracing::info!(
        mime = image_type.mime(),
        handle = img_handle,
        "BIP image pull starting"
    );

    let get = client
        .get([
            Header::Type(image_type.mime().to_string()),
            Header::Other {
                id: IMG_HANDLE_HEADER_ID,
                value: HeaderValue::Unicode(img_handle.to_string()),
            },
        ])
        .await;

    match get {
        Ok(response) => {
            tracing::info!(
                bytes = response.body.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "BIP image received"
            );
            if let Err(e) = client.disconnect().await {
                tracing::warn!(
                    body_len = response.body.len(),
                    "BIP cover-art disconnect failed after successful GET: {e}"
                );
            }
            Ok(response.body)
        }
        Err(e) => {
            let _ = client.disconnect().await;
            Err(map_obex_failure("OBEX GET", e))
        }
    }
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
    #[allow(
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
    #[allow(
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
    #[allow(
        clippy::double_must_use,
        reason = "async-trait injects #[must_use] on the desugared future while Result is already must_use"
    )]
    async fn keepalive(&mut self) -> Result<(), BipError>;

    /// Drops the held session (if any), closing the transport connection.
    fn release(&mut self);
}

/// Production [`BipSessionKeeper`] holding one live OBEX session.
///
/// The held [`Client`] owns the transport stream; dropping it closes the
/// L2CAP/RFCOMM link and releases the remote cover-art session.
#[derive(Default)]
pub struct PersistentBipSession {
    session: Option<Client<BipStream>>,
    address: Option<BtAddress>,
    transport: Option<BipTransport>,
}

impl fmt::Debug for PersistentBipSession {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistentBipSession")
            .field("address", &self.address)
            .field("transport", &self.transport)
            .field("session_held", &self.session.is_some())
            .finish()
    }
}

#[async_trait::async_trait]
impl BipSessionKeeper for PersistentBipSession {
    async fn ensure(
        &mut self,
        address: &BtAddress,
        transport: &BipTransport,
    ) -> Result<(), BipError> {
        if self.address.as_ref() == Some(address) && self.session.is_some() {
            tracing::trace!(address = %address.to_string(), "BIP session already established");
            return Ok(());
        }
        self.release();
        let started = Instant::now();
        let channel = bip_channel_for(*transport);
        tracing::debug!(
            address = %address.to_string(),
            ?transport,
            "establishing persistent BIP session"
        );
        let stream = channel.open(address).await?;
        let mut client = Client::new(stream);
        client
            .connect([Header::Target(AVRCP_COVER_ART_UUID.to_vec())])
            .await
            .map_err(|e| map_obex_failure("OBEX CONNECT", e))?;
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
        match client.set_path_root().await {
            Ok(_) => {
                tracing::debug!("BIP keepalive OK");
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
        self.transport = None;
        self.session = None;
    }
}

/// Builds the RFCOMM socket address for the resolved BIP channel.
fn bip_socket_address(octets: [u8; 6], channel: u8) -> bluer::rfcomm::SocketAddr {
    bluer::rfcomm::SocketAddr::new(bluer::Address(octets), channel)
}

/// Logs the SDP lookup outcome and maps it to a channel number or error.
///
/// Pure except for its tracing side effect, so both outcomes (channel found,
/// service record absent) are unit-testable on any host while the physical
/// L2CAP/RFCOMM transport stays isolated in [`RfcommBipChannel::open`].
fn resolve_sdp_channel(
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

#[cfg(test)]
mod tests;
