use super::*;
use obex_protocol::packet::{Request, Response};
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
    get_outcome: Result<Vec<u8>, Response>,
    connect_target: Arc<Mutex<Option<Vec<u8>>>>,
    get_handle: Arc<Mutex<Option<String>>>,
    get_type: Arc<Mutex<Option<String>>>,
    got_descriptor: Arc<Mutex<bool>>,
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
            Header::Name(name) => Some(name.clone()),
            _ => None,
        });
        let mime = request.headers.iter().find_map(|h| match h {
            Header::Type(m) => Some(m.clone()),
            _ => None,
        });
        let descriptor = request.headers.iter().any(|h| match h {
            Header::Other { id, value } => {
                *id == IMG_DESCRIPTOR_HEADER_ID && matches!(value, HeaderValue::Bytes(_))
            }
            _ => false,
        });
        let (handle_slot, mime_slot, descriptor_slot) = (
            Arc::clone(&self.get_handle),
            Arc::clone(&self.get_type),
            Arc::clone(&self.got_descriptor),
        );
        let outcome = match &self.get_outcome {
            Ok(body) => Ok(Object::new(
                [Header::Type("x-bt/img-img".to_string())],
                body.clone(),
            )),
            Err(response) => Err(response.clone()),
        };
        async move {
            *handle_slot.lock().await = handle;
            *mime_slot.lock().await = mime;
            *descriptor_slot.lock().await = descriptor;
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

fn new_stub(get_outcome: Result<Vec<u8>, Response>) -> SharedStub {
    Arc::new(Mutex::new(StubCoverArtServer {
        get_outcome,
        connect_target: Arc::new(Mutex::new(None)),
        get_handle: Arc::new(Mutex::new(None)),
        get_type: Arc::new(Mutex::new(None)),
        got_descriptor: Arc::new(Mutex::new(false)),
    }))
}

#[tokio::test]
async fn pull_image_returns_full_body_after_connect_get_disconnect() {
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
    assert!(*s.got_descriptor.lock().await);
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
        #[allow(
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
fn img_descriptor_header_encodes_tlv_with_tag_and_length() {
    match img_descriptor_header() {
        Header::Other { id, value } => {
            assert_eq!(id, 0x71);
            match value {
                HeaderValue::Bytes(bytes) => {
                    assert_eq!(bytes[0], 0x71);
                    assert_eq!(bytes[1] as usize, IMG_DESCRIPTOR_XML.len());
                    assert_eq!(&bytes[2..], IMG_DESCRIPTOR_XML.as_bytes());
                }
                other => panic!("expected Bytes header value, got {other:?}"),
            }
        }
        other => panic!("expected Other header, got {other:?}"),
    }
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

#[tokio::test]
async fn fetch_cover_art_via_uses_injected_channel_and_returns_body() {
    struct FakeChannel {
        payload: Vec<u8>,
        requested_address: Arc<Mutex<Option<String>>>,
    }
    #[async_trait::async_trait]
    impl BipChannel for FakeChannel {
        async fn open(&self, address: &BtAddress) -> Result<BipStream, BipError> {
            *self.requested_address.lock().await = Some(address.to_string());
            let stub = new_stub(Ok(self.payload.clone()));
            let (client_half, server_half) = tokio::io::duplex(65_536);
            drop(spawn_stub_server(stub, server_half));
            Ok(Box::new(client_half) as BipStream)
        }
    }

    let requested_address = Arc::new(Mutex::new(None));
    let channel = FakeChannel {
        payload: body_bytes(512, 0xCD),
        requested_address: Arc::clone(&requested_address),
    };
    let address = BtAddress::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    let result = fetch_cover_art_via(&channel, &address, "0000003", BipImageType::Thumbnail).await;

    assert_eq!(result.unwrap(), body_bytes(512, 0xCD));
    assert_eq!(
        requested_address.lock().await.as_deref(),
        Some("11:22:33:44:55:66")
    );
}

#[tokio::test]
async fn fetch_cover_art_via_propagates_channel_failures() {
    struct FailingChannel;
    #[async_trait::async_trait]
    impl BipChannel for FailingChannel {
        async fn open(&self, _address: &BtAddress) -> Result<BipStream, BipError> {
            Err(BipError::service_not_found("AA:BB:CC:DD:EE:FF"))
        }
    }
    let address = BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    let result = fetch_cover_art_via(&FailingChannel, &address, "0000001", BipImageType::Native)
        .await
        .expect_err("failing channel should propagate");
    assert!(result.is_service_not_found());
}
