// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The local boundary between a Deck and its privileged USB component.
//!
//! The component (the macOS helper, ADR 0011; the Windows service) captures
//! the tablet and the unprivileged Deck forwards URBs to it. This module is
//! the whole framing of that boundary, shared so every component and every
//! Deck parse the same bytes under the same bounds:
//!
//! - one frame is `u32` little-endian length (tag byte + payload), a tag
//!   byte, then the payload; a length of zero or above [`MAX_FRAME_BYTES`] is
//!   refused before anything is allocated;
//! - URB payloads ([`TAG_SUBMIT`], [`TAG_CANCEL`], [`TAG_COMPLETE`]) are the
//!   existing `arcen-protocol` frames, forwarded verbatim, so the boundary adds
//!   no second URB format;
//! - the component opens with [`TAG_HELLO`] (the captured device and the HID
//!   report descriptors that made it admissible) and one [`TAG_INFO`].
//!
//! Every byte that arrives here comes from the other side of a privilege
//! boundary: the component reads what a Deck sends, and a Deck reads what
//! something claiming to be the component sends. Nothing is trusted beyond
//! these bounds; the device itself is re-checked by the shared admission.

use std::io::{Read, Write};

use crate::{MAX_HID_REPORT_DESCRIPTOR_BYTES, MAX_INTERFACES, MAX_TRANSFER_BYTES, UsbSpeed};

/// Component → Deck: the captured device's identity.
pub const TAG_HELLO: u8 = 0x01;
/// Deck → component: an `arcen-protocol` `usb_urb_submit` frame, verbatim.
pub const TAG_SUBMIT: u8 = 0x02;
/// Deck → component: an `arcen-protocol` `usb_urb_cancel` frame, verbatim.
pub const TAG_CANCEL: u8 = 0x03;
/// Component → Deck: an `arcen-protocol` `usb_urb_complete` frame, verbatim.
pub const TAG_COMPLETE: u8 = 0x04;
/// Component → Deck: a bounded UTF-8 diagnostic (a refusal or a failure).
pub const TAG_ERROR: u8 = 0x05;
/// Component → Deck: facts about the component, sent once right after
/// [`TAG_HELLO`] (see [`encode_info`]). A Deck that predates it skips the
/// frame.
pub const TAG_INFO: u8 = 0x06;
/// Upper bound for a [`TAG_INFO`] payload.
pub const MAX_INFO_BYTES: usize = 256;
/// Upper bound for a [`TAG_ERROR`] payload.
pub const MAX_ERROR_BYTES: usize = 1024;

/// Upper bound for one frame (tag byte + payload).
///
/// [`MAX_TRANSFER_BYTES`] plus the largest protocol header, rounded up. Never
/// raised: older Decks and helpers enforce the same number.
pub const MAX_FRAME_BYTES: usize = MAX_TRANSFER_BYTES + 1024;

/// Bytes of the fixed device identity at the start of a hello.
const HELLO_IDENTITY_BYTES: usize = 8;

// The aggregate descriptor bound a host and the component enforce must fit
// one frame with every interface present (`<`: the frame also carries the tag).
const _: () = assert!(
    HELLO_IDENTITY_BYTES + 1 + 3 * MAX_INTERFACES + crate::MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES
        < MAX_FRAME_BYTES
);

/// Why a frame could not be read or written.
#[derive(Debug)]
pub enum FrameError {
    /// The stream failed or ended.
    Io(std::io::Error),
    /// The declared or requested length is above [`MAX_FRAME_BYTES`].
    Oversized(usize),
    /// A frame with no tag byte.
    Empty,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "USB component frame io: {error}"),
            Self::Oversized(len) => {
                write!(
                    formatter,
                    "USB component frame of {len} bytes exceeds bound"
                )
            }
            Self::Empty => formatter.write_str("USB component frame is empty"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<std::io::Error> for FrameError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// One frame as it goes on the wire.
///
/// # Errors
///
/// [`FrameError::Oversized`] when the payload does not fit [`MAX_FRAME_BYTES`].
pub fn encode_frame(tag: u8, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let len = payload.len().saturating_add(1);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized(len));
    }
    let header = u32::try_from(len).map_err(|_| FrameError::Oversized(len))?;
    let mut frame = Vec::with_capacity(4 + len);
    frame.extend_from_slice(&header.to_le_bytes());
    frame.push(tag);
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Writes one frame and flushes.
///
/// # Errors
///
/// [`FrameError::Oversized`] for a payload beyond the bound, or the stream's
/// error.
pub fn write_frame(writer: &mut impl Write, tag: u8, payload: &[u8]) -> Result<(), FrameError> {
    writer.write_all(&encode_frame(tag, payload)?)?;
    writer.flush()?;
    Ok(())
}

/// Reads one frame from a blocking stream.
///
/// # Errors
///
/// [`FrameError::Oversized`] when the declared length is beyond the bound
/// (checked before allocating), [`FrameError::Empty`] for a frame with no tag,
/// or the stream's error (including its end).
pub fn read_frame(reader: &mut impl Read) -> Result<(u8, Vec<u8>), FrameError> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header)?;
    let len = declared_len(header)?;
    let mut tag = [0_u8; 1];
    reader.read_exact(&mut tag)?;
    let mut payload = vec![0_u8; len - 1];
    reader.read_exact(&mut payload)?;
    Ok((tag[0], payload))
}

fn declared_len(header: [u8; 4]) -> Result<usize, FrameError> {
    let len = usize::try_from(u32::from_le_bytes(header)).unwrap_or(usize::MAX);
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized(len));
    }
    Ok(len)
}

/// Takes one whole frame from the front of `buffer`, or `None` when more bytes
/// are needed (and then consumes nothing).
///
/// The sans-I/O half for readers that may be cancelled mid-frame: keep every
/// received byte in `buffer`, and call this until it returns `None`.
///
/// # Errors
///
/// [`FrameError::Empty`] or [`FrameError::Oversized`] as soon as the length is
/// known; the stream is then unusable.
pub fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<(u8, Vec<u8>)>, FrameError> {
    let Some(header) = buffer.get(..4) else {
        return Ok(None);
    };
    let len = declared_len([header[0], header[1], header[2], header[3]])?;
    if buffer.len() < 4 + len {
        return Ok(None);
    }
    let tag = buffer[4];
    let payload = buffer[5..4 + len].to_vec();
    buffer.drain(..4 + len);
    Ok(Some((tag, payload)))
}

/// The tag of the whole frame at the front of `buffer`, if one is there.
#[must_use]
pub fn whole_frame_tag(buffer: &[u8]) -> Option<u8> {
    let header = buffer.get(..4)?;
    let len = declared_len([header[0], header[1], header[2], header[3]]).ok()?;
    (buffer.len() >= 4 + len).then(|| buffer[4])
}

/// Numeric encoding of [`UsbSpeed`] in the hello.
#[must_use]
pub const fn speed_code(speed: UsbSpeed) -> u8 {
    match speed {
        UsbSpeed::Low => 0,
        UsbSpeed::Full => 1,
        UsbSpeed::High => 2,
    }
}

/// The [`UsbSpeed`] a hello's speed code names; `None` for any other code.
#[must_use]
pub const fn speed_from_code(code: u8) -> Option<UsbSpeed> {
    match code {
        0 => Some(UsbSpeed::Low),
        1 => Some(UsbSpeed::Full),
        2 => Some(UsbSpeed::High),
        _ => None,
    }
}

/// The captured device, as the component announces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentHello {
    /// `idVendor`.
    pub vendor_id: u16,
    /// `idProduct`.
    pub product_id: u16,
    /// `bcdDevice`.
    pub bcd_device: u16,
    /// `bDeviceClass`.
    pub device_class: u8,
    /// Bus speed.
    pub speed: UsbSpeed,
    /// `(interface, HID report descriptor)` for the interfaces that made the
    /// device admissible.
    pub report_descriptors: Vec<(u8, Vec<u8>)>,
}

/// Bytes of a hello payload carrying descriptors of these lengths.
#[must_use]
pub fn hello_len(descriptor_lengths: impl IntoIterator<Item = usize>) -> usize {
    descriptor_lengths
        .into_iter()
        .fold(HELLO_IDENTITY_BYTES + 1, |total, length| {
            total.saturating_add(3).saturating_add(length)
        })
}

/// Whether a hello carrying these descriptors fits one frame. A component
/// asks this before it seizes the device.
#[must_use]
pub fn hello_fits(descriptor_lengths: impl IntoIterator<Item = usize>) -> bool {
    hello_len(descriptor_lengths).saturating_add(1) <= MAX_FRAME_BYTES
}

/// Encodes a [`TAG_HELLO`] payload.
///
/// Layout: vendor `u16`, product `u16`, bcdDevice `u16`, device class `u8`,
/// speed code `u8` (little-endian), then descriptor count `u8` and per
/// descriptor interface `u8`, length `u16`, bytes. A Deck that predates the
/// descriptors reads only the first eight bytes.
///
/// # Errors
///
/// Returns a message for more than [`MAX_INTERFACES`] descriptors or one
/// longer than [`MAX_HID_REPORT_DESCRIPTOR_BYTES`]: a Deck refuses both.
pub fn encode_hello(hello: &ComponentHello) -> Result<Vec<u8>, String> {
    if hello.report_descriptors.len() > MAX_INTERFACES {
        return Err("too many HID report descriptors for the component handshake".to_owned());
    }
    let mut out = Vec::with_capacity(hello_len(
        hello
            .report_descriptors
            .iter()
            .map(|(_, bytes)| bytes.len()),
    ));
    out.extend_from_slice(&hello.vendor_id.to_le_bytes());
    out.extend_from_slice(&hello.product_id.to_le_bytes());
    out.extend_from_slice(&hello.bcd_device.to_le_bytes());
    out.push(hello.device_class);
    out.push(speed_code(hello.speed));
    out.push(u8::try_from(hello.report_descriptors.len()).unwrap_or(u8::MAX));
    for (interface, descriptor) in &hello.report_descriptors {
        if descriptor.len() > MAX_HID_REPORT_DESCRIPTOR_BYTES {
            return Err("HID report descriptor too long for the component handshake".to_owned());
        }
        let length = u16::try_from(descriptor.len())
            .map_err(|_| "HID report descriptor too long for the component handshake")?;
        out.push(*interface);
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(descriptor);
    }
    Ok(out)
}

/// Decodes a [`TAG_HELLO`] payload, refusing anything out of bounds.
///
/// Exactly eight bytes (an older helper) decode with no descriptors. An
/// unknown speed code, more than [`MAX_INTERFACES`] descriptors, one longer
/// than [`MAX_HID_REPORT_DESCRIPTOR_BYTES`], a truncated descriptor or
/// trailing bytes are refused.
#[must_use]
pub fn decode_hello(payload: &[u8]) -> Option<ComponentHello> {
    let identity = payload.get(..HELLO_IDENTITY_BYTES)?;
    let speed = speed_from_code(identity[7])?;
    let mut report_descriptors = Vec::new();
    if let Some((&count, mut rest)) = payload[HELLO_IDENTITY_BYTES..].split_first() {
        if usize::from(count) > MAX_INTERFACES {
            return None;
        }
        for _ in 0..count {
            let (&interface, after) = rest.split_first()?;
            let length = usize::from(u16::from_le_bytes([*after.first()?, *after.get(1)?]));
            if length > MAX_HID_REPORT_DESCRIPTOR_BYTES {
                return None;
            }
            let bytes = after.get(2..2 + length)?;
            report_descriptors.push((interface, bytes.to_vec()));
            rest = &after[2 + length..];
        }
        if !rest.is_empty() {
            return None;
        }
    }
    Some(ComponentHello {
        vendor_id: u16::from_le_bytes([identity[0], identity[1]]),
        product_id: u16::from_le_bytes([identity[2], identity[3]]),
        bcd_device: u16::from_le_bytes([identity[4], identity[5]]),
        device_class: identity[6],
        speed,
        report_descriptors,
    })
}

/// Encodes the [`TAG_INFO`] payload: `key=value` lines, today only
/// `backend=<name>`, the capture backend this session uses.
#[must_use]
pub fn encode_info(backend: &str) -> Vec<u8> {
    let mut info = format!("backend={backend}").into_bytes();
    info.truncate(MAX_INFO_BYTES);
    info
}

/// The `backend=<name>` of an info payload, if it names one plainly
/// (1–32 ASCII letters, digits or `-`).
#[must_use]
pub fn info_backend(payload: &[u8]) -> Option<String> {
    std::str::from_utf8(payload)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("backend="))
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 32
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        .map(str::to_owned)
}

/// Encodes a [`TAG_ERROR`] payload, cut to [`MAX_ERROR_BYTES`] on a character
/// boundary.
#[must_use]
pub fn encode_error(message: &str) -> Vec<u8> {
    let mut end = message.len().min(MAX_ERROR_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.as_bytes()[..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(descriptors: Vec<(u8, Vec<u8>)>) -> ComponentHello {
        ComponentHello {
            vendor_id: 0x056a,
            product_id: 0x0317,
            bcd_device: 0x0100,
            device_class: 0,
            speed: UsbSpeed::Full,
            report_descriptors: descriptors,
        }
    }

    #[test]
    fn a_frame_round_trips_through_blocking_and_buffered_readers() {
        let mut stream = Vec::new();
        write_frame(&mut stream, TAG_SUBMIT, b"payload").expect("write");
        assert_eq!(
            read_frame(&mut stream.as_slice()).expect("read"),
            (TAG_SUBMIT, b"payload".to_vec())
        );
        let mut buffer = stream.clone();
        assert_eq!(whole_frame_tag(&buffer), Some(TAG_SUBMIT));
        assert_eq!(
            take_frame(&mut buffer).expect("valid"),
            Some((TAG_SUBMIT, b"payload".to_vec()))
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_hostile_length_is_refused_before_its_body_arrives() {
        for header in [u32::try_from(MAX_FRAME_BYTES + 1).expect("fits"), u32::MAX] {
            let mut buffer = header.to_le_bytes().to_vec();
            assert!(matches!(
                take_frame(&mut buffer),
                Err(FrameError::Oversized(_))
            ));
            assert!(matches!(
                read_frame(&mut header.to_le_bytes().as_slice()),
                Err(FrameError::Oversized(_))
            ));
            assert_eq!(whole_frame_tag(&buffer), None);
        }
        let mut empty = 0_u32.to_le_bytes().to_vec();
        assert!(matches!(take_frame(&mut empty), Err(FrameError::Empty)));
        assert!(matches!(
            read_frame(&mut 0_u32.to_le_bytes().as_slice()),
            Err(FrameError::Empty)
        ));
        assert!(matches!(
            encode_frame(TAG_COMPLETE, &vec![0; MAX_FRAME_BYTES]),
            Err(FrameError::Oversized(_))
        ));
        assert!(encode_frame(TAG_COMPLETE, &vec![0; MAX_FRAME_BYTES - 1]).is_ok());
    }

    #[test]
    fn split_and_coalesced_delivery_yields_every_frame_once() {
        let mut wire = encode_frame(TAG_COMPLETE, b"one").expect("frame");
        wire.extend(encode_frame(TAG_ERROR, b"two").expect("frame"));
        wire.extend(encode_frame(TAG_COMPLETE, b"").expect("frame"));
        let mut buffer = Vec::new();
        let mut seen = Vec::new();
        for byte in &wire {
            buffer.push(*byte);
            let before = buffer.clone();
            match take_frame(&mut buffer).expect("valid") {
                Some(frame) => seen.push(frame),
                None => assert_eq!(buffer, before, "a partial frame consumes nothing"),
            }
        }
        assert_eq!(
            seen,
            vec![
                (TAG_COMPLETE, b"one".to_vec()),
                (TAG_ERROR, b"two".to_vec()),
                (TAG_COMPLETE, Vec::new()),
            ]
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_hello_round_trips_with_report_descriptors() {
        let sent = hello(vec![
            (0, vec![0x05, 0x0d, 0x09, 0x02, 0xa1, 0x01, 0xc0]),
            (2, vec![0x05, 0x01]),
        ]);
        let encoded = encode_hello(&sent).expect("encodes");
        assert_eq!(encoded.len(), hello_len([7, 2]));
        assert_eq!(decode_hello(&encoded), Some(sent));
    }

    #[test]
    fn an_older_eight_byte_hello_decodes_without_descriptors() {
        let legacy = [0x6a, 0x05, 0x17, 0x03, 0x00, 0x01, 0x00, 0x01];
        assert_eq!(decode_hello(&legacy), Some(hello(Vec::new())));
    }

    #[test]
    fn a_malformed_hello_is_refused_rather_than_guessed() {
        assert_eq!(decode_hello(&[1, 2, 3]), None);
        let mut unknown_speed = encode_hello(&hello(Vec::new())).expect("encodes");
        unknown_speed[7] = 0xff;
        assert_eq!(decode_hello(&unknown_speed), None);
        let mut truncated = encode_hello(&hello(vec![(0, vec![1, 2, 3, 4])])).expect("encodes");
        truncated.pop();
        assert_eq!(decode_hello(&truncated), None);
        let mut trailing = encode_hello(&hello(vec![(0, vec![1])])).expect("encodes");
        trailing.push(9);
        assert_eq!(decode_hello(&trailing), None);
        let mut too_many = encode_hello(&hello(Vec::new())).expect("encodes");
        too_many[8] = u8::try_from(MAX_INTERFACES + 1).expect("fits");
        assert_eq!(decode_hello(&too_many), None);
        let mut too_long = encode_hello(&hello(Vec::new())).expect("encodes");
        too_long[8] = 1;
        too_long.push(0);
        too_long.extend_from_slice(
            &u16::try_from(MAX_HID_REPORT_DESCRIPTOR_BYTES + 1)
                .expect("fits")
                .to_le_bytes(),
        );
        too_long.extend(std::iter::repeat_n(0, MAX_HID_REPORT_DESCRIPTOR_BYTES + 1));
        assert_eq!(decode_hello(&too_long), None);
    }

    #[test]
    fn a_hello_the_deck_would_refuse_is_never_encoded() {
        assert!(encode_hello(&hello(vec![(0, vec![0]); MAX_INTERFACES + 1])).is_err());
        assert!(
            encode_hello(&hello(vec![(
                0,
                vec![0; MAX_HID_REPORT_DESCRIPTOR_BYTES + 1]
            )]))
            .is_err()
        );
    }

    /// The reviewed failure: five individually valid 4 KiB descriptors made a
    /// 20,505-byte hello that the frame refused only after the device had been
    /// seized.
    #[test]
    fn hello_size_is_known_before_any_device_is_seized() {
        assert!(!hello_fits([4000; 5]));
        assert_eq!(hello_len([4000; 5]), 9 + 5 * 4003);
        assert!(hello_fits([4000; 3]));
        let at_bound =
            vec![crate::MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES / MAX_INTERFACES; MAX_INTERFACES];
        assert!(hello_fits(at_bound));
        let three = hello((0..3).map(|i| (i, vec![0_u8; 4000])).collect());
        let encoded = encode_hello(&three).expect("encodes");
        assert_eq!(encoded.len(), hello_len([4000; 3]));
        assert!(encode_frame(TAG_HELLO, &encoded).is_ok());
    }

    #[test]
    fn the_info_frame_names_a_plain_backend_only() {
        assert_eq!(
            info_backend(&encode_info("iousbhost")).as_deref(),
            Some("iousbhost")
        );
        assert_eq!(
            info_backend(b"x=1\nbackend=winusb").as_deref(),
            Some("winusb")
        );
        assert_eq!(info_backend(b"backend="), None);
        assert_eq!(info_backend(b"backend=a b"), None);
        assert_eq!(info_backend(&[0xff, 0xfe]), None);
        assert!(encode_info(&"x".repeat(1000)).len() <= MAX_INFO_BYTES);
    }

    #[test]
    fn an_error_is_cut_on_a_character_boundary() {
        let long = "ø".repeat(MAX_ERROR_BYTES);
        let encoded = encode_error(&long);
        assert!(encoded.len() <= MAX_ERROR_BYTES);
        assert!(std::str::from_utf8(&encoded).is_ok());
        assert_eq!(encode_error("short"), b"short");
    }

    #[test]
    fn speed_codes_round_trip() {
        for speed in [UsbSpeed::Low, UsbSpeed::Full, UsbSpeed::High] {
            assert_eq!(speed_from_code(speed_code(speed)), Some(speed));
        }
        assert_eq!(speed_from_code(3), None);
    }
}
