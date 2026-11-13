// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The USB/IP wire protocol, as spoken between an in-tree `vhci-hcd` virtual
//! host controller and the server that plays the device.
//!
//! Written from the public protocol description (the kernel's
//! `Documentation/usb/usbip_protocol.rst`): every PDU starts with a 48-byte
//! big-endian header whose first 20 bytes (`command`, `seqnum`, `devid`,
//! `direction`, `ep`) are common. `CMD_SUBMIT` carries the OUT payload after
//! the header; `RET_SUBMIT` carries the IN payload after its header.
//!
//! The codec is pure and bounded: a header that claims more than
//! [`crate::MAX_TRANSFER_BYTES`], an isochronous transfer, an unknown command
//! or an out-of-range endpoint is refused before any payload is read.
//! [`UsbipServerState`] holds the per-connection bookkeeping a server needs to
//! answer the kernel exactly once per URB (duplicate sequence numbers are
//! refused; an unlink of an URB already answered is acknowledged with status
//! 0, and nothing is ever answered after its unlink), and [`VhciStatus`]
//! parses the controller's sysfs `status` table to choose a free port.

use crate::{MAX_IN_FLIGHT_URBS, MAX_TRANSFER_BYTES, SetupPacket, TransferDirection, TransferKind};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

/// Bytes in every USB/IP URB header.
pub const USBIP_HEADER_BYTES: usize = 48;

const CMD_SUBMIT: u32 = 0x0000_0001;
const CMD_UNLINK: u32 = 0x0000_0002;
const RET_SUBMIT: u32 = 0x0000_0003;
const RET_UNLINK: u32 = 0x0000_0004;
const DIRECTION_OUT: u32 = 0;
const DIRECTION_IN: u32 = 1;
/// Linux sends 0 for a non-isochronous transfer; the protocol description
/// says `0xffffffff`. Both mean "not isochronous".
const NON_ISO_PACKETS: [u32; 2] = [0, u32::MAX];
const MAX_ENDPOINT_NUMBER: u32 = 15;

/// Why a USB/IP header or exchange was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbipError {
    /// The command code is not `CMD_SUBMIT` or `CMD_UNLINK`.
    UnknownCommand(u32),
    /// The direction field is neither 0 (OUT) nor 1 (IN).
    InvalidDirection(u32),
    /// The endpoint number exceeds 15.
    InvalidEndpoint(u32),
    /// The transfer length exceeds [`crate::MAX_TRANSFER_BYTES`] or is negative.
    TransferTooLarge(u32),
    /// The transfer is isochronous; a pen tablet never needs one.
    Isochronous(u32),
    /// The kernel reused a sequence number that is still in flight.
    DuplicateSeqnum(u32),
    /// More URBs are in flight than [`crate::MAX_IN_FLIGHT_URBS`].
    TooManyInFlight,
    /// A reply payload exceeds the transfer length the kernel asked for.
    ReplyTooLong { requested: usize, actual: usize },
}

impl Display for UsbipError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCommand(code) => write!(formatter, "unknown USB/IP command {code:#x}"),
            Self::InvalidDirection(value) => write!(formatter, "invalid USB/IP direction {value}"),
            Self::InvalidEndpoint(value) => write!(formatter, "invalid USB/IP endpoint {value}"),
            Self::TransferTooLarge(length) => {
                write!(
                    formatter,
                    "USB/IP transfer of {length} bytes exceeds the bound"
                )
            }
            Self::Isochronous(packets) => write!(
                formatter,
                "isochronous USB/IP transfer ({packets} packets) is not bridged"
            ),
            Self::DuplicateSeqnum(seqnum) => {
                write!(
                    formatter,
                    "USB/IP sequence number {seqnum} reused while in flight"
                )
            }
            Self::TooManyInFlight => formatter.write_str("too many USB/IP URBs in flight"),
            Self::ReplyTooLong { requested, actual } => write!(
                formatter,
                "USB/IP reply of {actual} bytes exceeds the requested {requested}"
            ),
        }
    }
}

impl std::error::Error for UsbipError {}

/// One decoded `CMD_SUBMIT` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbipSubmit {
    /// The kernel's sequence number; the reply carries it back.
    pub seqnum: u32,
    /// Endpoint address with the direction bit (0x80 for IN).
    pub endpoint_address: u8,
    /// Transfer direction.
    pub direction: TransferDirection,
    /// `transfer_buffer_length`, bounded by [`crate::MAX_TRANSFER_BYTES`].
    pub length: usize,
    /// The URB interval as the kernel reported it.
    pub interval: i32,
    /// The setup packet (meaningful on endpoint 0 only).
    pub setup: SetupPacket,
}

impl UsbipSubmit {
    /// Bytes of OUT payload that follow this header on the stream.
    #[must_use]
    pub const fn out_payload_len(&self) -> usize {
        match self.direction {
            TransferDirection::Out => self.length,
            TransferDirection::In => 0,
        }
    }

    /// USB/IP carries no transfer type: endpoint 0 is the control pipe, and
    /// every other endpoint of a bridged pen tablet is an interrupt pipe (the
    /// guard and the Deck's exporter refuse anything else).
    #[must_use]
    pub const fn transfer_kind(&self) -> TransferKind {
        if matches!(self.endpoint_address, 0x00 | 0x80) {
            TransferKind::Control
        } else {
            TransferKind::Interrupt
        }
    }
}

/// One decoded command from the virtual host controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbipCommand {
    /// `CMD_SUBMIT`: a new URB.
    Submit(UsbipSubmit),
    /// `CMD_UNLINK`: cancel the URB with `target`.
    Unlink {
        /// Sequence number of this unlink request; the reply carries it.
        seqnum: u32,
        /// Sequence number of the URB to cancel.
        target: u32,
    },
}

fn be_u32(header: &[u8; USBIP_HEADER_BYTES], offset: usize) -> u32 {
    u32::from_be_bytes([
        header[offset],
        header[offset + 1],
        header[offset + 2],
        header[offset + 3],
    ])
}

fn put_be(out: &mut [u8; USBIP_HEADER_BYTES], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

/// Decodes one command header from the virtual host controller.
///
/// # Errors
///
/// [`UsbipError`] for an unknown command, an invalid direction or endpoint,
/// an oversized or isochronous transfer.
pub fn decode_command(header: &[u8; USBIP_HEADER_BYTES]) -> Result<UsbipCommand, UsbipError> {
    let command = be_u32(header, 0);
    let seqnum = be_u32(header, 4);
    match command {
        CMD_SUBMIT => {
            let direction = match be_u32(header, 12) {
                DIRECTION_OUT => TransferDirection::Out,
                DIRECTION_IN => TransferDirection::In,
                other => return Err(UsbipError::InvalidDirection(other)),
            };
            let endpoint = be_u32(header, 16);
            if endpoint > MAX_ENDPOINT_NUMBER {
                return Err(UsbipError::InvalidEndpoint(endpoint));
            }
            let raw_length = be_u32(header, 24);
            let length = usize::try_from(raw_length)
                .ok()
                .filter(|length| *length <= MAX_TRANSFER_BYTES)
                .ok_or(UsbipError::TransferTooLarge(raw_length))?;
            let packets = be_u32(header, 32);
            if !NON_ISO_PACKETS.contains(&packets) {
                return Err(UsbipError::Isochronous(packets));
            }
            #[allow(
                clippy::cast_possible_wrap,
                reason = "the field is a signed 32-bit integer"
            )]
            let interval = be_u32(header, 36) as i32;
            let setup = SetupPacket {
                request_type: header[40],
                request: header[41],
                value: u16::from_le_bytes([header[42], header[43]]),
                index: u16::from_le_bytes([header[44], header[45]]),
                length: u16::from_le_bytes([header[46], header[47]]),
            };
            #[allow(clippy::cast_possible_truncation, reason = "bounded to 15 above")]
            let number = endpoint as u8;
            let endpoint_address = match direction {
                TransferDirection::In => number | 0x80,
                TransferDirection::Out => number,
            };
            Ok(UsbipCommand::Submit(UsbipSubmit {
                seqnum,
                endpoint_address,
                direction,
                length,
                interval,
                setup,
            }))
        }
        CMD_UNLINK => Ok(UsbipCommand::Unlink {
            seqnum,
            target: be_u32(header, 20),
        }),
        other => Err(UsbipError::UnknownCommand(other)),
    }
}

/// Encodes a `RET_SUBMIT` header followed by the IN payload (empty for OUT).
///
/// `status` is 0 or a negative Linux errno.
///
/// # Errors
///
/// [`UsbipError::TransferTooLarge`] when `data` exceeds the bound.
pub fn encode_ret_submit(seqnum: u32, status: i32, data: &[u8]) -> Result<Vec<u8>, UsbipError> {
    encode_ret_submit_with_actual(seqnum, status, data.len(), data)
}

fn encode_ret_submit_with_actual(
    seqnum: u32,
    status: i32,
    actual: usize,
    data: &[u8],
) -> Result<Vec<u8>, UsbipError> {
    let actual = u32::try_from(actual)
        .ok()
        .filter(|_| actual <= MAX_TRANSFER_BYTES && data.len() <= MAX_TRANSFER_BYTES)
        .ok_or(UsbipError::TransferTooLarge(u32::MAX))?;
    let mut header = [0_u8; USBIP_HEADER_BYTES];
    put_be(&mut header, 0, RET_SUBMIT);
    put_be(&mut header, 4, seqnum);
    #[allow(clippy::cast_sign_loss, reason = "two's-complement wire encoding")]
    put_be(&mut header, 20, status as u32);
    put_be(&mut header, 24, actual);
    let mut pdu = Vec::with_capacity(USBIP_HEADER_BYTES + data.len());
    pdu.extend_from_slice(&header);
    pdu.extend_from_slice(data);
    Ok(pdu)
}

/// Encodes a `RET_UNLINK` header. `status` is `-ECONNRESET` when the URB was
/// cancelled, or 0 when it had already completed.
#[must_use]
pub fn encode_ret_unlink(seqnum: u32, status: i32) -> [u8; USBIP_HEADER_BYTES] {
    let mut header = [0_u8; USBIP_HEADER_BYTES];
    put_be(&mut header, 0, RET_UNLINK);
    put_be(&mut header, 4, seqnum);
    #[allow(clippy::cast_sign_loss, reason = "two's-complement wire encoding")]
    put_be(&mut header, 20, status as u32);
    header
}

/// What a server must do with a decoded `CMD_UNLINK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlinkAction {
    /// The URB is still in flight: cancel it, answer `RET_UNLINK` with
    /// `-ECONNRESET`, and never answer its `RET_SUBMIT`.
    Cancel,
    /// The URB was already answered (or never existed): answer `RET_UNLINK`
    /// with status 0 and do nothing else.
    AlreadyDone,
}

/// Per-connection URB bookkeeping for a USB/IP server.
///
/// Guarantees that each submitted sequence number is answered at most once,
/// that nothing is answered for an URB after its unlink (the kernel treats a
/// `RET_SUBMIT` for an unknown sequence number as a connection error), and
/// that no reply carries more bytes than the kernel's buffer holds.
#[derive(Debug, Default)]
pub struct UsbipServerState {
    in_flight: BTreeMap<u32, (TransferDirection, usize)>,
}

impl UsbipServerState {
    /// Records a new URB.
    ///
    /// # Errors
    ///
    /// A reused sequence number, or more than [`crate::MAX_IN_FLIGHT_URBS`].
    pub fn submit(&mut self, submit: &UsbipSubmit) -> Result<(), UsbipError> {
        if self.in_flight.contains_key(&submit.seqnum) {
            return Err(UsbipError::DuplicateSeqnum(submit.seqnum));
        }
        if self.in_flight.len() >= MAX_IN_FLIGHT_URBS {
            return Err(UsbipError::TooManyInFlight);
        }
        self.in_flight
            .insert(submit.seqnum, (submit.direction, submit.length));
        Ok(())
    }

    /// Encodes the one `RET_SUBMIT` for `seqnum`, or `None` when the URB is
    /// no longer in flight (it was unlinked, or already answered) and must
    /// not be answered at all.
    ///
    /// For an IN transfer `data` is the payload and its length the actual
    /// length; for an OUT transfer `out_actual` bytes were consumed and no
    /// payload follows.
    ///
    /// # Errors
    ///
    /// [`UsbipError::ReplyTooLong`] when the reply exceeds the requested
    /// length; the URB stays in flight so the caller can fail it instead.
    pub fn reply(
        &mut self,
        seqnum: u32,
        status: i32,
        out_actual: usize,
        data: &[u8],
    ) -> Result<Option<Vec<u8>>, UsbipError> {
        let Some(&(direction, requested)) = self.in_flight.get(&seqnum) else {
            return Ok(None);
        };
        let (actual, payload) = match direction {
            TransferDirection::In => (data.len(), data),
            TransferDirection::Out => (out_actual, &[][..]),
        };
        if actual > requested {
            return Err(UsbipError::ReplyTooLong { requested, actual });
        }
        let pdu = encode_ret_submit_with_actual(seqnum, status, actual, payload)?;
        self.in_flight.remove(&seqnum);
        Ok(Some(pdu))
    }

    /// Resolves an unlink against what is still in flight.
    pub fn unlink(&mut self, target: u32) -> UnlinkAction {
        if self.in_flight.remove(&target).is_some() {
            UnlinkAction::Cancel
        } else {
            UnlinkAction::AlreadyDone
        }
    }

    /// URBs still awaiting an answer.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

/// Root-hub kind of one `vhci_hcd` port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VhciHub {
    /// The USB 2 root hub (low, full and high speed).
    High,
    /// The USB 3 root hub.
    Super,
}

/// One row of `/sys/devices/platform/vhci_hcd.N/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VhciPortRow {
    /// Which root hub the port belongs to.
    pub hub: VhciHub,
    /// Controller-wide port number, as `attach`/`detach` expect it.
    pub port: u32,
    /// `usbip_device_status` of the port (4 = free).
    pub status: u32,
}

impl VhciPortRow {
    /// The kernel's `VDEV_ST_NULL`: nothing attached.
    pub const FREE: u32 = 4;

    /// Whether nothing is attached to this port.
    #[must_use]
    pub const fn is_free(&self) -> bool {
        self.status == Self::FREE
    }
}

/// Bound on the rows read from one `status` file (the kernel's maximum is
/// 2 × 15 ports per controller).
pub const MAX_VHCI_STATUS_ROWS: usize = 64;

/// The parsed `status` table of one `vhci_hcd` controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VhciStatus {
    rows: Vec<VhciPortRow>,
}

impl VhciStatus {
    /// Parses `status` (`hub port sta spd dev sockfd local_busid` rows).
    ///
    /// Unknown or malformed rows are skipped; at most
    /// [`MAX_VHCI_STATUS_ROWS`] rows are kept.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let rows = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let hub = match fields.next()? {
                    "hs" => VhciHub::High,
                    "ss" => VhciHub::Super,
                    _ => return None,
                };
                let port = fields.next()?.parse().ok()?;
                let status = fields.next()?.parse().ok()?;
                Some(VhciPortRow { hub, port, status })
            })
            .take(MAX_VHCI_STATUS_ROWS)
            .collect();
        Self { rows }
    }

    /// Every parsed row.
    #[must_use]
    pub fn rows(&self) -> &[VhciPortRow] {
        &self.rows
    }

    /// The row for `port`, if present.
    #[must_use]
    pub fn row(&self, port: u32) -> Option<VhciPortRow> {
        self.rows.iter().copied().find(|row| row.port == port)
    }

    /// Free ports on the USB 2 root hub (the one a low-, full- or high-speed
    /// device attaches to), lowest first.
    pub fn free_high_speed_ports(&self) -> impl Iterator<Item = u32> + '_ {
        self.rows
            .iter()
            .filter(|row| row.hub == VhciHub::High && row.is_free())
            .map(|row| row.port)
    }

    /// The 1-based root-hub port index of a USB 2 `port`: the device appears
    /// at `<bus>-<index>`. USB 2 ports come first in the controller's
    /// numbering, so the index is the port's rank among USB 2 rows plus one.
    #[must_use]
    pub fn high_speed_root_index(&self, port: u32) -> Option<u32> {
        let row = self.row(port)?;
        if row.hub != VhciHub::High {
            return None;
        }
        let rank = self
            .rows
            .iter()
            .filter(|candidate| candidate.hub == VhciHub::High && candidate.port < port)
            .count();
        u32::try_from(rank).ok().map(|rank| rank + 1)
    }
}

/// The sysfs device path every node of the device on `root_index` of the USB 2
/// root hub `bus` lives under (`/devices/platform/<controller>/usb<B>/<B>-<P>`).
///
/// `controller` is the platform device name (`vhci_hcd.0`). The result has no
/// trailing slash; a match must accept this path exactly or followed by `/`
/// or `:` (so `<B>-<P>` never matches `<B>-<P>0`).
#[must_use]
pub fn port_devpath_prefix(controller: &str, bus: u32, root_index: u32) -> String {
    format!("/devices/platform/{controller}/usb{bus}/{bus}-{root_index}")
}

/// Whether `devpath` is the device at `prefix` or one of its descendants.
#[must_use]
pub fn devpath_under(devpath: &str, prefix: &str) -> bool {
    devpath
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with(':'))
}

#[cfg(test)]
#[path = "usbip_tests.rs"]
mod tests;
