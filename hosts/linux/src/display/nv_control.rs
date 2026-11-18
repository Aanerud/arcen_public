// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! NVIDIA's NV-CONTROL X extension, spoken over an X connection the caller
//! already opened and proved, for the one attribute Arcen changes on a
//! session's X server: the X screen's current MetaMode.
//!
//! This replaces running `nvidia-settings` as root against a desktop's X
//! server: no privileged child process parses that server's replies, reads
//! its authority file by path, or picks the socket to send the cookie to.
//! The connection is the caller's (`session::console::connect`: authority
//! file opened without following links, the X server proved by its peer
//! credentials), and every reply is parsed here, bounded.
//!
//! Wire facts (NV-CONTROL 1.x, the extension's own protocol header): the
//! string query is minor opcode 4 and the string assignment minor opcode 9;
//! both address a target (type 0 = X screen) and an attribute, here
//! `CurrentMetaMode` in its second syntax (45, the `DPY-N:` names
//! `nvidia-settings --query CurrentMetaMode` prints).

use std::io::IoSlice;

use thiserror::Error;
use x11rb::connection::RequestConnection;
use x11rb::errors::ParseError;
use x11rb::x11_utils::TryParse;

/// The extension's name on the X server.
pub const EXTENSION_NAME: &str = "NV-CONTROL";
const QUERY_STRING_ATTRIBUTE: u8 = 4;
const SET_STRING_ATTRIBUTE: u8 = 9;
const TARGET_TYPE_X_SCREEN: u16 = 0;
/// `NV_CTRL_STRING_CURRENT_METAMODE_VERSION_2`.
const CURRENT_METAMODE: u32 = 45;
/// A MetaMode longer than this is not one Arcen wrote or can journal.
pub const MAX_METAMODE_BYTES: usize = 16 * 1024;
const REPLY_HEADER_BYTES: usize = 32;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NvControlError {
    #[error("the X server offers no NV-CONTROL extension")]
    Missing,
    #[error("NV-CONTROL request failed: {0}")]
    Connection(String),
    #[error("NV-CONTROL refused the MetaMode")]
    Refused,
    #[error("NV-CONTROL returned no MetaMode")]
    NoMetaMode,
    #[error("the MetaMode is not one Arcen sends: {0}")]
    Invalid(&'static str),
}

/// The X screen's current MetaMode, without the `token=value … ::` prefix
/// the driver may put before it.
///
/// # Errors
///
/// When the server has no NV-CONTROL, the request fails, or the answer is
/// not a MetaMode.
pub fn current_metamode(
    connection: &impl RequestConnection,
    screen: u16,
) -> Result<String, NvControlError> {
    let major = major_opcode(connection)?;
    let request = query_string_request(major, screen, CURRENT_METAMODE);
    let reply = connection
        .send_request_with_reply::<StringReply>(&[IoSlice::new(&request)], Vec::new())
        .map_err(|error| NvControlError::Connection(error.to_string()))?
        .reply()
        .map_err(|error| NvControlError::Connection(error.to_string()))?;
    metamode_from_reply(&reply)
}

/// Switches the X screen to `metamode` (what `nvidia-settings --assign
/// CurrentMetaMode=…` does).
///
/// # Errors
///
/// When `metamode` is not a plain MetaMode string, the server has no
/// NV-CONTROL, the request fails, or the driver refuses the MetaMode.
pub fn set_current_metamode(
    connection: &impl RequestConnection,
    screen: u16,
    metamode: &str,
) -> Result<(), NvControlError> {
    let major = major_opcode(connection)?;
    let request = set_string_request(major, screen, CURRENT_METAMODE, metamode)?;
    let reply = connection
        .send_request_with_reply::<SetReply>(&[IoSlice::new(&request)], Vec::new())
        .map_err(|error| NvControlError::Connection(error.to_string()))?
        .reply()
        .map_err(|error| NvControlError::Connection(error.to_string()))?;
    if reply.accepted {
        Ok(())
    } else {
        Err(NvControlError::Refused)
    }
}

fn major_opcode(connection: &impl RequestConnection) -> Result<u8, NvControlError> {
    connection
        .extension_information(EXTENSION_NAME)
        .map_err(|error| NvControlError::Connection(error.to_string()))?
        .map(|extension| extension.major_opcode)
        .ok_or(NvControlError::Missing)
}

/// A MetaMode Arcen may send: printable ASCII (the driver's MetaMode
/// syntax), not empty, bounded.
fn validate_metamode(metamode: &str) -> Result<(), NvControlError> {
    if metamode.trim().is_empty() {
        return Err(NvControlError::Invalid("empty"));
    }
    if metamode.len() > MAX_METAMODE_BYTES {
        return Err(NvControlError::Invalid("too long"));
    }
    if !metamode.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return Err(NvControlError::Invalid("not printable ASCII"));
    }
    Ok(())
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

/// `X_nvCtrlQueryStringAttribute` for an X screen: 16 bytes, in the
/// connection's (native) byte order.
fn query_string_request(major: u8, screen: u16, attribute: u32) -> Vec<u8> {
    let mut bytes = vec![major, QUERY_STRING_ATTRIBUTE];
    put_u16(&mut bytes, 4);
    put_u16(&mut bytes, screen);
    put_u16(&mut bytes, TARGET_TYPE_X_SCREEN);
    put_u32(&mut bytes, 0);
    put_u32(&mut bytes, attribute);
    bytes
}

/// `X_nvCtrlSetStringAttribute` for an X screen: a 20-byte header and the
/// NUL-terminated value, padded to four bytes.
fn set_string_request(
    major: u8,
    screen: u16,
    attribute: u32,
    value: &str,
) -> Result<Vec<u8>, NvControlError> {
    validate_metamode(value)?;
    let with_nul = value.len() + 1;
    let padded = with_nul.next_multiple_of(4);
    let length =
        u16::try_from((20 + padded) / 4).map_err(|_| NvControlError::Invalid("too long"))?;
    let num_bytes = u32::try_from(with_nul).map_err(|_| NvControlError::Invalid("too long"))?;
    let mut bytes = vec![major, SET_STRING_ATTRIBUTE];
    put_u16(&mut bytes, length);
    put_u16(&mut bytes, screen);
    put_u16(&mut bytes, TARGET_TYPE_X_SCREEN);
    put_u32(&mut bytes, 0);
    put_u32(&mut bytes, attribute);
    put_u32(&mut bytes, num_bytes);
    bytes.extend_from_slice(value.as_bytes());
    bytes.resize(20 + padded, 0);
    Ok(bytes)
}

fn reply_u32(value: &[u8], offset: usize) -> Result<u32, ParseError> {
    value
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_ne_bytes)
        .ok_or(ParseError::InsufficientData)
}

/// The reply's header and the length of its body (the reply's own length
/// field, in four-byte units), bounded by what arrived.
fn reply_body(value: &[u8]) -> Result<(&[u8], &[u8]), ParseError> {
    if value.len() < REPLY_HEADER_BYTES || value[0] != 1 {
        return Err(ParseError::InvalidValue);
    }
    let extra = usize::try_from(reply_u32(value, 4)?)
        .ok()
        .and_then(|units| units.checked_mul(4))
        .ok_or(ParseError::InvalidValue)?;
    let end = REPLY_HEADER_BYTES
        .checked_add(extra)
        .ok_or(ParseError::InvalidValue)?;
    if value.len() < end {
        return Err(ParseError::InsufficientData);
    }
    Ok((&value[..end], &value[end..]))
}

/// `xnvCtrlQueryStringAttributeReply` and its string.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StringReply {
    exists: bool,
    value: Vec<u8>,
}

impl TryParse for StringReply {
    fn try_parse(value: &[u8]) -> Result<(Self, &[u8]), ParseError> {
        let (reply, remaining) = reply_body(value)?;
        let exists = reply_u32(reply, 8)? != 0;
        let count = usize::try_from(reply_u32(reply, 12)?).map_err(|_| ParseError::InvalidValue)?;
        let body = &reply[REPLY_HEADER_BYTES..];
        let value = if exists {
            body.get(..count).ok_or(ParseError::InvalidValue)?.to_vec()
        } else {
            Vec::new()
        };
        Ok((Self { exists, value }, remaining))
    }
}

/// `xnvCtrlSetStringAttributeReply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SetReply {
    accepted: bool,
}

impl TryParse for SetReply {
    fn try_parse(value: &[u8]) -> Result<(Self, &[u8]), ParseError> {
        let (reply, remaining) = reply_body(value)?;
        Ok((
            Self {
                accepted: reply_u32(reply, 8)? != 0,
            },
            remaining,
        ))
    }
}

fn metamode_from_reply(reply: &StringReply) -> Result<String, NvControlError> {
    if !reply.exists {
        return Err(NvControlError::NoMetaMode);
    }
    let end = reply
        .value
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(reply.value.len());
    let text = std::str::from_utf8(&reply.value[..end])
        .map_err(|_| NvControlError::Invalid("not text"))?;
    let metamode = text
        .split_once("::")
        .map_or(text, |(_, metamode)| metamode)
        .trim();
    validate_metamode(metamode).map_err(|_| NvControlError::NoMetaMode)?;
    Ok(metamode.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(flags: u32, words: &[u32], body: &[u8]) -> Vec<u8> {
        let padded = body.len().next_multiple_of(4);
        let mut bytes = vec![1, 0];
        bytes.extend_from_slice(&7_u16.to_ne_bytes());
        bytes.extend_from_slice(&u32::try_from(padded / 4).unwrap().to_ne_bytes());
        bytes.extend_from_slice(&flags.to_ne_bytes());
        for word in words {
            bytes.extend_from_slice(&word.to_ne_bytes());
        }
        bytes.resize(REPLY_HEADER_BYTES, 0);
        bytes.extend_from_slice(body);
        bytes.resize(REPLY_HEADER_BYTES + padded, 0);
        bytes
    }

    #[test]
    fn the_query_names_the_screens_current_metamode() {
        let request = query_string_request(150, 0, CURRENT_METAMODE);
        assert_eq!(request.len(), 16);
        assert_eq!(&request[..2], &[150, 4]);
        assert_eq!(u16::from_ne_bytes([request[2], request[3]]), 4);
        assert_eq!(u16::from_ne_bytes([request[4], request[5]]), 0);
        assert_eq!(u16::from_ne_bytes([request[6], request[7]]), 0);
        assert_eq!(u32::from_ne_bytes(request[12..16].try_into().unwrap()), 45);
    }

    #[test]
    fn the_assignment_carries_the_terminated_padded_metamode() {
        let metamode = "DPY-0: nvidia-auto-select @2560x1600 +0+0";
        let request = set_string_request(150, 0, CURRENT_METAMODE, metamode).unwrap();
        assert_eq!(request.len() % 4, 0);
        assert_eq!(&request[..2], &[150, 9]);
        let length = u16::from_ne_bytes([request[2], request[3]]);
        assert_eq!(usize::from(length) * 4, request.len());
        assert_eq!(
            u32::from_ne_bytes(request[16..20].try_into().unwrap()),
            u32::try_from(metamode.len() + 1).unwrap()
        );
        assert_eq!(&request[20..20 + metamode.len()], metamode.as_bytes());
        assert!(request[20 + metamode.len()..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn only_a_plain_metamode_is_sent() {
        for bad in ["", "   ", "DPY-0: \u{1b}[2J", "DPY-0:\n+0+0", "DPY-0\0x"] {
            assert!(
                set_string_request(150, 0, CURRENT_METAMODE, bad).is_err(),
                "{bad:?}"
            );
        }
        let long = "x".repeat(MAX_METAMODE_BYTES + 1);
        assert_eq!(
            set_string_request(150, 0, CURRENT_METAMODE, &long),
            Err(NvControlError::Invalid("too long"))
        );
    }

    #[test]
    fn the_current_metamode_loses_the_drivers_prefix() {
        let text =
            b"id=50, switchable=no, source=xconfig :: DPY-0: nvidia-auto-select @2560x1600 +0+0\0";
        let bytes = reply(1, &[u32::try_from(text.len()).unwrap()], text);
        let (parsed, rest) = StringReply::try_parse(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(
            metamode_from_reply(&parsed).unwrap(),
            "DPY-0: nvidia-auto-select @2560x1600 +0+0"
        );
        let plain = b"DPY-1: 1920x1080 +0+0\0";
        let bytes = reply(1, &[u32::try_from(plain.len()).unwrap()], plain);
        let (parsed, _) = StringReply::try_parse(&bytes).unwrap();
        assert_eq!(
            metamode_from_reply(&parsed).unwrap(),
            "DPY-1: 1920x1080 +0+0"
        );
    }

    #[test]
    fn a_reply_that_claims_more_than_arrived_is_refused() {
        let text = b"DPY-0: 1920x1080 +0+0\0";
        let mut bytes = reply(1, &[u32::try_from(text.len()).unwrap()], text);
        bytes.truncate(bytes.len() - 4);
        assert!(StringReply::try_parse(&bytes).is_err());
        // A string longer than the reply's body.
        let bytes = reply(1, &[4096], text);
        assert!(StringReply::try_parse(&bytes).is_err());
        // Not a reply at all.
        let mut bytes = reply(1, &[0], b"");
        bytes[0] = 0;
        assert!(StringReply::try_parse(&bytes).is_err());
    }

    #[test]
    fn a_missing_attribute_or_a_refused_assignment_is_an_error() {
        let bytes = reply(0, &[0], b"");
        let (parsed, _) = StringReply::try_parse(&bytes).unwrap();
        assert_eq!(
            metamode_from_reply(&parsed),
            Err(NvControlError::NoMetaMode)
        );
        let (set, _) = SetReply::try_parse(&reply(0, &[], b"")).unwrap();
        assert!(!set.accepted);
        let (set, _) = SetReply::try_parse(&reply(1, &[], b"")).unwrap();
        assert!(set.accepted);
    }
}
