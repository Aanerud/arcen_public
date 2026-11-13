// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Which registry entry to capture, and what it must look like once captured.
//!
//! Everything here is a pure function of values read from the I/O Registry or
//! from the captured device, so it is tested on every operating system.

use arcen_usb_bridge::{
    CapturedDeviceFacts, DeviceSnapshot, EndpointAddress, InterfaceReportDescriptor,
    MAX_CONFIGURATION_DESCRIPTOR_BYTES, ParsedConfiguration, UsbDeviceId, UsbSpeed,
    parse_configuration_descriptor,
};
use std::fmt::{Display, Formatter};

/// Bytes in a standard USB device descriptor.
pub const DEVICE_DESCRIPTOR_LENGTH: usize = 18;
const DESCRIPTOR_DEVICE: u8 = 0x01;
const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const CONFIGURATION_HEADER_LENGTH: usize = 9;

/// One `IOUSBHostDevice` as the I/O Registry describes it.
///
/// Reading these properties opens nothing and sends nothing to the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryUsbDevice {
    /// The registry entry ID: unique for this attachment until the device
    /// re-enumerates or the machine restarts.
    pub registry_id: u64,
    /// `locationID`: the physical port path, stable across re-enumeration.
    pub location_id: Option<u32>,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: Option<u16>,
    pub device_class: Option<u8>,
    pub speed: Option<UsbSpeed>,
    /// `UsbExclusiveOwner`: the process or service holding an exclusive
    /// session to the device, if any.
    pub exclusive_owner: Option<String>,
}

impl RegistryUsbDevice {
    /// Whether some process holds an exclusive session (a capture) on it.
    #[must_use]
    pub const fn is_captured(&self) -> bool {
        self.exclusive_owner.is_some()
    }
}

/// The device a capture is aimed at, as classification found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureTarget {
    pub registry_id: u64,
    /// When known, a re-enumerated device (new registry ID) on the same port
    /// with the same identity still matches.
    pub location_id: Option<u32>,
    pub vendor_id: u16,
    pub product_id: u16,
}

impl CaptureTarget {
    /// The target for a device classification picked from the registry.
    #[must_use]
    pub const fn of(device: &RegistryUsbDevice) -> Self {
        Self {
            registry_id: device.registry_id,
            location_id: device.location_id,
            vendor_id: device.vendor_id,
            product_id: device.product_id,
        }
    }
}

/// How the registry entry was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedBy {
    RegistryId,
    /// The registry ID is gone but the same identity sits on the same port:
    /// the device re-enumerated.
    Location,
}

/// Why no registry entry could be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    /// Nothing with that registry ID, and nothing with that identity at that
    /// location.
    NotFound,
    /// The registry ID now names a different device.
    IdentityChanged {
        registry_id: u64,
        vendor_id: u16,
        product_id: u16,
    },
    /// The registry ID names the same identity on a different port.
    LocationChanged {
        registry_id: u64,
        expected: u32,
        found: Option<u32>,
    },
    /// Several devices fit and none may be preferred.
    Ambiguous,
}

impl Display for LookupError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("the USB device is no longer attached"),
            Self::IdentityChanged {
                registry_id,
                vendor_id,
                product_id,
            } => write!(
                formatter,
                "registry entry {registry_id:#x} is now {vendor_id:04x}:{product_id:04x}, \
                 not the device that was classified"
            ),
            Self::LocationChanged {
                registry_id,
                expected,
                found,
            } => match found {
                Some(found) => write!(
                    formatter,
                    "registry entry {registry_id:#x} moved from location {expected:#010x} \
                     to {found:#010x}"
                ),
                None => write!(
                    formatter,
                    "registry entry {registry_id:#x} has no location; expected {expected:#010x}"
                ),
            },
            Self::Ambiguous => {
                formatter.write_str("several attached USB devices match; refusing to guess")
            }
        }
    }
}

impl std::error::Error for LookupError {}

/// Chooses the one registry entry a capture may open.
///
/// The registry ID decides when it still exists, and then the identity and
/// (when known) the location must agree with what was classified. When the
/// registry ID is gone, the same vendor and product at the same location is
/// the same device after a re-enumeration. Anything else refuses, so one
/// tablet's classification can never authorize the capture of another device.
///
/// # Errors
///
/// Returns a [`LookupError`] when no single entry fits.
pub fn select_target<'a>(
    devices: &'a [RegistryUsbDevice],
    target: &CaptureTarget,
) -> Result<(&'a RegistryUsbDevice, MatchedBy), LookupError> {
    let same_identity = |device: &RegistryUsbDevice| {
        device.vendor_id == target.vendor_id && device.product_id == target.product_id
    };
    let mut by_id = devices
        .iter()
        .filter(|device| device.registry_id == target.registry_id);
    if let Some(device) = by_id.next() {
        if by_id.next().is_some() {
            return Err(LookupError::Ambiguous);
        }
        if !same_identity(device) {
            return Err(LookupError::IdentityChanged {
                registry_id: device.registry_id,
                vendor_id: device.vendor_id,
                product_id: device.product_id,
            });
        }
        if let Some(expected) = target.location_id {
            if device.location_id != Some(expected) {
                return Err(LookupError::LocationChanged {
                    registry_id: device.registry_id,
                    expected,
                    found: device.location_id,
                });
            }
        }
        return Ok((device, MatchedBy::RegistryId));
    }
    let Some(location) = target.location_id else {
        return Err(LookupError::NotFound);
    };
    let mut at_location = devices
        .iter()
        .filter(|device| device.location_id == Some(location) && same_identity(device));
    match (at_location.next(), at_location.next()) {
        (Some(device), None) => Ok((device, MatchedBy::Location)),
        (Some(_), Some(_)) => Err(LookupError::Ambiguous),
        (None, _) => Err(LookupError::NotFound),
    }
}

/// Maps the registry's `USBSpeed` value (`tIOUSBHostConnectionSpeed`) to the
/// speeds input bridge v1 carries. `SuperSpeed` and unknown values are `None`.
#[must_use]
pub const fn speed_from_registry(raw: i64) -> Option<UsbSpeed> {
    match raw {
        1 => Some(UsbSpeed::Full),
        2 => Some(UsbSpeed::Low),
        3 => Some(UsbSpeed::High),
        _ => None,
    }
}

/// The total length a configuration descriptor header announces, checked
/// against the bridge bound before anything copies that many bytes.
///
/// # Errors
///
/// Returns a reason when the header is short, of the wrong type, or announces
/// an impossible or oversized length.
pub fn configuration_total_length(header: &[u8]) -> Result<usize, String> {
    if header.len() < 4 || header[1] != DESCRIPTOR_CONFIGURATION {
        return Err("the device returned no configuration descriptor header".to_owned());
    }
    let total = usize::from(u16::from_le_bytes([header[2], header[3]]));
    if total < CONFIGURATION_HEADER_LENGTH {
        return Err(format!(
            "configuration descriptor length {total} is too short"
        ));
    }
    if total > MAX_CONFIGURATION_DESCRIPTOR_BYTES {
        return Err(format!(
            "configuration descriptor length {total} exceeds the bridge limit \
             {MAX_CONFIGURATION_DESCRIPTOR_BYTES}"
        ));
    }
    Ok(total)
}

/// Builds the facts the shared engine verifies, from descriptors read from the
/// captured device itself.
///
/// The device descriptor must still name the classified identity: a device
/// that changed under the capture is refused here, before any transfer.
/// `report_descriptors` are the HID report descriptors classification read
/// from the registry; the shared engine re-runs the whole admission policy on
/// them together with these descriptors.
///
/// # Errors
///
/// Returns a reason when a descriptor is malformed, the identity changed, or
/// the speed is outside input bridge v1.
pub fn captured_facts(
    device_descriptor: &[u8],
    configuration_descriptor: &[u8],
    speed: Option<UsbSpeed>,
    target: &CaptureTarget,
    report_descriptors: Vec<InterfaceReportDescriptor>,
) -> Result<CapturedDeviceFacts, String> {
    if device_descriptor.len() < DEVICE_DESCRIPTOR_LENGTH
        || usize::from(device_descriptor[0]) < DEVICE_DESCRIPTOR_LENGTH
        || device_descriptor[1] != DESCRIPTOR_DEVICE
    {
        return Err("the captured device returned an invalid device descriptor".to_owned());
    }
    let id = UsbDeviceId {
        vendor_id: u16::from_le_bytes([device_descriptor[8], device_descriptor[9]]),
        product_id: u16::from_le_bytes([device_descriptor[10], device_descriptor[11]]),
        bcd_device: u16::from_le_bytes([device_descriptor[12], device_descriptor[13]]),
    };
    if id.vendor_id != target.vendor_id || id.product_id != target.product_id {
        return Err(format!(
            "the captured device is {:04x}:{:04x}, not the classified {:04x}:{:04x}",
            id.vendor_id, id.product_id, target.vendor_id, target.product_id
        ));
    }
    let configuration = parse_configuration_descriptor(configuration_descriptor)
        .map_err(|error| format!("captured configuration descriptor: {error}"))?;
    let speed = speed.ok_or_else(|| "USB speed is outside input bridge v1".to_owned())?;
    Ok(CapturedDeviceFacts {
        snapshot: DeviceSnapshot {
            id,
            device_class: device_descriptor[4],
            configuration,
        },
        speed,
        report_descriptors,
    })
}

/// One `IOHIDDevice` and the report descriptor macOS published for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHid {
    pub vendor_id: u16,
    pub product_id: u16,
    pub location_id: Option<u32>,
    /// `bInterfaceNumber` of the `IOUSBHostInterface` it sits on.
    pub interface: Option<u8>,
    pub report_descriptor: Vec<u8>,
}

/// The report descriptors the registry publishes for one USB device, by
/// interface, read before anything is opened.
///
/// When the device has a `locationID`, only entries at that location count, so
/// a twin tablet's descriptors never describe this one. Entries with no
/// interface number are ignored; two entries for one interface must agree.
///
/// # Errors
///
/// Returns a reason when two entries for one interface disagree.
pub fn hid_descriptors_for(
    entries: &[RegistryHid],
    device: &RegistryUsbDevice,
) -> Result<Vec<InterfaceReportDescriptor>, String> {
    let mut found: Vec<InterfaceReportDescriptor> = Vec::new();
    for entry in entries {
        if entry.vendor_id != device.vendor_id || entry.product_id != device.product_id {
            continue;
        }
        if device.location_id.is_some() && entry.location_id != device.location_id {
            continue;
        }
        let Some(interface) = entry.interface else {
            continue;
        };
        match found.iter().find(|known| known.interface == interface) {
            Some(known) if known.descriptor != entry.report_descriptor => {
                return Err(format!(
                    "two registry entries disagree about interface {interface}'s report descriptor"
                ));
            }
            Some(_) => {}
            None => found.push(InterfaceReportDescriptor {
                interface,
                descriptor: entry.report_descriptor.clone(),
            }),
        }
    }
    found.sort_by_key(|entry| entry.interface);
    Ok(found)
}

/// Which `IOUSBHost` object carries a control request on the default pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ControlRoute {
    /// The device object: device- and other-recipient requests.
    Device,
    /// The interface object that owns the request's interface or endpoint.
    Interface(u8),
}

/// Routes a control request to the object that owns its recipient.
///
/// `IOUSBHost` gives each interface its own object; a request addressed to an
/// interface (recipient 1, interface in `wIndex`) or to an endpoint
/// (recipient 2, endpoint address in `wIndex`) goes through the interface
/// object that owns it. A recipient the configuration does not have goes to
/// the device, which answers it as the device would.
#[must_use]
pub fn control_route(
    configuration: &ParsedConfiguration,
    request_type: u8,
    index: u16,
) -> ControlRoute {
    let [low, _] = index.to_le_bytes();
    let owner = match request_type & 0x1f {
        1 => configuration
            .interfaces
            .iter()
            .find(|interface| interface.number.0 == low),
        2 => configuration.interfaces.iter().find(|interface| {
            interface
                .endpoints
                .iter()
                .any(|endpoint| endpoint.address.0 == low)
        }),
        _ => None,
    };
    owner.map_or(ControlRoute::Device, |interface| {
        ControlRoute::Interface(interface.number.0)
    })
}

/// Interface numbers of `configuration` with no `IOUSBHostInterface` service
/// published yet, in ascending order.
#[must_use]
pub fn missing_interfaces(configuration: &ParsedConfiguration, published: &[u8]) -> Vec<u8> {
    let mut missing: Vec<u8> = configuration
        .interfaces
        .iter()
        .map(|interface| interface.number.0)
        .filter(|number| !published.contains(number))
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
}

/// The endpoints whose pipes an interface has in one alternate setting.
#[must_use]
pub fn pipes_for(
    configuration: &ParsedConfiguration,
    interface: u8,
    alternate: u8,
) -> Vec<EndpointAddress> {
    configuration
        .interfaces
        .iter()
        .filter(|candidate| {
            candidate.number.0 == interface && candidate.alternate_setting.0 == alternate
        })
        .flat_map(|candidate| candidate.endpoints.iter().map(|endpoint| endpoint.address))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_usb_bridge::{
        ARCEN_LAB_PRODUCT_ID, ARCEN_LAB_VENDOR_ID, SyntheticTabletDevice, verify_captured_tablet,
    };

    fn device(registry_id: u64, location: Option<u32>, vid: u16, pid: u16) -> RegistryUsbDevice {
        RegistryUsbDevice {
            registry_id,
            location_id: location,
            vendor_id: vid,
            product_id: pid,
            bcd_device: Some(0x0100),
            device_class: Some(0),
            speed: Some(UsbSpeed::Full),
            exclusive_owner: None,
        }
    }

    fn target(registry_id: u64, location: Option<u32>) -> CaptureTarget {
        CaptureTarget {
            registry_id,
            location_id: location,
            vendor_id: 0x056a,
            product_id: 0x0027,
        }
    }

    #[test]
    fn the_registry_id_decides_when_it_still_exists() {
        let devices = [
            device(0x10, Some(0x0110_0000), 0x056a, 0x0027),
            device(0x11, Some(0x0120_0000), 0x056a, 0x0027),
        ];
        let (found, how) = select_target(&devices, &target(0x11, Some(0x0120_0000))).unwrap();
        assert_eq!(found.registry_id, 0x11);
        assert_eq!(how, MatchedBy::RegistryId);
    }

    #[test]
    fn a_registry_id_reused_by_another_device_is_refused() {
        let devices = [device(0x10, Some(0x0110_0000), 0x046d, 0xc077)];
        assert_eq!(
            select_target(&devices, &target(0x10, Some(0x0110_0000))),
            Err(LookupError::IdentityChanged {
                registry_id: 0x10,
                vendor_id: 0x046d,
                product_id: 0xc077,
            })
        );
    }

    #[test]
    fn the_same_registry_id_on_another_port_is_refused() {
        let devices = [device(0x10, Some(0x0130_0000), 0x056a, 0x0027)];
        assert!(matches!(
            select_target(&devices, &target(0x10, Some(0x0110_0000))),
            Err(LookupError::LocationChanged {
                expected: 0x0110_0000,
                found: Some(0x0130_0000),
                ..
            })
        ));
    }

    #[test]
    fn a_re_enumerated_device_is_found_by_location_and_identity() {
        let devices = [
            device(0x20, Some(0x0110_0000), 0x056a, 0x0027),
            device(0x21, Some(0x0120_0000), 0x056a, 0x0027),
        ];
        let (found, how) = select_target(&devices, &target(0x10, Some(0x0110_0000))).unwrap();
        assert_eq!(found.registry_id, 0x20);
        assert_eq!(how, MatchedBy::Location);
    }

    #[test]
    fn a_different_device_at_the_location_is_not_the_target() {
        let devices = [device(0x20, Some(0x0110_0000), 0x046d, 0xc077)];
        assert_eq!(
            select_target(&devices, &target(0x10, Some(0x0110_0000))),
            Err(LookupError::NotFound)
        );
        assert_eq!(
            select_target(&devices, &target(0x10, None)),
            Err(LookupError::NotFound)
        );
        assert_eq!(
            select_target(&[], &target(0x10, None)),
            Err(LookupError::NotFound)
        );
    }

    #[test]
    fn two_twins_at_one_location_or_one_id_are_ambiguous() {
        let twins = [
            device(0x20, Some(0x0110_0000), 0x056a, 0x0027),
            device(0x21, Some(0x0110_0000), 0x056a, 0x0027),
        ];
        assert_eq!(
            select_target(&twins, &target(0x10, Some(0x0110_0000))),
            Err(LookupError::Ambiguous)
        );
        let duplicated = [
            device(0x10, Some(0x0110_0000), 0x056a, 0x0027),
            device(0x10, Some(0x0110_0000), 0x056a, 0x0027),
        ];
        assert_eq!(
            select_target(&duplicated, &target(0x10, Some(0x0110_0000))),
            Err(LookupError::Ambiguous)
        );
    }

    #[test]
    fn registry_speeds_map_to_bridge_speeds() {
        assert_eq!(speed_from_registry(1), Some(UsbSpeed::Full));
        assert_eq!(speed_from_registry(2), Some(UsbSpeed::Low));
        assert_eq!(speed_from_registry(3), Some(UsbSpeed::High));
        for outside in [0, 4, 5, 6, 7, -1] {
            assert_eq!(speed_from_registry(outside), None);
        }
    }

    #[test]
    fn configuration_length_is_bounded_before_any_copy() {
        let config = SyntheticTabletDevice::configuration_descriptor();
        assert_eq!(configuration_total_length(&config), Ok(config.len()));
        assert!(configuration_total_length(&[9, 2, 8, 0]).is_err());
        assert!(configuration_total_length(&[9, 2, 0x01, 0x10]).is_err());
        assert!(configuration_total_length(&[18, 1, 0, 2]).is_err());
        assert!(configuration_total_length(&[9, 2]).is_err());
    }

    fn lab_target() -> CaptureTarget {
        CaptureTarget {
            registry_id: 1,
            location_id: None,
            vendor_id: ARCEN_LAB_VENDOR_ID,
            product_id: ARCEN_LAB_PRODUCT_ID,
        }
    }

    fn lab_reports() -> Vec<InterfaceReportDescriptor> {
        vec![InterfaceReportDescriptor {
            interface: 0,
            descriptor: SyntheticTabletDevice::report_descriptor().to_vec(),
        }]
    }

    #[test]
    fn captured_descriptors_become_the_facts_the_shared_engine_admits() {
        let facts = captured_facts(
            &SyntheticTabletDevice::device_descriptor(),
            &SyntheticTabletDevice::configuration_descriptor(),
            Some(UsbSpeed::Full),
            &lab_target(),
            lab_reports(),
        )
        .unwrap();
        assert_eq!(facts, SyntheticTabletDevice::captured_facts().unwrap());
        assert!(verify_captured_tablet(&facts).is_ok());
    }

    #[test]
    fn a_device_that_changed_under_the_capture_is_refused() {
        let mut other = SyntheticTabletDevice::device_descriptor();
        other[10] ^= 0xff;
        let error = captured_facts(
            &other,
            &SyntheticTabletDevice::configuration_descriptor(),
            Some(UsbSpeed::Full),
            &lab_target(),
            lab_reports(),
        )
        .unwrap_err();
        assert!(error.contains("not the classified"), "{error}");
    }

    #[test]
    fn malformed_descriptors_and_unsupported_speeds_are_refused() {
        let config = SyntheticTabletDevice::configuration_descriptor();
        let device = SyntheticTabletDevice::device_descriptor();
        assert!(
            captured_facts(
                &device[..17],
                &config,
                Some(UsbSpeed::Full),
                &lab_target(),
                lab_reports()
            )
            .is_err()
        );
        let mut wrong_type = device;
        wrong_type[1] = 2;
        assert!(
            captured_facts(
                &wrong_type,
                &config,
                Some(UsbSpeed::Full),
                &lab_target(),
                lab_reports()
            )
            .is_err()
        );
        assert!(
            captured_facts(
                &device,
                &config[..20],
                Some(UsbSpeed::Full),
                &lab_target(),
                lab_reports()
            )
            .is_err()
        );
        let error =
            captured_facts(&device, &config, None, &lab_target(), lab_reports()).unwrap_err();
        assert!(error.contains("speed"), "{error}");
    }

    fn hid(location: Option<u32>, interface: Option<u8>, bytes: &[u8]) -> RegistryHid {
        RegistryHid {
            vendor_id: 0x056a,
            product_id: 0x0027,
            location_id: location,
            interface,
            report_descriptor: bytes.to_vec(),
        }
    }

    #[test]
    fn report_descriptors_come_only_from_the_device_at_its_location() {
        let tablet = device(0x10, Some(0x0110_0000), 0x056a, 0x0027);
        let entries = [
            hid(Some(0x0110_0000), Some(1), &[1]),
            hid(Some(0x0110_0000), Some(0), &[0]),
            hid(Some(0x0110_0000), Some(0), &[0]),
            hid(Some(0x0120_0000), Some(2), &[9]),
            hid(Some(0x0110_0000), None, &[7]),
            RegistryHid {
                vendor_id: 0x046d,
                ..hid(Some(0x0110_0000), Some(3), &[8])
            },
        ];
        let found = hid_descriptors_for(&entries, &tablet).unwrap();
        assert_eq!(
            found,
            vec![
                InterfaceReportDescriptor {
                    interface: 0,
                    descriptor: vec![0]
                },
                InterfaceReportDescriptor {
                    interface: 1,
                    descriptor: vec![1]
                },
            ]
        );
        let without_location = RegistryUsbDevice {
            location_id: None,
            ..tablet.clone()
        };
        assert_eq!(
            hid_descriptors_for(&entries, &without_location)
                .unwrap()
                .len(),
            3
        );
        let conflicting = [
            hid(Some(0x0110_0000), Some(0), &[0]),
            hid(Some(0x0110_0000), Some(0), &[1]),
        ];
        assert!(hid_descriptors_for(&conflicting, &tablet).is_err());
    }

    #[test]
    fn control_requests_go_to_the_object_that_owns_their_recipient() {
        let configuration =
            parse_configuration_descriptor(&SyntheticTabletDevice::configuration_descriptor())
                .unwrap();
        // GET_DESCRIPTOR(Device), device recipient.
        assert_eq!(control_route(&configuration, 0x80, 0), ControlRoute::Device);
        // HID GET_DESCRIPTOR(Report) and SET_IDLE, interface 0.
        assert_eq!(
            control_route(&configuration, 0x81, 0),
            ControlRoute::Interface(0)
        );
        assert_eq!(
            control_route(&configuration, 0x21, 0),
            ControlRoute::Interface(0)
        );
        // An interface the device does not have.
        assert_eq!(control_route(&configuration, 0x21, 4), ControlRoute::Device);
        // GET_STATUS on endpoint 0x81, and on an unknown endpoint.
        assert_eq!(
            control_route(&configuration, 0x82, 0x81),
            ControlRoute::Interface(0)
        );
        assert_eq!(
            control_route(&configuration, 0x82, 0x02),
            ControlRoute::Device
        );
        // Other recipient.
        assert_eq!(control_route(&configuration, 0x83, 0), ControlRoute::Device);
    }

    #[test]
    fn interfaces_still_unpublished_are_listed_once_in_order() {
        let mut configuration =
            parse_configuration_descriptor(&SyntheticTabletDevice::configuration_descriptor())
                .unwrap();
        let mut second = configuration.interfaces[0].clone();
        second.number = arcen_usb_bridge::InterfaceNumber(2);
        let mut second_alternate = second.clone();
        second_alternate.alternate_setting = arcen_usb_bridge::AlternateSetting(1);
        configuration.interfaces.push(second_alternate);
        configuration.interfaces.push(second);
        assert_eq!(missing_interfaces(&configuration, &[]), vec![0, 2]);
        assert_eq!(missing_interfaces(&configuration, &[2]), vec![0]);
        assert!(missing_interfaces(&configuration, &[0, 2, 5]).is_empty());
    }

    #[test]
    fn pipes_follow_the_selected_alternate_setting() {
        let configuration =
            parse_configuration_descriptor(&SyntheticTabletDevice::configuration_descriptor())
                .unwrap();
        assert_eq!(pipes_for(&configuration, 0, 0), vec![EndpointAddress(0x81)]);
        assert!(pipes_for(&configuration, 0, 1).is_empty());
        assert!(pipes_for(&configuration, 1, 0).is_empty());
    }
}
