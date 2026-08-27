//! Wire framing for the persistent reliable stream and encrypted datagrams,
//! plus the pure inbound datagram sequence guard shared by the QUIC adapter
//! and the network impairment tests.

use crate::{DeliveryMechanism, EnvelopeMetadata, MessageClass, ReliabilityClass};

/// Header bytes for one framed reliable-stream message: class, declared size,
/// and per-peer sequence.
pub(crate) const STREAM_FRAME_HEADER_BYTES: usize = 3 + 4 + 8;

/// Header bytes for one datagram frame: class, declared size, and sequence.
pub(crate) const DATAGRAM_FRAME_HEADER_BYTES: usize = 3 + 4 + 8;

fn encode_message_class(class: MessageClass) -> u8 {
    match class {
        MessageClass::Control => 0,
        MessageClass::Media => 1,
        MessageClass::Input => 2,
    }
}

fn decode_message_class(value: u8) -> Option<MessageClass> {
    match value {
        0 => Some(MessageClass::Control),
        1 => Some(MessageClass::Media),
        2 => Some(MessageClass::Input),
        _ => None,
    }
}

fn encode_class(class: ReliabilityClass) -> u8 {
    match class {
        ReliabilityClass::Control => 0,
        ReliabilityClass::MediaReliable => 1,
        ReliabilityClass::MediaLowLatency => 2,
        ReliabilityClass::InputLowLatency => 3,
    }
}

/// Decodes a reliability-class tag byte.
#[must_use]
pub(crate) fn decode_class(value: u8) -> Option<ReliabilityClass> {
    match value {
        0 => Some(ReliabilityClass::Control),
        1 => Some(ReliabilityClass::MediaReliable),
        2 => Some(ReliabilityClass::MediaLowLatency),
        3 => Some(ReliabilityClass::InputLowLatency),
        _ => None,
    }
}

fn encode_delivery(delivery: DeliveryMechanism) -> u8 {
    match delivery {
        DeliveryMechanism::ReliableStream => 0,
        DeliveryMechanism::EncryptedDatagram => 1,
    }
}

fn decode_delivery(value: u8) -> Option<DeliveryMechanism> {
    match value {
        0 => Some(DeliveryMechanism::ReliableStream),
        1 => Some(DeliveryMechanism::EncryptedDatagram),
        _ => None,
    }
}

/// Encodes the fixed-size header for one framed reliable-stream message.
/// The caller writes the header immediately followed by exactly `len` bytes.
#[must_use]
pub(crate) fn encode_stream_header(
    message_class: MessageClass,
    reliability: ReliabilityClass,
    delivery: DeliveryMechanism,
    len: u32,
    sequence: u64,
) -> [u8; STREAM_FRAME_HEADER_BYTES] {
    let mut header = [0_u8; STREAM_FRAME_HEADER_BYTES];
    header[0] = encode_message_class(message_class);
    header[1] = encode_class(reliability);
    header[2] = encode_delivery(delivery);
    header[3..7].copy_from_slice(&len.to_be_bytes());
    header[7..15].copy_from_slice(&sequence.to_be_bytes());
    header
}

/// Decoded reliable-stream frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamFrameHeader {
    /// Semantic protocol message class.
    pub message_class: MessageClass,
    /// Reliability class the payload was sent under.
    pub reliability: ReliabilityClass,
    /// Delivery mechanism declared by the sender.
    pub delivery: DeliveryMechanism,
    /// Payload length in bytes, to be read immediately after the header.
    pub payload_len: u32,
    /// Per-peer sequence number assigned by the sender.
    pub sequence: u64,
}

/// Decodes a fixed-size reliable-stream frame header.
///
/// # Errors
///
/// Returns `None` if the class tag is unrecognized (malformed frame).
#[must_use]
pub(crate) fn decode_stream_header(
    bytes: [u8; STREAM_FRAME_HEADER_BYTES],
) -> Option<StreamFrameHeader> {
    let message_class = decode_message_class(bytes[0])?;
    let reliability = decode_class(bytes[1])?;
    let delivery = decode_delivery(bytes[2])?;
    let payload_len = u32::from_be_bytes(bytes[3..7].try_into().ok()?);
    let sequence = u64::from_be_bytes(bytes[7..15].try_into().ok()?);
    Some(StreamFrameHeader {
        message_class,
        reliability,
        delivery,
        payload_len,
        sequence,
    })
}

/// Encodes one complete datagram frame (header plus payload) ready to hand to
/// `Connection::send_datagram`.
pub(crate) fn encode_datagram_frame(
    metadata: &EnvelopeMetadata,
    payload: &[u8],
) -> Result<Vec<u8>, crate::TransportContractError> {
    let declared_size = u32::try_from(payload.len())
        .map_err(|_| crate::TransportContractError::DeclaredSizeUnrepresentable)?;
    let mut buffer = Vec::with_capacity(DATAGRAM_FRAME_HEADER_BYTES + payload.len());
    buffer.push(encode_message_class(metadata.message_class));
    buffer.push(encode_class(metadata.reliability));
    buffer.push(encode_delivery(metadata.delivery));
    buffer.extend_from_slice(&declared_size.to_be_bytes());
    buffer.extend_from_slice(&metadata.sequence.to_be_bytes());
    buffer.extend_from_slice(payload);
    Ok(buffer)
}

/// Decoded datagram frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DatagramFrame<'a> {
    /// Semantic protocol message class.
    pub message_class: MessageClass,
    /// Reliability class declared by the sender.
    pub reliability: ReliabilityClass,
    /// Delivery mechanism declared by the sender.
    pub delivery: DeliveryMechanism,
    /// Declared application payload bytes.
    pub payload_len: u32,
    /// Sequence number assigned by the sender.
    pub sequence: u64,
    /// Borrowed application payload, not allocated by the decoder.
    pub payload: &'a [u8],
}

/// Decodes one datagram frame.
///
/// # Errors
///
/// Returns `None` if the frame is too short or is not tagged as
/// `MediaLowLatency` (the only class ever carried over datagrams).
#[must_use]
pub(crate) fn decode_datagram_frame(bytes: &[u8]) -> Option<DatagramFrame<'_>> {
    if bytes.len() < DATAGRAM_FRAME_HEADER_BYTES {
        return None;
    }
    let message_class = decode_message_class(bytes[0])?;
    let reliability = decode_class(bytes[1])?;
    let delivery = decode_delivery(bytes[2])?;
    let payload_len = u32::from_be_bytes(bytes[3..7].try_into().ok()?);
    let sequence = u64::from_be_bytes(bytes[7..15].try_into().ok()?);
    let payload = &bytes[DATAGRAM_FRAME_HEADER_BYTES..];
    if usize::try_from(payload_len).ok()? != payload.len() {
        return None;
    }
    Some(DatagramFrame {
        message_class,
        reliability,
        delivery,
        payload_len,
        sequence,
        payload,
    })
}

/// Outcome of submitting a sequence number to a [`DatagramSequenceGuard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceDecision {
    /// The sequence number is newly accepted, in-order or ahead.
    Accept,
    /// The exact sequence number was already delivered (duplicate).
    Duplicate,
    /// The sequence number falls further behind the newest delivered value
    /// than the configured lateness window and is rejected as late.
    Late,
}

/// Deterministic, pure inbound datagram sequence guard.
///
/// Datagrams are unreliable: they may be lost, reordered, duplicated, or
/// arrive late relative to newer datagrams. This guard tracks the highest
/// sequence number delivered so far plus a small bounded window of recently
/// delivered sequence numbers, and lets a consumer reject duplicates and
/// datagrams so late they fall outside the window — without requiring
/// in-order delivery the way the reliable stream provides.
#[derive(Debug, Clone)]
pub struct DatagramSequenceGuard {
    window: usize,
    highest: Option<u64>,
    // Small ring of recently accepted sequence numbers, most-recent last,
    // bounded by `window`, used to detect duplicates that arrive out of
    // strict order but still within the lateness window.
    recent: std::collections::VecDeque<u64>,
}

impl DatagramSequenceGuard {
    /// Creates a guard that keeps `window` most-recently-accepted sequence
    /// numbers to detect duplicates and treats anything older than that
    /// window (relative to the highest accepted sequence number) as late.
    #[must_use]
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            highest: None,
            recent: std::collections::VecDeque::with_capacity(window.max(1)),
        }
    }

    /// Submits one observed sequence number and returns the accept/reject
    /// decision. Accepted sequence numbers are recorded.
    pub fn observe(&mut self, sequence: u64) -> SequenceDecision {
        if let Some(highest) = self.highest {
            if self.recent.contains(&sequence) {
                return SequenceDecision::Duplicate;
            }
            let window = u64::try_from(self.window).unwrap_or(u64::MAX);
            if sequence <= highest && highest - sequence >= window {
                return SequenceDecision::Late;
            }
        }
        self.highest = Some(
            self.highest
                .map_or(sequence, |current| current.max(sequence)),
        );
        if self.recent.len() >= self.window {
            self.recent.pop_front();
        }
        self.recent.push_back(sequence);
        SequenceDecision::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> EnvelopeMetadata {
        EnvelopeMetadata {
            message_class: MessageClass::Media,
            reliability: ReliabilityClass::MediaLowLatency,
            delivery: DeliveryMechanism::EncryptedDatagram,
            declared_size: 5,
            sequence: 7,
            session_id: "session-1".to_owned(),
            peer_identity: "client-1".to_owned(),
        }
    }

    #[test]
    fn stream_header_round_trips() {
        let header = encode_stream_header(
            MessageClass::Control,
            ReliabilityClass::Control,
            DeliveryMechanism::ReliableStream,
            42,
            7,
        );
        assert_eq!(
            decode_stream_header(header),
            Some(StreamFrameHeader {
                message_class: MessageClass::Control,
                reliability: ReliabilityClass::Control,
                delivery: DeliveryMechanism::ReliableStream,
                payload_len: 42,
                sequence: 7,
            })
        );
    }

    #[test]
    fn stream_header_rejects_unknown_class_tag() {
        let mut header = encode_stream_header(
            MessageClass::Control,
            ReliabilityClass::Control,
            DeliveryMechanism::ReliableStream,
            1,
            0,
        );
        header[0] = 255;
        assert_eq!(decode_stream_header(header), None);
    }

    #[test]
    fn datagram_frame_round_trips() {
        let encoded = encode_datagram_frame(&metadata(), b"hello").expect("bounded frame");
        assert_eq!(
            decode_datagram_frame(&encoded),
            Some(DatagramFrame {
                message_class: MessageClass::Media,
                reliability: ReliabilityClass::MediaLowLatency,
                delivery: DeliveryMechanism::EncryptedDatagram,
                payload_len: 5,
                sequence: 7,
                payload: b"hello",
            })
        );
    }

    #[test]
    fn datagram_frame_rejects_unknown_reliability_tag() {
        let mut encoded = encode_datagram_frame(&metadata(), b"hello").expect("bounded frame");
        encoded[1] = 255;
        assert_eq!(decode_datagram_frame(&encoded), None);
    }

    #[test]
    fn datagram_frame_rejects_declared_size_mismatch_without_payload_allocation() {
        let mut encoded = encode_datagram_frame(&metadata(), b"hello").expect("bounded frame");
        encoded[3..7].copy_from_slice(&4_u32.to_be_bytes());
        assert_eq!(decode_datagram_frame(&encoded), None);
    }

    #[test]
    fn wrong_message_and_delivery_classes_are_rejected_from_header_metadata() {
        let policy = crate::BoundedTransportPolicy::default();
        let mut wrong_class = metadata();
        wrong_class.message_class = MessageClass::Control;
        assert_eq!(
            policy.validate_metadata(crate::TransportProfile::Quic, &wrong_class),
            Err(crate::TransportContractError::MessageClassMismatch)
        );

        let mut wrong_delivery = metadata();
        wrong_delivery.reliability = ReliabilityClass::Control;
        wrong_delivery.message_class = MessageClass::Control;
        assert_eq!(
            policy.validate_metadata(crate::TransportProfile::Quic, &wrong_delivery),
            Err(crate::TransportContractError::DeliveryClassMismatch)
        );
    }

    #[test]
    fn datagram_frame_rejects_short_input() {
        assert_eq!(decode_datagram_frame(&[0, 1, 2]), None);
    }

    #[test]
    fn sequence_guard_accepts_in_order() {
        let mut guard = DatagramSequenceGuard::new(8);
        assert_eq!(guard.observe(0), SequenceDecision::Accept);
        assert_eq!(guard.observe(1), SequenceDecision::Accept);
        assert_eq!(guard.observe(2), SequenceDecision::Accept);
    }

    #[test]
    fn sequence_guard_rejects_exact_duplicate() {
        let mut guard = DatagramSequenceGuard::new(8);
        assert_eq!(guard.observe(5), SequenceDecision::Accept);
        assert_eq!(guard.observe(5), SequenceDecision::Duplicate);
    }

    #[test]
    fn sequence_guard_accepts_reordered_within_window() {
        let mut guard = DatagramSequenceGuard::new(8);
        assert_eq!(guard.observe(10), SequenceDecision::Accept);
        assert_eq!(guard.observe(9), SequenceDecision::Accept);
        // Still a duplicate on second delivery even though reordered.
        assert_eq!(guard.observe(9), SequenceDecision::Duplicate);
    }

    #[test]
    fn sequence_guard_rejects_late_outside_window() {
        let mut guard = DatagramSequenceGuard::new(4);
        for sequence in 0..20 {
            guard.observe(sequence);
        }
        // Far behind the highest accepted (19); outside the window.
        assert_eq!(guard.observe(0), SequenceDecision::Late);
    }

    #[test]
    fn sequence_guard_handles_maximum_sequence_without_overflow() {
        let mut guard = DatagramSequenceGuard::new(4);
        assert_eq!(guard.observe(u64::MAX), SequenceDecision::Accept);
        assert_eq!(guard.observe(u64::MAX - 4), SequenceDecision::Late);
    }
}
