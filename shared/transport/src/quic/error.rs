//! QUIC adapter error surface.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::io;

use crate::TransportContractError;

use super::handshake::HandshakeRejectReason;
use super::identity::IdentityAuthorizationError;
use super::monitor::MonitorStreamPrefaceError;

/// Failure surfaced by the QUIC transport adapter.
#[derive(Debug)]
pub enum QuicTransportError {
    /// The pure transport contract rejected an envelope (cap or class mismatch).
    Contract(TransportContractError),
    /// Binding the local UDP socket or endpoint failed.
    Endpoint(io::Error),
    /// The QUIC connection attempt failed.
    Connect(quinn::ConnectError),
    /// An established QUIC connection failed or was closed with an error.
    Connection(quinn::ConnectionError),
    /// The peer did not begin the direct product stream with the fixed preface.
    DirectPreface,
    /// The peer's direct-monitor stream preface was rejected (malformed,
    /// oversized, truncated, or an invalid embedded session id).
    MonitorPreface(MonitorStreamPrefaceError),
    /// Accepting or parsing a direct-monitor stream preface exceeded its
    /// bounded deadline.
    MonitorStreamTimedOut,
    /// Writing to the persistent reliable stream failed.
    StreamWrite(quinn::WriteError),
    /// Reading from the persistent reliable stream failed.
    StreamRead(quinn::ReadExactError),
    /// Sending a QUIC datagram failed.
    DatagramSend(quinn::SendDatagramError),
    /// The authenticated binding handshake was rejected or malformed.
    Handshake(HandshakeRejectReason),
    /// The binding handshake or required stream setup exceeded its configured deadline.
    EstablishmentTimedOut,
    /// The peer identity/session authorizer rejected the connection.
    Unauthorized(IdentityAuthorizationError),
    /// The remote peer did not present a TLS certificate chain to authorize.
    MissingPeerIdentity,
    /// The outbound reliable-message queue rejected a message (bounded, explicit).
    OutboundQueueFull,
    /// The inbound message queue is bounded and full; the peer must be drained.
    InboundQueueFull,
    /// The adapter was closed or cancelled.
    Closed,
}

impl Display for QuicTransportError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(error) => {
                write!(formatter, "transport contract rejected envelope: {error}")
            }
            Self::Endpoint(error) => write!(formatter, "quic endpoint failure: {error}"),
            Self::Connect(error) => write!(formatter, "quic connect failure: {error}"),
            Self::Connection(error) => write!(formatter, "quic connection failure: {error}"),
            Self::DirectPreface => formatter.write_str("invalid direct QUIC stream preface"),
            Self::MonitorPreface(error) => {
                write!(formatter, "invalid direct-monitor stream preface: {error}")
            }
            Self::MonitorStreamTimedOut => {
                formatter.write_str("direct-monitor stream preface accept/parse timed out")
            }
            Self::StreamWrite(error) => write!(formatter, "quic stream write failure: {error}"),
            Self::StreamRead(error) => write!(formatter, "quic stream read failure: {error}"),
            Self::DatagramSend(error) => write!(formatter, "quic datagram send failure: {error}"),
            Self::Handshake(reason) => write!(formatter, "binding handshake rejected: {reason}"),
            Self::EstablishmentTimedOut => {
                formatter.write_str("quic binding or stream establishment timed out")
            }
            Self::Unauthorized(error) => write!(formatter, "peer identity unauthorized: {error}"),
            Self::MissingPeerIdentity => {
                formatter.write_str("remote peer presented no TLS certificate chain")
            }
            Self::OutboundQueueFull => {
                formatter.write_str("bounded outbound reliable queue is full")
            }
            Self::InboundQueueFull => formatter.write_str("bounded inbound message queue is full"),
            Self::Closed => formatter.write_str("quic peer is closed"),
        }
    }
}

impl Error for QuicTransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Contract(error) => Some(error),
            Self::Endpoint(error) => Some(error),
            Self::Connect(error) => Some(error),
            Self::Connection(error) => Some(error),
            Self::StreamWrite(error) => Some(error),
            Self::StreamRead(error) => Some(error),
            Self::DatagramSend(error) => Some(error),
            Self::Unauthorized(error) => Some(error),
            Self::MonitorPreface(error) => Some(error),
            Self::DirectPreface
            | Self::MonitorStreamTimedOut
            | Self::Handshake(_)
            | Self::EstablishmentTimedOut
            | Self::MissingPeerIdentity
            | Self::OutboundQueueFull
            | Self::InboundQueueFull
            | Self::Closed => None,
        }
    }
}

impl From<TransportContractError> for QuicTransportError {
    fn from(error: TransportContractError) -> Self {
        Self::Contract(error)
    }
}
