//! QUIC listener for the macOS Pier.
//!
//! This is what makes the host reachable. A Deck dials UDP 18444 over QUIC with
//! TLS 1.3, the connection is pinned to Arcen's own ALPN so plain HTTP/3 and
//! WebSocket clients are rejected at the crypto layer, and the session then
//! speaks the same framed messages every other Pier does.
//!
//! Framing is WebSocket over the QUIC stream rather than raw bytes, because
//! that is what the Deck and the other two hosts already speak; the transport
//! changed, the message vocabulary did not.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::time::Instant as TokioInstant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, Role};

use arcen_transport::quic::DirectQuicStream;

/// The only port an Arcen host listens on.
pub const DEFAULT_PORT: u16 = 18444;
const REFUSAL_TIMEOUT: Duration = Duration::from_secs(5);
const ADMISSION_DEADLINE_ENV: &str = "ARCEN_ADMISSION_DEADLINE_MS";

/// The byte stream a Deck's session travels over.
///
/// Either the Deck's own QUIC stream, when this process terminates TLS, or the
/// local socket the network service relays that stream through, when this
/// process is a desktop agent. The framing above it is identical, so the
/// session code cannot tell and does not need to.
#[derive(Debug)]
pub enum PierStream {
    /// The Deck's QUIC stream, terminated here.
    Quic(DirectQuicStream),
    /// The Deck's stream, relayed by the network service.
    Relayed(tokio::net::UnixStream),
}

impl tokio::io::AsyncRead for PierStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Quic(stream) => std::pin::Pin::new(stream).poll_read(context, buffer),
            Self::Relayed(stream) => std::pin::Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl tokio::io::AsyncWrite for PierStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Quic(stream) => std::pin::Pin::new(stream).poll_write(context, buffer),
            Self::Relayed(stream) => std::pin::Pin::new(stream).poll_write(context, buffer),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Quic(stream) => std::pin::Pin::new(stream).poll_flush(context),
            Self::Relayed(stream) => std::pin::Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Quic(stream) => std::pin::Pin::new(stream).poll_shutdown(context),
            Self::Relayed(stream) => std::pin::Pin::new(stream).poll_shutdown(context),
        }
    }
}

/// A framed session with one Deck.
pub type PierSocket = WebSocketStream<PierStream>;

/// The largest message a Deck may send.
///
/// A Deck sends authentication, input, control and clipboard chunks; the
/// largest of those is one clipboard chunk and its header. Tungstenite's
/// default is 64 MiB per message, which let an unauthenticated peer make the
/// host buffer that much before a password had been checked. The same bound
/// the Linux Pier applies.
pub(crate) const MAX_INBOUND_MESSAGE: usize =
    arcen_protocol::CLIPBOARD_HEADER_SIZE + arcen_protocol::CHUNK_BYTES;

/// Frames a Deck's stream as a Pier session.
pub async fn framed(stream: PierStream) -> PierSocket {
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(MAX_INBOUND_MESSAGE),
        max_frame_size: Some(MAX_INBOUND_MESSAGE),
        ..Default::default()
    };
    WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await
}

/// One admitted capacity-one Pier session.
pub struct AdmittedPierSession {
    socket: PierSocket,
    peer: SocketAddr,
    runtime: Arc<arcen_session::session_admission::SessionAdmissionRuntime>,
    lease: Option<arcen_session::session_admission::SessionAdmissionLease>,
}

impl std::fmt::Debug for AdmittedPierSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdmittedPierSession")
            .field("peer", &self.peer)
            .field("lease_held", &self.lease.is_some())
            .finish_non_exhaustive()
    }
}

impl AdmittedPierSession {
    /// Returns the admitted peer address.
    #[must_use]
    pub const fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Returns the admitted framed socket.
    pub fn socket_mut(&mut self) -> &mut PierSocket {
        &mut self.socket
    }
}

impl Drop for AdmittedPierSession {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            let _ = self.runtime.complete(&lease);
        }
    }
}

/// Result of accepting a connection through the session admission gate.
#[derive(Debug)]
pub enum SessionAdmissionAccept {
    /// This connection owns the host's capacity-one session slot.
    Admitted(Box<AdmittedPierSession>),
    /// This connection was accepted and refused because another session owns it.
    Refused { peer: SocketAddr, reason: String },
}

/// Why the listener could not start or serve.
#[derive(Debug)]
pub enum ServeError {
    /// Certificate material could not be read.
    Material(String),
    /// The TLS configuration was rejected.
    Tls(String),
    /// The UDP socket could not be bound.
    Bind(String),
    /// A connection failed before a session existed.
    Accept(String),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Material(detail) => write!(formatter, "certificate material: {detail}"),
            Self::Tls(detail) => write!(formatter, "TLS configuration: {detail}"),
            Self::Bind(detail) => write!(formatter, "bind: {detail}"),
            Self::Accept(detail) => write!(formatter, "accept: {detail}"),
        }
    }
}

impl std::error::Error for ServeError {}

/// Builds the QUIC server configuration from PEM material on disk.
///
/// # Errors
///
/// Returns [`ServeError::Material`] when the certificate or key cannot be
/// read, and [`ServeError::Tls`] when rustls or Quinn refuse the result.
pub fn server_config(
    certificate_path: &Path,
    key_path: &Path,
) -> Result<quinn::ServerConfig, ServeError> {
    let certificate_pem = std::fs::read(certificate_path).map_err(|error| {
        ServeError::Material(format!("read {}: {error}", certificate_path.display()))
    })?;
    let key_pem = std::fs::read(key_path)
        .map_err(|error| ServeError::Material(format!("read {}: {error}", key_path.display())))?;

    let certificates: Vec<rustls_pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut certificate_pem.as_slice())
            .collect::<Result<_, _>>()
            .map_err(|error| ServeError::Material(format!("parse certificate: {error}")))?;
    if certificates.is_empty() {
        return Err(ServeError::Material(
            "certificate file holds no certificate".to_owned(),
        ));
    }
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|error| ServeError::Material(format!("parse key: {error}")))?
        .ok_or_else(|| ServeError::Material("key file holds no private key".to_owned()))?;

    let mut rustls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|error| ServeError::Tls(format!("certificate and key do not match: {error}")))?;
    // Pinning the ALPN means a browser or generic QUIC client is refused during
    // the handshake rather than after it has spoken to the session layer.
    rustls_config.alpn_protocols = vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config)
        .map_err(|error| ServeError::Tls(format!("QUIC crypto: {error}")))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(arcen_transport::quic::recommended_transport_config_arc(
        &arcen_transport::BoundedTransportPolicy::default(),
    ));
    arcen_transport::quic::apply_direct_server_limits(&mut config);
    Ok(config)
}

/// How long a peer has to establish TLS, open its stream, and send a preface.
///
/// Generous, because a slow or distant link is not a fault and refusing a
/// legitimate Deck is worse than waiting for it. Bounded, because the accept
/// loop serves one connection at a time and every pre-session stage has the
/// same denial shape: traffic can keep one adjacent wait alive forever while
/// every other Deck stays outside the host.
const ADMISSION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

fn admission_deadline() -> Duration {
    std::env::var(ADMISSION_DEADLINE_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(ADMISSION_DEADLINE, Duration::from_millis)
}

/// A bound listener.
pub struct Listener {
    endpoint: quinn::Endpoint,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Listener")
            .field("local_addr", &self.endpoint.local_addr().ok())
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Binds the QUIC endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError::Bind`] when the address is unavailable.
    pub fn bind(address: SocketAddr, config: quinn::ServerConfig) -> Result<Self, ServeError> {
        let endpoint = quinn::Endpoint::server(config, address)
            .map_err(|error| ServeError::Bind(format!("{address}: {error}")))?;
        Ok(Self { endpoint })
    }

    /// Returns the address actually bound, which matters when port 0 was asked
    /// for.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError::Bind`] when the socket cannot report its address.
    pub fn local_addr(&self) -> Result<SocketAddr, ServeError> {
        self.endpoint
            .local_addr()
            .map_err(|error| ServeError::Bind(format!("local address: {error}")))
    }

    /// Waits for one Deck and returns its framed session.
    ///
    /// A peer that fails its handshake — wrong ALPN, an untrusted client, a
    /// half-open connection — is discarded and the next one is awaited. Only
    /// the endpoint itself closing ends this loop. Returning a per-peer error
    /// would let anyone who can reach the port take the host offline by
    /// connecting once with the wrong ALPN, which is not a defensible way for
    /// a remote-access host to behave.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError::Accept`] only when the endpoint is closed.
    pub async fn accept(&self) -> Result<(PierSocket, SocketAddr), ServeError> {
        let (stream, peer) = self.accept_raw().await?;
        Ok((framed(PierStream::Quic(stream)).await, peer))
    }

    /// Waits for one Deck and returns its stream unframed, for a service that
    /// relays the stream rather than speaking the session itself.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError::Accept`] only when the endpoint is closed.
    pub async fn accept_raw(&self) -> Result<(DirectQuicStream, SocketAddr), ServeError> {
        self.accept_with_admission_budget(admission_deadline())
            .await
    }

    async fn accept_with_admission_budget(
        &self,
        admission_budget: Duration,
    ) -> Result<(DirectQuicStream, SocketAddr), ServeError> {
        loop {
            let incoming = self
                .endpoint
                .accept()
                .await
                .ok_or_else(|| ServeError::Accept("endpoint closed".to_owned()))?;

            let peer = incoming.remote_address();
            let connecting = match incoming.accept() {
                Ok(connecting) => connecting,
                Err(error) => {
                    tracing::debug!(target: "arcen::net", %peer, %error, "refused a connection");
                    continue;
                }
            };
            let deadline = TokioInstant::now() + admission_budget;
            let connection = match connecting.into_0rtt() {
                Ok((connection, _accepted)) => connection,
                Err(connecting) => match tokio::time::timeout_at(deadline, connecting).await {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(error)) => {
                        tracing::debug!(target: "arcen::net", %peer, %error, "refused a connection");
                        continue;
                    }
                    Err(_) => {
                        tracing::debug!(
                            target: "arcen::net",
                            %peer,
                            timeout_secs = admission_budget.as_secs(),
                            "connection did not complete admission before its deadline"
                        );
                        continue;
                    }
                },
            };
            let peer = connection.remote_address();
            if let Err(error) = wait_for_tls(&connection, deadline).await {
                connection.close(0_u32.into(), b"admission deadline exceeded");
                tracing::debug!(
                    target: "arcen::net",
                    %peer,
                    %error,
                    "connection did not complete TLS admission"
                );
                continue;
            }
            let close_on_timeout = connection.clone();
            let opened =
                tokio::time::timeout_at(deadline, arcen_transport::quic::accept_direct(connection))
                    .await;
            match opened {
                Ok(Ok(stream)) => return Ok((stream, peer)),
                Err(_) => {
                    close_on_timeout.close(0_u32.into(), b"admission deadline exceeded");
                    tracing::debug!(
                        target: "arcen::net",
                        %peer,
                        timeout_secs = admission_budget.as_secs(),
                        "connection opened no stream before its admission deadline"
                    );
                }
                Ok(Err(error)) => {
                    tracing::debug!(
                        target: "arcen::net",
                        %peer,
                        %error,
                        "connection opened no usable stream"
                    );
                }
            }
        }
    }

    /// Accepts one Deck and either admits it or tells it why the host is busy.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError::Accept`] only when the endpoint is closed.
    pub async fn accept_with_session_admission(
        &self,
        runtime: &Arc<arcen_session::session_admission::SessionAdmissionRuntime>,
    ) -> Result<SessionAdmissionAccept, ServeError> {
        let (mut socket, peer) = self.accept().await?;
        match runtime.admit_new() {
            Ok(lease) => Ok(SessionAdmissionAccept::Admitted(Box::new(
                AdmittedPierSession {
                    socket,
                    peer,
                    runtime: Arc::clone(runtime),
                    lease: Some(lease),
                },
            ))),
            Err(error) => {
                let reason = error.to_string();
                let _ = refuse_with_reason(&mut socket, &reason).await;
                Ok(SessionAdmissionAccept::Refused { peer, reason })
            }
        }
    }

    /// Stops accepting and lets in-flight connections drain.
    pub fn close(&self) {
        self.endpoint.close(0_u32.into(), b"pier shutting down");
    }
}

/// Sends a close frame that names why this accepted Deck cannot proceed.
///
/// # Errors
///
/// Returns the send failure or timeout reason.
pub async fn refuse_with_reason(socket: &mut PierSocket, reason: &str) -> Result<(), String> {
    let frame = Message::Close(Some(CloseFrame {
        code: CloseCode::Policy,
        reason: reason.to_owned().into(),
    }));
    tokio::time::timeout(REFUSAL_TIMEOUT, socket.send(frame))
        .await
        .map_err(|_| format!("timed out sending refusal: {reason}"))?
        .map_err(|error| format!("send refusal: {error}"))
}

/// Sends one control message.
///
/// # Errors
///
/// Returns the framing error when the peer has gone.
pub async fn send_json(socket: &mut PierSocket, json: String) -> Result<(), String> {
    socket
        .send(Message::Text(json))
        .await
        .map_err(|error| format!("send: {error}"))
}

/// Receives one control message, ignoring keepalives.
///
/// Returns `None` when the peer closed the session.
///
/// # Errors
///
/// Returns the framing error when the stream fails.
pub async fn receive_json(socket: &mut PierSocket) -> Result<Option<String>, String> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return Ok(Some(text.clone())),
            // Ping/Pong, binary and raw frames are transport noise here, not
            // session messages, so none of them reach the caller.
            Some(Ok(
                Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_),
            )) => {}
            Some(Ok(Message::Close(_))) | None => return Ok(None),
            Some(Err(error)) => return Err(format!("receive: {error}")),
        }
    }
}

async fn wait_for_tls(
    connection: &quinn::Connection,
    deadline: TokioInstant,
) -> Result<(), String> {
    loop {
        if connection.handshake_data().is_some() {
            return Ok(());
        }
        if let Some(reason) = connection.close_reason() {
            return Err(reason.to_string());
        }
        if TokioInstant::now() >= deadline {
            return Err("timed out before the admission deadline".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn missing_material_is_reported_rather_than_panicking() {
        let error = server_config(
            Path::new("/nonexistent/host.crt"),
            Path::new("/nonexistent/host.key"),
        )
        .expect_err("missing material must fail");
        assert!(matches!(error, ServeError::Material(_)));
    }

    #[test]
    fn an_empty_certificate_file_is_refused() {
        let dir = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("arcen-net-tests")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cert = dir.join("host.crt");
        let key = dir.join("host.key");
        std::fs::write(&cert, b"").expect("write");
        std::fs::write(&key, b"").expect("write");

        let error = server_config(&cert, &key).expect_err("empty material must fail");
        assert!(matches!(error, ServeError::Material(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn admission_deadline_covers_tls_and_preface_before_accepting_next_peer() {
        let dir = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("arcen-net-admission-tests")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");
        let cert = dir.join("host.crt");
        let key = dir.join("host.key");
        let key_pair = rcgen::KeyPair::generate().expect("key");
        let certificate = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
            .expect("params")
            .self_signed(&key_pair)
            .expect("cert");
        std::fs::write(&cert, certificate.pem()).expect("write cert");
        std::fs::write(&key, key_pair.serialize_pem()).expect("write key");

        let listener = Listener::bind(
            "127.0.0.1:0".parse().expect("addr"),
            server_config(&cert, &key).expect("config"),
        )
        .expect("listener");
        let server_addr = listener.local_addr().expect("local addr");
        let server_cert = rustls_pemfile::certs(&mut certificate.pem().as_bytes())
            .next()
            .expect("one cert")
            .expect("parse cert");
        let client_config = client_config_for_test(server_cert);
        let stalled = quinn::Endpoint::client("127.0.0.1:0".parse().expect("client addr"))
            .expect("client endpoint");
        let healthy = quinn::Endpoint::client("127.0.0.1:0".parse().expect("client addr"))
            .expect("client endpoint");

        let accept = tokio::spawn(async move {
            listener
                .accept_with_admission_budget(Duration::from_millis(40))
                .await
        });
        let stalled_connection = stalled
            .connect_with(client_config.clone(), server_addr, "localhost")
            .expect("connect stalled")
            .await
            .expect("stalled handshake");
        tokio::time::sleep(Duration::from_millis(80)).await;
        let stream =
            arcen_transport::quic::connect_direct(arcen_transport::quic::DirectQuicDialParams {
                endpoint: healthy,
                client_config,
                remote_addr: server_addr,
                server_name: "localhost",
            })
            .await
            .expect("healthy peer accepted after stalled peer expired");

        let accepted = tokio::time::timeout(Duration::from_secs(1), accept)
            .await
            .expect("accept loop must keep serving after an admission timeout")
            .expect("accept task")
            .expect("accept result");
        drop(accepted);
        stream.close(0, b"test complete");
        stalled_connection.close(0_u32.into(), b"test complete");
        stalled.close(0_u32.into(), b"test complete");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn client_config_for_test(
        server_cert: rustls::pki_types::CertificateDer<'static>,
    ) -> quinn::ClientConfig {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(server_cert).expect("add root");
        let mut rustls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        rustls.alpn_protocols = vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()];
        let crypto =
            quinn::crypto::rustls::QuicClientConfig::try_from(rustls).expect("quic client config");
        let mut config = quinn::ClientConfig::new(std::sync::Arc::new(crypto));
        config.transport_config(arcen_transport::quic::recommended_transport_config_arc(
            &arcen_transport::BoundedTransportPolicy::default(),
        ));
        config
    }

    #[test]
    fn the_default_port_is_the_only_one_arcen_uses() {
        assert_eq!(DEFAULT_PORT, 18444);
    }
}
