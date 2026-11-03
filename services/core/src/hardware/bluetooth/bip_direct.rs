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

/// OBEX header identifier of the BIP `Img-Descriptor` header.
const IMG_DESCRIPTOR_HEADER_ID: u8 = 0x71;

/// Minimal BIP image descriptor requested alongside every GET.
///
/// Without an `Img-Descriptor` header, strict BIP responders reject the GET
/// or answer with their (potentially enormous) original-resolution image.
const IMG_DESCRIPTOR_XML: &str = concat!(
    r#"<image-descriptor version="1.0.0" encoding="vlcard">"#,
    r#"<image encoding="JPEG" pixel="1024-1024"/>"#,
    r#"</image-descriptor>"#
);

/// Largest accepted image-handle string (e.g. `0000001`).
const IMG_HANDLE_MAX_LEN: usize = 64;

const _: () = {
    assert!(AVRCP_COVER_ART_UUID.len() == 16);
    assert!(IMG_DESCRIPTOR_XML.len() + 2 <= u8::MAX as usize);
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
/// original [`bluesdp::SdpError`], [`bluer::Error`] or [`ObexError`]) so the
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
#[derive(Debug, Clone, Copy, Default)]
pub struct RfcommBipChannel;

#[async_trait::async_trait]
impl BipChannel for RfcommBipChannel {
    async fn open(&self, address: &BtAddress) -> Result<BipStream, BipError> {
        let canonical = address.to_string();
        let octets = *address.as_bytes();
        let started = Instant::now();
        tracing::debug!(address = %canonical, "SDP: querying BIP Image Pull service record");
        let channel = resolve_sdp_channel(
            &canonical,
            started.elapsed().as_millis() as u64,
            bluesdp::query_rfcomm_channel(&canonical, bluesdp::Uuid16(BIP_IMAGE_PULL_UUID16))
                .await?,
        )?;

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
    if img_handle.is_empty()
        || img_handle.len() > IMG_HANDLE_MAX_LEN
        || !img_handle.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(BipError::invalid_handle(
            "expected a non-empty image handle of at most 64 printable ASCII characters",
        ));
    }

    let started = Instant::now();
    tracing::info!(
        mime = image_type.mime(),
        handle = img_handle,
        "BIP image pull starting"
    );

    let mut client = Client::new(stream);
    client
        .connect([Header::Target(AVRCP_COVER_ART_UUID.to_vec())])
        .await
        .map_err(|e| map_obex_failure("OBEX CONNECT", e))?;
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "OBEX CONNECT accepted"
    );

    let get = client
        .get([
            Header::Type(image_type.mime().to_string()),
            Header::Name(img_handle.to_string()),
            img_descriptor_header(),
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

/// Validates the direct BIP transport path with a CONNECT/DISCONNECT-only session.
///
/// Performs the full OBEX session establishment against the AVRCP cover-art
/// target (useful as a live transport probe when no image handle is known)
/// and closes the session again. The remote responder's acceptance of the
/// target UUID is the pass criterion.
///
/// # Errors
/// Returns [`BipError`] when the OBEX CONNECT is rejected or the transport fails.
pub async fn probe_session<T>(stream: T) -> Result<(), BipError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
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
    if let Err(e) = client.disconnect().await {
        tracing::warn!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "BIP probe disconnect failed: {e}"
        );
    }
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "BIP OBEX session probe succeeded"
    );
    Ok(())
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

/// Builds the BIP `Img-Descriptor` (0x71) header: a single TLV triplet
/// carrying the minimal XML descriptor.
fn img_descriptor_header() -> Header {
    let xml = IMG_DESCRIPTOR_XML.as_bytes();
    let mut value = Vec::with_capacity(xml.len() + 2);
    value.push(IMG_DESCRIPTOR_HEADER_ID);
    value.push(
        xml.len()
            .try_into()
            .expect("descriptor length proven <= u8::MAX by const assertion"),
    );
    value.extend_from_slice(xml);
    Header::Other {
        id: IMG_DESCRIPTOR_HEADER_ID,
        value: HeaderValue::Bytes(value),
    }
}

#[cfg(test)]
mod tests;
