//! Length-prefixed framing for the Deck↔helper privilege boundary.
//!
//! Deliberately tiny and dependency-free: this code runs as root, so it must be
//! auditable in one sitting. The URB payloads are the *existing*
//! `arcen-protocol` frames, forwarded verbatim, so this boundary introduces no
//! second wire format to review. See
//! `docs/adr/0011-macos-privileged-usb-helper.md`.

use std::io::{Read, Write};

/// Helper → Deck: the captured device's identity.
pub const TAG_HELLO: u8 = 0x01;
/// Deck → helper: an `arcen-protocol` `usb_urb_submit` frame, verbatim.
pub const TAG_SUBMIT: u8 = 0x02;
/// Deck → helper: an `arcen-protocol` `usb_urb_cancel` frame, verbatim.
pub const TAG_CANCEL: u8 = 0x03;
/// Helper → Deck: an `arcen-protocol` `usb_urb_complete` frame, verbatim.
pub const TAG_COMPLETE: u8 = 0x04;
/// Helper → Deck: a bounded UTF-8 diagnostic.
pub const TAG_ERROR: u8 = 0x05;

/// Upper bound for one framed message.
///
/// `MAX_TRANSFER_BYTES` plus the largest protocol header, rounded up. A frame
/// larger than this is refused before any allocation, so a hostile peer cannot
/// make the root process allocate arbitrarily.
pub const MAX_FRAME_BYTES: usize = arcen_usb_bridge::MAX_TRANSFER_BYTES + 1024;

#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    Oversized(usize),
    Empty,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "helper frame io: {error}"),
            Self::Oversized(len) => write!(formatter, "helper frame of {len} bytes exceeds bound"),
            Self::Empty => formatter.write_str("helper frame is empty"),
        }
    }
}

impl From<std::io::Error> for FrameError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Writes one tagged frame.
///
/// # Errors
///
/// Returns [`FrameError::Oversized`] if the payload exceeds [`MAX_FRAME_BYTES`],
/// or an I/O error if the peer has gone away.
pub fn write_frame(writer: &mut impl Write, tag: u8, payload: &[u8]) -> Result<(), FrameError> {
    let len = payload.len().saturating_add(1);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized(len));
    }
    let header = u32::try_from(len).map_err(|_| FrameError::Oversized(len))?;
    writer.write_all(&header.to_le_bytes())?;
    writer.write_all(&[tag])?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

/// Reads one tagged frame.
///
/// # Errors
///
/// Returns [`FrameError::Oversized`] when the declared length is beyond the
/// bound — checked *before* allocating — [`FrameError::Empty`] for a frame with
/// no tag byte, or an I/O error at end of stream.
pub fn read_frame(reader: &mut impl Read) -> Result<(u8, Vec<u8>), FrameError> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized(len));
    }
    let mut buffer = vec![0_u8; len];
    reader.read_exact(&mut buffer)?;
    let tag = buffer[0];
    buffer.remove(0);
    Ok((tag, buffer))
}

/// Encodes the captured device identity for [`TAG_HELLO`].
#[must_use]
pub fn encode_hello(
    vendor_id: u16,
    product_id: u16,
    bcd_device: u16,
    device_class: u8,
    speed: u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&vendor_id.to_le_bytes());
    out.extend_from_slice(&product_id.to_le_bytes());
    out.extend_from_slice(&bcd_device.to_le_bytes());
    out.push(device_class);
    out.push(speed);
    out
}

/// Decodes a [`TAG_HELLO`] payload into `(vendor, product, bcd, class, speed)`.
///
/// Used by the Deck-side client of this boundary; kept here so both halves of
/// the handshake are defined and tested in one place.
#[allow(dead_code)]
#[must_use]
pub fn decode_hello(payload: &[u8]) -> Option<(u16, u16, u16, u8, u8)> {
    if payload.len() < 8 {
        return None;
    }
    Some((
        u16::from_le_bytes([payload[0], payload[1]]),
        u16::from_le_bytes([payload[2], payload[3]]),
        u16::from_le_bytes([payload[4], payload[5]]),
        payload[6],
        payload[7],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, TAG_SUBMIT, b"payload").expect("write");
        let (tag, payload) = read_frame(&mut buffer.as_slice()).expect("read");
        assert_eq!(tag, TAG_SUBMIT);
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn oversized_declared_length_is_refused_before_allocation() {
        let mut framed = Vec::new();
        let huge = u32::try_from(MAX_FRAME_BYTES + 1).expect("fits");
        framed.extend_from_slice(&huge.to_le_bytes());
        let error = read_frame(&mut framed.as_slice()).expect_err("must refuse");
        assert!(matches!(error, FrameError::Oversized(_)));
    }

    #[test]
    fn empty_frame_is_rejected() {
        let mut framed = Vec::new();
        framed.extend_from_slice(&0_u32.to_le_bytes());
        assert!(matches!(
            read_frame(&mut framed.as_slice()),
            Err(FrameError::Empty)
        ));
    }

    #[test]
    fn hello_round_trips() {
        let encoded = encode_hello(0x056a, 0x0317, 0x0100, 0, 1);
        assert_eq!(decode_hello(&encoded), Some((0x056a, 0x0317, 0x0100, 0, 1)));
    }

    #[test]
    fn short_hello_is_rejected() {
        assert_eq!(decode_hello(&[1, 2, 3]), None);
    }
}
