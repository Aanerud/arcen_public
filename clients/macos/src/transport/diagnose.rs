// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The connection test behind `arcen-deck diagnose` and **Test connection**.
//!
//! This is the macOS adapter only: it resolves, probes, dials and reads, and
//! hands what it saw to [`arcen_session::connection_diagnostics`], which owns
//! the steps, findings and wording. It never sends a user name or password:
//! the only thing written to the Pier is the QUIC/TLS handshake, the control
//! lane's opening, and one empty UDP datagram.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arcen_protocol::messages::{msg_type, AuthRequest, ServerHelloMsg, AUTH_REQUEST, SERVER_HELLO};
use arcen_session::connection_diagnostics::{
    assess, classify_udp_probe_error, AddressAttempt, DiagnosticReport, DiagnosticTarget,
    ExpectedIdentity, ExpectedPin, GreetingObservation, HandshakeOutcome, Observations,
    PathObservation, PinKind, PresentedCertificate, ResolutionObservation, RoundTripObservation,
    UdpProbe,
};
use arcen_telemetry::QosTargets;
use arcen_transport::quic::{
    connect_session_lanes, lane_transport_config_arc, DirectQuicDialParams, LaneRuntimeConfig,
    MtuDiscovery, QuicTransportError, SessionLaneSetupError, SessionLaneSocket,
    DIRECT_QUIC_ALPN_PROTOCOL,
};
use arcen_transport::tls::TlsPin;
use arcen_transport::BoundedTransportPolicy;
use tokio::time::timeout;

use crate::transport::lanes::DeckLaneSocket;
use crate::transport::tls::{
    diagnostic_quic_rustls_config, format_fingerprint, is_tofu_pin_mismatch_message,
    DiagnosticCapture,
};
use crate::transport::websocket::{recv_json, ConnectSmokeError};

/// How long the resolver may take.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one address may take to complete the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(6);
/// How long the Pier may take to ask for sign-in.
pub const GREETING_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to listen for an ICMP refusal of the probe datagram.
const PROBE_WAIT: Duration = Duration::from_millis(600);
/// Most addresses tried for one name.
const MAX_ADDRESSES: usize = 4;
/// How long a connected test waits for path MTU discovery, when it is on.
const MTU_SEARCH_WAIT: Duration = Duration::from_millis(1_500);

/// Where an expected pin came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOrigin {
    /// This Deck's `trusted_pins.json`.
    Remembered,
    /// Given for this run.
    Supplied,
}

/// What to test.
#[derive(Debug, Clone)]
pub struct DiagnoseRequest {
    pub target: DiagnosticTarget,
    pub pin: Option<(TlsPin, PinOrigin)>,
}

impl DiagnoseRequest {
    /// Tests `target`, expecting the identity this Deck remembers for it.
    #[must_use]
    pub fn with_remembered_pin(target: DiagnosticTarget) -> Self {
        let pin = remembered_pin(&target).map(|pin| (pin, PinOrigin::Remembered));
        Self { target, pin }
    }
}

/// The identity this Deck remembers for `target`, keyed as the Deck keys it.
#[must_use]
pub fn remembered_pin(target: &DiagnosticTarget) -> Option<TlsPin> {
    let path = crate::ui::app::trusted_pins_path()?;
    crate::ui::trusted_pins::load(&path)
        .resolved()
        .get(&format!("{}:{}", target.host, target.port))
        .copied()
}

/// One line naming this Deck build, for the report header.
#[must_use]
pub fn deck_label() -> String {
    let build = crate::build_identity::current();
    format!(
        "Arcen Deck {} (build {}, {})",
        build.version, build.build_id, build.source_revision
    )
}

/// Runs every step and assesses the result.
pub async fn run(request: DiagnoseRequest) -> DiagnosticReport {
    let observations = observe(&request).await;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let report = assess(&observations, now, QosTargets::default());
    tracing::info!(
        target: crate::logging::target::TRANSPORT,
        endpoint = %request.target.display(),
        codes = %report
            .steps
            .iter()
            .map(|step| step.finding.code())
            .collect::<Vec<_>>()
            .join(","),
        "connection test finished",
    );
    report
}

async fn observe(request: &DiagnoseRequest) -> Observations {
    let target = request.target.clone();
    let expected_identity = match request.pin {
        None => ExpectedIdentity::Unknown,
        Some((pin, origin)) => {
            let expected = ExpectedPin {
                kind: match pin.kind {
                    arcen_transport::tls::PinKind::CertificateSha256 => PinKind::Certificate,
                    arcen_transport::tls::PinKind::SubjectPublicKeyInfoSha256 => PinKind::Spki,
                },
                digest: format_fingerprint(&pin.digest),
            };
            match origin {
                PinOrigin::Remembered => ExpectedIdentity::Remembered(expected),
                PinOrigin::Supplied => ExpectedIdentity::Supplied(expected),
            }
        }
    };
    let mut observations = Observations {
        resolution: ResolutionObservation::NoAddresses,
        attempts: Vec::new(),
        expected_identity,
        certificate: None,
        greeting: None,
        path: PathObservation::default(),
        round_trip: RoundTripObservation::default(),
        target,
    };

    let started = Instant::now();
    let addresses = match timeout(
        RESOLVE_TIMEOUT,
        tokio::net::lookup_host((request.target.host.as_str(), request.target.port)),
    )
    .await
    {
        Err(_) => {
            observations.resolution = ResolutionObservation::TimedOut {
                waited_ms: millis(RESOLVE_TIMEOUT),
            };
            return observations;
        }
        Ok(Err(error)) => {
            observations.resolution = ResolutionObservation::Failed {
                detail: error.to_string(),
            };
            return observations;
        }
        Ok(Ok(addresses)) => {
            let mut unique = Vec::new();
            for address in addresses {
                if !unique.contains(&address) {
                    unique.push(address);
                }
            }
            unique
        }
    };
    if addresses.is_empty() {
        return observations;
    }
    observations.resolution = ResolutionObservation::Resolved {
        addresses: addresses.iter().map(SocketAddr::ip).collect(),
        elapsed_ms: millis(started.elapsed()),
    };

    let (interface_kind, interface_mtu) = crate::netinfo::outgoing_interface(addresses[0].ip());
    observations.path = PathObservation {
        interface_kind,
        interface_mtu,
        quic_mtu: None,
    };

    for address in addresses.into_iter().take(MAX_ADDRESSES) {
        let probe = udp_probe(address).await;
        if probe != UdpProbe::NoError {
            // The network already answered for this address: closed port or
            // no route. A handshake would only wait out its timeout.
            observations.attempts.push(AddressAttempt {
                address,
                outcome: HandshakeOutcome::Failed {
                    detail: String::new(),
                }
                .with_probe(probe),
            });
            continue;
        }
        let dial = dial(address, request).await;
        if let Some(capture) = dial.capture {
            observations.certificate = Some(presented(capture));
        }
        let outcome = dial.outcome.with_probe(probe);
        let stop = matches!(
            outcome,
            HandshakeOutcome::Completed { .. } | HandshakeOutcome::PinMismatch
        );
        observations
            .attempts
            .push(AddressAttempt { address, outcome });
        if let Some(mut socket) = dial.socket {
            observations.greeting = Some(read_greeting(&mut socket).await);
            if MtuDiscovery::from_env() == MtuDiscovery::On {
                // Discovery probes one size per round trip after the
                // handshake; give the search a moment to finish.
                tokio::time::sleep(MTU_SEARCH_WAIT).await;
            }
            observations.path.quic_mtu = Some(u32::from(
                socket.lanes().connection().stats().path.current_mtu,
            ));
            observations.round_trip = RoundTripObservation {
                smoothed_ms: Some(socket.lanes().connection().rtt().as_secs_f64() * 1_000.0),
            };
            // Leave like a client that walks away from the sign-in screen: no
            // session record (a Pier reads any record there as the answer to
            // its sign-in request), just the QUIC close with a reason.
            socket
                .lanes()
                .connection()
                .close(quinn::VarInt::from_u32(0), b"connection test");
        }
        if stop {
            break;
        }
    }
    observations
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn presented(capture: DiagnosticCapture) -> PresentedCertificate {
    PresentedCertificate {
        certificate_sha256: capture.info.certificate_sha256_display,
        spki_sha256: capture.info.spki_sha256_display,
        not_before_epoch_secs: capture.info.not_before_epoch_secs,
        not_after_epoch_secs: capture.info.not_after_epoch_secs,
        system_trusted: capture.system_trusted,
        leaf_problem: capture.leaf_problem,
    }
}

/// Sends one empty datagram on a connected socket and listens briefly: a
/// closed port answers with ICMP port unreachable, which a connected socket
/// reports as a refusal.
async fn udp_probe(address: SocketAddr) -> UdpProbe {
    let bind = if address.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0_u16; 8], 0))
    };
    let Ok(socket) = tokio::net::UdpSocket::bind(bind).await else {
        return UdpProbe::NoError;
    };
    if let Err(error) = socket.connect(address).await {
        return classify_udp_probe_error(&error, address);
    }
    if let Err(error) = socket.send(&[]).await {
        return classify_udp_probe_error(&error, address);
    }
    let mut buffer = [0_u8; 64];
    match timeout(PROBE_WAIT, socket.recv(&mut buffer)).await {
        Ok(Err(error)) => classify_udp_probe_error(&error, address),
        Ok(Ok(_)) | Err(_) => UdpProbe::NoError,
    }
}

struct Dial {
    outcome: HandshakeOutcome,
    capture: Option<DiagnosticCapture>,
    socket: Option<DeckLaneSocket>,
}

async fn dial(address: SocketAddr, request: &DiagnoseRequest) -> Dial {
    let failed = |detail: String| Dial {
        outcome: HandshakeOutcome::Failed { detail },
        capture: None,
        socket: None,
    };
    let (rustls_config, slot) = match diagnostic_quic_rustls_config(
        request.pin.map(|(pin, _)| pin),
        DIRECT_QUIC_ALPN_PROTOCOL,
    ) {
        Ok(config) => config,
        Err(error) => return failed(error.to_string()),
    };
    let crypto = match quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config) {
        Ok(crypto) => crypto,
        Err(error) => return failed(format!("QUIC TLS configuration is unavailable: {error}")),
    };
    let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));
    client_config.transport_config(lane_transport_config_arc(
        &BoundedTransportPolicy::default(),
        arcen_protocol::lanes::LaneEndpoint::Deck,
    ));
    let bind = if address.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0_u16; 8], 0))
    };
    let endpoint = match quinn::Endpoint::client(bind) {
        Ok(endpoint) => endpoint,
        Err(error) => return failed(format!("could not open a UDP socket: {error}")),
    };
    let started = Instant::now();
    let result = timeout(
        HANDSHAKE_TIMEOUT,
        connect_session_lanes(
            DirectQuicDialParams {
                endpoint: endpoint.clone(),
                client_config,
                remote_addr: address,
                server_name: request.target.host.as_str(),
            },
            LaneRuntimeConfig::new(arcen_protocol::lanes::LaneEndpoint::Deck),
        ),
    )
    .await;
    let capture = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let (outcome, socket) = match result {
        Err(_) => (
            HandshakeOutcome::TimedOut {
                waited_ms: millis(HANDSHAKE_TIMEOUT),
            },
            None,
        ),
        Ok(Ok((lanes, control))) => (
            HandshakeOutcome::Completed {
                handshake_ms: millis(started.elapsed()),
            },
            Some(DeckLaneSocket::new(
                SessionLaneSocket::new(lanes, control),
                endpoint,
            )),
        ),
        Ok(Err(error)) => (handshake_failure(&error), None),
    };
    Dial {
        outcome,
        capture,
        socket,
    }
}

fn handshake_failure(error: &SessionLaneSetupError) -> HandshakeOutcome {
    let message = error.to_string();
    if is_tofu_pin_mismatch_message(&message) {
        return HandshakeOutcome::PinMismatch;
    }
    match error {
        SessionLaneSetupError::Mismatch(mismatch) => HandshakeOutcome::TransportMismatch {
            detail: mismatch.message(),
        },
        SessionLaneSetupError::Connect(
            QuicTransportError::EstablishmentTimedOut
            | QuicTransportError::Connection(quinn::ConnectionError::TimedOut),
        )
        | SessionLaneSetupError::Timeout => HandshakeOutcome::TimedOut {
            waited_ms: millis(HANDSHAKE_TIMEOUT),
        },
        SessionLaneSetupError::Connect(QuicTransportError::Endpoint(io)) => {
            io_outcome(io, &message)
        }
        _ => HandshakeOutcome::Failed { detail: message },
    }
}

fn io_outcome(error: &std::io::Error, message: &str) -> HandshakeOutcome {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused => HandshakeOutcome::Refused,
        _ => HandshakeOutcome::Failed {
            detail: message.to_owned(),
        },
    }
}

async fn read_greeting(socket: &mut DeckLaneSocket) -> GreetingObservation {
    let first = match recv_json(socket, GREETING_TIMEOUT).await {
        Ok(first) => first,
        Err(ConnectSmokeError::Timeout) => {
            return GreetingObservation::TimedOut {
                waited_ms: millis(GREETING_TIMEOUT),
            };
        }
        Err(ConnectSmokeError::ClosedByHost(reason)) => {
            return GreetingObservation::Closed {
                detail: if reason.is_empty() {
                    "closed without a reason".to_owned()
                } else {
                    reason.trim_start_matches(": ").to_owned()
                },
            };
        }
        Err(other) => {
            return GreetingObservation::Closed {
                detail: other.to_string(),
            };
        }
    };
    match msg_type(&first).unwrap_or("<missing>") {
        AUTH_REQUEST => match serde_json::from_value::<AuthRequest>(first) {
            Ok(request) => GreetingObservation::SignIn {
                resume_supported: request.supports_resume(),
                max_monitors: request
                    .multi_monitor_v1_offer()
                    .map(arcen_protocol::messages::AuthMultiMonitorOfferMsg::max_monitors),
                disclaimer: request.disclaimer.is_some(),
                auth_mode: request.auth_mode,
                auth_methods: request.auth_methods,
            },
            Err(error) => GreetingObservation::Unexpected {
                message_type: format!("auth_request that could not be read ({error})"),
            },
        },
        SERVER_HELLO => match serde_json::from_value::<ServerHelloMsg>(first) {
            Ok(hello) => GreetingObservation::NoSignIn {
                server_name: hello.server_name,
                version: hello.version,
            },
            Err(error) => GreetingObservation::Unexpected {
                message_type: format!("server_hello that could not be read ({error})"),
            },
        },
        other => GreetingObservation::Unexpected {
            message_type: other.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_session::connection_diagnostics::Finding;
    use futures_util::{SinkExt, StreamExt};
    use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    /// A Pier that grants the attachment, sends `greeting` and reports the
    /// first record the Deck sends afterwards (`None` when it only closes).
    async fn fake_pier(
        greeting: &'static str,
    ) -> (
        u16,
        CertificateDer<'static>,
        tokio::task::JoinHandle<Option<arcen_protocol::lanes::SessionRecord>>,
    ) {
        let key_pair = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params.self_signed(&key_pair).unwrap();
        let certificate_der: CertificateDer<'static> = certificate.der().clone();
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        let mut rustls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate_der.clone()], private_key)
            .unwrap();
        rustls_config.alpn_protocols = vec![DIRECT_QUIC_ALPN_PROTOCOL.to_vec()];
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config).unwrap();
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        server_config.transport_config(lane_transport_config_arc(
            &BoundedTransportPolicy::default(),
            arcen_protocol::lanes::LaneEndpoint::Pier,
        ));
        arcen_transport::quic::apply_direct_server_limits(&mut server_config);
        let endpoint =
            quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let port = endpoint.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let connection = endpoint.accept().await?.await.ok()?;
            let (lanes, control) = arcen_transport::quic::accept_session_lanes(
                connection,
                LaneRuntimeConfig::new(arcen_protocol::lanes::LaneEndpoint::Pier),
                Duration::from_secs(5),
            )
            .await
            .ok()?;
            let mut socket = SessionLaneSocket::new(lanes, control);
            socket
                .send(arcen_protocol::lanes::SessionRecord::Text(greeting.into()))
                .await
                .ok()?;
            match timeout(Duration::from_secs(5), socket.next()).await {
                Ok(Some(Ok(record))) => Some(record),
                _ => None,
            }
        });
        (port, certificate_der, task)
    }

    fn localhost(port: u16) -> DiagnosticTarget {
        DiagnosticTarget::parse(&format!("localhost:{port}")).unwrap()
    }

    #[tokio::test]
    async fn a_reachable_pier_is_tested_up_to_sign_in_and_sent_nothing() {
        let (port, certificate, pier) = fake_pier(
            r#"{"type":"auth_request","auth_methods":["password","resume"],"challenge":"c","salt":"s"}"#,
        )
        .await;
        let pin = TlsPin::new(
            arcen_transport::tls::PinKind::CertificateSha256,
            crate::transport::tls::fingerprint_sha256(certificate.as_ref()),
        );
        let report = run(DiagnoseRequest {
            target: localhost(port),
            pin: Some((pin, PinOrigin::Supplied)),
        })
        .await;
        let findings: Vec<_> = report.steps.iter().map(|step| step.finding).collect();
        assert_eq!(
            &findings[..4],
            &[
                Finding::DnsResolved,
                Finding::UdpHandshakeCompleted,
                Finding::CertSupplied,
                Finding::GreetingSignIn,
            ],
            "{}",
            report.render_text("test")
        );
        assert_ne!(findings[5], Finding::NotRun);
        assert_eq!(
            pier.await.unwrap(),
            None,
            "the test must never answer the sign-in request"
        );
    }

    #[tokio::test]
    async fn a_changed_identity_stops_at_the_certificate() {
        let (port, _certificate, _pier) = fake_pier(r#"{"type":"auth_request"}"#).await;
        let report = run(DiagnoseRequest {
            target: localhost(port),
            pin: Some((
                TlsPin::new(
                    arcen_transport::tls::PinKind::SubjectPublicKeyInfoSha256,
                    [7; 32],
                ),
                PinOrigin::Remembered,
            )),
        })
        .await;
        assert_eq!(report.steps[1].finding, Finding::UdpHandshakeCompleted);
        assert_eq!(report.steps[2].finding, Finding::CertMismatch);
        assert_eq!(report.steps[3].finding, Finding::NotRun);
        assert_eq!(report.exit_code(), 2);
    }

    #[tokio::test]
    async fn a_first_connection_reads_the_greeting_without_trusting_the_pier() {
        let (port, _certificate, _pier) = fake_pier(r#"{"type":"quality_settings"}"#).await;
        let report = run(DiagnoseRequest {
            target: localhost(port),
            pin: None,
        })
        .await;
        assert_eq!(report.steps[2].finding, Finding::CertFirstUse);
        assert_eq!(report.steps[3].finding, Finding::GreetingUnexpected);
    }

    #[tokio::test]
    async fn a_closed_local_port_is_reported_as_refused_not_as_a_timeout() {
        // Bind and drop a UDP socket to find a port nothing listens on.
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let target = DiagnosticTarget::parse(&format!("127.0.0.1:{port}")).unwrap();
        let report = run(DiagnoseRequest { target, pin: None }).await;
        assert_eq!(
            report.steps[1].finding,
            Finding::UdpRefused,
            "{}",
            report.render_text("test")
        );
    }
}
