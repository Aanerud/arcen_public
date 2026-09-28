//! End-to-end proof that a client can reach the macOS Pier.
//!
//! This dials the real listener over QUIC with the real certificate, using the
//! same `connect_direct` the Deck uses, and completes the real handshake. It
//! is not a mock: the only thing it stands in for is the Deck's user
//! interface.

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::Duration;

use arcen_pier_macos::host_cert::MaterialPaths;
use arcen_pier_macos::net::Listener;
use arcen_transport::cert_provisioning::ProvisioningRequest;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::{Message, Role};

/// Trusts exactly the certificate the Pier was given, the way a pinned Deck
/// does. Nothing else is accepted.
#[derive(Debug)]
struct PinnedServer {
    expected: Vec<u8>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.expected.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "certificate is not the pinned one".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // TLS 1.2 is below the transport's floor and never negotiated here.
        Err(rustls::Error::General("TLS 1.2 is not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(hosts) = manifest.parent() else {
        panic!("manifest path has no hosts parent: {}", manifest.display());
    };
    let Some(root) = hosts.parent() else {
        panic!(
            "manifest path has no repository parent: {}",
            manifest.display()
        );
    };
    let base = root
        .join("target")
        .join(format!("arcen-e2e-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&base).expect("temp dir");
    base
}

/// Completes the hello exchange and answers the credential prompt.
///
/// Returns the `auth_result` the Pier sent back.
/// Completes the production handshake: credentials first, then hello.
///
/// This deliberately uses the shared `AuthResponse` type rather than
/// hand-written JSON. An earlier version of this helper sent a `password`
/// field, which no host reads — the Deck puts the password in `credential` —
/// so it agreed with a matching bug on the host and proved nothing. Building
/// the message the way the Deck builds it is what makes this a real test.
async fn hello_and_authenticate(
    client: &mut WebSocketStream<arcen_transport::quic::DirectQuicStream>,
    username: &str,
    password: &str,
) -> serde_json::Value {
    // The Pier must ask for credentials first. A `server_hello` here would
    // put the real Deck on its no-authentication path and zeroize the
    // password it was given, so the order is asserted, not assumed.
    let request = tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .expect("auth_request in time")
        .expect("frame")
        .expect("valid");
    let Message::Text(request) = request else {
        panic!("expected a text auth_request");
    };
    let request: arcen_protocol::messages::AuthRequest =
        serde_json::from_str(&request).expect("a decodable auth_request");
    assert_eq!(request.msg_type, arcen_protocol::messages::AUTH_REQUEST);
    assert!(
        request.auth_methods.iter().any(|method| method == "pam"),
        "the host must offer the method it actually implements",
    );

    let response = arcen_protocol::messages::AuthResponse::pam(username, password);
    client
        .send(Message::Text(
            serde_json::to_string(&response).expect("encode").into(),
        ))
        .await
        .expect("auth_response");

    let result = tokio::time::timeout(Duration::from_secs(20), client.next())
        .await
        .expect("auth_result in time")
        .expect("frame")
        .expect("valid");
    let Message::Text(result) = result else {
        panic!("expected a text auth_result");
    };
    serde_json::from_str(&result).expect("json")
}

/// These tests share real system resources: `ScreenCaptureKit` serves one
/// stream at a time, and there is a single pasteboard and a single cursor.
/// Run in parallel they interfere and fail in ways that look like product
/// bugs, so anything touching the desktop takes this first.
static DESKTOP: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn exclusive_desktop() -> std::sync::MutexGuard<'static, ()> {
    DESKTOP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Server-side hello without the credential gate.
///
/// Used by the video and input tests, which are exercising streaming rather
/// than authentication. The credential gate has its own test.
async fn hello_only(
    socket: &mut arcen_pier_macos::net::PierSocket,
) -> arcen_pier_macos::session::AdvertisedCapabilities {
    let offered = arcen_pier_macos::session::advertise().expect("a display to offer");
    arcen_pier_macos::net::send_json(
        socket,
        arcen_pier_macos::session::server_hello_json(&offered, "e2e"),
    )
    .await
    .expect("server_hello");
    let reply = arcen_pier_macos::net::receive_json(socket)
        .await
        .expect("receive")
        .expect("a client_hello");
    assert!(reply.contains(arcen_protocol::messages::CLIENT_HELLO));
    offered
}

/// Client-side half of [`hello_only`].
async fn client_hello_only(client: &mut WebSocketStream<arcen_transport::quic::DirectQuicStream>) {
    let _hello = tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .expect("server_hello in time")
        .expect("frame")
        .expect("valid");
    client
        .send(Message::Text(
            serde_json::json!({ "type": arcen_protocol::messages::CLIENT_HELLO })
                .to_string()
                .into(),
        ))
        .await
        .expect("client_hello");
}

/// Dials the Pier with its own certificate pinned.
async fn connect_pinned(
    certificate: &std::path::Path,
    bound: std::net::SocketAddr,
) -> WebSocketStream<arcen_transport::quic::DirectQuicStream> {
    let certificate_pem = std::fs::read(certificate).expect("read certificate");
    let client_config = pinned_client_config(
        &certificate_pem,
        vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()],
    );
    let endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("address")).expect("endpoint");
    let stream =
        arcen_transport::quic::connect_direct(arcen_transport::quic::DirectQuicDialParams {
            endpoint,
            client_config,
            remote_addr: bound,
            server_name: "localhost",
        })
        .await
        .expect("connect");
    WebSocketStream::from_raw_socket(stream, Role::Client, None).await
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn a_second_client_is_refused_while_a_session_is_active() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("session-admission");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");
    let admission = arcen_session::session_admission::SessionAdmissionRuntime::new();

    let server = tokio::spawn(async move {
        let first = listener
            .accept_with_session_admission(&admission)
            .await
            .expect("first accept");
        let arcen_pier_macos::net::SessionAdmissionAccept::Admitted(first) = first else {
            panic!("first client must be admitted");
        };

        let second = listener
            .accept_with_session_admission(&admission)
            .await
            .expect("second accept");
        let arcen_pier_macos::net::SessionAdmissionAccept::Refused { reason, .. } = second else {
            panic!("second client must be refused while the lease is held");
        };
        assert_eq!(reason, "a session is already active");

        drop(first);

        let third = listener
            .accept_with_session_admission(&admission)
            .await
            .expect("third accept");
        let arcen_pier_macos::net::SessionAdmissionAccept::Admitted(_third) = third else {
            panic!("dropping the first session must release the admission lease");
        };
        reason
    });

    let first = connect_pinned(&paths.certificate, bound).await;
    let mut second = connect_pinned(&paths.certificate, bound).await;
    let refused = tokio::time::timeout(Duration::from_secs(10), second.next())
        .await
        .expect("second client receives an explicit refusal")
        .expect("second client sees a frame")
        .expect("second frame is valid");
    let Message::Close(Some(frame)) = refused else {
        panic!("second client must receive a close frame with a reason, got {refused:?}");
    };
    assert_eq!(frame.reason.as_ref(), "a session is already active");

    drop(first);
    let _third = connect_pinned(&paths.certificate, bound).await;
    let reason = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server finishes")
        .expect("server task did not panic");
    assert_eq!(reason, "a session is already active");
    println!("e2e admission: second client refused with reason: {reason}");

    std::fs::remove_dir_all(&dir).ok();
}

fn pinned_client_config(certificate_pem: &[u8], alpn: Vec<Vec<u8>>) -> quinn::ClientConfig {
    let expected = rustls_pemfile::certs(&mut &certificate_pem[..])
        .next()
        .expect("a certificate")
        .expect("parses")
        .as_ref()
        .to_vec();
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServer { expected }))
        .with_no_client_auth();
    crypto.alpn_protocols = alpn;
    quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).expect("quic crypto"),
    ))
}

/// Awaits the Pier while the client keeps draining its socket.
///
/// A test that stops reading and then waits for the stream to finish deadlocks
/// against itself: the transport stops accepting writes once nobody drains
/// them, so the frame budget is never spent and the session never ends. It
/// looks exactly like a hung Pier. Both tests that wait for a settled side
/// effect — a pasteboard write, a pointer move — used to break out of their
/// read loop the moment they saw it, and were only passing while the effect
/// happened to take most of the window.
async fn finish_while_draining(
    server: tokio::task::JoinHandle<arcen_pier_macos::stream::StreamStats>,
    client: &mut WebSocketStream<arcen_transport::quic::DirectQuicStream>,
    budget: Duration,
) -> arcen_pier_macos::stream::StreamStats {
    let drain = async { while client.next().await.is_some() {} };
    tokio::time::timeout(budget, async {
        tokio::pin!(drain);
        tokio::select! {
            result = server => result,
            () = &mut drain => panic!("the client's socket closed before the Pier finished"),
        }
    })
    .await
    .expect("the Pier finishes")
    .expect("no panic")
}

/// A clipboard negotiation that allows everything, for tests exercising the
/// clipboard path rather than the policy intersection.
fn test_clipboard() -> Option<arcen_media::clipboard::ClipboardNegotiation> {
    arcen_media::clipboard::ClipboardNegotiation::resolve(
        arcen_media::clipboard::ClipboardPolicy::default(),
        true,
        arcen_media::clipboard::ClipboardRequest {
            protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
            text: arcen_media::clipboard::ClipboardDirections::both(),
            image: arcen_media::clipboard::ClipboardDirections::both(),
        },
    )
}

#[tokio::test]
async fn a_client_connects_over_quic_and_completes_the_handshake() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("handshake");

    // Real material, produced by the real provisioning path.
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision certificate");
    let paths = MaterialPaths::in_directory(&dir);

    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config from real material");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound address");

    let user = "e2e-user".to_owned();
    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept a client");
        // Returns the offer so the client's view can be checked against it.
        let offered = arcen_pier_macos::session::advertise().expect("a display");
        arcen_pier_macos::net::send_json(
            &mut socket,
            arcen_pier_macos::session::server_hello_json(&offered, &user),
        )
        .await
        .expect("server_hello");
        let reply = arcen_pier_macos::net::receive_json(&mut socket)
            .await
            .expect("receive")
            .expect("a client_hello");
        assert!(reply.contains(arcen_protocol::messages::CLIENT_HELLO));
        offered
    });

    let certificate_pem = std::fs::read(&paths.certificate).expect("read certificate");
    let client_config = pinned_client_config(
        &certificate_pem,
        vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()],
    );
    let endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("address")).expect("client endpoint");

    let stream =
        arcen_transport::quic::connect_direct(arcen_transport::quic::DirectQuicDialParams {
            endpoint,
            client_config,
            remote_addr: bound,
            server_name: "localhost",
        })
        .await
        .expect("client connects to the Pier");

    let mut client = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;

    // The Pier speaks first.
    let hello = tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .expect("server_hello arrives in time")
        .expect("a frame")
        .expect("a valid frame");
    let Message::Text(hello) = hello else {
        panic!("expected a text server_hello, got {hello:?}");
    };
    let parsed: arcen_protocol::messages::ServerHelloMsg =
        serde_json::from_str(&hello).expect("the Deck must be able to parse server_hello");
    assert_eq!(parsed.msg_type, arcen_protocol::messages::SERVER_HELLO);
    assert!(
        parsed.screen_width > 0 && parsed.screen_height > 0,
        "the Pier must offer a real display, got {}x{}",
        parsed.screen_width,
        parsed.screen_height
    );
    assert_eq!(parsed.os_user, "e2e-user");

    client
        .send(Message::Text(
            serde_json::json!({ "type": arcen_protocol::messages::CLIENT_HELLO })
                .to_string()
                .into(),
        ))
        .await
        .expect("client_hello sends");

    let offered = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the Pier finishes in time")
        .expect("the server task did not panic");
    assert_eq!(offered.width, parsed.screen_width);
    assert!(offered.hevc || offered.h264);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn frames_reach_the_client_and_a_full_frame_request_forces_a_keyframe() {
    let _desktop = exclusive_desktop();
    // The point of the whole exercise: a client that connects must actually
    // receive pictures, not just a handshake.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("frames");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    const WANTED: u64 = 120;
    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let offered = hello_only(&mut socket).await;
        let capture = arcen_pier_macos::capture::CaptureConfig::sdr(offered.display_id, 64, 64, 60);
        arcen_pier_macos::stream::stream(
            &mut socket,
            arcen_pier_macos::stream::StreamSession {
                capture,
                codec: arcen_pier_macos::encode::EncoderCodec::Hevc,
                frame_budget: Some(WANTED),
                input_bounds: arcen_pier_macos::input::DesktopBounds::new(
                    0.0,
                    0.0,
                    f64::from(offered.width),
                    f64::from(offered.height),
                ),
                telemetry: arcen_pier_macos::observability::HostTelemetry::disabled(),
                session_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([9; 16]),
                audio: None,
                clipboard: test_clipboard(),
                cursor_mode: arcen_protocol::messages::CursorMode::Local,
                audio_channel: None,
                audio_encoding: arcen_pier_macos::stream::AudioEncoding::Pcm,
                motion_priority: arcen_media::video::MotionPriority::Detail,
                input_mode_results: Default::default(),
                path_signal_connection: None,
            },
        )
        .await
        .expect("stream")
    });

    let certificate_pem = std::fs::read(&paths.certificate).expect("read certificate");
    let client_config = pinned_client_config(
        &certificate_pem,
        vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()],
    );
    let endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("address")).expect("endpoint");
    let stream =
        arcen_transport::quic::connect_direct(arcen_transport::quic::DirectQuicDialParams {
            endpoint,
            client_config,
            remote_addr: bound,
            server_name: "localhost",
        })
        .await
        .expect("connect");
    let mut client = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;

    // Hello, then read pictures. The credential gate has its own test.
    client_hello_only(&mut client).await;

    let mut received = 0_u64;
    let mut keyframes = 0_u64;
    let mut payload_bytes = 0_usize;
    let mut full_frame_requested = false;
    while received < WANTED {
        let message = tokio::time::timeout(Duration::from_secs(20), client.next())
            .await
            .expect("a frame arrives in time")
            .expect("a frame")
            .expect("a valid frame");
        let Message::Binary(bytes) = message else {
            continue;
        };
        let header = arcen_protocol::wire::decode_video_header(&bytes)
            .expect("every frame must carry a header the Deck can decode");
        assert_eq!(
            header.frame_type,
            arcen_protocol::wire::FrameType::VideoH265
        );
        assert_eq!(header.codec, arcen_protocol::wire::VideoCodec::H265);
        assert!(
            bytes.len() > arcen_protocol::wire::VIDEO_HEADER_SIZE,
            "a header with no picture behind it is not a frame"
        );
        // Annex B: every access unit starts with a start code.
        let payload = &bytes[arcen_protocol::wire::VIDEO_HEADER_SIZE..];
        assert_eq!(
            &payload[..4],
            &[0, 0, 0, 1],
            "payload must be Annex B for the Deck decoder"
        );
        if header.is_keyframe() {
            keyframes += 1;
        }
        payload_bytes += payload.len();
        received += 1;
        if received == 20 && !full_frame_requested {
            client
                .send(Message::Text(
                    serde_json::json!({ "type": arcen_protocol::messages::REQUEST_FULL_FRAME })
                        .to_string()
                        .into(),
                ))
                .await
                .expect("request_full_frame sends");
            full_frame_requested = true;
        }
    }

    let stats = tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the Pier finishes")
        .expect("no panic");
    assert_eq!(stats.frames_sent, WANTED);
    assert_eq!(
        stats.full_frame_requests, 1,
        "the Pier must receive the recovery request"
    );
    assert!(
        keyframes >= 2,
        "the stream normally starts with one keyframe; request_full_frame must force another"
    );
    assert!(payload_bytes > 0);
    println!(
        "e2e: {received} frames, {keyframes} keyframe(s), {payload_bytes} payload bytes, \
         {:.1} fps sent, {:.2} ms mean, {:.2} ms worst | stages: capture-wait {:.2} ms, \
         encode {:.2} ms, send {:.2} ms",
        stats.sent_fps,
        stats.mean_frame_ms,
        stats.max_frame_ms,
        stats.mean_capture_wait_ms,
        stats.mean_encode_ms,
        stats.mean_send_ms
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn idle_desktop_full_frame_request_reencodes_the_last_frame_as_a_keyframe() {
    let _desktop = exclusive_desktop();
    struct FrameTimeoutOverride;
    impl Drop for FrameTimeoutOverride {
        fn drop(&mut self) {
            // SAFETY: this guard is only constructed while `DESKTOP` is held,
            // so no other streaming test can observe the process-wide change.
            unsafe {
                std::env::remove_var("ARCEN_FRAME_TIMEOUT_MS");
            }
        }
    }
    // SAFETY: desktop-touching tests take `DESKTOP`, so no other streaming
    // test reads this process-wide knob while this test owns it.
    unsafe {
        std::env::set_var("ARCEN_FRAME_TIMEOUT_MS", "500");
    }
    let _frame_timeout_override = FrameTimeoutOverride;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("idle-full-frame");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let offered = hello_only(&mut socket).await;
        let capture = arcen_pier_macos::capture::CaptureConfig::sdr(
            offered.display_id,
            offered.width as usize,
            offered.height as usize,
            1,
        );
        arcen_pier_macos::stream::stream(
            &mut socket,
            arcen_pier_macos::stream::StreamSession {
                capture,
                codec: arcen_pier_macos::encode::EncoderCodec::Hevc,
                frame_budget: Some(1_000),
                cursor_mode: arcen_protocol::messages::CursorMode::Local,
                audio_channel: None,
                audio_encoding: arcen_pier_macos::stream::AudioEncoding::Pcm,
                motion_priority: arcen_media::video::MotionPriority::Detail,
                input_mode_results: Default::default(),
                path_signal_connection: None,
                input_bounds: arcen_pier_macos::input::DesktopBounds::new(
                    0.0,
                    0.0,
                    f64::from(offered.width),
                    f64::from(offered.height),
                ),
                telemetry: arcen_pier_macos::observability::HostTelemetry::disabled(),
                session_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([12; 16]),
                audio: None,
                clipboard: None,
            },
        )
        .await
        .expect("stream")
    });

    let mut client = connect_pinned(&paths.certificate, bound).await;
    client_hello_only(&mut client).await;

    let mut received_before_idle = 0_u64;
    let first_header = loop {
        let message = tokio::time::timeout(Duration::from_secs(20), client.next())
            .await
            .expect("initial frame arrives")
            .expect("a frame")
            .expect("a valid frame");
        let Message::Binary(bytes) = message else {
            continue;
        };
        received_before_idle += 1;
        let header = arcen_protocol::wire::decode_video_header(&bytes).expect("video header");
        assert!(
            header.is_keyframe(),
            "the first access unit must bootstrap the decoder"
        );
        break header;
    };
    assert!(first_header.is_keyframe());

    let requested_at = std::time::Instant::now();
    client
        .send(Message::Text(
            serde_json::json!({ "type": arcen_protocol::messages::REQUEST_FULL_FRAME })
                .to_string()
                .into(),
        ))
        .await
        .expect("request_full_frame sends");

    let second = tokio::time::timeout(Duration::from_secs(8), client.next())
        .await
        .expect("idle recovery keyframe arrives within the bounded window")
        .expect("a frame")
        .expect("a valid frame");
    let Message::Binary(second) = second else {
        panic!("expected a recovery video frame");
    };
    let second_header = arcen_protocol::wire::decode_video_header(&second).expect("video header");
    assert!(
        second_header.is_keyframe(),
        "a full-frame request on a still desktop must force a recovery keyframe"
    );
    let recovery_ms = requested_at.elapsed().as_millis();

    client.close(None).await.expect("client close");
    let stats = tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the Pier finishes")
        .expect("no panic");
    assert_eq!(stats.full_frame_requests, 1);
    assert!(
        stats.keyframes >= 2,
        "the stream should have its initial keyframe plus the idle recovery keyframe"
    );
    assert!(
        stats.frames_encoded > stats.frames_captured,
        "idle recovery must re-encode the retained last frame rather than wait for fresh damage"
    );
    println!(
        "idle e2e: requested recovery after idle, keyframe in {recovery_ms} ms; \
         saw {received_before_idle} frame(s) before idle; captured {}, encoded {}, sent {}, \
         keyframes {}",
        stats.frames_captured, stats.frames_encoded, stats.frames_sent, stats.keyframes
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[ignore = "moves the real pointer on the machine running it"]
#[tokio::test]
async fn input_sent_during_a_stream_moves_the_real_pointer() {
    let _desktop = exclusive_desktop();
    // A desktop you can see but not drive is not a desktop. This sends a
    // pointer move over the live session and checks the physical cursor.
    let _ = rustls::crypto::ring::default_provider().install_default();
    if arcen_pier_macos::input::current_pointer_position().is_none() {
        // No window server session to drive; nothing to assert against.
        return;
    }
    let dir = temp_dir("input");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    let original = arcen_pier_macos::input::current_pointer_position();
    // The expected landing point has to come from the real display, not a
    // guessed 1920x1080: this Mac reports 1800x1169.
    let offered = arcen_pier_macos::session::advertise().expect("a display to offer");
    let expected_x = f64::from(offered.width) * 0.25;
    let expected_y = f64::from(offered.height) * 0.25;

    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let offered = hello_only(&mut socket).await;
        let capture = arcen_pier_macos::capture::CaptureConfig::sdr(
            offered.display_id,
            offered.width as usize,
            offered.height as usize,
            60,
        );
        arcen_pier_macos::stream::stream(
            &mut socket,
            arcen_pier_macos::stream::StreamSession {
                capture,
                codec: arcen_pier_macos::encode::EncoderCodec::Hevc,
                frame_budget: Some(30),
                input_bounds: arcen_pier_macos::input::DesktopBounds::new(
                    0.0,
                    0.0,
                    f64::from(offered.width),
                    f64::from(offered.height),
                ),
                telemetry: arcen_pier_macos::observability::HostTelemetry::disabled(),
                session_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([9; 16]),
                audio: None,
                clipboard: test_clipboard(),
                cursor_mode: arcen_protocol::messages::CursorMode::Local,
                audio_channel: None,
                audio_encoding: arcen_pier_macos::stream::AudioEncoding::Pcm,
                motion_priority: arcen_media::video::MotionPriority::Detail,
                input_mode_results: Default::default(),
                path_signal_connection: None,
            },
        )
        .await
        .expect("stream")
    });

    let mut client = connect_pinned(&paths.certificate, bound).await;
    client_hello_only(&mut client).await;

    // Drive the pointer to a known place, a quarter into the desktop.
    client
        .send(Message::Text(
            serde_json::json!({
                "type": "mouse_move",
                "x": 0.25, "y": 0.25,
                "server_x": 0, "server_y": 0,
                "sequence": 1, "timestamp_ns": 0
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("mouse_move sends");

    // Keep reading frames so the session stays alive while it settles.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut moved = false;
    while std::time::Instant::now() < deadline {
        if let Some((x, y)) = arcen_pier_macos::input::current_pointer_position() {
            if (x - expected_x).abs() <= 2.0 && (y - expected_y).abs() <= 2.0 {
                moved = true;
                break;
            }
        }
        let _ = tokio::time::timeout(Duration::from_millis(200), client.next()).await;
    }

    let stats = finish_while_draining(server, &mut client, Duration::from_secs(20)).await;
    assert!(
        stats.input.applied >= 1,
        "the session must have applied the client's input, stats: {:?}",
        stats.input
    );
    assert_eq!(stats.input.malformed, 0);
    assert!(
        moved,
        "the client's pointer move must reach the real cursor; expected ({expected_x:.1}, \
         {expected_y:.1}), observed {:?}",
        arcen_pier_macos::input::current_pointer_position()
    );
    println!(
        "e2e input: applied {}, out of order {}, cursor reached ({expected_x:.1}, {expected_y:.1})",
        stats.input.applied, stats.input.out_of_order
    );

    // Put the cursor back exactly where the person left it. The previous
    // version called `probe` with guessed 1920x1080 bounds, which saved and
    // restored the *current* position rather than the original one — it moved
    // the cursor a second time and left it wherever the test had put it.
    if let Some((x, y)) = original {
        let bounds = arcen_pier_macos::input::DesktopBounds::new(
            0.0,
            0.0,
            f64::from(offered.width),
            f64::from(offered.height),
        );
        if let Ok(mut restore) = arcen_pier_macos::input::InputController::new(bounds) {
            let _ = restore.pointer_motion(&arcen_input::PointerMotion {
                x: x / f64::from(offered.width),
                y: y / f64::from(offered.height),
                server_x: None,
                server_y: None,
                metadata: arcen_input::LowLatencyMetadata::default(),
            });
        }
        if let Some((back_x, back_y)) = arcen_pier_macos::input::current_pointer_position() {
            assert!(
                (back_x - x).abs() < 2.0 && (back_y - y).abs() < 2.0,
                "the cursor must be returned to ({x:.1}, {y:.1}), not left at \
                 ({back_x:.1}, {back_y:.1})"
            );
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn the_pier_asks_for_credentials_before_it_greets() {
    // The Deck chooses how a session works from the *first* message it
    // receives. `auth_request` selects its authenticated path; `server_hello`
    // selects its no-authentication path and zeroizes the password it was
    // given. A host that greets first therefore does not merely greet early —
    // it tells every real client that no credentials are wanted.
    //
    // This needs no valid account, because the order is visible before any
    // credential is sent.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("auth-order");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        arcen_pier_macos::session::perform(&mut socket, "e2e").await
    });

    let mut client = connect_pinned(&paths.certificate, bound).await;
    let first = tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .expect("a first message in time")
        .expect("frame")
        .expect("valid");
    let Message::Text(first) = first else {
        panic!("the handshake is text");
    };
    let parsed: serde_json::Value = serde_json::from_str(&first).expect("json");

    assert_eq!(
        parsed["type"],
        arcen_protocol::messages::AUTH_REQUEST,
        "the Pier's first message must be auth_request, not {}",
        parsed["type"],
    );
    assert_ne!(
        parsed["type"],
        arcen_protocol::messages::SERVER_HELLO,
        "greeting first puts the Deck on its no-authentication path",
    );

    drop(client);
    let _ = tokio::time::timeout(Duration::from_secs(20), server).await;
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn an_unauthenticated_client_gets_no_desktop() {
    // Reaching the port and pinning the certificate proves which machine the
    // client is talking to. It does not prove anyone may use it.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("auth");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        arcen_pier_macos::session::perform(&mut socket, "e2e").await
    });

    let mut client = connect_pinned(&paths.certificate, bound).await;
    let result = hello_and_authenticate(&mut client, "root", "wrong-on-purpose").await;

    assert_eq!(
        result["success"], false,
        "a wrong password must not yield a session"
    );
    // The client is told it failed, never which half was wrong.
    let message = result["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        !message.contains("no such") && !message.contains("unknown user"),
        "the refusal must not reveal whether the account exists: {message}"
    );

    let outcome = tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the Pier finishes")
        .expect("no panic");
    assert!(
        outcome.is_err(),
        "the handshake must fail rather than returning a session"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[ignore = "overwrites the real pasteboard on the machine running it"]
#[tokio::test]
async fn a_client_clipboard_reaches_the_real_pasteboard() {
    let _desktop = exclusive_desktop();
    // Clipboard is only a feature if it crosses the wire into the desktop.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("clipboard");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let offered = hello_only(&mut socket).await;
        let capture = arcen_pier_macos::capture::CaptureConfig::sdr(
            offered.display_id,
            offered.width as usize,
            offered.height as usize,
            60,
        );
        arcen_pier_macos::stream::stream(
            &mut socket,
            arcen_pier_macos::stream::StreamSession {
                capture,
                codec: arcen_pier_macos::encode::EncoderCodec::Hevc,
                frame_budget: Some(40),
                input_bounds: arcen_pier_macos::input::DesktopBounds::new(
                    0.0,
                    0.0,
                    f64::from(offered.width),
                    f64::from(offered.height),
                ),
                telemetry: arcen_pier_macos::observability::HostTelemetry::disabled(),
                session_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([9; 16]),
                audio: None,
                clipboard: test_clipboard(),
                cursor_mode: arcen_protocol::messages::CursorMode::Local,
                audio_channel: None,
                audio_encoding: arcen_pier_macos::stream::AudioEncoding::Pcm,
                motion_priority: arcen_media::video::MotionPriority::Detail,
                input_mode_results: Default::default(),
                path_signal_connection: None,
            },
        )
        .await
        .expect("stream")
    });

    let mut client = connect_pinned(&paths.certificate, bound).await;
    client_hello_only(&mut client).await;

    const TEXT: &str = "arcen clipboard crossed the wire";
    // Framed exactly as the Deck frames it: a `clipboard_data` offer, then
    // chunks that begin with `FrameType::Clipboard`. The previous version of
    // this test sent the offer by hand and the payload bare, which no Deck
    // does — so it agreed with a matching bug on the host and proved nothing.
    let offer = arcen_protocol::messages::ClipboardDataMsg::new(
        1,
        arcen_protocol::messages::ClipboardContentKind::TextUtf8,
        u32::try_from(TEXT.len()).expect("fits"),
        false,
    );
    client
        .send(Message::Text(
            serde_json::to_string(&offer).expect("encode").into(),
        ))
        .await
        .expect("clipboard offer");

    let chunk = arcen_protocol::encode_clipboard_chunk(
        arcen_protocol::ClipboardChunkHeader {
            kind: arcen_protocol::messages::ClipboardContentKind::TextUtf8,
            sequence: 1,
            total_size: u32::try_from(TEXT.len()).expect("fits"),
            offset: 0,
        },
        TEXT.as_bytes(),
    )
    .expect("frame the chunk");
    client
        .send(Message::Binary(chunk.into()))
        .await
        .expect("clipboard payload");

    // Keep the session alive while the offer is applied.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut applied = false;
    while std::time::Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(200), client.next()).await;
        if arcen_pier_macos::clipboard::Pasteboard::general()
            .ok()
            .and_then(|board| board.read())
            == Some(arcen_pier_macos::clipboard::ClipboardPayload::Text(
                TEXT.to_owned(),
            ))
        {
            applied = true;
            break;
        }
    }
    assert!(applied, "the clipboard never reached the pasteboard");

    let stats = finish_while_draining(server, &mut client, Duration::from_secs(25)).await;
    assert_eq!(
        stats.clipboard.received, 1,
        "the session must have applied the client's clipboard, stats: {:?}",
        stats.clipboard
    );
    assert_eq!(stats.clipboard.refused, 0);

    let landed = arcen_pier_macos::clipboard::Pasteboard::general()
        .expect("pasteboard")
        .read();
    assert_eq!(
        landed,
        Some(arcen_pier_macos::clipboard::ClipboardPayload::Text(
            TEXT.to_owned()
        )),
        "the client's text must be on the real pasteboard"
    );
    println!(
        "e2e clipboard: received {}, refused {}",
        stats.clipboard.received, stats.clipboard.refused
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_client_without_the_right_alpn_is_refused() {
    // A browser or generic QUIC client must be rejected during the handshake,
    // before it can speak to the session layer at all.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("alpn");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    let server = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

    let certificate_pem = std::fs::read(&paths.certificate).expect("read certificate");
    let client_config = pinned_client_config(&certificate_pem, vec![b"h3".to_vec()]);
    let endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("address")).expect("endpoint");

    let outcome =
        arcen_transport::quic::connect_direct(arcen_transport::quic::DirectQuicDialParams {
            endpoint,
            client_config,
            remote_addr: bound,
            server_name: "localhost",
        })
        .await;
    assert!(
        outcome.is_err(),
        "a client offering the wrong ALPN must not get a session"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    std::fs::remove_dir_all(&dir).ok();
}

/// Streams a real desktop and reads the records the host wrote about it.
///
/// This is the test that makes "latency is fine" checkable rather than
/// asserted. It streams genuine captured, encoded frames and then reads the
/// canonical log the host produced, so a regression that leaves the numbers
/// at zero — a counter never incremented, a snapshot never emitted, a sink
/// never flushed — fails here instead of being discovered by an operator
/// looking at an empty file during an incident.
#[tokio::test]
async fn a_streamed_session_records_what_it_measured() {
    let _desktop = exclusive_desktop();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("telemetry");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    // A one-second cadence so a short stream still produces a snapshot. At the
    // production five seconds this test would finish before the first one and
    // prove nothing.
    unsafe {
        std::env::set_var("ARCEN_HEALTH_SNAPSHOT_SECS", "1");
    }

    const WANTED: u64 = 90;
    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let offered = hello_only(&mut socket).await;
        let capture = arcen_pier_macos::capture::CaptureConfig::sdr(
            offered.display_id,
            offered.width as usize,
            offered.height as usize,
            60,
        );
        arcen_pier_macos::stream::stream(
            &mut socket,
            arcen_pier_macos::stream::StreamSession {
                capture,
                codec: arcen_pier_macos::encode::EncoderCodec::Hevc,
                frame_budget: Some(WANTED),
                input_bounds: arcen_pier_macos::input::DesktopBounds::new(
                    0.0,
                    0.0,
                    f64::from(offered.width),
                    f64::from(offered.height),
                ),
                telemetry: arcen_pier_macos::observability::HostTelemetry::disabled(),
                session_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([11; 16]),
                audio: None,
                clipboard: test_clipboard(),
                cursor_mode: arcen_protocol::messages::CursorMode::Local,
                audio_channel: None,
                audio_encoding: arcen_pier_macos::stream::AudioEncoding::Pcm,
                motion_priority: arcen_media::video::MotionPriority::Detail,
                input_mode_results: Default::default(),
                path_signal_connection: None,
            },
        )
        .await
        .expect("stream")
    });

    let certificate_pem = std::fs::read(&paths.certificate).expect("read certificate");
    let client_config = pinned_client_config(
        &certificate_pem,
        vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()],
    );
    let endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("address")).expect("endpoint");
    let stream =
        arcen_transport::quic::connect_direct(arcen_transport::quic::DirectQuicDialParams {
            endpoint,
            client_config,
            remote_addr: bound,
            server_name: "localhost",
        })
        .await
        .expect("connect");
    let mut client = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;
    client_hello_only(&mut client).await;

    let mut received = 0_u64;
    while received < WANTED {
        match tokio::time::timeout(Duration::from_secs(20), client.next()).await {
            Ok(Some(Ok(Message::Binary(_)))) => received += 1,
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    let stats = server.await.expect("join");

    // The measurements have to be real numbers, not defaults. A mean encode
    // time of exactly zero after ninety encoded frames means the timing was
    // never taken, which reads in a log as an impossibly fast host.
    assert!(stats.frames_sent > 0, "no frames were sent");
    assert_eq!(
        stats.frames_sent, received,
        "every sent frame reached the client"
    );
    assert!(
        stats.mean_encode_ms > 0.0,
        "encode time was never measured: {stats:?}",
    );
    assert!(
        stats.mean_frame_ms > 0.0,
        "capture-to-socket time was never measured: {stats:?}",
    );
    assert!(
        stats.max_frame_ms >= stats.mean_frame_ms,
        "the worst frame cannot be faster than the mean: {stats:?}",
    );
    assert!(stats.sent_fps > 0.0, "frame rate was never computed");
    eprintln!(
        "measured: {} frames, {:.1} fps, capture-wait {:.2} ms, encode {:.2} ms, \
         send {:.2} ms, worst {:.2} ms",
        stats.frames_sent,
        stats.sent_fps,
        stats.mean_capture_wait_ms,
        stats.mean_encode_ms,
        stats.mean_send_ms,
        stats.max_frame_ms,
    );
}

/// Sends real pen samples over the wire and checks the host injected them.
///
/// Basic Tablet cannot be exercised with the Deck's own `input-smoke`: that
/// sends pen only in Hard USB mode, which this host refuses. So the samples are
/// sent here as the Deck would send them in light mode, which is the only way
/// the host's pen termination gets proven end to end rather than by unit test.
///
/// A stroke, not a single sample: proximity in, tip down, three moves with
/// changing pressure and tilt, tip up, proximity out. Each of those is a
/// different edge in the shared planner, and a host that handled only the
/// steady-state samples would still pass a one-sample test.
#[ignore = "injects real tablet events on the machine running it"]
#[tokio::test]
async fn pen_samples_sent_by_a_client_are_injected_as_tablet_events() {
    let _desktop = exclusive_desktop();
    let _ = rustls::crypto::ring::default_provider().install_default();
    if arcen_pier_macos::input::InputController::new(arcen_pier_macos::input::DesktopBounds::new(
        0.0, 0.0, 100.0, 100.0,
    ))
    .is_err()
    {
        // No window server session to drive; the conversion has unit tests.
        return;
    }
    let dir = temp_dir("pen");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    const WANTED: u64 = 60;
    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let offered = hello_only(&mut socket).await;
        let capture = arcen_pier_macos::capture::CaptureConfig::sdr(
            offered.display_id,
            offered.width as usize,
            offered.height as usize,
            60,
        );
        arcen_pier_macos::stream::stream(
            &mut socket,
            arcen_pier_macos::stream::StreamSession {
                capture,
                codec: arcen_pier_macos::encode::EncoderCodec::Hevc,
                frame_budget: Some(WANTED),
                input_bounds: arcen_pier_macos::input::DesktopBounds::new(
                    0.0,
                    0.0,
                    f64::from(offered.width),
                    f64::from(offered.height),
                ),
                telemetry: arcen_pier_macos::observability::HostTelemetry::disabled(),
                session_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([21; 16]),
                audio: None,
                clipboard: test_clipboard(),
                cursor_mode: arcen_protocol::messages::CursorMode::Local,
                audio_channel: None,
                audio_encoding: arcen_pier_macos::stream::AudioEncoding::Pcm,
                motion_priority: arcen_media::video::MotionPriority::Detail,
                input_mode_results: Default::default(),
                path_signal_connection: None,
            },
        )
        .await
        .expect("stream")
    });

    let mut client = connect_pinned(&paths.certificate, bound).await;
    client_hello_only(&mut client).await;

    // A stroke, in the order a digitizer reports one.
    let stroke = [
        (0.40, 0.40, 0.0_f32, false, true),
        (0.41, 0.41, 0.25, true, true),
        (0.42, 0.42, 0.60, true, true),
        (0.43, 0.43, 0.90, true, true),
        (0.44, 0.44, 0.0, false, true),
        (0.44, 0.44, 0.0, false, false),
    ];
    for (index, (x, y, pressure, touching, in_proximity)) in stroke.iter().enumerate() {
        client
            .send(Message::Text(
                serde_json::json!({
                    "type": "pen_event",
                    "x": x, "y": y,
                    "pressure": pressure,
                    "tilt_x_degrees": 30.0, "tilt_y_degrees": -15.0,
                    "rotation_degrees": 12.0,
                    "tool": "tip",
                    "in_proximity": in_proximity,
                    "touching": touching,
                    "buttons": 0,
                    "server_x": 0, "server_y": 0,
                    "sequence": index as u64 + 1,
                    "timestamp_ns": 0
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("pen_event sends");
        // Let the host drain between samples; a burst would still be ordered
        // but would not exercise the per-sample path the way a stroke does.
        let _ = tokio::time::timeout(Duration::from_millis(60), client.next()).await;
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(200), client.next()).await;
    }
    drop(client);

    let stats = tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the Pier finishes")
        .expect("no panic");

    assert_eq!(
        stats.input.pen_rejected, 0,
        "every sample was inside its physical range, stats: {:?}",
        stats.input
    );
    assert!(
        stats.input.pen_samples >= stroke.len() as u64,
        "every pen sample must be injected, stats: {:?}",
        stats.input
    );
    // Entering and leaving proximity are the two edges that a host handling
    // only steady-state samples would silently drop, stranding the tool.
    assert!(
        stats.input.pen_proximity_edges >= 2,
        "proximity in and out must both be posted, stats: {:?}",
        stats.input
    );
    println!(
        "e2e pen: {} samples, {} proximity edges, {} rejected",
        stats.input.pen_samples, stats.input.pen_proximity_edges, stats.input.pen_rejected,
    );
}

/// Streams with host audio attached and checks packets reach the client.
///
/// This is the half the lab machine cannot show while its system audio consent
/// is unanswered. Here the grant exists — `probe-audio` reports callbacks and
/// a non-zero peak — so a failure is the wire path, not the permission.
///
/// The session is started with the capture already running, the way the agent
/// collects one that finished starting in the background, so the test
/// exercises the same handoff rather than a shape only tests use.
#[tokio::test]
async fn host_audio_reaches_the_client_when_capture_is_available() {
    let _desktop = exclusive_desktop();
    let _ = rustls::crypto::ring::default_provider().install_default();

    // A machine with no capturable audio is a legitimate build agent, and a
    // test that asserted otherwise would fail for the wrong reason.
    let Ok(mut audio) = arcen_pier_macos::audio::AudioCaptureSession::start(
        arcen_session::pier_config::LocalPlayback::Muted,
    ) else {
        return;
    };

    let dir = temp_dir("audio-wire");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    const WANTED: u64 = 45;
    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let offered = hello_only(&mut socket).await;
        let capture = arcen_pier_macos::capture::CaptureConfig::sdr(
            offered.display_id,
            offered.width as usize,
            offered.height as usize,
            60,
        );
        let stats = arcen_pier_macos::stream::stream(
            &mut socket,
            arcen_pier_macos::stream::StreamSession {
                capture,
                codec: arcen_pier_macos::encode::EncoderCodec::Hevc,
                frame_budget: Some(WANTED),
                input_bounds: arcen_pier_macos::input::DesktopBounds::new(
                    0.0,
                    0.0,
                    f64::from(offered.width),
                    f64::from(offered.height),
                ),
                telemetry: arcen_pier_macos::observability::HostTelemetry::disabled(),
                session_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([31; 16]),
                audio: Some(&mut audio),
                clipboard: test_clipboard(),
                cursor_mode: arcen_protocol::messages::CursorMode::Local,
                audio_channel: None,
                audio_encoding: arcen_pier_macos::stream::AudioEncoding::Pcm,
                motion_priority: arcen_media::video::MotionPriority::Detail,
                input_mode_results: Default::default(),
                path_signal_connection: None,
            },
        )
        .await
        .expect("stream");
        let _ = audio.stop();
        stats
    });

    let mut client = connect_pinned(&paths.certificate, bound).await;
    client_hello_only(&mut client).await;

    // Count audio frames by their wire type rather than by size: a video frame
    // and an audio packet are both binary, and distinguishing them by length
    // would pass on the wrong thing.
    // Read until the host finishes its frame budget and closes, rather than
    // leaving as soon as both counters move: an early drop is a PeerGone the
    // host reports as a failed session, which would fail this test for a
    // reason that has nothing to do with audio.
    let mut audio_frames = 0_u64;
    let mut video_frames = 0_u64;
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), client.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => match bytes.first().copied() {
                Some(byte) if byte == arcen_protocol::wire::FrameType::Audio as u8 => {
                    audio_frames += 1;
                }
                _ => video_frames += 1,
            },
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_)) | None) => break,
            Err(_) => {}
        }
        if video_frames >= WANTED {
            break;
        }
    }

    let stats = tokio::time::timeout(Duration::from_secs(30), server)
        .await
        .expect("the Pier finishes")
        .expect("no panic");

    println!(
        "e2e audio: {audio_frames} audio frames and {video_frames} video frames reached the \
         client; host counted {} audio packets sent",
        stats.audio_packets_sent,
    );
    assert!(
        stats.audio_packets_sent > 0,
        "the host must have sent audio when a capture session was attached",
    );
    assert!(
        audio_frames > 0,
        "audio packets must reach the client, not merely be counted by the host",
    );
}

/// Captures every attached display at once and checks each delivers frames.
///
/// This is the capability the multi-monitor gate waits on. Advertising
/// `multi_monitor_v1` needs a topology negotiation and region-framed video on
/// top; none of that is worth writing while the question underneath it — can
/// this host run two `ScreenCaptureKit` streams at once and tell their frames
/// apart — is unanswered.
///
/// Skips on a machine with one display rather than failing: a single-screen
/// build agent is a legitimate place to run the suite.
#[tokio::test]
async fn every_attached_display_can_be_captured_at_once() {
    let _desktop = exclusive_desktop();
    let Ok(displays) = arcen_pier_macos::displays::probe() else {
        return;
    };
    if displays.len() < 2 {
        println!(
            "e2e multi-capture: skipped, {} display(s) attached",
            displays.len()
        );
        return;
    }

    let capture = match arcen_pier_macos::multi_capture::MultiDisplayCapture::start(&displays, 60) {
        Ok(capture) => capture,
        Err(error) => panic!("every attached display must capture: {error}"),
    };
    assert_eq!(
        capture.len(),
        displays.len(),
        "one stream per display, or none",
    );

    // Each monitor has to produce a frame of its own size. A single stream
    // answering for every display would pass a frame count and fail this.
    let mut delivered = Vec::new();
    for monitor in capture.monitors() {
        let frame = monitor
            .next_frame(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!("display {} delivered no frame: {error}", monitor.display_id)
            });
        delivered.push((
            monitor.monitor_index,
            monitor.display_id,
            frame.width,
            frame.height,
        ));
    }

    for (index, display_id, width, height) in &delivered {
        let expected = displays
            .iter()
            .find(|display| display.display_id == *display_id)
            .expect("the display was captured, so it was attached");
        assert_eq!(
            (*width, *height),
            (
                expected.pixel_width as usize,
                expected.pixel_height as usize
            ),
            "monitor {index} (display {display_id}) delivered a frame of the wrong size, \
             which is what a single stream answering for every display looks like",
        );
    }
    let indices: Vec<u8> = delivered.iter().map(|(index, ..)| *index).collect();
    let mut unique = indices.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        indices.len(),
        "each monitor needs its own index for frame headers to be separable",
    );

    println!(
        "e2e multi-capture: {} displays captured concurrently: {:?}",
        delivered.len(),
        delivered,
    );
}

/// A 30 fps request must deliver about 30 fps from a 1080p desktop.
///
/// What this does prove: capture, encode and the writer overlap well enough
/// that a session reaches its requested cadence, and that the host's own
/// reported rate agrees with what the client counted.
///
/// What it cannot prove, and must not be read as proving: that a slow writer
/// no longer stalls capture. Sending over loopback to a reader in the same
/// process costs about 0.05 ms per frame, where the recorded lab session spent
/// 25.84 ms. The condition that made that session run at 15 fps does not exist
/// here, and this test passed before the stages were split as well as after.
/// Treat it as a floor that would catch a regression into something much
/// worse, not as evidence about backpressure.
#[tokio::test]
async fn a_thirty_fps_session_delivers_near_thirty_fps() {
    let _desktop = exclusive_desktop();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("fps");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    const WANTED: u64 = 90;
    const REQUESTED_FPS: u32 = 30;
    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let offered = hello_only(&mut socket).await;
        let capture = arcen_pier_macos::capture::CaptureConfig::sdr(
            offered.display_id,
            1920,
            1080,
            REQUESTED_FPS,
        );
        arcen_pier_macos::stream::stream(
            &mut socket,
            arcen_pier_macos::stream::StreamSession {
                capture,
                codec: arcen_pier_macos::encode::EncoderCodec::H264,
                frame_budget: Some(WANTED),
                input_bounds: arcen_pier_macos::input::DesktopBounds::new(
                    0.0,
                    0.0,
                    f64::from(offered.width),
                    f64::from(offered.height),
                ),
                telemetry: arcen_pier_macos::observability::HostTelemetry::disabled(),
                session_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([11; 16]),
                audio: None,
                clipboard: test_clipboard(),
                cursor_mode: arcen_protocol::messages::CursorMode::Local,
                audio_channel: None,
                audio_encoding: arcen_pier_macos::stream::AudioEncoding::Pcm,
                motion_priority: arcen_media::video::MotionPriority::Detail,
                input_mode_results: Default::default(),
                path_signal_connection: None,
            },
        )
        .await
        .expect("stream")
    });

    let certificate_pem = std::fs::read(&paths.certificate).expect("read certificate");
    let client_config = pinned_client_config(
        &certificate_pem,
        vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()],
    );
    let endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("address")).expect("endpoint");
    let stream =
        arcen_transport::quic::connect_direct(arcen_transport::quic::DirectQuicDialParams {
            endpoint,
            client_config,
            remote_addr: bound,
            server_name: "localhost",
        })
        .await
        .expect("connect");
    let mut client = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;
    client_hello_only(&mut client).await;

    // Timed from the first picture, so the measurement is of streaming rather
    // than of session start-up.
    let mut received = 0_u64;
    let mut first_frame_at = None;
    while received < WANTED {
        let message = tokio::time::timeout(Duration::from_secs(30), client.next())
            .await
            .expect("a frame arrives in time")
            .expect("a frame")
            .expect("a valid frame");
        if matches!(message, Message::Binary(_)) {
            first_frame_at.get_or_insert_with(std::time::Instant::now);
            received += 1;
        }
    }
    let elapsed = first_frame_at.expect("a first frame").elapsed();
    let delivered = (received - 1) as f64 / elapsed.as_secs_f64();
    let stats = server.await.expect("join");
    eprintln!(
        "delivered {delivered:.2} fps over {elapsed:?} | host {:.2} fps captured={} encoded={} sent={} suppressed={} | capture_wait={:.2}ms encode={:.2}ms send={:.2}ms frame={:.2}ms worst={:.2}ms",
        stats.sent_fps,
        stats.frames_captured,
        stats.frames_encoded,
        stats.frames_sent,
        stats.frames_suppressed,
        stats.mean_capture_wait_ms,
        stats.mean_encode_ms,
        stats.mean_send_ms,
        stats.mean_frame_ms,
        stats.max_frame_ms,
    );

    // A desktop that is not changing does not produce frames, and this test
    // does not own the screen of the machine it runs on. Tell the two apart by
    // what the host waited for: a capture wait far longer than the frame
    // interval is a still desktop, not a pipeline that cannot keep up.
    let frame_interval_ms = 1000.0 / f64::from(REQUESTED_FPS);
    if stats.mean_capture_wait_ms > frame_interval_ms * 1.5 {
        eprintln!(
            "source delivered every {:.1} ms against a {frame_interval_ms:.1} ms interval; \
             the desktop was idle, so this run proves the accounting, not the rate",
            stats.mean_capture_wait_ms
        );
        return;
    }
    assert!(
        delivered > 22.0,
        "a 30 fps request delivered {delivered:.2} fps while the source was delivering \
         every {:.1} ms, so the pipeline and not the desktop is behind",
        stats.mean_capture_wait_ms
    );
    assert!(
        stats.frames_sent >= WANTED,
        "the host must account for every frame the client counted"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `ScreenCaptureKit` must actually hand this host its dirty rectangles.
///
/// Damage-driven scheduling is worth nothing if the attachment never arrives,
/// and the failure would be silent: every frame would read as "unknown", the
/// host would conservatively treat all of them as fully changed, and the only
/// evidence would be bandwidth that never drops on a still desktop.
#[test]
fn capture_carries_the_compositor_s_damage() {
    let _desktop = exclusive_desktop();
    let display = unsafe { objc2_core_graphics::CGMainDisplayID() };
    let session = arcen_pier_macos::capture::CaptureSession::start(
        arcen_pier_macos::capture::CaptureConfig::sdr(display, 1280, 720, 30),
    )
    .expect("capture starts");

    let mut known = 0_u32;
    let mut unknown = 0_u32;
    let mut total_rects = 0_usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while known + unknown < 20 && std::time::Instant::now() < deadline {
        let Ok(frame) = session.next_frame(Duration::from_millis(500)) else {
            continue;
        };
        match &frame.damage {
            arcen_pier_macos::capture::FrameDamage::Rects(rects) => {
                known += 1;
                total_rects += rects.len();
                for rect in rects {
                    assert!(
                        rect.x <= frame.width as u32 && rect.y <= frame.height as u32,
                        "a damage rectangle at {},{} is outside a {}x{} surface",
                        rect.x,
                        rect.y,
                        frame.width,
                        frame.height
                    );
                }
            }
            arcen_pier_macos::capture::FrameDamage::Unknown => unknown += 1,
        }
    }
    session.stop();

    eprintln!("damage: {known} frames carried rects ({total_rects} total), {unknown} unknown");
    assert!(
        known > 0,
        "no frame carried damage; scheduling on it would save nothing"
    );
}

/// A still desktop must stop costing bandwidth.
///
/// This is the whole purpose of damage tracking, and the only way to see it is
/// to leave a session running over a desktop nobody is touching and count what
/// it sends. Without suppression a 30 fps session sends 30 pictures a second of
/// an image that has not changed; with it, the keepalive.
///
/// Reports rather than asserts a rate when the desktop turns out to be busy:
/// this runs on a real machine whose screen the test does not control, and a
/// moving desktop legitimately produces frames.
#[tokio::test]
async fn a_still_desktop_sends_almost_nothing() {
    let _desktop = exclusive_desktop();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = temp_dir("idle");
    arcen_pier_macos::host_cert::provision(
        &dir,
        ProvisioningRequest::Ensure,
        &["localhost".to_owned()],
        1_800_000_000,
    )
    .expect("provision");
    let paths = MaterialPaths::in_directory(&dir);
    let config = arcen_pier_macos::net::server_config(&paths.certificate, &paths.key)
        .expect("server config");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("address"), config).expect("bind");
    let bound = listener.local_addr().expect("bound");

    const OBSERVE: Duration = Duration::from_secs(4);
    const REQUESTED_FPS: u32 = 30;
    let server = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let offered = hello_only(&mut socket).await;
        let capture = arcen_pier_macos::capture::CaptureConfig::sdr(
            offered.display_id,
            1280,
            720,
            REQUESTED_FPS,
        );
        arcen_pier_macos::stream::stream(
            &mut socket,
            arcen_pier_macos::stream::StreamSession {
                capture,
                codec: arcen_pier_macos::encode::EncoderCodec::H264,
                frame_budget: Some(10_000),
                input_bounds: arcen_pier_macos::input::DesktopBounds::new(
                    0.0,
                    0.0,
                    f64::from(offered.width),
                    f64::from(offered.height),
                ),
                telemetry: arcen_pier_macos::observability::HostTelemetry::disabled(),
                session_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([13; 16]),
                audio: None,
                clipboard: test_clipboard(),
                cursor_mode: arcen_protocol::messages::CursorMode::Local,
                audio_channel: None,
                audio_encoding: arcen_pier_macos::stream::AudioEncoding::Pcm,
                motion_priority: arcen_media::video::MotionPriority::Detail,
                input_mode_results: Default::default(),
                path_signal_connection: None,
            },
        )
        .await
    });

    let certificate_pem = std::fs::read(&paths.certificate).expect("read certificate");
    let client_config = pinned_client_config(
        &certificate_pem,
        vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()],
    );
    let endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("address")).expect("endpoint");
    let stream =
        arcen_transport::quic::connect_direct(arcen_transport::quic::DirectQuicDialParams {
            endpoint,
            client_config,
            remote_addr: bound,
            server_name: "localhost",
        })
        .await
        .expect("connect");
    let mut client = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;
    client_hello_only(&mut client).await;

    // Count only after the first picture, so the baseline frame every session
    // owes is not counted against the idle budget.
    let mut frames = 0_u64;
    let mut bytes = 0_usize;
    let deadline = tokio::time::Instant::now() + OBSERVE;
    let mut started = None;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(message))) = tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            client.next(),
        )
        .await
        else {
            break;
        };
        if let Message::Binary(payload) = message {
            if started.is_none() {
                started = Some(tokio::time::Instant::now());
                continue;
            }
            frames += 1;
            bytes += payload.len();
        }
    }
    let observed = started.map_or(Duration::ZERO, |at| at.elapsed());
    let rate = frames as f64 / observed.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "idle desktop: {frames} frames in {observed:?} ({rate:.2} fps, {} bytes, {:.1} kbps)",
        bytes,
        (bytes as f64 * 8.0) / observed.as_secs_f64().max(f64::EPSILON) / 1000.0
    );
    drop(client);
    let stats = server.await.expect("join");
    std::fs::remove_dir_all(&dir).ok();
    let stats = match stats {
        Ok(stats) => stats,
        Err(ended) => ended.stats,
    };
    eprintln!(
        "host: captured={} sent={} suppressed={}",
        stats.frames_captured, stats.frames_sent, stats.frames_suppressed
    );

    // Every frame the host took is either sent or suppressed. Accounting that
    // does not balance means frames are going missing somewhere between the
    // compositor and the wire, which is worth failing over whatever the screen
    // happened to be doing.
    assert!(
        stats.frames_captured >= stats.frames_suppressed,
        "suppressed {} of {} captured frames, which cannot be right",
        stats.frames_suppressed,
        stats.frames_captured
    );

    // The screen on this machine is not the test's to control, so a moving
    // desktop legitimately produces frames and is reported rather than failed.
    if stats.frames_suppressed == 0 {
        eprintln!(
            "desktop was busy for the whole run ({rate:.2} fps); \
             this run proves the accounting, not the saving"
        );
        return;
    }
    assert!(
        rate < f64::from(REQUESTED_FPS) * 0.9,
        "the host suppressed {} frames yet still sent {rate:.2} fps",
        stats.frames_suppressed
    );
}
