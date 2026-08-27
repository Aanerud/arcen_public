//! Authenticated binding handshake codec and exchange.
//!
//! Before any application payload flows on the persistent reliable stream or
//! as an encrypted datagram, both peers perform one request/response exchange
//! over a dedicated QUIC bidirectional stream. The initiator states its
//! claimed role, grant-bound session, supported capabilities, and policy
//! requirements; the acceptor authorizes the initiator's TLS peer identity
//! against that claim (via
//! [`super::identity::PeerIdentityAuthorizer`]) and either acknowledges with
//! its own role/session plus the negotiated intersection or explicitly rejects.
//! Malformed, oversized, incompatible, or
//! unauthorized handshakes are rejected explicitly; nothing is silently
//! dropped.

use std::fmt::{Display, Formatter};

use crate::{MAX_NEGOTIATED_CAPABILITIES, MAX_TRANSPORT_IDENTITY_BYTES};

use super::identity::QuicRole;

/// Current binding handshake wire version. Bumping this is a reviewed,
/// breaking protocol change.
pub const HANDSHAKE_PROTOCOL_VERSION: u16 = 2;

/// Hard cap on an encoded handshake frame, guarding against unbounded
/// allocation from a malformed or hostile peer before any authorization runs.
pub const MAX_HANDSHAKE_FRAME_BYTES: usize = 4096;

/// Hard cap on the claimed session identifier length within a handshake frame.
pub const MAX_SESSION_ID_BYTES: usize = 512;

const REQUEST_MAGIC: [u8; 4] = *b"ARQH";
const RESPONSE_MAGIC: [u8; 4] = *b"ARQR";
const RESPONSE_TAG_ACK: u8 = 0;
const RESPONSE_TAG_REJECT: u8 = 1;

/// Initiator's binding request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeRequest {
    /// Wire protocol version the initiator speaks.
    pub protocol_version: u16,
    /// Role the initiator claims.
    pub role: QuicRole,
    /// Session the initiator wants to bind to.
    pub session_id: String,
    /// Capabilities the initiator can support on this connection.
    pub supported_capabilities: Vec<String>,
    /// Capabilities the initiator's policy requires.
    pub required_capabilities: Vec<String>,
}

/// Acceptor's binding response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeResponse {
    /// The acceptor authorized the request and agrees on the bound session.
    Ack {
        /// Wire protocol version the acceptor speaks.
        protocol_version: u16,
        /// Role the acceptor identifies as.
        role: QuicRole,
        /// Session both peers are now bound to.
        session_id: String,
        /// Exact capability intersection selected by the acceptor.
        negotiated_capabilities: Vec<String>,
    },
    /// The acceptor explicitly rejected the request.
    Reject(HandshakeRejectReason),
}

/// Explicit, non-silent handshake rejection reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeRejectReason {
    /// The frame could not be decoded (bad magic, truncated, or oversized).
    Malformed,
    /// Protocol versions are incompatible.
    ProtocolVersionMismatch,
    /// The peer identity authorizer rejected the claimed role/session.
    Unauthorized,
    /// The acceptor and initiator disagree on the bound session identifier.
    SessionMismatch,
    /// Required transport or delivery capabilities do not overlap.
    CapabilityMismatch,
}

impl Display for HandshakeRejectReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Malformed => "handshake frame was malformed",
            Self::ProtocolVersionMismatch => "handshake protocol version mismatch",
            Self::Unauthorized => "peer identity/session was not authorized",
            Self::SessionMismatch => "claimed session identifier mismatch",
            Self::CapabilityMismatch => "required transport capabilities do not overlap",
        };
        formatter.write_str(message)
    }
}

fn encode_role(role: QuicRole) -> u8 {
    match role {
        QuicRole::Host => 0,
        QuicRole::Client => 1,
        QuicRole::Gateway => 2,
    }
}

fn decode_role(value: u8) -> Option<QuicRole> {
    match value {
        0 => Some(QuicRole::Host),
        1 => Some(QuicRole::Client),
        2 => Some(QuicRole::Gateway),
        _ => None,
    }
}

impl HandshakeRequest {
    /// Encodes the request as a bounded binary frame.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeRejectReason::Malformed`] when the session identifier
    /// is empty or exceeds [`MAX_SESSION_ID_BYTES`].
    pub fn encode(&self) -> Result<Vec<u8>, HandshakeRejectReason> {
        let session_bytes = self.session_id.as_bytes();
        validate_session_id(session_bytes)?;
        let mut buffer = Vec::with_capacity(4 + 2 + 1 + 2 + session_bytes.len());
        buffer.extend_from_slice(&REQUEST_MAGIC);
        buffer.extend_from_slice(&self.protocol_version.to_be_bytes());
        buffer.push(encode_role(self.role));
        let length =
            u16::try_from(session_bytes.len()).map_err(|_| HandshakeRejectReason::Malformed)?;
        buffer.extend_from_slice(&length.to_be_bytes());
        buffer.extend_from_slice(session_bytes);
        encode_capabilities(&mut buffer, &self.supported_capabilities)?;
        encode_capabilities(&mut buffer, &self.required_capabilities)?;
        if buffer.len() > MAX_HANDSHAKE_FRAME_BYTES {
            return Err(HandshakeRejectReason::Malformed);
        }
        Ok(buffer)
    }

    /// Decodes a bounded binary frame produced by [`Self::encode`].
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeRejectReason::Malformed`] for any structurally
    /// invalid or oversized input.
    pub fn decode(bytes: &[u8]) -> Result<Self, HandshakeRejectReason> {
        if bytes.len() > MAX_HANDSHAKE_FRAME_BYTES {
            return Err(HandshakeRejectReason::Malformed);
        }
        if bytes.len() < 4 + 2 + 1 + 2 || bytes[0..4] != REQUEST_MAGIC {
            return Err(HandshakeRejectReason::Malformed);
        }
        let protocol_version = u16::from_be_bytes(
            bytes[4..6]
                .try_into()
                .map_err(|_| HandshakeRejectReason::Malformed)?,
        );
        let role = decode_role(bytes[6]).ok_or(HandshakeRejectReason::Malformed)?;
        let session_len = u16::from_be_bytes(
            bytes[7..9]
                .try_into()
                .map_err(|_| HandshakeRejectReason::Malformed)?,
        ) as usize;
        if session_len == 0 || session_len > MAX_SESSION_ID_BYTES || bytes.len() < 9 + session_len {
            return Err(HandshakeRejectReason::Malformed);
        }
        let session_id = String::from_utf8(bytes[9..9 + session_len].to_vec())
            .map_err(|_| HandshakeRejectReason::Malformed)?;
        let mut offset = 9 + session_len;
        let supported_capabilities = decode_capabilities(bytes, &mut offset)?;
        let required_capabilities = decode_capabilities(bytes, &mut offset)?;
        if offset != bytes.len() {
            return Err(HandshakeRejectReason::Malformed);
        }
        Ok(Self {
            protocol_version,
            role,
            session_id,
            supported_capabilities,
            required_capabilities,
        })
    }
}

impl HandshakeResponse {
    /// Encodes the response as a bounded binary frame.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeRejectReason::Malformed`] when an acknowledgement
    /// contains an empty or oversized session identifier.
    pub fn encode(&self) -> Result<Vec<u8>, HandshakeRejectReason> {
        match self {
            Self::Ack {
                protocol_version,
                role,
                session_id,
                negotiated_capabilities,
            } => {
                let session_bytes = session_id.as_bytes();
                validate_session_id(session_bytes)?;
                let mut buffer = Vec::with_capacity(4 + 1 + 2 + 1 + 2 + session_bytes.len());
                buffer.extend_from_slice(&RESPONSE_MAGIC);
                buffer.push(RESPONSE_TAG_ACK);
                buffer.extend_from_slice(&protocol_version.to_be_bytes());
                buffer.push(encode_role(*role));
                let length = u16::try_from(session_bytes.len())
                    .map_err(|_| HandshakeRejectReason::Malformed)?;
                buffer.extend_from_slice(&length.to_be_bytes());
                buffer.extend_from_slice(session_bytes);
                encode_capabilities(&mut buffer, negotiated_capabilities)?;
                if buffer.len() > MAX_HANDSHAKE_FRAME_BYTES {
                    return Err(HandshakeRejectReason::Malformed);
                }
                Ok(buffer)
            }
            Self::Reject(reason) => {
                let mut buffer = Vec::with_capacity(4 + 1 + 1);
                buffer.extend_from_slice(&RESPONSE_MAGIC);
                buffer.push(RESPONSE_TAG_REJECT);
                buffer.push(match reason {
                    HandshakeRejectReason::Malformed => 0,
                    HandshakeRejectReason::ProtocolVersionMismatch => 1,
                    HandshakeRejectReason::Unauthorized => 2,
                    HandshakeRejectReason::SessionMismatch => 3,
                    HandshakeRejectReason::CapabilityMismatch => 4,
                });
                Ok(buffer)
            }
        }
    }

    /// Decodes a bounded binary frame produced by [`Self::encode`].
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeRejectReason::Malformed`] for any structurally
    /// invalid or oversized input.
    pub fn decode(bytes: &[u8]) -> Result<Self, HandshakeRejectReason> {
        if bytes.len() > MAX_HANDSHAKE_FRAME_BYTES {
            return Err(HandshakeRejectReason::Malformed);
        }
        if bytes.len() < 5 || bytes[0..4] != RESPONSE_MAGIC {
            return Err(HandshakeRejectReason::Malformed);
        }
        match bytes[4] {
            RESPONSE_TAG_ACK => {
                if bytes.len() < 4 + 1 + 2 + 1 + 2 {
                    return Err(HandshakeRejectReason::Malformed);
                }
                let protocol_version = u16::from_be_bytes(
                    bytes[5..7]
                        .try_into()
                        .map_err(|_| HandshakeRejectReason::Malformed)?,
                );
                let role = decode_role(bytes[7]).ok_or(HandshakeRejectReason::Malformed)?;
                let session_len = u16::from_be_bytes(
                    bytes[8..10]
                        .try_into()
                        .map_err(|_| HandshakeRejectReason::Malformed)?,
                ) as usize;
                if session_len == 0
                    || session_len > MAX_SESSION_ID_BYTES
                    || bytes.len() < 10 + session_len
                {
                    return Err(HandshakeRejectReason::Malformed);
                }
                let session_id = String::from_utf8(bytes[10..10 + session_len].to_vec())
                    .map_err(|_| HandshakeRejectReason::Malformed)?;
                let mut offset = 10 + session_len;
                let negotiated_capabilities = decode_capabilities(bytes, &mut offset)?;
                if offset != bytes.len() {
                    return Err(HandshakeRejectReason::Malformed);
                }
                Ok(Self::Ack {
                    protocol_version,
                    role,
                    session_id,
                    negotiated_capabilities,
                })
            }
            RESPONSE_TAG_REJECT => {
                if bytes.len() != 6 {
                    return Err(HandshakeRejectReason::Malformed);
                }
                let reason = match bytes[5] {
                    0 => HandshakeRejectReason::Malformed,
                    1 => HandshakeRejectReason::ProtocolVersionMismatch,
                    2 => HandshakeRejectReason::Unauthorized,
                    3 => HandshakeRejectReason::SessionMismatch,
                    4 => HandshakeRejectReason::CapabilityMismatch,
                    _ => return Err(HandshakeRejectReason::Malformed),
                };
                Ok(Self::Reject(reason))
            }
            _ => Err(HandshakeRejectReason::Malformed),
        }
    }
}

fn validate_session_id(session_bytes: &[u8]) -> Result<(), HandshakeRejectReason> {
    if session_bytes.is_empty() || session_bytes.len() > MAX_SESSION_ID_BYTES {
        return Err(HandshakeRejectReason::Malformed);
    }
    Ok(())
}

fn encode_capabilities(
    buffer: &mut Vec<u8>,
    capabilities: &[String],
) -> Result<(), HandshakeRejectReason> {
    if capabilities.len() > MAX_NEGOTIATED_CAPABILITIES {
        return Err(HandshakeRejectReason::Malformed);
    }
    let count = u16::try_from(capabilities.len()).map_err(|_| HandshakeRejectReason::Malformed)?;
    buffer.extend_from_slice(&count.to_be_bytes());
    for capability in capabilities {
        let bytes = capability.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_TRANSPORT_IDENTITY_BYTES {
            return Err(HandshakeRejectReason::Malformed);
        }
        let length = u16::try_from(bytes.len()).map_err(|_| HandshakeRejectReason::Malformed)?;
        buffer.extend_from_slice(&length.to_be_bytes());
        buffer.extend_from_slice(bytes);
    }
    Ok(())
}

fn decode_capabilities(
    bytes: &[u8],
    offset: &mut usize,
) -> Result<Vec<String>, HandshakeRejectReason> {
    let count_bytes = bytes
        .get(*offset..*offset + 2)
        .ok_or(HandshakeRejectReason::Malformed)?;
    let count = u16::from_be_bytes(
        count_bytes
            .try_into()
            .map_err(|_| HandshakeRejectReason::Malformed)?,
    ) as usize;
    *offset += 2;
    if count > MAX_NEGOTIATED_CAPABILITIES {
        return Err(HandshakeRejectReason::Malformed);
    }
    let mut capabilities = Vec::with_capacity(count);
    for _ in 0..count {
        let length_bytes = bytes
            .get(*offset..*offset + 2)
            .ok_or(HandshakeRejectReason::Malformed)?;
        let length = u16::from_be_bytes(
            length_bytes
                .try_into()
                .map_err(|_| HandshakeRejectReason::Malformed)?,
        ) as usize;
        *offset += 2;
        if length == 0 || length > MAX_TRANSPORT_IDENTITY_BYTES {
            return Err(HandshakeRejectReason::Malformed);
        }
        let value = bytes
            .get(*offset..*offset + length)
            .ok_or(HandshakeRejectReason::Malformed)?;
        *offset += length;
        capabilities
            .push(String::from_utf8(value.to_vec()).map_err(|_| HandshakeRejectReason::Malformed)?);
    }
    Ok(capabilities)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let request = HandshakeRequest {
            protocol_version: HANDSHAKE_PROTOCOL_VERSION,
            role: QuicRole::Host,
            session_id: "session-abc".to_owned(),
            supported_capabilities: vec!["transport:quic-v1".to_owned()],
            required_capabilities: vec!["delivery:reliable-stream-v1".to_owned()],
        };
        let encoded = request.encode().expect("valid request");
        assert_eq!(HandshakeRequest::decode(&encoded), Ok(request));
    }

    #[test]
    fn ack_round_trips() {
        let response = HandshakeResponse::Ack {
            protocol_version: HANDSHAKE_PROTOCOL_VERSION,
            role: QuicRole::Gateway,
            session_id: "session-xyz".to_owned(),
            negotiated_capabilities: vec!["transport:quic-v1".to_owned()],
        };
        let encoded = response.encode().expect("valid response");
        assert_eq!(HandshakeResponse::decode(&encoded), Ok(response));
    }

    #[test]
    fn reject_round_trips_every_reason() {
        for reason in [
            HandshakeRejectReason::Malformed,
            HandshakeRejectReason::ProtocolVersionMismatch,
            HandshakeRejectReason::Unauthorized,
            HandshakeRejectReason::SessionMismatch,
            HandshakeRejectReason::CapabilityMismatch,
        ] {
            let response = HandshakeResponse::Reject(reason);
            let encoded = response.encode().expect("reject response");
            assert_eq!(HandshakeResponse::decode(&encoded), Ok(response));
        }
    }

    #[test]
    fn malformed_request_bytes_are_rejected_explicitly() {
        assert_eq!(
            HandshakeRequest::decode(b"not-a-handshake"),
            Err(HandshakeRejectReason::Malformed)
        );
        assert_eq!(
            HandshakeRequest::decode(&[]),
            Err(HandshakeRejectReason::Malformed)
        );
        let oversized = vec![0_u8; MAX_HANDSHAKE_FRAME_BYTES + 1];
        assert_eq!(
            HandshakeRequest::decode(&oversized),
            Err(HandshakeRejectReason::Malformed)
        );
    }

    #[test]
    fn request_encoder_rejects_empty_and_oversized_sessions() {
        for session_id in [String::new(), "x".repeat(MAX_SESSION_ID_BYTES + 1)] {
            let request = HandshakeRequest {
                protocol_version: HANDSHAKE_PROTOCOL_VERSION,
                role: QuicRole::Client,
                session_id,
                supported_capabilities: Vec::new(),
                required_capabilities: Vec::new(),
            };
            assert_eq!(request.encode(), Err(HandshakeRejectReason::Malformed));
        }
    }

    #[test]
    fn truncated_session_id_length_is_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&REQUEST_MAGIC);
        bytes.extend_from_slice(&HANDSHAKE_PROTOCOL_VERSION.to_be_bytes());
        bytes.push(encode_role(QuicRole::Client));
        // Claims a 100-byte session id but supplies none.
        bytes.extend_from_slice(&100_u16.to_be_bytes());
        assert_eq!(
            HandshakeRequest::decode(&bytes),
            Err(HandshakeRejectReason::Malformed)
        );
    }

    #[test]
    fn malformed_response_bytes_are_rejected_explicitly() {
        assert_eq!(
            HandshakeResponse::decode(b"nope"),
            Err(HandshakeRejectReason::Malformed)
        );
        assert_eq!(
            HandshakeResponse::decode(&[]),
            Err(HandshakeRejectReason::Malformed)
        );
    }
}
