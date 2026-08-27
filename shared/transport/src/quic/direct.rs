//! Direct product QUIC stream used by Pier and Deck.
//!
//! This is a transport carrier, not an admission decision. It establishes one
//! full-duplex QUIC stream and keeps the owning endpoint/connection alive while
//! the product runs its existing bounded authentication and session protocol.
//! Callers must not treat successful QUIC/TLS establishment as user or session
//! authorization.

use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Semaphore;

use super::{FeedbackSnapshot, QuicTransportError};

const DIRECT_STREAM_PREFACE: &[u8; 16] = b"arcen-direct-v1\0";
const DIRECT_CONNECTION_LINGER: Duration = Duration::from_secs(1);
const DIRECT_CONNECTION_LINGER_CAPACITY: usize = 32;
static DIRECT_CONNECTION_LINGER_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Parameters for an outbound direct QUIC connection.
pub struct DirectQuicDialParams<'a> {
    /// Bound client endpoint. It is retained for the stream lifetime.
    pub endpoint: Endpoint,
    /// Caller-owned TLS and transport configuration.
    pub client_config: quinn::ClientConfig,
    /// Remote UDP address.
    pub remote_addr: SocketAddr,
    /// TLS server name validated by rustls.
    pub server_name: &'a str,
}

impl Debug for DirectQuicDialParams<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DirectQuicDialParams")
            .field("local_addr", &self.endpoint.local_addr().ok())
            .field("remote_addr", &self.remote_addr)
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

/// One full-duplex QUIC stream with its connection lifetime guards.
///
/// The type implements Tokio's `AsyncRead`/`AsyncWrite`, so product adapters
/// can preserve their existing framed protocol while replacing TCP/TLS with
/// QUIC/TLS. Application authentication remains mandatory above this stream.
pub struct DirectQuicStream {
    endpoint: Option<Endpoint>,
    connection: Connection,
    send: SendStream,
    recv: RecvStream,
}

impl Debug for DirectQuicStream {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DirectQuicStream")
            .field(
                "local_addr",
                &self.endpoint.as_ref().and_then(|ep| ep.local_addr().ok()),
            )
            .field("remote_addr", &self.connection.remote_address())
            .finish_non_exhaustive()
    }
}

impl Drop for DirectQuicStream {
    fn drop(&mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let slots = Arc::clone(
            DIRECT_CONNECTION_LINGER_SLOTS
                .get_or_init(|| Arc::new(Semaphore::new(DIRECT_CONNECTION_LINGER_CAPACITY))),
        );
        let Ok(permit) = slots.try_acquire_owned() else {
            return;
        };
        let connection = self.connection.clone();
        let endpoint = self.endpoint.clone();
        runtime.spawn(async move {
            let _permit = permit;
            tokio::select! {
                _ = connection.closed() => {}
                () = tokio::time::sleep(DIRECT_CONNECTION_LINGER) => {
                    connection.close(0_u32.into(), b"direct stream closed");
                }
            }
            drop(endpoint);
        });
    }
}

impl DirectQuicStream {
    fn new(
        endpoint: Option<Endpoint>,
        connection: Connection,
        send: SendStream,
        recv: RecvStream,
    ) -> Self {
        Self {
            endpoint,
            connection,
            send,
            recv,
        }
    }

    /// Returns the live QUIC connection for bounded telemetry snapshots.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Returns an owned clone of the live QUIC connection handle.
    ///
    /// Future product adapters (for example, the additive direct-monitor
    /// per-stream foundation in [`super::monitor`]) can use this to open or
    /// accept further streams on the same connection without borrowing this
    /// stream's `AsyncRead`/`AsyncWrite` implementation. Cloning a
    /// `quinn::Connection` only clones an internal handle to the same
    /// connection state; it does not open a new connection, and it does not
    /// extend this stream's own bounded endpoint/linger `Drop` behavior,
    /// which is governed solely by this value's lifetime.
    #[must_use]
    pub fn connection_handle(&self) -> Connection {
        self.connection.clone()
    }

    /// Returns the current remote UDP address.
    #[must_use]
    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// Returns a point-in-time QUIC path and congestion snapshot.
    #[must_use]
    pub fn feedback_snapshot(&self) -> FeedbackSnapshot {
        FeedbackSnapshot::from_connection(&self.connection)
    }

    /// Closes the QUIC connection with an application reason.
    pub fn close(&self, code: u32, reason: &[u8]) {
        self.connection.close(code.into(), reason);
    }
}

impl AsyncRead for DirectQuicStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(context, buffer)
    }
}

impl AsyncWrite for DirectQuicStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), context)
    }
}

/// Dials a QUIC connection and opens its one product protocol stream.
///
/// # Errors
///
/// Returns a typed connect or connection failure.
pub async fn connect_direct(
    params: DirectQuicDialParams<'_>,
) -> Result<DirectQuicStream, QuicTransportError> {
    let connecting = params
        .endpoint
        .connect_with(params.client_config, params.remote_addr, params.server_name)
        .map_err(QuicTransportError::Connect)?;
    let connection = connecting.await.map_err(QuicTransportError::Connection)?;
    let (mut send, recv) = connection
        .open_bi()
        .await
        .map_err(QuicTransportError::Connection)?;
    send.write_all(DIRECT_STREAM_PREFACE)
        .await
        .map_err(QuicTransportError::StreamWrite)?;
    Ok(DirectQuicStream::new(
        Some(params.endpoint),
        connection,
        send,
        recv,
    ))
}

/// Accepts the first full-duplex product protocol stream on a QUIC connection.
///
/// # Errors
///
/// Returns a connection failure when the peer closes before opening the
/// required stream.
pub async fn accept_direct(connection: Connection) -> Result<DirectQuicStream, QuicTransportError> {
    let (send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(QuicTransportError::Connection)?;
    let mut preface = [0_u8; DIRECT_STREAM_PREFACE.len()];
    if recv.read_exact(&mut preface).await.is_err() || preface != *DIRECT_STREAM_PREFACE {
        connection.close(0_u32.into(), b"invalid direct stream preface");
        return Err(QuicTransportError::DirectPreface);
    }
    Ok(DirectQuicStream::new(None, connection, send, recv))
}
