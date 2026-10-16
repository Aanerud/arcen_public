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
///
/// Layout: vendor `u16`, product `u16`, bcdDevice `u16`, device class `u8`,
/// speed `u8` (all little-endian), then the HID report descriptors that made
/// the device admissible: count `u8`, and per descriptor interface `u8`,
/// length `u16`, bytes. A Deck that predates the descriptors reads only the
/// first eight bytes and ignores the rest.
///
/// # Errors
///
/// Returns a message when there are more than 255 descriptors or one is
/// longer than `u16::MAX`; the caller bounds both far below that.
pub fn encode_hello(
    vendor_id: u16,
    product_id: u16,
    bcd_device: u16,
    device_class: u8,
    speed: u8,
    descriptors: &[(u8, &[u8])],
) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(9);
    out.extend_from_slice(&vendor_id.to_le_bytes());
    out.extend_from_slice(&product_id.to_le_bytes());
    out.extend_from_slice(&bcd_device.to_le_bytes());
    out.push(device_class);
    out.push(speed);
    out.push(
        u8::try_from(descriptors.len())
            .map_err(|_| "too many HID report descriptors for the helper handshake".to_owned())?,
    );
    for (interface, descriptor) in descriptors {
        let length = u16::try_from(descriptor.len())
            .map_err(|_| "HID report descriptor too long for the helper handshake".to_owned())?;
        out.push(*interface);
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(descriptor);
    }
    Ok(out)
}

/// Bytes of a [`TAG_HELLO`] payload carrying `descriptor_lengths`: the
/// eight-byte identity, the count byte, and three bytes of framing per
/// descriptor.
#[must_use]
pub fn hello_len(descriptor_lengths: impl IntoIterator<Item = usize>) -> usize {
    descriptor_lengths
        .into_iter()
        .fold(9_usize, |total, length| {
            total.saturating_add(3).saturating_add(length)
        })
}

/// Whether a hello carrying these descriptors fits one frame (payload plus
/// its tag byte).
#[must_use]
pub fn hello_fits(descriptor_lengths: impl IntoIterator<Item = usize>) -> bool {
    hello_len(descriptor_lengths).saturating_add(1) <= MAX_FRAME_BYTES
}

// The shared aggregate bound a host and the helper enforce must always fit
// the unchanged frame, with every interface present. Older Decks read frames
// up to `MAX_FRAME_BYTES`, so the frame itself is never raised.
// (`<` because the frame also carries the one tag byte.)
const _: () = assert!(
    9 + 3 * arcen_usb_bridge::MAX_INTERFACES
        + arcen_usb_bridge::MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES
        < MAX_FRAME_BYTES
);

/// Decoded [`TAG_HELLO`] payload: `(vendor, product, bcd, class, speed)` and
/// the `(interface, descriptor)` list.
pub type DecodedHello = ((u16, u16, u16, u8, u8), Vec<(u8, Vec<u8>)>);

/// Decodes a [`TAG_HELLO`] payload.
///
/// Used by the Deck-side client of this boundary; kept here so both halves of
/// the handshake are defined and tested in one place. A payload of exactly
/// eight bytes (an older helper) decodes with no descriptors; a malformed
/// descriptor trailer is refused.
#[allow(dead_code)]
#[must_use]
pub fn decode_hello(payload: &[u8]) -> Option<DecodedHello> {
    if payload.len() < 8 {
        return None;
    }
    let identity = (
        u16::from_le_bytes([payload[0], payload[1]]),
        u16::from_le_bytes([payload[2], payload[3]]),
        u16::from_le_bytes([payload[4], payload[5]]),
        payload[6],
        payload[7],
    );
    let mut descriptors = Vec::new();
    if let Some((&count, mut rest)) = payload[8..].split_first() {
        for _ in 0..count {
            let (&interface, after) = rest.split_first()?;
            let length = usize::from(u16::from_le_bytes([*after.first()?, *after.get(1)?]));
            let bytes = after.get(2..2 + length)?;
            descriptors.push((interface, bytes.to_vec()));
            rest = &after[2 + length..];
        }
        if !rest.is_empty() {
            return None;
        }
    }
    Some((identity, descriptors))
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
    fn hello_round_trips_with_report_descriptors() {
        let pen: &[u8] = &[0x05, 0x0d, 0x09, 0x02, 0xa1, 0x01, 0xc0];
        let encoded = encode_hello(
            0x1234,
            0x0317,
            0x0100,
            0,
            1,
            &[(0, pen), (2, &[0x05, 0x01])],
        )
        .unwrap();
        assert_eq!(
            decode_hello(&encoded),
            Some((
                (0x1234, 0x0317, 0x0100, 0, 1),
                vec![(0, pen.to_vec()), (2, vec![0x05, 0x01])]
            ))
        );
    }

    /// The reviewed failure: five individually valid 4 KiB descriptors
    /// produced a 20,505-byte hello that the frame refused only after the
    /// device had been seized.
    #[test]
    fn hello_size_is_known_before_any_device_is_seized() {
        assert!(!hello_fits([4000; 5]));
        assert_eq!(hello_len([4000; 5]), 9 + 5 * 4003);
        assert!(hello_fits([4000; 3]));
        let at_bound = vec![
            arcen_usb_bridge::MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES
                / arcen_usb_bridge::MAX_INTERFACES;
            arcen_usb_bridge::MAX_INTERFACES
        ];
        assert!(hello_fits(at_bound));
        let descriptor = vec![0_u8; 4000];
        let three: Vec<(u8, &[u8])> = (0..3).map(|i| (i, descriptor.as_slice())).collect();
        let encoded = encode_hello(1, 2, 3, 0, 1, &three).unwrap();
        assert_eq!(encoded.len(), hello_len([4000; 3]));
        let mut framed = Vec::new();
        write_frame(&mut framed, TAG_HELLO, &encoded).expect("fits one frame");
    }

    #[test]
    fn an_older_eight_byte_hello_decodes_without_descriptors() {
        let legacy = [0x6a, 0x05, 0x17, 0x03, 0x00, 0x01, 0x00, 0x01];
        assert_eq!(
            decode_hello(&legacy),
            Some(((0x056a, 0x0317, 0x0100, 0, 1), Vec::new()))
        );
    }

    #[test]
    fn short_or_malformed_hello_is_rejected() {
        assert_eq!(decode_hello(&[1, 2, 3]), None);
        let mut truncated = encode_hello(1, 2, 3, 0, 1, &[(0, &[1, 2, 3, 4])]).unwrap();
        truncated.pop();
        assert_eq!(decode_hello(&truncated), None);
        let mut trailing = encode_hello(1, 2, 3, 0, 1, &[(0, &[1])]).unwrap();
        trailing.push(9);
        assert_eq!(decode_hello(&trailing), None);
    }
}
