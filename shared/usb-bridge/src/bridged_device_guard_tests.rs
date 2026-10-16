use super::*;
use crate::HidClassDescriptorError;
use crate::descriptor_fixtures::BOOT_KEYBOARD;
use crate::{
    SYNTHETIC_TABLET_REPORT_DESCRIPTOR, SyntheticTabletDevice, hid_report_descriptor_request,
};
use std::num::NonZeroU32;

fn urb(id: u32) -> UrbId {
    UrbId::new(NonZeroU32::new(id).unwrap())
}

fn claim(interface: u8, descriptor: &[u8]) -> InterfaceReportDescriptor {
    InterfaceReportDescriptor {
        interface,
        descriptor: descriptor.to_vec(),
    }
}

fn get_descriptor(kind: u8, length: u16) -> SetupPacket {
    SetupPacket {
        request_type: 0x80,
        request: 0x06,
        value: u16::from(kind) << 8,
        index: 0,
        length,
    }
}

fn synthetic_guard() -> BridgedDeviceGuard {
    BridgedDeviceGuard::new(
        SyntheticTabletDevice::identity(),
        vec![claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR)],
    )
}

/// Submits one descriptor request and checks its completion.
fn answer(
    guard: &mut BridgedDeviceGuard,
    id: u32,
    setup: SetupPacket,
    payload: &[u8],
) -> Result<(), BridgedDeviceViolation> {
    guard.observe_submit(urb(id), Some(setup)).unwrap();
    guard.check_completion(urb(id), true, payload)
}

/// Report-descriptor length the test configurations advertise per interface:
/// what the guards in these tests authorize there.
fn default_report_length(interface: u8) -> usize {
    match interface {
        0 => SYNTHETIC_TABLET_REPORT_DESCRIPTOR.len(),
        1 => BOOT_KEYBOARD.len(),
        _ => 0x40,
    }
}

/// Builds a configuration descriptor from `(number, alternate, class,
/// subclass, protocol, endpoint attributes)` interfaces, advertising
/// [`default_report_length`] in each HID class descriptor.
fn configuration(
    kind: u8,
    declared_interfaces: u8,
    interfaces: &[(u8, u8, u8, u8, u8, u8)],
) -> Vec<u8> {
    configuration_with(kind, declared_interfaces, interfaces, default_report_length)
}

/// As [`configuration`], advertising `report_length(interface)`.
fn configuration_with(
    kind: u8,
    declared_interfaces: u8,
    interfaces: &[(u8, u8, u8, u8, u8, u8)],
    report_length: impl Fn(u8) -> usize,
) -> Vec<u8> {
    let mut body = Vec::new();
    for &(number, alternate, class, subclass, protocol, attributes) in interfaces {
        body.extend_from_slice(&[9, 4, number, alternate, 1, class, subclass, protocol, 0]);
        if class == 3 {
            let [low, high] = u16::try_from(report_length(number)).unwrap().to_le_bytes();
            body.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, low, high]);
        }
        body.extend_from_slice(&[7, 5, 0x81 + number, attributes, 64, 0, 1]);
    }
    let total = u16::try_from(9 + body.len()).unwrap().to_le_bytes();
    let mut out = vec![
        9,
        kind,
        total[0],
        total[1],
        declared_interfaces,
        1,
        0,
        0x80,
        50,
    ];
    out.extend_from_slice(&body);
    out
}

#[test]
fn the_synthetic_lab_tablet_passes_its_own_enumeration() {
    let mut guard = synthetic_guard();
    let device = SyntheticTabletDevice::device_descriptor();
    // The host reads the first 8 bytes, then all 18.
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(1, 8), &device[..8]),
        Ok(())
    );
    assert_eq!(
        answer(&mut guard, 2, get_descriptor(1, 18), &device),
        Ok(())
    );
    let config = SyntheticTabletDevice::configuration_descriptor();
    assert_eq!(
        answer(&mut guard, 3, get_descriptor(2, 9), &config[..9]),
        Ok(())
    );
    assert_eq!(
        answer(&mut guard, 4, get_descriptor(2, 255), &config),
        Ok(())
    );
    assert_eq!(
        answer(
            &mut guard,
            5,
            hid_report_descriptor_request(
                0,
                u16::try_from(SYNTHETIC_TABLET_REPORT_DESCRIPTOR.len()).unwrap()
            ),
            SYNTHETIC_TABLET_REPORT_DESCRIPTOR
        ),
        Ok(())
    );
    assert_eq!(guard.pending(), 0);
}

/// The reviewed attack: a peer claims a pen tablet, then enumerates as mass
/// storage. Before this guard only report descriptors were checked.
#[test]
fn a_mass_storage_configuration_is_refused_before_the_kernel_sees_it() {
    let mut guard = synthetic_guard();
    let storage = configuration(2, 1, &[(0, 0, 0x08, 0x06, 0x50, 0x02)]);
    assert!(matches!(
        answer(&mut guard, 1, get_descriptor(2, 255), &storage),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::Unparseable(_) | ConfigurationViolation::NotHid { .. }
        ))
    ));
    // Even with interrupt endpoints, a non-HID interface is refused.
    let storage_interrupt = configuration(2, 1, &[(0, 0, 0x08, 0x06, 0x50, 0x03)]);
    assert_eq!(
        answer(&mut guard, 2, get_descriptor(2, 255), &storage_interrupt),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::NotHid {
                interface: 0,
                class: 0x08
            }
        ))
    );
}

#[test]
fn a_device_descriptor_that_changes_class_identity_or_configurations_is_refused() {
    let mut guard = synthetic_guard();
    let mut storage = SyntheticTabletDevice::device_descriptor();
    storage[4] = 0x08;
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(1, 8), &storage[..8]),
        Err(BridgedDeviceViolation::Device(
            DeviceDescriptorViolation::Class(0x08)
        ))
    );
    let mut other = SyntheticTabletDevice::device_descriptor();
    other[10] ^= 0xFF;
    assert_eq!(
        answer(&mut guard, 2, get_descriptor(1, 18), &other),
        Err(BridgedDeviceViolation::Device(
            DeviceDescriptorViolation::Identity
        ))
    );
    let mut two = SyntheticTabletDevice::device_descriptor();
    two[17] = 2;
    assert_eq!(
        answer(&mut guard, 3, get_descriptor(1, 18), &two),
        Err(BridgedDeviceViolation::Device(
            DeviceDescriptorViolation::ConfigurationCount(2)
        ))
    );
    let mut long = SyntheticTabletDevice::device_descriptor().to_vec();
    long.push(0);
    assert_eq!(
        answer(&mut guard, 4, get_descriptor(1, 64), &long),
        Err(BridgedDeviceViolation::Device(
            DeviceDescriptorViolation::Malformed
        ))
    );
    let mut wrong_type = SyntheticTabletDevice::device_descriptor();
    wrong_type[1] = 2;
    assert_eq!(
        answer(&mut guard, 5, get_descriptor(1, 18), &wrong_type),
        Err(BridgedDeviceViolation::Device(
            DeviceDescriptorViolation::Malformed
        ))
    );
}

#[test]
fn a_device_qualifier_must_stay_hid_or_per_interface() {
    let mut guard = synthetic_guard();
    let qualifier = [10, 6, 0x00, 0x02, 0, 0, 0, 64, 1, 0];
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(6, 10), &qualifier),
        Ok(())
    );
    let mut storage = qualifier;
    storage[4] = 0x08;
    assert_eq!(
        answer(&mut guard, 2, get_descriptor(6, 10), &storage),
        Err(BridgedDeviceViolation::Device(
            DeviceDescriptorViolation::Class(0x08)
        ))
    );
}

/// A tablet's express keys as a boot-keyboard interface are bridged with it;
/// the interface was authorized with its report descriptor.
#[test]
fn an_authorized_express_key_boot_keyboard_interface_passes() {
    let mut guard = BridgedDeviceGuard::new(
        SyntheticTabletDevice::identity(),
        vec![
            claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR),
            claim(1, BOOT_KEYBOARD),
        ],
    );
    let config = configuration(2, 2, &[(0, 0, 3, 0, 0, 3), (1, 0, 3, 1, 1, 3)]);
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(2, 255), &config),
        Ok(())
    );
}

/// A pen tablet that also exposes a mass-storage interface (a driver CD,
/// for instance) is still refused: only HID interfaces are bridged.
#[test]
fn a_pen_with_a_mass_storage_interface_is_refused() {
    let mut guard = BridgedDeviceGuard::new(
        SyntheticTabletDevice::identity(),
        vec![
            claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR),
            claim(1, BOOT_KEYBOARD),
        ],
    );
    let config = configuration(2, 2, &[(0, 0, 3, 0, 0, 3), (1, 0, 0x08, 6, 0x50, 3)]);
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(2, 255), &config),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::NotHid {
                interface: 1,
                class: 0x08
            }
        ))
    );
    let bulk = configuration(2, 2, &[(0, 0, 3, 0, 0, 3), (1, 0, 0x08, 6, 0x50, 2)]);
    assert!(matches!(
        answer(&mut guard, 2, get_descriptor(2, 255), &bulk),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::Unparseable(_)
        ))
    ));
}

#[test]
fn interfaces_beyond_the_authorized_report_set_are_refused() {
    let mut guard = synthetic_guard();
    // Honest count: two interfaces where one was authorized.
    let extra = configuration(2, 2, &[(0, 0, 3, 0, 0, 3), (1, 0, 3, 0, 0, 3)]);
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(2, 9), &extra[..9]),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::InterfaceCount {
                declared: 2,
                authorized: 1
            }
        ))
    );
    // Lying count: header says one, body carries two.
    let lying = configuration(2, 1, &[(0, 0, 3, 0, 0, 3), (1, 0, 3, 0, 0, 3)]);
    assert_eq!(
        answer(&mut guard, 2, get_descriptor(2, 255), &lying),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::InterfaceSet
        ))
    );
    // A different interface number than the one authorized.
    let renumbered = configuration(2, 1, &[(3, 0, 3, 0, 0, 3)]);
    assert_eq!(
        answer(&mut guard, 3, get_descriptor(2, 255), &renumbered),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::InterfaceSet
        ))
    );
    // A non-HID alternate setting of an authorized interface.
    let alternate = configuration(2, 1, &[(0, 0, 3, 0, 0, 3), (0, 1, 0xFF, 0, 0, 3)]);
    assert_eq!(
        answer(&mut guard, 4, get_descriptor(2, 255), &alternate),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::NotHid {
                interface: 0,
                class: 0xFF
            }
        ))
    );
}

#[test]
fn partial_trailing_and_oversized_configuration_reads_are_refused() {
    let mut guard = synthetic_guard();
    let config = SyntheticTabletDevice::configuration_descriptor();
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(2, 20), &config[..20]),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::Malformed
        ))
    );
    let mut trailing = config.clone();
    trailing.push(0);
    assert_eq!(
        answer(&mut guard, 2, get_descriptor(2, 255), &trailing),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::Malformed
        ))
    );
    let huge = [9, 2, 0xFF, 0xFF, 1, 1, 0, 0x80, 50];
    assert_eq!(
        answer(&mut guard, 3, get_descriptor(2, 9), &huge),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::TooLarge(0xFFFF)
        ))
    );
}

#[test]
fn other_speed_configurations_are_held_to_the_same_rules() {
    let mut guard = synthetic_guard();
    let honest = configuration(7, 1, &[(0, 0, 3, 0, 0, 3)]);
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(7, 255), &honest),
        Ok(())
    );
    let storage = configuration(7, 1, &[(0, 0, 0x08, 6, 0x50, 3)]);
    assert!(answer(&mut guard, 2, get_descriptor(7, 255), &storage).is_err());
    // A type mismatch between request and answer is malformed.
    let swapped = configuration(2, 1, &[(0, 0, 3, 0, 0, 3)]);
    assert_eq!(
        answer(&mut guard, 3, get_descriptor(7, 255), &swapped),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::Malformed
        ))
    );
}

#[test]
fn an_intuos5_style_configuration_with_a_boot_mouse_passes() {
    use crate::descriptor_fixtures::{
        INTUOS5_STYLE_MOUSE_INTERFACE, INTUOS5_STYLE_PEN_INTERFACE,
        INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE,
    };
    let identity = UsbDeviceId {
        vendor_id: 0x056a,
        product_id: 0x0317,
        bcd_device: 0x0100,
    };
    let mut guard = BridgedDeviceGuard::new(
        identity,
        vec![
            claim(0, INTUOS5_STYLE_PEN_INTERFACE),
            claim(1, INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE),
            claim(2, INTUOS5_STYLE_MOUSE_INTERFACE),
        ],
    );
    let config = configuration_with(
        2,
        3,
        &[(0, 0, 3, 0, 0, 3), (1, 0, 3, 0, 0, 3), (2, 0, 3, 1, 2, 3)],
        |interface| match interface {
            0 => INTUOS5_STYLE_PEN_INTERFACE.len(),
            1 => INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE.len(),
            _ => INTUOS5_STYLE_MOUSE_INTERFACE.len(),
        },
    );
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(2, 255), &config),
        Ok(())
    );
    let device = [
        18, 1, 0x00, 0x02, 0, 0, 0, 64, 0x6a, 0x05, 0x17, 0x03, 0x00, 0x01, 1, 2, 3, 1,
    ];
    assert_eq!(
        answer(&mut guard, 2, get_descriptor(1, 18), &device),
        Ok(())
    );
}

#[test]
fn report_descriptors_must_match_the_classified_bytes() {
    let mut guard = synthetic_guard();
    assert_eq!(
        answer(
            &mut guard,
            1,
            hid_report_descriptor_request(0, 9),
            &SYNTHETIC_TABLET_REPORT_DESCRIPTOR[..9]
        ),
        Ok(())
    );
    assert_eq!(
        answer(
            &mut guard,
            2,
            hid_report_descriptor_request(0, 4096),
            BOOT_KEYBOARD
        ),
        Err(BridgedDeviceViolation::ReportMismatch { interface: 0 })
    );
    assert_eq!(
        answer(
            &mut guard,
            3,
            hid_report_descriptor_request(2, 64),
            BOOT_KEYBOARD
        ),
        Err(BridgedDeviceViolation::ReportUnclaimed { interface: 2 })
    );
}

#[test]
fn failed_cancelled_and_unrelated_urbs_pass_untouched() {
    let mut guard = synthetic_guard();
    guard
        .observe_submit(urb(4), Some(get_descriptor(2, 255)))
        .unwrap();
    assert_eq!(guard.check_completion(urb(4), false, BOOT_KEYBOARD), Ok(()));
    guard
        .observe_submit(urb(5), Some(get_descriptor(1, 18)))
        .unwrap();
    guard.observe_cancel(urb(5));
    assert_eq!(guard.check_completion(urb(5), true, BOOT_KEYBOARD), Ok(()));
    // A string descriptor and a non-control URB are not tracked.
    guard
        .observe_submit(urb(6), Some(get_descriptor(3, 255)))
        .unwrap();
    guard.observe_submit(urb(7), None).unwrap();
    assert_eq!(guard.pending(), 0);
    assert_eq!(guard.check_completion(urb(6), true, BOOT_KEYBOARD), Ok(()));
}

#[test]
fn the_pending_table_is_bounded() {
    let mut guard = synthetic_guard();
    for id in 1..=u32::try_from(MAX_IN_FLIGHT_URBS).unwrap() {
        guard
            .observe_submit(urb(id), Some(get_descriptor(1, 18)))
            .unwrap();
    }
    assert_eq!(
        guard.observe_submit(urb(100_000), Some(get_descriptor(1, 18))),
        Err(BridgedDeviceViolation::TooManyPending)
    );
}

/// The reviewed bypass: claim a keyboard+pen composite (separate report ids,
/// keyboard first), advertise only the keyboard prefix length in the
/// configuration's HID descriptor, and serve only that prefix. The host then
/// reads 47 bytes, never sees the pen, and a standalone keyboard is
/// forwarded. Both halves are now refused.
#[test]
fn a_configuration_advertising_a_shorter_report_length_is_refused() {
    let mut composite = BOOT_KEYBOARD.to_vec();
    let keyboard_length = composite.len();
    composite.extend_from_slice(SYNTHETIC_TABLET_REPORT_DESCRIPTOR);
    let mut guard = BridgedDeviceGuard::new(
        SyntheticTabletDevice::identity(),
        vec![claim(0, &composite)],
    );
    let short = configuration_with(2, 1, &[(0, 0, 3, 0, 0, 3)], |_| keyboard_length);
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(2, 255), &short),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::ReportLength {
                interface: 0,
                declared: Some(u16::try_from(keyboard_length).unwrap()),
                authorized: composite.len()
            }
        ))
    );
    let honest = configuration_with(2, 1, &[(0, 0, 3, 0, 0, 3)], |_| composite.len());
    assert_eq!(
        answer(&mut guard, 2, get_descriptor(2, 255), &honest),
        Ok(())
    );
}

#[test]
fn a_report_prefix_served_for_a_full_request_is_refused() {
    let mut composite = BOOT_KEYBOARD.to_vec();
    let keyboard_length = composite.len();
    composite.extend_from_slice(SYNTHETIC_TABLET_REPORT_DESCRIPTOR);
    let mut guard = BridgedDeviceGuard::new(
        SyntheticTabletDevice::identity(),
        vec![claim(0, &composite)],
    );
    let full = u16::try_from(composite.len()).unwrap();
    // The host asked for everything; only the keyboard prefix came back.
    assert_eq!(
        answer(
            &mut guard,
            1,
            hid_report_descriptor_request(0, full),
            &composite[..keyboard_length]
        ),
        Err(BridgedDeviceViolation::ReportTruncated {
            interface: 0,
            returned: keyboard_length,
            expected: composite.len()
        })
    );
    // Asking for more than exists still requires the whole descriptor.
    assert_eq!(
        answer(
            &mut guard,
            2,
            hid_report_descriptor_request(0, 4096),
            &composite[..keyboard_length]
        ),
        Err(BridgedDeviceViolation::ReportTruncated {
            interface: 0,
            returned: keyboard_length,
            expected: composite.len()
        })
    );
    // An explicit shorter request may be answered with exactly that prefix.
    assert_eq!(
        answer(
            &mut guard,
            3,
            hid_report_descriptor_request(0, 9),
            &composite[..9]
        ),
        Ok(())
    );
    assert_eq!(
        answer(
            &mut guard,
            4,
            hid_report_descriptor_request(0, 9),
            &composite[..5]
        ),
        Err(BridgedDeviceViolation::ReportTruncated {
            interface: 0,
            returned: 5,
            expected: 9
        })
    );
    assert_eq!(
        answer(
            &mut guard,
            5,
            hid_report_descriptor_request(0, full),
            &composite
        ),
        Ok(())
    );
}

#[test]
fn every_interface_needs_exactly_one_hid_descriptor_with_the_authorized_length() {
    let mut guard = synthetic_guard();
    // An interface without a HID class descriptor.
    let mut missing = SyntheticTabletDevice::configuration_descriptor();
    missing.drain(18..27);
    missing[2] = u8::try_from(missing.len()).unwrap();
    assert_eq!(
        answer(&mut guard, 1, get_descriptor(2, 255), &missing),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::ReportLength {
                interface: 0,
                declared: None,
                authorized: SYNTHETIC_TABLET_REPORT_DESCRIPTOR.len()
            }
        ))
    );
    // Two HID class descriptors for one interface.
    let mut doubled = SyntheticTabletDevice::configuration_descriptor();
    let hid: Vec<u8> = doubled[18..27].to_vec();
    doubled.splice(27..27, hid);
    doubled[2] = u8::try_from(doubled.len()).unwrap();
    assert!(matches!(
        answer(&mut guard, 2, get_descriptor(2, 255), &doubled),
        Err(BridgedDeviceViolation::Configuration(
            ConfigurationViolation::ReportLength {
                interface: 0,
                declared: None,
                ..
            }
        ))
    ));
    // An alternate setting advertising another length.
    let alternate = configuration_with(2, 1, &[(0, 0, 3, 0, 0, 3), (0, 1, 3, 0, 0, 3)], |_| {
        SYNTHETIC_TABLET_REPORT_DESCRIPTOR.len()
    });
    assert_eq!(
        answer(&mut guard, 3, get_descriptor(2, 255), &alternate),
        Ok(())
    );
}

/// Builds the synthetic configuration with `hid` as its only HID class
/// descriptor.
fn synthetic_configuration_with_hid(hid: &[u8]) -> Vec<u8> {
    let mut config = SyntheticTabletDevice::configuration_descriptor();
    config.splice(18..27, hid.iter().copied());
    let total = u16::try_from(config.len()).unwrap().to_le_bytes();
    config[2] = total[0];
    config[3] = total[1];
    config
}

/// The reviewed bypass: one 12-byte HID class descriptor with two Report
/// entries. The guard used to read the first (the authorized length) while
/// Linux uses the last (a keyboard-only prefix), so the pen never reached
/// the kernel. A list with more than one Report entry is now refused in
/// either order.
#[test]
fn duplicate_report_entries_are_refused_in_either_order() {
    let mut composite = BOOT_KEYBOARD.to_vec();
    let keyboard = u16::try_from(composite.len()).unwrap().to_le_bytes();
    composite.extend_from_slice(SYNTHETIC_TABLET_REPORT_DESCRIPTOR);
    let full = u16::try_from(composite.len()).unwrap().to_le_bytes();
    let mut guard = BridgedDeviceGuard::new(
        SyntheticTabletDevice::identity(),
        vec![claim(0, &composite)],
    );
    for (first, second) in [(full, keyboard), (keyboard, full)] {
        let hid = [
            12, 0x21, 0x11, 0x01, 0, 2, 0x22, first[0], first[1], 0x22, second[0], second[1],
        ];
        let config = synthetic_configuration_with_hid(&hid);
        assert_eq!(
            answer(&mut guard, 1, get_descriptor(2, 255), &config),
            Err(BridgedDeviceViolation::Configuration(
                ConfigurationViolation::HidClassDescriptor {
                    interface: 0,
                    error: HidClassDescriptorError::ReportEntries(2)
                }
            ))
        );
    }
}

#[test]
fn a_report_entry_beside_a_physical_entry_is_accepted() {
    let mut guard = synthetic_guard();
    let [low, high] = u16::try_from(SYNTHETIC_TABLET_REPORT_DESCRIPTOR.len())
        .unwrap()
        .to_le_bytes();
    for hid in [
        [
            12, 0x21, 0x11, 0x01, 0, 2, 0x22, low, high, 0x23, 0x10, 0x00,
        ],
        [
            12, 0x21, 0x11, 0x01, 0, 2, 0x23, 0x10, 0x00, 0x22, low, high,
        ],
    ] {
        let config = synthetic_configuration_with_hid(&hid);
        assert_eq!(
            answer(&mut guard, 1, get_descriptor(2, 255), &config),
            Ok(())
        );
    }
}

#[test]
fn malformed_subordinate_lists_are_refused() {
    let mut guard = synthetic_guard();
    let [low, high] = u16::try_from(SYNTHETIC_TABLET_REPORT_DESCRIPTOR.len())
        .unwrap()
        .to_le_bytes();
    let cases: [(&[u8], HidClassDescriptorError); 5] = [
        // bNumDescriptors claims two entries in a 9-byte descriptor.
        (
            &[9, 0x21, 0x11, 0x01, 0, 2, 0x22, low, high],
            HidClassDescriptorError::InconsistentCount,
        ),
        // bNumDescriptors claims one entry in a 12-byte descriptor.
        (
            &[
                12, 0x21, 0x11, 0x01, 0, 1, 0x22, low, high, 0x22, 0x2F, 0x00,
            ],
            HidClassDescriptorError::InconsistentCount,
        ),
        // No entries at all.
        (
            &[6, 0x21, 0x11, 0x01, 0, 0],
            HidClassDescriptorError::InconsistentCount,
        ),
        // A list with no Report entry.
        (
            &[9, 0x21, 0x11, 0x01, 0, 1, 0x23, 0x10, 0x00],
            HidClassDescriptorError::ReportEntries(0),
        ),
        // A zero-length Report entry.
        (
            &[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 0x00, 0x00],
            HidClassDescriptorError::ReportLength(0),
        ),
    ];
    for (id, (hid, error)) in (1_u32..).zip(cases) {
        let config = synthetic_configuration_with_hid(hid);
        assert_eq!(
            answer(&mut guard, id, get_descriptor(2, 255), &config),
            Err(BridgedDeviceViolation::Configuration(
                ConfigurationViolation::HidClassDescriptor {
                    interface: 0,
                    error
                }
            )),
            "{hid:02x?}"
        );
    }
}
