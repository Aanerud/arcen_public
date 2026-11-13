// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Frames between a session and the process that owns its bridged HID devices
//! (ADR 0022 §9).
//!
//! A macOS Pier session cannot create `IOHIDUserDevice`s itself: only the
//! Pier's own signature carries the entitlement, so its entitled
//! `hid-injector` child owns them. The session plans the devices
//! ([`crate::HidHostAdapter`]) and speaks this contract to the child over its
//! standard input and output. It is a private, versionless pipe between two
//! copies of one executable, so it carries no negotiation — only bounds.
//!
//! A frame is `kind: u8`, `length: u32` little-endian, then `length` payload
//! bytes, at most [`MAX_HID_DEVICE_FRAME_PAYLOAD`].

use crate::{BridgedHidDevice, EndpointAddress, MAX_INTERFACES, MAX_TRANSFER_BYTES};
use std::fmt::{Display, Formatter};
use std::io::{Read, Write};

/// Largest payload of one frame.
pub const MAX_HID_DEVICE_FRAME_PAYLOAD: usize = 32 * 1024;
/// Bytes before the payload.
pub const HID_DEVICE_FRAME_HEADER: usize = 5;
/// Longest string carried (reasons, properties), in bytes.
pub const MAX_HID_DEVICE_STRING: usize = 512;

const KIND_CREATE: u8 = 0x01;
const KIND_ACTIVATE: u8 = 0x02;
const KIND_REPORT: u8 = 0x03;
const KIND_REPORT_REPLY: u8 = 0x04;
const KIND_READY: u8 = 0x81;
const KIND_REFUSED: u8 = 0x82;
const KIND_GET_REPORT: u8 = 0x83;
const KIND_SET_REPORT: u8 = 0x84;

/// One frame of the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HidDeviceFrame {
    /// Session → child: describe one device; sent once per device, in index
    /// order, before [`Self::Activate`].
    Create(BridgedHidDevice),
    /// Session → child: create and activate every described device.
    Activate,
    /// Session → child: one input report for device `index`.
    Report { index: u8, report: Vec<u8> },
    /// Session → child: the answer to report request `token`. `ok` false
    /// fails the host's call.
    ReportReply { token: u32, ok: bool, data: Vec<u8> },
    /// Child → session: every device exists.
    Ready { devices: u8 },
    /// Child → session: the devices could not be created, and why.
    Refused { reason: String },
    /// Child → session: the host asked device `index` for a report.
    GetReport {
        token: u32,
        index: u8,
        kind: u8,
        report_id: u8,
        length: u16,
    },
    /// Child → session: the host set a report on device `index`.
    SetReport {
        token: u32,
        index: u8,
        kind: u8,
        report_id: u8,
        data: Vec<u8>,
    },
}

/// A frame that breaks the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidDeviceFrameError {
    /// Unknown kind byte.
    Kind(u8),
    /// Payload longer than [`MAX_HID_DEVICE_FRAME_PAYLOAD`].
    TooLarge(usize),
    /// Payload shorter than its fields, or with bytes left over.
    Malformed(u8),
    /// A field exceeds its own bound.
    OutOfBounds(u8),
}

impl Display for HidDeviceFrameError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kind(kind) => write!(formatter, "unknown HID device frame kind {kind:#04x}"),
            Self::TooLarge(length) => write!(formatter, "HID device frame of {length} bytes"),
            Self::Malformed(kind) => write!(formatter, "malformed HID device frame {kind:#04x}"),
            Self::OutOfBounds(kind) => {
                write!(formatter, "HID device frame {kind:#04x} is out of bounds")
            }
        }
    }
}

impl std::error::Error for HidDeviceFrameError {}

impl HidDeviceFrame {
    /// Encodes the frame, header included.
    ///
    /// # Errors
    ///
    /// [`HidDeviceFrameError`] when a field exceeds its bound.
    pub fn encode(&self) -> Result<Vec<u8>, HidDeviceFrameError> {
        let mut payload = Vec::new();
        let kind = match self {
            Self::Create(device) => {
                payload.push(device.index);
                payload.push(device.interface);
                put_u16(&mut payload, device.vendor_id);
                put_u16(&mut payload, device.product_id);
                put_u16(&mut payload, device.version);
                payload.push(device.country_code);
                put_u16(&mut payload, device.primary_usage_page);
                put_u16(&mut payload, device.primary_usage);
                payload.extend_from_slice(&device.location_id.to_le_bytes());
                payload.push(device.input_endpoint.0);
                put_u16(&mut payload, device.input_length);
                put_string(&mut payload, &device.manufacturer, KIND_CREATE)?;
                put_string(&mut payload, &device.product, KIND_CREATE)?;
                put_string(&mut payload, &device.serial, KIND_CREATE)?;
                put_bytes(
                    &mut payload,
                    &device.report_descriptor,
                    crate::MAX_HID_REPORT_DESCRIPTOR_BYTES,
                    KIND_CREATE,
                )?;
                KIND_CREATE
            }
            Self::Activate => KIND_ACTIVATE,
            Self::Report { index, report } => {
                payload.push(*index);
                put_bytes(&mut payload, report, MAX_TRANSFER_BYTES, KIND_REPORT)?;
                KIND_REPORT
            }
            Self::ReportReply { token, ok, data } => {
                payload.extend_from_slice(&token.to_le_bytes());
                payload.push(u8::from(*ok));
                put_bytes(&mut payload, data, MAX_TRANSFER_BYTES, KIND_REPORT_REPLY)?;
                KIND_REPORT_REPLY
            }
            Self::Ready { devices } => {
                payload.push(*devices);
                KIND_READY
            }
            Self::Refused { reason } => {
                put_string(&mut payload, reason, KIND_REFUSED)?;
                KIND_REFUSED
            }
            Self::GetReport {
                token,
                index,
                kind,
                report_id,
                length,
            } => {
                payload.extend_from_slice(&token.to_le_bytes());
                payload.extend_from_slice(&[*index, *kind, *report_id]);
                put_u16(&mut payload, *length);
                KIND_GET_REPORT
            }
            Self::SetReport {
                token,
                index,
                kind,
                report_id,
                data,
            } => {
                payload.extend_from_slice(&token.to_le_bytes());
                payload.extend_from_slice(&[*index, *kind, *report_id]);
                put_bytes(&mut payload, data, MAX_TRANSFER_BYTES, KIND_SET_REPORT)?;
                KIND_SET_REPORT
            }
        };
        if payload.len() > MAX_HID_DEVICE_FRAME_PAYLOAD {
            return Err(HidDeviceFrameError::TooLarge(payload.len()));
        }
        let mut frame = Vec::with_capacity(HID_DEVICE_FRAME_HEADER + payload.len());
        frame.push(kind);
        frame.extend_from_slice(
            &u32::try_from(payload.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        frame.extend_from_slice(&payload);
        Ok(frame)
    }

    /// Reads the kind and payload length from a header.
    ///
    /// # Errors
    ///
    /// [`HidDeviceFrameError::TooLarge`] for a payload over the bound.
    pub fn header(
        header: [u8; HID_DEVICE_FRAME_HEADER],
    ) -> Result<(u8, usize), HidDeviceFrameError> {
        let length = usize::try_from(u32::from_le_bytes([
            header[1], header[2], header[3], header[4],
        ]))
        .unwrap_or(usize::MAX);
        if length > MAX_HID_DEVICE_FRAME_PAYLOAD {
            return Err(HidDeviceFrameError::TooLarge(length));
        }
        Ok((header[0], length))
    }

    /// Decodes one payload of `kind`.
    ///
    /// # Errors
    ///
    /// [`HidDeviceFrameError`] for an unknown kind, a malformed payload or a
    /// field out of bounds.
    pub fn decode(kind: u8, payload: &[u8]) -> Result<Self, HidDeviceFrameError> {
        let mut reader = Fields {
            bytes: payload,
            kind,
        };
        let frame = match kind {
            KIND_CREATE => {
                let index = reader.u8()?;
                let interface = reader.u8()?;
                let vendor_id = reader.u16()?;
                let product_id = reader.u16()?;
                let version = reader.u16()?;
                let country_code = reader.u8()?;
                let primary_usage_page = reader.u16()?;
                let primary_usage = reader.u16()?;
                let location_id = reader.u32()?;
                let input_endpoint = EndpointAddress(reader.u8()?);
                let input_length = reader.u16()?;
                let manufacturer = reader.string()?;
                let product = reader.string()?;
                let serial = reader.string()?;
                let report_descriptor = reader.bytes(crate::MAX_HID_REPORT_DESCRIPTOR_BYTES)?;
                if usize::from(index) >= MAX_INTERFACES
                    || report_descriptor.is_empty()
                    || input_endpoint.0 & 0x80 == 0
                    || input_length == 0
                    || usize::from(input_length) > MAX_TRANSFER_BYTES
                {
                    return Err(HidDeviceFrameError::OutOfBounds(kind));
                }
                Self::Create(BridgedHidDevice {
                    index,
                    interface,
                    report_descriptor,
                    vendor_id,
                    product_id,
                    version,
                    country_code,
                    primary_usage_page,
                    primary_usage,
                    location_id,
                    manufacturer,
                    product,
                    serial,
                    input_endpoint,
                    input_length,
                })
            }
            KIND_ACTIVATE => Self::Activate,
            KIND_REPORT => Self::Report {
                index: reader.u8()?,
                report: reader.bytes(MAX_TRANSFER_BYTES)?,
            },
            KIND_REPORT_REPLY => {
                let token = reader.u32()?;
                let ok = match reader.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(HidDeviceFrameError::Malformed(kind)),
                };
                Self::ReportReply {
                    token,
                    ok,
                    data: reader.bytes(MAX_TRANSFER_BYTES)?,
                }
            }
            KIND_READY => Self::Ready {
                devices: reader.u8()?,
            },
            KIND_REFUSED => Self::Refused {
                reason: reader.string()?,
            },
            KIND_GET_REPORT => Self::GetReport {
                token: reader.u32()?,
                index: reader.u8()?,
                kind: reader.u8()?,
                report_id: reader.u8()?,
                length: reader.u16()?,
            },
            KIND_SET_REPORT => Self::SetReport {
                token: reader.u32()?,
                index: reader.u8()?,
                kind: reader.u8()?,
                report_id: reader.u8()?,
                data: reader.bytes(MAX_TRANSFER_BYTES)?,
            },
            other => return Err(HidDeviceFrameError::Kind(other)),
        };
        if !reader.bytes.is_empty() {
            return Err(HidDeviceFrameError::Malformed(kind));
        }
        Ok(frame)
    }

    /// Reads one frame. `Ok(None)` at a clean end of input.
    ///
    /// # Errors
    ///
    /// An I/O error, or `InvalidData` carrying the [`HidDeviceFrameError`].
    pub fn read_from(reader: &mut impl Read) -> std::io::Result<Option<Self>> {
        let mut header = [0_u8; HID_DEVICE_FRAME_HEADER];
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        let invalid = |error: HidDeviceFrameError| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        };
        let (kind, length) = Self::header(header).map_err(invalid)?;
        let mut payload = vec![0_u8; length];
        reader.read_exact(&mut payload)?;
        Self::decode(kind, &payload).map(Some).map_err(invalid)
    }

    /// Writes one frame and flushes.
    ///
    /// # Errors
    ///
    /// An I/O error, or `InvalidData` for a frame out of bounds.
    pub fn write_to(&self, writer: &mut impl Write) -> std::io::Result<()> {
        let frame = self.encode().map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })?;
        writer.write_all(&frame)?;
        writer.flush()
    }
}

fn put_u16(payload: &mut Vec<u8>, value: u16) {
    payload.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(
    payload: &mut Vec<u8>,
    bytes: &[u8],
    maximum: usize,
    kind: u8,
) -> Result<(), HidDeviceFrameError> {
    if bytes.len() > maximum {
        return Err(HidDeviceFrameError::OutOfBounds(kind));
    }
    let length = u16::try_from(bytes.len()).map_err(|_| HidDeviceFrameError::OutOfBounds(kind))?;
    put_u16(payload, length);
    payload.extend_from_slice(bytes);
    Ok(())
}

fn put_string(payload: &mut Vec<u8>, text: &str, kind: u8) -> Result<(), HidDeviceFrameError> {
    put_bytes(payload, text.as_bytes(), MAX_HID_DEVICE_STRING, kind)
}

struct Fields<'a> {
    bytes: &'a [u8],
    kind: u8,
}

impl Fields<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8], HidDeviceFrameError> {
        if self.bytes.len() < count {
            return Err(HidDeviceFrameError::Malformed(self.kind));
        }
        let (head, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, HidDeviceFrameError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, HidDeviceFrameError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, HidDeviceFrameError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn bytes(&mut self, maximum: usize) -> Result<Vec<u8>, HidDeviceFrameError> {
        let length = usize::from(self.u16()?);
        if length > maximum {
            return Err(HidDeviceFrameError::OutOfBounds(self.kind));
        }
        Ok(self.take(length)?.to_vec())
    }

    fn string(&mut self) -> Result<String, HidDeviceFrameError> {
        let kind = self.kind;
        let bytes = self.bytes(MAX_HID_DEVICE_STRING)?;
        String::from_utf8(bytes).map_err(|_| HidDeviceFrameError::Malformed(kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> BridgedHidDevice {
        BridgedHidDevice {
            index: 0,
            interface: 0,
            report_descriptor: crate::INTUOS5_PEN_REPORT_DESCRIPTOR.to_vec(),
            vendor_id: 0x056a,
            product_id: 0x0317,
            version: 0x0107,
            country_code: 0,
            primary_usage_page: 0xff0d,
            primary_usage: 1,
            location_id: 0xA2C0_0100,
            manufacturer: "Wacom Co.,Ltd.".to_owned(),
            product: "Intuos5 touch L".to_owned(),
            serial: "arcen-0000000000000001-0".to_owned(),
            input_endpoint: EndpointAddress(0x81),
            input_length: 16,
        }
    }

    #[test]
    fn every_frame_round_trips_through_a_pipe() {
        let frames = vec![
            HidDeviceFrame::Create(device()),
            HidDeviceFrame::Activate,
            HidDeviceFrame::Report {
                index: 0,
                report: vec![2, 0xe0, 1, 2, 3, 4, 5, 6, 7, 8],
            },
            HidDeviceFrame::ReportReply {
                token: 7,
                ok: true,
                data: vec![2, 2],
            },
            HidDeviceFrame::Ready { devices: 3 },
            HidDeviceFrame::Refused {
                reason: "no entitlement".to_owned(),
            },
            HidDeviceFrame::GetReport {
                token: 9,
                index: 1,
                kind: 3,
                report_id: 2,
                length: 2,
            },
            HidDeviceFrame::SetReport {
                token: 10,
                index: 0,
                kind: 3,
                report_id: 2,
                data: vec![2, 2],
            },
        ];
        let mut pipe = Vec::new();
        for frame in &frames {
            frame.write_to(&mut pipe).unwrap();
        }
        let mut reader = pipe.as_slice();
        for frame in &frames {
            assert_eq!(
                HidDeviceFrame::read_from(&mut reader).unwrap().as_ref(),
                Some(frame)
            );
        }
        assert_eq!(HidDeviceFrame::read_from(&mut reader).unwrap(), None);
    }

    #[test]
    fn oversized_and_malformed_frames_are_refused() {
        let mut header = [KIND_REPORT, 0, 0, 0, 0];
        header[1..].copy_from_slice(
            &u32::try_from(MAX_HID_DEVICE_FRAME_PAYLOAD + 1)
                .unwrap()
                .to_le_bytes(),
        );
        assert!(matches!(
            HidDeviceFrame::header(header),
            Err(HidDeviceFrameError::TooLarge(_))
        ));
        assert_eq!(
            HidDeviceFrame::decode(0x55, &[]),
            Err(HidDeviceFrameError::Kind(0x55))
        );
        // A report whose length runs past the payload, and one with a trailing
        // byte.
        assert_eq!(
            HidDeviceFrame::decode(KIND_REPORT, &[0, 4, 0, 1]),
            Err(HidDeviceFrameError::Malformed(KIND_REPORT))
        );
        assert_eq!(
            HidDeviceFrame::decode(KIND_ACTIVATE, &[0]),
            Err(HidDeviceFrameError::Malformed(KIND_ACTIVATE))
        );
        // A reply flag other than 0/1.
        assert_eq!(
            HidDeviceFrame::decode(KIND_REPORT_REPLY, &[1, 0, 0, 0, 2, 0, 0]),
            Err(HidDeviceFrameError::Malformed(KIND_REPORT_REPLY))
        );
        // Out-of-bounds fields never encode.
        let too_long = HidDeviceFrame::Refused {
            reason: "x".repeat(MAX_HID_DEVICE_STRING + 1),
        };
        assert_eq!(
            too_long.encode(),
            Err(HidDeviceFrameError::OutOfBounds(KIND_REFUSED))
        );
    }

    #[test]
    fn a_create_frame_must_describe_a_usable_device() {
        for broken in [
            BridgedHidDevice {
                index: u8::try_from(MAX_INTERFACES).unwrap(),
                ..device()
            },
            BridgedHidDevice {
                input_endpoint: EndpointAddress(0x01),
                ..device()
            },
            BridgedHidDevice {
                input_length: 0,
                ..device()
            },
            BridgedHidDevice {
                report_descriptor: Vec::new(),
                ..device()
            },
        ] {
            let frame = HidDeviceFrame::Create(broken).encode().unwrap();
            assert_eq!(
                HidDeviceFrame::decode(frame[0], &frame[HID_DEVICE_FRAME_HEADER..]),
                Err(HidDeviceFrameError::OutOfBounds(KIND_CREATE))
            );
        }
    }
}
