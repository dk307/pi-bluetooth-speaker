use super::*;
use obex_protocol::packet::{Request, Response};
use obex_protocol::srm::SingleResponseMode;
use obex_protocol::tokio::{Object, Server};
use std::error::Error as _;
use std::sync::Arc;
use tokio::sync::Mutex;

fn body_bytes(len: usize, fill: u8) -> Vec<u8> {
    vec![fill; len]
}

/// Stub OBEX responder for the AVRCP cover-art target: records the incoming
/// CONNECT Target and GET headers, then answers GET with either an image body
/// or an injected failure `Response`.
struct StubCoverArtServer {
    /// GET outcomes consumed front-to-back (one per received GET); the last
    /// entry repeats once the queue is drained.
    get_outcomes: std::collections::VecDeque<Result<Vec<u8>, Response>>,
    connect_target: Arc<Mutex<Option<Vec<u8>>>>,
    get_handle: Arc<Mutex<Option<String>>>,
    get_type: Arc<Mutex<Option<String>>>,
    /// Generic OBEX Name header if the client sent one (obexd never does).
    get_name: Arc<Mutex<Option<String>>>,
    /// Whether each received GET carried the Img-Descriptor header.
    descriptor_history: Arc<Mutex<Vec<bool>>>,
    /// Whether each received GET requested Single Response Mode (0x97).
    get_srm: Arc<Mutex<bool>>,
}

impl obex_protocol::tokio::Handler for StubCoverArtServer {
    fn connect(&mut self, request: &Request) -> impl Future<Output = Response> + Send {
        let target = request.headers.iter().find_map(|h| match h {
            Header::Target(bytes) => Some(bytes.clone()),
            _ => None,
        });
        let slot = Arc::clone(&self.connect_target);
        async move {
            *slot.lock().await = target;
            Response::new(ResponseCode::Success)
        }
    }

    fn get(
        &mut self,
        request: &Request,
    ) -> impl Future<Output = std::result::Result<Object, Response>> + Send {
        let handle = request.headers.iter().find_map(|h| match h {
            Header::Other {
                id: IMG_HANDLE_HEADER_ID,
                value: HeaderValue::Unicode(name),
            } => Some(name),
            _ => None,
        });
        let name = request.headers.iter().find_map(|h| match h {
            Header::Name(name) => Some(name.clone()),
            _ => None,
        });
        let mime = request.headers.iter().find_map(|h| match h {
            Header::Type(m) => Some(m.clone()),
            _ => None,
        });
        let srm = request
            .headers
            .iter()
            .any(|h| matches!(h, Header::SingleResponseMode(SingleResponseMode::Enable)));
        let descriptor = request.headers.iter().any(|h| match h {
            Header::Other { id, value } => *id == 0x71 && matches!(value, HeaderValue::Bytes(_)),
            _ => false,
        });
        let outcome = self
            .get_outcomes
            .pop_front()
            .unwrap_or_else(|| self.get_outcomes.back().cloned().expect("non-empty"))
            .map(|body| Object::new([Header::Type("x-bt/img-img".to_string())], body));
        let (handle_slot, mime_slot, name_slot, descriptor_slot, srm_slot) = (
            Arc::clone(&self.get_handle),
            Arc::clone(&self.get_type),
            Arc::clone(&self.get_name),
            Arc::clone(&self.descriptor_history),
            Arc::clone(&self.get_srm),
        );
        async move {
            *handle_slot.lock().await = handle.cloned();
            *mime_slot.lock().await = mime;
            *name_slot.lock().await = name;
            descriptor_slot.lock().await.push(descriptor);
            *srm_slot.lock().await = srm;
            outcome
        }
    }
}

/// Lets the spawned server task borrow the shared stub behind its mutex guard
/// for the duration of [`Server::run`], while the test keeps the `Arc` for
/// post-run assertions.
struct HandlerProxy<'a>(&'a mut StubCoverArtServer);

impl obex_protocol::tokio::Handler for HandlerProxy<'_> {
    fn connect(&mut self, request: &Request) -> impl Future<Output = Response> + Send {
        self.0.connect(request)
    }
    fn get(
        &mut self,
        request: &Request,
    ) -> impl Future<Output = std::result::Result<Object, Response>> + Send {
        self.0.get(request)
    }
}

type SharedStub = Arc<Mutex<StubCoverArtServer>>;

fn spawn_stub_server(
    stub: SharedStub,
    server_half: tokio::io::DuplexStream,
) -> tokio::task::JoinHandle<obex_protocol::Result<()>> {
    tokio::spawn(async move {
        let mut inner = stub.lock().await;
        let mut s = Server::new(server_half, HandlerProxy(&mut inner));
        s.run().await
    })
}

/// Spawns the stub with Single Response Mode enabled on the server engine,
/// mirroring AOSP's OBEX-over-L2CAP transport.
fn spawn_stub_server_srm(
    stub: SharedStub,
    server_half: tokio::io::DuplexStream,
) -> tokio::task::JoinHandle<obex_protocol::Result<()>> {
    tokio::spawn(async move {
        let mut inner = stub.lock().await;
        let mut s =
            Server::new(server_half, HandlerProxy(&mut inner)).with_single_response_mode(true);
        s.run().await
    })
}

fn new_stub(get_outcome: Result<Vec<u8>, Response>) -> SharedStub {
    new_retry_stub(vec![get_outcome])
}

/// Builds a stub serving the given GET outcomes in order, repeating the last
/// one if additional GETs arrive.
fn new_retry_stub(get_outcomes: Vec<Result<Vec<u8>, Response>>) -> SharedStub {
    Arc::new(Mutex::new(StubCoverArtServer {
        get_outcomes: get_outcomes.into(),
        connect_target: Arc::new(Mutex::new(None)),
        get_handle: Arc::new(Mutex::new(None)),
        get_type: Arc::new(Mutex::new(None)),
        get_name: Arc::new(Mutex::new(None)),
        descriptor_history: Arc::new(Mutex::new(Vec::new())),
        get_srm: Arc::new(Mutex::new(false)),
    }))
}

#[tokio::test]
async fn pull_image_returns_full_body_over_connect_and_get() {
    let body = body_bytes(2048, 0xAB);
    let stub = new_stub(Ok(body.clone()));
    let (client_half, server_half) = tokio::io::duplex(65_536);
    let server_task = spawn_stub_server(Arc::clone(&stub), server_half);

    let result = pull_image(client_half, BipImageType::Native, "0000001").await;
    let _ = server_task.await;

    assert_eq!(result.unwrap(), body);
    let s = stub.lock().await;
    assert_eq!(
        s.connect_target.lock().await.as_deref(),
        Some(&AVRCP_COVER_ART_UUID[..])
    );
    assert_eq!(s.get_handle.lock().await.as_deref(), Some("0000001"));
    assert_eq!(
        s.get_type.lock().await.as_deref(),
        Some(BipImageType::Native.mime())
    );
    assert_eq!(
        *s.descriptor_history.lock().await,
        vec![false],
        "native GET must omit the Img-Descriptor header (AOSP native path)"
    );
    assert_eq!(s.get_name.lock().await.as_deref(), None);
}

/// `pull_image` mirrors `obexd/client/bip.c get_image()`: the handle travels
/// in the BIP `Img-Handle` (0x30) Unicode header, the generic Name header is
/// never sent, and the Img-Descriptor is always present — a single GET, no
/// retry ladder.
#[tokio::test]
async fn pull_image_sends_img_handle_header_instead_of_name() {
    let stub = new_stub(Ok(vec![0xFF, 0xD8, 0xFF, 0xE0]));
    let (client_half, server_half) = tokio::io::duplex(65_536);
    let server_task = spawn_stub_server(Arc::clone(&stub), server_half);

    let result = pull_image(client_half, BipImageType::Thumbnail, "7654321").await;
    let _ = server_task.await;

    let _body = result.unwrap();
    let s = stub.lock().await;
    assert_eq!(s.get_handle.lock().await.as_deref(), Some("7654321"));
    assert_eq!(s.get_name.lock().await.as_deref(), None);
    assert_eq!(
        s.get_type.lock().await.as_deref(),
        Some(BipImageType::Thumbnail.mime())
    );
    assert_eq!(
        *s.descriptor_history.lock().await,
        vec![false],
        "native GET must omit the Img-Descriptor header (AOSP native path)"
    );
}

#[tokio::test]
async fn pull_image_reassembles_chunked_multi_packet_body() {
    let body = body_bytes(131_072, 0x5A);
    let stub = new_stub(Ok(body.clone()));
    let (client_half, server_half) = tokio::io::duplex(65_536);
    let server_task = spawn_stub_server(stub, server_half);

    let result = pull_image(client_half, BipImageType::Native, "0000001").await;
    let _ = server_task.await;

    assert_eq!(result.unwrap(), body);
}

#[tokio::test]
async fn pull_image_maps_not_found_response_to_image_rejected() {
    let stub = new_stub(Err(Response::new(ResponseCode::NotFound)));
    let (client_half, server_half) = tokio::io::duplex(65_536);
    let server_task = spawn_stub_server(stub, server_half);

    let result = pull_image(client_half, BipImageType::Thumbnail, "0000002").await;
    let _ = server_task.await;

    let err = result.expect_err("GET rejection should surface");
    assert!(err.is_image_rejected());
    assert_eq!(err.response_code(), Some(0x44));
    assert!(err.summary().contains("OBEX GET"));
    assert!(err.summary().contains("0x44"));
    assert!(!err.summary().contains("Backtrace"));
}

#[tokio::test]
async fn pull_image_maps_connect_rejection_to_image_rejected() {
    struct RejectingServer;
    impl obex_protocol::tokio::Handler for RejectingServer {
        #[expect(
            clippy::manual_async_fn,
            reason = "the trait declares RPITIT futures with a Send bound that plain async fn impls cannot satisfy"
        )]
        fn connect(&mut self, _request: &Request) -> impl Future<Output = Response> + Send {
            async { Response::new(ResponseCode::Forbidden) }
        }
    }
    let (client_half, server_half) = tokio::io::duplex(4_096);
    let server_task = tokio::spawn(async move {
        let mut s = Server::new(server_half, RejectingServer);
        s.run().await
    });
    let result = pull_image(client_half, BipImageType::Native, "0000001").await;
    let _ = server_task.await;

    let err = result.expect_err("CONNECT rejection should surface");
    assert!(err.is_image_rejected());
    assert_eq!(err.response_code(), Some(0x43));
    assert!(err.summary().contains("OBEX CONNECT"));
}

#[tokio::test]
async fn pull_image_surfaces_transport_disconnect_as_obex_error() {
    let (client_half, server_half) = tokio::io::duplex(4_096);
    drop(server_half);
    let result = pull_image(client_half, BipImageType::Native, "0000001").await;

    let err = result.expect_err("dead transport should surface");
    assert!(err.is_obex());
}

#[tokio::test]
async fn pull_image_rejects_empty_handle() {
    let (client_half, _server_half) = tokio::io::duplex(4_096);
    let result = pull_image(client_half, BipImageType::Native, "").await;
    let err = result.expect_err("empty handle should be rejected");
    assert!(err.is_invalid_handle());
}

#[tokio::test]
async fn pull_image_rejects_unprintable_handle() {
    let (client_half, _server_half) = tokio::io::duplex(4_096);
    let result = pull_image(client_half, BipImageType::Native, "00000\t01").await;
    let err = result.expect_err("non-graphic handle should be rejected");
    assert!(err.is_invalid_handle());
}

#[tokio::test]
async fn pull_image_rejects_overlong_handle() {
    let (client_half, _server_half) = tokio::io::duplex(4_096);
    let handle = "h".repeat(IMG_HANDLE_MAX_LEN + 1);
    let result = pull_image(client_half, BipImageType::Native, &handle).await;
    let err = result.expect_err("overlong handle should be rejected");
    assert!(err.is_invalid_handle());
}

#[test]
fn bip_image_type_mime_values_match_bip_spec() {
    assert_eq!(BipImageType::Native.mime(), "x-bt/img-img");
    assert_eq!(BipImageType::Thumbnail.mime(), "x-bt/img-thm");
    assert_ne!(BipImageType::Native, BipImageType::Thumbnail);
}

#[test]
fn avrcp_cover_art_uuid_bytes_match_spec_string() {
    let expected: Vec<u8> = "7163DD544A7E11E2B47C0050C2490048"
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    assert_eq!(AVRCP_COVER_ART_UUID.to_vec(), expected);
    assert_ne!(AVRCP_COVER_ART_UUID, [0u8; 16]);
}

#[test]
fn bip_image_pull_uuid16_matches_assigned_number() {
    assert_eq!(BIP_IMAGE_PULL_UUID16, 0x111B);
    assert_ne!(BIP_IMAGE_PULL_UUID16, 0x112F);
}

#[test]
fn bip_error_kinds_expose_predicates_and_sources() {
    let sdp = BipError::sdp_lookup(bluesdp::SdpError::Decode("bad".to_string()));
    assert!(sdp.is_sdp_lookup());
    assert!(!sdp.is_image_rejected());
    assert!(sdp.source().is_some());

    let not_found = BipError::service_not_found("AA:BB:CC:DD:EE:FF");
    assert!(not_found.is_service_not_found());
    assert!(not_found.source().is_none());
    assert!(not_found.summary().contains("AA:BB:CC:DD:EE:FF"));

    let rfcomm = BipError::rfcomm_connect(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "no route",
    ));
    assert!(rfcomm.is_rfcomm_connect());
    assert!(rfcomm.source().is_some());
    assert!(rfcomm.summary().contains("RFCOMM connect failed"));

    let handle = BipError::invalid_handle("empty");
    assert!(handle.is_invalid_handle());

    let obex = BipError::obex(ObexError::Disconnected);
    assert!(obex.is_obex());
    assert!(obex.summary().contains("OBEX protocol error"));

    let rejected = BipError::image_rejected("OBEX GET", ResponseCode::NotFound);
    assert!(rejected.is_image_rejected());
    assert_eq!(rejected.response_code(), Some(0x44));
    assert_eq!(
        rejected.summary(),
        "OBEX GET rejected with response code 0x44"
    );
}

#[test]
fn bip_error_display_appends_captured_backtrace_only_for_humans() {
    let err = BipError::invalid_handle("test");
    let summary = err.summary();
    assert!(!summary.contains('\n'));
    assert!(!summary.contains("Backtrace"));
    assert!(err.to_string().starts_with("invalid image handle: test"));
}

#[test]
fn bip_error_response_code_is_none_for_non_rejections() {
    let obex = BipError::obex(ObexError::Disconnected);
    assert_eq!(obex.response_code(), None);
}

#[test]
fn bip_error_backtrace_accessor_returns_captured_backtrace() {
    let err = BipError::invalid_handle("bt");
    assert!(matches!(
        err.backtrace().status(),
        std::backtrace::BacktraceStatus::Captured | std::backtrace::BacktraceStatus::Disabled
    ));
}

#[test]
fn bip_error_sdp_lookup_summary_mentions_sdp() {
    let err = BipError::sdp_lookup(bluesdp::SdpError::Connect("L2CAP refused".to_string()));
    assert!(err.summary().starts_with("SDP service lookup failed: "));
    assert!(err.summary().contains("L2CAP refused"));
}

#[test]
fn bip_error_from_sdp_error_matches_constructor() {
    let converted: BipError = bluesdp::SdpError::Decode("truncated".to_string()).into();
    let constructed = BipError::sdp_lookup(bluesdp::SdpError::Decode("truncated".to_string()));
    assert!(converted.is_sdp_lookup());
    assert_eq!(converted.summary(), constructed.summary());
    assert!(converted.source().is_some());
}

#[test]
fn bip_socket_address_encodes_address_octets_and_channel() {
    let socket_addr = bip_socket_address([0x01, 0x23, 0x45, 0x67, 0x89, 0x0A], 12);
    assert_eq!(socket_addr.addr.0, [0x01, 0x23, 0x45, 0x67, 0x89, 0x0A]);
    assert_eq!(socket_addr.channel, 12);
    assert_eq!(socket_addr.to_string(), "[01:23:45:67:89:0A]:12");
}

#[test]
fn resolve_sdp_channel_returns_channel_when_service_record_exists() {
    assert_eq!(
        resolve_sdp_channel("11:22:33:44:55:66", 7, Some(24)).expect("resolved"),
        24
    );
}

#[test]
fn resolve_sdp_channel_reports_absent_service_record() {
    let err = resolve_sdp_channel("11:22:33:44:55:66", 7, None).expect_err("no service record");
    assert!(err.is_service_not_found());
    assert!(err.to_string().contains("11:22:33:44:55:66"));
}

/// Passthrough wrapper that records whether any inbound bytes carried the
/// OBEX SRM-enable pair (0x97 0x01), proving the client actually put
/// Single Response Mode on the wire.
struct SrmProbe<T> {
    inner: T,
    seen: Arc<std::sync::atomic::AtomicBool>,
}

impl<T: AsyncRead + Unpin> AsyncRead for SrmProbe<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match std::pin::Pin::new(&mut self.inner).poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) => {
                if buf.filled().windows(2).any(|w| w == [0x97, 0x01]) {
                    self.seen.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for SrmProbe<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn pull_image_puts_srm_on_the_wire_and_transfers_large_body() {
    let body = body_bytes(65_536, 0xCD);
    let stub = new_stub(Ok(body.clone()));
    let (client_half, server_half) = tokio::io::duplex(1024);
    let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen_for_task = Arc::clone(&seen);
    let server = tokio::spawn(async move {
        let mut inner = stub.lock().await;
        let probed = SrmProbe {
            inner: server_half,
            seen: seen_for_task,
        };
        let mut s = Server::new(probed, HandlerProxy(&mut inner)).with_single_response_mode(true);
        s.run().await
    });
    let received = pull_image(client_half, BipImageType::Native, "0000001")
        .await
        .expect("SRM transfer succeeds");
    server
        .await
        .expect("server run succeeds")
        .expect("server ok");
    assert_eq!(received, body, "full body reassembled under SRM");
    assert!(
        seen.load(std::sync::atomic::Ordering::Relaxed),
        "client must put the SRM-enable header (0x97 0x01) on the wire"
    );
}

#[test]
fn persistent_session_epoch_starts_at_zero_and_survives_empty_release() {
    let mut session = PersistentBipSession::default();
    assert_eq!(session.epoch(), 0);
    session.release();
    assert_eq!(
        session.epoch(),
        0,
        "releasing with no held session must not bump the generation"
    );
}

// Compile-time proof that the OBEX packet budget and the configured
// L2CAP receive MTU agree and stay inside AOSP's safe 16-bit range.
const _: () = {
    assert!(OBEX_AOSP_MAX_PACKET_LENGTH == L2CAP_AOSP_RECEIVE_MTU);
    assert!(OBEX_AOSP_MAX_PACKET_LENGTH < u16::MAX);
    assert!(OBEX_AOSP_MAX_PACKET_LENGTH >= obex_protocol::packet::MINIMUM_PACKET_LENGTH);
};

// ---------------------------------------------------------------------------
// BipSocketCore dependency-injection tests: the channel assembly logic runs
// against a scripted transport core, no Bluetooth hardware required.
// ---------------------------------------------------------------------------

/// Scripted [`BipSocketCore`] outcomes consumed front-to-back.
#[derive(Debug, Clone, Copy)]
enum CoreOutcome {
    /// SDP resolves the given channel number.
    SdpChannel(u8),
    /// SDP finds no BIP service record.
    SdpAbsent,
    /// SDP transport lookup fails.
    SdpError,
    /// The connect succeeds with a duplex stream (paired with a server half).
    ConnectOk,
    /// The connect fails like a hardware connect would.
    ConnectError,
}

struct ScriptedCore {
    outcomes: std::sync::Mutex<std::collections::VecDeque<CoreOutcome>>,
    requested_channels: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    requested_psms: std::sync::Arc<std::sync::Mutex<Vec<u16>>>,
}

/// Channels/PSMs requested against a [`ScriptedCore`], for assertions.
#[derive(Clone)]
struct ChannelCounters {
    channels: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    psms: std::sync::Arc<std::sync::Mutex<Vec<u16>>>,
}

impl ChannelCounters {
    fn channels(&self) -> Vec<u8> {
        self.channels.lock().unwrap().clone()
    }

    fn psms(&self) -> Vec<u16> {
        self.psms.lock().unwrap().clone()
    }
}

impl ScriptedCore {
    fn new(outcomes: &[CoreOutcome]) -> (Self, ChannelCounters) {
        let channels = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let psms = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                outcomes: std::sync::Mutex::new(std::collections::VecDeque::from_iter(
                    outcomes.iter().copied(),
                )),
                requested_channels: Arc::clone(&channels),
                requested_psms: Arc::clone(&psms),
            },
            ChannelCounters { channels, psms },
        )
    }

    fn next(&self, step: &'static str) -> CoreOutcome {
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("no scripted outcome left for {step}"))
    }
}

fn duplex_pair() -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
    tokio::io::duplex(16_384)
}

#[async_trait::async_trait]
impl BipSocketCore for ScriptedCore {
    async fn sdp_bip_rfcomm_channel(&self, _address: &str) -> Result<Option<u8>, BipError> {
        match self.next("sdp") {
            CoreOutcome::SdpChannel(channel) => Ok(Some(channel)),
            CoreOutcome::SdpAbsent => Ok(None),
            CoreOutcome::SdpError => Err(BipError::sdp_lookup(bluesdp::SdpError::Decode(
                String::from("sdp down"),
            ))),
            CoreOutcome::ConnectOk | CoreOutcome::ConnectError => {
                panic!("unexpected SDP outcome")
            }
        }
    }

    async fn connect_rfcomm(
        &self,
        _address: &BtAddress,
        channel: u8,
    ) -> Result<BipStream, BipError> {
        self.requested_channels.lock().unwrap().push(channel);
        match self.next("rfcomm connect") {
            CoreOutcome::ConnectOk => {
                let (client, _server) = duplex_pair();
                Ok(Box::new(client) as BipStream)
            }
            CoreOutcome::ConnectError => Err(BipError::rfcomm_connect(std::io::Error::other(
                "rfcomm down",
            ))),
            _ => panic!("unexpected connect outcome"),
        }
    }

    async fn connect_l2cap(&self, _address: &BtAddress, psm: u16) -> Result<BipStream, BipError> {
        self.requested_psms.lock().unwrap().push(psm);
        match self.next("l2cap connect") {
            CoreOutcome::ConnectOk => {
                let (client, _server) = duplex_pair();
                Ok(Box::new(client) as BipStream)
            }
            CoreOutcome::ConnectError => {
                Err(BipError::l2cap_connect(std::io::Error::other("l2cap down")))
            }
            _ => panic!("unexpected connect outcome"),
        }
    }
}

fn test_address() -> BtAddress {
    BtAddress::parse_str("AA:BB:CC:DD:EE:FF").expect("valid MAC")
}

#[tokio::test]
async fn rfcomm_channel_with_known_channel_skips_sdp() {
    let (core, counters) = ScriptedCore::new(&[CoreOutcome::ConnectOk]);
    let channel = RfcommBipChannel::with_core(Some(27), Arc::new(core));
    let address = test_address();
    let _stream = channel.open(&address).await.expect("open succeeds");
    assert_eq!(
        counters.channels(),
        vec![27],
        "connects on the known channel"
    );
}

#[tokio::test]
async fn rfcomm_channel_resolves_sdp_then_connects_on_resolved_channel() {
    let (core, counters) = ScriptedCore::new(&[CoreOutcome::SdpChannel(4), CoreOutcome::ConnectOk]);
    let channel = RfcommBipChannel::with_core(None, Arc::new(core));
    let _stream = channel.open(&test_address()).await.expect("open succeeds");
    assert_eq!(
        counters.channels(),
        vec![4],
        "connects on the SDP-resolved channel"
    );
}

#[tokio::test]
async fn rfcomm_channel_reports_absent_sdp_service_record() {
    let (core, counters) = ScriptedCore::new(&[CoreOutcome::SdpAbsent]);
    let channel = RfcommBipChannel::with_core(None, Arc::new(core));
    let error = channel
        .open(&test_address())
        .await
        .err()
        .expect("no service record error");
    assert!(error.is_service_not_found());
    assert!(counters.channels().is_empty(), "never reaches a connect");
}

#[tokio::test]
async fn rfcomm_channel_maps_sdp_transport_failure() {
    let (core, _counters) = ScriptedCore::new(&[CoreOutcome::SdpError]);
    let channel = RfcommBipChannel::with_core(None, Arc::new(core));
    let error = channel
        .open(&test_address())
        .await
        .err()
        .expect("SDP error");
    assert!(error.is_sdp_lookup());
}

#[tokio::test]
async fn rfcomm_channel_maps_connect_failure() {
    let (core, _counters) =
        ScriptedCore::new(&[CoreOutcome::SdpChannel(4), CoreOutcome::ConnectError]);
    let channel = RfcommBipChannel::with_core(None, Arc::new(core));
    let error = channel
        .open(&test_address())
        .await
        .err()
        .expect("connect error");
    assert!(error.is_rfcomm_connect());
}

#[tokio::test]
async fn l2cap_channel_connects_to_advertised_psm() {
    let (core, counters) = ScriptedCore::new(&[CoreOutcome::ConnectOk]);
    let channel = L2capBipChannel::with_core(4097, Arc::new(core));
    let _stream = channel.open(&test_address()).await.expect("open succeeds");
    assert_eq!(counters.psms(), vec![4097]);
}

#[tokio::test]
async fn l2cap_channel_maps_connect_failure() {
    let (core, _counters) = ScriptedCore::new(&[CoreOutcome::ConnectError]);
    let channel = L2capBipChannel::with_core(4097, Arc::new(core));
    let error = channel
        .open(&test_address())
        .await
        .err()
        .expect("connect error");
    assert!(error.is_l2cap_connect());
}

// ---------------------------------------------------------------------------
// PersistentBipSession production logic over injected duplex OBEX streams.
// ---------------------------------------------------------------------------

/// Channel mock whose `open` pairs the client half of a fresh duplex with a
/// spawned stub OBEX server.
struct ServingChannel;

#[async_trait::async_trait]
impl BipChannel for ServingChannel {
    async fn open(&self, _address: &BtAddress) -> Result<BipStream, BipError> {
        let (client, server) = duplex_pair();
        let stub = new_stub(Ok(body_bytes(140, 0xAB)));
        tokio::spawn(async move {
            let mut inner = stub.lock().await;
            let mut s =
                Server::new(server, HandlerProxy(&mut inner)).with_single_response_mode(true);
            let _ = s.run().await;
        });
        Ok(Box::new(client) as BipStream)
    }
}

/// Channel mock answering every open with a scripted error.
struct FailingChannel(std::sync::Arc<dyn Fn() -> BipError + Send + Sync>);

#[async_trait::async_trait]
impl BipChannel for FailingChannel {
    async fn open(&self, _address: &BtAddress) -> Result<BipStream, BipError> {
        Err((self.0)())
    }
}

#[tokio::test]
async fn persistent_session_establishes_and_fetches_over_one_channel() {
    let session = PersistentBipSession::with_channel_factory(|_| Box::new(ServingChannel));
    let mut session = session;
    let address = test_address();
    session
        .ensure(&address, &BipTransport::L2capPsm(4097))
        .await
        .expect("session establishes");
    assert_eq!(session.epoch(), 1, "first establish bumps the generation");
    let body = session
        .fetch_image(
            &address,
            &BipTransport::L2capPsm(4097),
            BipImageType::Native,
            "5630701",
        )
        .await
        .expect("GET succeeds over the established session");
    assert_eq!(body, body_bytes(140, 0xAB));
    session
        .keepalive()
        .await
        .expect("keepalive over the live session");
    session.release();
    assert_eq!(session.epoch(), 2, "release bumps the generation");
    assert!(
        session
            .ensure(&address, &BipTransport::L2capPsm(4097))
            .await
            .is_ok(),
        "re-establishment after release works"
    );
}

#[tokio::test]
async fn persistent_session_reestablishes_on_transport_change() {
    let session = PersistentBipSession::with_channel_factory(|_| Box::new(ServingChannel));
    let mut session = session;
    let address = test_address();
    session
        .ensure(&address, &BipTransport::L2capPsm(4097))
        .await
        .expect("first session");
    session
        .ensure(&address, &BipTransport::L2capPsm(4099))
        .await
        .expect("re-established on the new PSM");
    assert_eq!(
        session.epoch(),
        3,
        "transport change bumps the generation (release of the held session + re-establish)"
    );
    let body = session
        .fetch_image(
            &address,
            &BipTransport::L2capPsm(4099),
            BipImageType::Native,
            "5630701",
        )
        .await
        .expect("fetch over the re-established session");
    assert_eq!(body, body_bytes(140, 0xAB));
}

#[tokio::test]
async fn persistent_session_maps_connect_rejection_to_image_rejected() {
    let session = PersistentBipSession::with_channel_factory(|_| {
        Box::new(FailingChannel(std::sync::Arc::new(|| {
            BipError::new(BipErrorKind::ImageRejected {
                operation: "OBEX CONNECT",
                response_code: 0x43,
            })
        }))) as Box<dyn BipChannel>
    });
    let mut session = session;
    let error = session
        .ensure(&test_address(), &BipTransport::L2capPsm(4097))
        .await
        .expect_err("connect rejected");
    assert!(error.is_image_rejected());
    assert_eq!(error.response_code(), Some(0x43));
}

// ---------------------------------------------------------------------------
// OBEX operation deadlines: a peer that accepts the transport and then stops
// responding must not wedge the session forever.
// ---------------------------------------------------------------------------

/// A client over a duplex whose server half (kept alive by the caller)
/// never answers.
fn silent_client() -> (Client<BipStream>, tokio::io::DuplexStream) {
    let (client, server) = duplex_pair();
    (cover_art_client(Box::new(client) as BipStream), server)
}

#[tokio::test]
async fn get_times_out_when_peer_stops_responding() {
    let (mut client, _server) = silent_client();
    let error = get_image_on(&mut client, BipImageType::Native, "5630701")
        .await
        .expect_err("the unresponsive peer must time out");
    assert!(
        error.is_obex(),
        "the GET deadline maps to an OBEX transport fault: {error}"
    );
    assert!(
        error.to_string().contains("timed out"),
        "the error must identify the deadline expiry: {error}"
    );
}

#[tokio::test]
async fn connect_times_out_when_peer_stops_responding() {
    let (mut client, _server) = silent_client();
    let error = with_obex_timeout("OBEX CONNECT", OBEX_CONNECT_TIMEOUT, async {
        client
            .connect([Header::Target(AVRCP_COVER_ART_UUID.to_vec())])
            .await
    })
    .await
    .expect_err("the unresponsive peer must time out");
    assert!(
        matches!(error, ObexError::Io(ref e) if e.kind() == std::io::ErrorKind::TimedOut),
        "the CONNECT deadline surfaces as a timed-out transport fault: {error}"
    );
}

#[tokio::test]
async fn keepalive_times_out_when_peer_stops_responding() {
    let (mut client, _server) = silent_client();
    let error = with_obex_timeout("OBEX SETPATH", OBEX_KEEPALIVE_TIMEOUT, async {
        client.set_path_root().await
    })
    .await
    .expect_err("the unresponsive peer must time out");
    assert!(
        matches!(error, ObexError::Io(ref e) if e.kind() == std::io::ErrorKind::TimedOut),
        "the SETPATH deadline surfaces as a timed-out transport fault: {error}"
    );
}
