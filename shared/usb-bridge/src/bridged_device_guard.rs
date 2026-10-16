//! Runtime proof that a bridged device keeps telling the truth about itself.
//!
//! A Pier authorizes a Hard USB device from claims
//! ([`crate::authorize_hard_usb_tablet`]): an identity, a device class and the
//! report descriptor of every HID interface. Once the virtual device exists,
//! the host kernel enumerates it by asking the device itself, through the
//! bridge, for its device, configuration and report descriptors — and those
//! answers come from the same peer that made the claims. A dishonest peer
//! could claim a pen tablet and then enumerate as mass storage.
//!
//! [`BridgedDeviceGuard`] sits on the URB path. It remembers every standard
//! `GET_DESCRIPTOR` request the host sends and checks each completion before
//! it is handed to the host kernel:
//!
//! * **Device** (and **device qualifier**): length and type exact, device class
//!   `0x00` or `0x03`, the authorized vendor/product/revision, exactly one
//!   configuration.
//! * **Configuration** (and **other-speed configuration**): bounded and well
//!   formed (`crate::parse_configuration_descriptor`, interrupt endpoints
//!   only), every interface and alternate setting HID class, and exactly the
//!   interface numbers whose report descriptors were authorized. A
//!   boot-keyboard interface is allowed (tablet express keys): its report
//!   descriptor was authorized with the rest. Partial reads are accepted only for the nine-byte
//!   header the host uses to learn the total length.
//! * **Report**: exactly the classified bytes, or exactly the prefix the host
//!   asked for when it requested fewer bytes than the descriptor holds. Each
//!   interface's HID class descriptor in the configuration must advertise the
//!   classified length, so the host never reads a shorter prefix that hides
//!   the pen.
//!
//! Any mismatch is final: the host must tear the attachment down rather than
//! pass the completion on.

use crate::{
    DESCRIPTOR_HID, DescriptorError, HidClassDescriptorError, InterfaceReportDescriptor,
    MAX_CONFIGURATION_DESCRIPTOR_BYTES, MAX_IN_FLIGHT_URBS, SetupPacket, UrbId, UsbDeviceId,
    parse_configuration_descriptor, parse_hid_class_descriptor,
    requested_report_descriptor_interface,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};

const REQUEST_TYPE_STANDARD_DEVICE_IN: u8 = 0x80;
const REQUEST_GET_DESCRIPTOR: u8 = 0x06;
const DESCRIPTOR_DEVICE: u8 = 0x01;
const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const DESCRIPTOR_INTERFACE: u8 = 0x04;
const DESCRIPTOR_DEVICE_QUALIFIER: u8 = 0x06;
const DESCRIPTOR_OTHER_SPEED_CONFIGURATION: u8 = 0x07;
const DEVICE_DESCRIPTOR_LENGTH: usize = 18;
const DEVICE_QUALIFIER_LENGTH: usize = 10;
const CONFIGURATION_HEADER_LENGTH: usize = 9;
const USB_CLASS_PER_INTERFACE: u8 = 0x00;
const USB_CLASS_HID: u8 = 0x03;

/// A device or device-qualifier descriptor that contradicts the authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceDescriptorViolation {
    /// Wrong length or type, or longer than the descriptor can be.
    Malformed,
    /// The device class is neither per-interface nor HID.
    Class(u8),
    /// Vendor, product or revision differ from the authorized identity.
    Identity,
    /// More (or fewer) than one configuration.
    ConfigurationCount(u8),
}

/// A configuration descriptor that contradicts the authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigurationViolation {
    /// Wrong header, a partial read beyond the header, or trailing bytes.
    Malformed,
    /// The structure fails the bounded parser (lengths, limits, non-interrupt
    /// endpoints).
    Unparseable(DescriptorError),
    /// `wTotalLength` exceeds the bridge bound.
    TooLarge(usize),
    /// `bNumInterfaces` differs from the authorized interface count.
    InterfaceCount {
        /// Count the descriptor declares.
        declared: u8,
        /// Count that was authorized.
        authorized: usize,
    },
    /// An interface (or alternate setting) is not HID class.
    NotHid {
        /// Interface number.
        interface: u8,
        /// Its class.
        class: u8,
    },
    /// The interface numbers differ from those whose report descriptors were
    /// authorized.
    InterfaceSet,
    /// An interface's HID class descriptor is malformed or ambiguous: a
    /// subordinate list inconsistent with `bLength`, or more (or fewer) than
    /// one Report entry.
    HidClassDescriptor {
        /// Interface number.
        interface: u8,
        /// What is wrong with it.
        error: HidClassDescriptorError,
    },
    /// An interface (or alternate setting) has no HID class descriptor, more
    /// than one, or one whose report-descriptor length (`wDescriptorLength`)
    /// differs from the authorized descriptor's length. Advertising a shorter
    /// length would make the host read only a prefix, hiding whatever made
    /// the device classify as a pen tablet.
    ReportLength {
        /// Interface number.
        interface: u8,
        /// Length the configuration declares (`None`: missing, unusable or
        /// duplicated HID descriptor).
        declared: Option<u16>,
        /// Length of the authorized report descriptor.
        authorized: usize,
    },
}

/// A completion the guard refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgedDeviceViolation {
    /// More outstanding descriptor requests than URBs may be in flight.
    TooManyPending,
    /// A report descriptor for an interface no claim covered.
    ReportUnclaimed {
        /// Interface number the host asked about.
        interface: u8,
    },
    /// A report descriptor whose bytes differ from the classified one.
    ReportMismatch {
        /// Interface number the host asked about.
        interface: u8,
    },
    /// A report descriptor shorter than the host asked for and than the
    /// classified one: a prefix served where the whole was requested.
    ReportTruncated {
        /// Interface number the host asked about.
        interface: u8,
        /// Bytes returned.
        returned: usize,
        /// Bytes an honest device returns: the smaller of the requested
        /// length and the classified descriptor's length.
        expected: usize,
    },
    /// The device (or device-qualifier) descriptor contradicts the claims.
    Device(DeviceDescriptorViolation),
    /// A configuration descriptor contradicts the claims.
    Configuration(ConfigurationViolation),
}

impl Display for BridgedDeviceViolation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyPending => formatter.write_str("too many outstanding descriptor requests"),
            Self::ReportUnclaimed { interface } => write!(
                formatter,
                "device returned a report descriptor for unclassified interface {interface}"
            ),
            Self::ReportMismatch { interface } => write!(
                formatter,
                "device report descriptor for interface {interface} differs from the classified one"
            ),
            Self::ReportTruncated {
                interface,
                returned,
                expected,
            } => write!(
                formatter,
                "device returned {returned} report-descriptor bytes for interface {interface}; \
                 {expected} were due"
            ),
            Self::Device(violation) => {
                write!(
                    formatter,
                    "device descriptor contradicts the authorization: {violation:?}"
                )
            }
            Self::Configuration(violation) => write!(
                formatter,
                "configuration descriptor contradicts the authorization: {violation:?}"
            ),
        }
    }
}

impl std::error::Error for BridgedDeviceViolation {}

/// Validates a device descriptor (or a prefix of one) against the
/// authorized identity.
///
/// # Errors
///
/// Returns [`DeviceDescriptorViolation`] on any contradiction.
pub fn validate_device_descriptor(
    payload: &[u8],
    identity: UsbDeviceId,
) -> Result<(), DeviceDescriptorViolation> {
    if payload.len() > DEVICE_DESCRIPTOR_LENGTH {
        return Err(DeviceDescriptorViolation::Malformed);
    }
    check_byte(payload, 0, |length| {
        usize::from(length) == DEVICE_DESCRIPTOR_LENGTH
    })?;
    check_byte(payload, 1, |kind| kind == DESCRIPTOR_DEVICE)?;
    if let Some(&class) = payload.get(4) {
        if class != USB_CLASS_PER_INTERFACE && class != USB_CLASS_HID {
            return Err(DeviceDescriptorViolation::Class(class));
        }
    }
    if payload.len() == DEVICE_DESCRIPTOR_LENGTH {
        let field = |offset: usize| u16::from_le_bytes([payload[offset], payload[offset + 1]]);
        if field(8) != identity.vendor_id
            || field(10) != identity.product_id
            || field(12) != identity.bcd_device
        {
            return Err(DeviceDescriptorViolation::Identity);
        }
        if payload[17] != 1 {
            return Err(DeviceDescriptorViolation::ConfigurationCount(payload[17]));
        }
    }
    Ok(())
}

/// Validates a device-qualifier descriptor (or a prefix of one).
///
/// # Errors
///
/// Returns [`DeviceDescriptorViolation`] on any contradiction.
pub fn validate_device_qualifier(payload: &[u8]) -> Result<(), DeviceDescriptorViolation> {
    if payload.len() > DEVICE_QUALIFIER_LENGTH {
        return Err(DeviceDescriptorViolation::Malformed);
    }
    check_byte(payload, 0, |length| {
        usize::from(length) == DEVICE_QUALIFIER_LENGTH
    })?;
    check_byte(payload, 1, |kind| kind == DESCRIPTOR_DEVICE_QUALIFIER)?;
    if let Some(&class) = payload.get(4) {
        if class != USB_CLASS_PER_INTERFACE && class != USB_CLASS_HID {
            return Err(DeviceDescriptorViolation::Class(class));
        }
    }
    if let Some(&count) = payload.get(8) {
        if count > 1 {
            return Err(DeviceDescriptorViolation::ConfigurationCount(count));
        }
    }
    Ok(())
}

fn check_byte(
    payload: &[u8],
    offset: usize,
    accept: impl Fn(u8) -> bool,
) -> Result<(), DeviceDescriptorViolation> {
    match payload.get(offset) {
        Some(&byte) if !accept(byte) => Err(DeviceDescriptorViolation::Malformed),
        _ => Ok(()),
    }
}

/// Validates a configuration (or other-speed configuration) descriptor
/// against the authorized HID interfaces, given as interface number →
/// authorized report-descriptor length.
///
/// A read of at most the nine-byte header is checked field by field; any
/// longer read must be exactly `wTotalLength` bytes and is then fully parsed:
/// every interface and alternate setting must be HID, the interface numbers
/// must be exactly the authorized ones, and each interface's HID class
/// descriptor must advertise exactly the authorized report-descriptor length.
///
/// # Errors
///
/// Returns [`ConfigurationViolation`] on any contradiction.
pub fn validate_configuration_descriptor(
    payload: &[u8],
    descriptor_type: u8,
    authorized: &BTreeMap<u8, usize>,
) -> Result<(), ConfigurationViolation> {
    if payload
        .first()
        .is_some_and(|&length| usize::from(length) != CONFIGURATION_HEADER_LENGTH)
        || payload.get(1).is_some_and(|&kind| kind != descriptor_type)
    {
        return Err(ConfigurationViolation::Malformed);
    }
    if payload.len() >= 4 {
        let total = usize::from(u16::from_le_bytes([payload[2], payload[3]]));
        if total > MAX_CONFIGURATION_DESCRIPTOR_BYTES {
            return Err(ConfigurationViolation::TooLarge(total));
        }
        if total < CONFIGURATION_HEADER_LENGTH {
            return Err(ConfigurationViolation::Malformed);
        }
        if payload.len() > CONFIGURATION_HEADER_LENGTH && payload.len() != total {
            return Err(ConfigurationViolation::Malformed);
        }
    }
    if let Some(&declared) = payload.get(4) {
        if usize::from(declared) != authorized.len() {
            return Err(ConfigurationViolation::InterfaceCount {
                declared,
                authorized: authorized.len(),
            });
        }
    }
    if payload.len() <= CONFIGURATION_HEADER_LENGTH {
        return Ok(());
    }

    // The bounded parser reads a standard configuration header.
    let mut normalized = payload.to_vec();
    normalized[1] = DESCRIPTOR_CONFIGURATION;
    let parsed =
        parse_configuration_descriptor(&normalized).map_err(ConfigurationViolation::Unparseable)?;
    let mut numbers = BTreeSet::new();
    for interface in &parsed.interfaces {
        if interface.class != USB_CLASS_HID {
            return Err(ConfigurationViolation::NotHid {
                interface: interface.number.0,
                class: interface.class,
            });
        }
        numbers.insert(interface.number.0);
    }
    if !numbers.iter().eq(authorized.keys()) {
        return Err(ConfigurationViolation::InterfaceSet);
    }
    check_report_lengths(&normalized, authorized)
}

/// Requires every interface descriptor (each alternate setting) to be
/// followed by exactly one HID class descriptor whose report-descriptor
/// length equals the authorized one. `bytes` has already passed the bounded
/// parser, so every descriptor length is valid.
fn check_report_lengths(
    bytes: &[u8],
    authorized: &BTreeMap<u8, usize>,
) -> Result<(), ConfigurationViolation> {
    // (interface, declared length, HID descriptors seen) per interface
    // descriptor, in order.
    let mut entries: Vec<(u8, Option<u16>, usize)> = Vec::new();
    let mut offset = 0_usize;
    while offset + 1 < bytes.len() {
        let length = usize::from(bytes[offset]);
        if length < 2 || offset + length > bytes.len() {
            return Err(ConfigurationViolation::Malformed);
        }
        let descriptor = &bytes[offset..offset + length];
        match descriptor[1] {
            DESCRIPTOR_INTERFACE if length >= 3 => entries.push((descriptor[2], None, 0)),
            DESCRIPTOR_HID => {
                let Some(entry) = entries.last_mut() else {
                    return Err(ConfigurationViolation::Malformed);
                };
                // The whole subordinate list must be well formed with exactly one
                // Report entry: hosts differ on which of several entries wins.
                match parse_hid_class_descriptor(descriptor) {
                    Ok(declared) => entry.1 = Some(declared),
                    Err(error) => {
                        return Err(ConfigurationViolation::HidClassDescriptor {
                            interface: entry.0,
                            error,
                        });
                    }
                }
                entry.2 += 1;
            }
            _ => {}
        }
        offset += length;
    }
    for (interface, declared, seen) in entries {
        let expected = authorized.get(&interface).copied().unwrap_or(usize::MAX);
        if seen != 1 || declared.map(usize::from) != Some(expected) {
            return Err(ConfigurationViolation::ReportLength {
                interface,
                declared: if seen == 1 { declared } else { None },
                authorized: expected,
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Requested {
    Device,
    DeviceQualifier,
    Configuration(u8),
    Report { interface: u8, length: u16 },
}

fn requested(setup: SetupPacket) -> Option<Requested> {
    if let Some(interface) = requested_report_descriptor_interface(setup) {
        return Some(Requested::Report {
            interface,
            length: setup.length,
        });
    }
    if setup.request_type != REQUEST_TYPE_STANDARD_DEVICE_IN
        || setup.request != REQUEST_GET_DESCRIPTOR
    {
        return None;
    }
    match setup.descriptor_type() {
        DESCRIPTOR_DEVICE => Some(Requested::Device),
        DESCRIPTOR_DEVICE_QUALIFIER => Some(Requested::DeviceQualifier),
        kind @ (DESCRIPTOR_CONFIGURATION | DESCRIPTOR_OTHER_SPEED_CONFIGURATION) => {
            Some(Requested::Configuration(kind))
        }
        _ => None,
    }
}

/// Checks every descriptor a bridged device returns against what was
/// authorized for it.
#[derive(Debug, Clone)]
pub struct BridgedDeviceGuard {
    identity: UsbDeviceId,
    claims: Vec<InterfaceReportDescriptor>,
    report_lengths: BTreeMap<u8, usize>,
    pending: BTreeMap<UrbId, Requested>,
}

impl BridgedDeviceGuard {
    /// Guards one attachment with the identity and report descriptors that
    /// were authorized.
    #[must_use]
    pub fn new(identity: UsbDeviceId, claims: Vec<InterfaceReportDescriptor>) -> Self {
        let report_lengths = claims
            .iter()
            .map(|claim| (claim.interface, claim.descriptor.len()))
            .collect();
        Self {
            identity,
            claims,
            report_lengths,
            pending: BTreeMap::new(),
        }
    }

    /// Records a submitted URB. Only descriptor requests are remembered.
    ///
    /// # Errors
    ///
    /// Returns [`BridgedDeviceViolation::TooManyPending`] when the bounded
    /// table is full.
    pub fn observe_submit(
        &mut self,
        urb_id: UrbId,
        setup: Option<SetupPacket>,
    ) -> Result<(), BridgedDeviceViolation> {
        let Some(requested) = setup.and_then(requested) else {
            return Ok(());
        };
        if self.pending.len() >= MAX_IN_FLIGHT_URBS && !self.pending.contains_key(&urb_id) {
            return Err(BridgedDeviceViolation::TooManyPending);
        }
        self.pending.insert(urb_id, requested);
        Ok(())
    }

    /// Forgets a cancelled URB.
    pub fn observe_cancel(&mut self, urb_id: UrbId) {
        self.pending.remove(&urb_id);
    }

    /// Checks one completion before it is handed to the host kernel. A failed
    /// completion carries no data and passes.
    ///
    /// # Errors
    ///
    /// Returns [`BridgedDeviceViolation`] when the returned descriptor
    /// contradicts the authorization.
    pub fn check_completion(
        &mut self,
        urb_id: UrbId,
        success: bool,
        payload: &[u8],
    ) -> Result<(), BridgedDeviceViolation> {
        let Some(requested) = self.pending.remove(&urb_id) else {
            return Ok(());
        };
        if !success {
            return Ok(());
        }
        match requested {
            Requested::Device => validate_device_descriptor(payload, self.identity)
                .map_err(BridgedDeviceViolation::Device),
            Requested::DeviceQualifier => {
                validate_device_qualifier(payload).map_err(BridgedDeviceViolation::Device)
            }
            Requested::Configuration(kind) => {
                validate_configuration_descriptor(payload, kind, &self.report_lengths)
                    .map_err(BridgedDeviceViolation::Configuration)
            }
            Requested::Report { interface, length } => {
                let Some(claim) = self
                    .claims
                    .iter()
                    .find(|claim| claim.interface == interface)
                else {
                    return Err(BridgedDeviceViolation::ReportUnclaimed { interface });
                };
                if !claim.descriptor.starts_with(payload) {
                    return Err(BridgedDeviceViolation::ReportMismatch { interface });
                }
                // A prefix is honest only when the host asked for fewer bytes
                // than the descriptor holds; otherwise the whole is due.
                let expected = claim.descriptor.len().min(usize::from(length));
                if payload.len() == expected {
                    Ok(())
                } else {
                    Err(BridgedDeviceViolation::ReportTruncated {
                        interface,
                        returned: payload.len(),
                        expected,
                    })
                }
            }
        }
    }

    /// Number of remembered outstanding descriptor requests.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
#[path = "bridged_device_guard_tests.rs"]
mod tests;
