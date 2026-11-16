// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
#![allow(clippy::float_cmp)]

use super::*;
use crate::descriptor_fixtures::{
    BOOT_KEYBOARD, GAMEPAD, GAOMON_STYLE_PEN, GENERIC_PEN_FIXTURES, HUION_STYLE_PEN,
    INTUOS5_STYLE_PEN_INTERFACE, TOUCH_PAD_ONLY, WACOM_GENERIC_STYLE_PEN, WINDOWS_STYLE_PEN,
    XP_PEN_STYLE_COMPOSITE,
};
use crate::{
    HostPenRoute, InterfaceReportDescriptor, UsbDeviceId, authorize_hard_usb_tablet, host_pen_route,
};

fn reader(descriptor: &[u8]) -> GenericPenReader {
    GenericPenReader::new(generic_pen_layout(descriptor).expect("a pen layout"))
}

fn huion(switches: u8, x: u16, y: u16, pressure: u16, tilt: (i8, i8)) -> Vec<u8> {
    let mut report = vec![0x0A, switches];
    report.extend_from_slice(&x.to_le_bytes());
    report.extend_from_slice(&y.to_le_bytes());
    report.extend_from_slice(&pressure.to_le_bytes());
    report.extend_from_slice(&[tilt.0.to_le_bytes()[0], tilt.1.to_le_bytes()[0]]);
    report
}

const HUION_TIP: u8 = 0x01;
const HUION_BARREL: u8 = 0x02;
const HUION_SECONDARY: u8 = 0x04;
const HUION_IN_RANGE: u8 = 0x20;

#[test]
fn every_generic_fixture_is_a_pen_tablet_and_has_one_readable_pen_report() {
    for (label, descriptor) in GENERIC_PEN_FIXTURES {
        let layout = generic_pen_layout(descriptor).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(layout.reports.len(), 1, "{label}");
        authorize_hard_usb_tablet(
            0,
            &[InterfaceReportDescriptor {
                interface: 0,
                descriptor: descriptor.to_vec(),
            }],
        )
        .unwrap_or_else(|e| panic!("{label} is not admitted as a tablet: {e:?}"));
    }
}

#[test]
fn a_huion_style_report_reads_position_pressure_tilt_and_buttons() {
    let layout = generic_pen_layout(HUION_STYLE_PEN).unwrap();
    assert!(layout.report_ids);
    let report = &layout.reports[0];
    assert_eq!(report.report_id, 0x0A);
    assert_eq!((report.x.bit_offset, report.x.bits), (8, 16));
    assert_eq!((report.y.bit_offset, report.y.bits), (24, 16));
    assert_eq!(report.x.logical_max, 50_800);
    assert_eq!(report.in_range.map(|f| f.bit_offset), Some(5));

    let mut pen = GenericPenReader::new(layout);
    let hover = pen
        .read(&huion(HUION_IN_RANGE, 25_400, 0, 0, (0, 0)))
        .unwrap();
    assert!(hover.in_proximity && !hover.touching);
    assert_eq!(hover.pressure, 0.0);
    assert_eq!((hover.device_x, hover.device_max_x), (25_400, 50_800));
    assert!((hover.x - 25_400.0 / 50_801.0).abs() < 1e-12);

    let down = pen
        .read(&huion(
            HUION_IN_RANGE | HUION_TIP | HUION_BARREL | HUION_SECONDARY,
            50_800,
            31_836,
            8191,
            (-60, 30),
        ))
        .unwrap();
    assert!(down.touching && !down.eraser);
    assert_eq!(down.pressure, 1.0);
    assert_eq!(down.barrel_buttons, 0b11);
    assert_eq!((down.tilt_x_degrees, down.tilt_y_degrees), (-60.0, 30.0));
    assert!(down.x < 1.0 && down.y < 1.0);

    let half = pen
        .read(&huion(HUION_IN_RANGE | HUION_TIP, 0, 0, 4096, (0, 0)))
        .unwrap();
    assert!((half.pressure - 4096.0 / 8191.0).abs() < 1e-6);

    let exit = pen.read(&huion(0, 0, 0, 0, (0, 0))).unwrap();
    assert!(!exit.in_proximity && !exit.touching);
    assert_eq!((exit.device_x, exit.pressure), (half.device_x, 0.0));
    assert_eq!(pen.read(&huion(0, 0, 0, 0, (0, 0))), None, "one exit only");
    assert_eq!(pen.counts(), (5, 0));
}

#[test]
fn reports_of_other_ids_short_reports_and_empty_reports_yield_nothing() {
    let mut pen = reader(HUION_STYLE_PEN);
    let mut other = huion(HUION_IN_RANGE, 1, 1, 0, (0, 0));
    other[0] = 0x0B;
    assert_eq!(pen.read(&other), None);
    let full = huion(HUION_IN_RANGE, 1, 1, 0, (0, 0));
    for length in 0..full.len() {
        assert_eq!(pen.read(&full[..length]), None, "length {length}");
    }
    assert!(pen.read(&full).is_some());
}

#[test]
fn a_report_without_ids_and_without_tilt_is_read_from_byte_zero() {
    let layout = generic_pen_layout(GAOMON_STYLE_PEN).unwrap();
    assert!(!layout.report_ids);
    assert!(layout.reports[0].tilt_x.is_none());
    let mut pen = GenericPenReader::new(layout);
    let mut report = vec![0b101];
    report.extend_from_slice(&16_384_u16.to_le_bytes());
    report.extend_from_slice(&32_767_u16.to_le_bytes());
    report.extend_from_slice(&1024_u16.to_le_bytes());
    let sample = pen.read(&report).unwrap();
    assert!(sample.touching && sample.in_proximity);
    assert_eq!(sample.device_max_x, 32_767);
    assert_eq!(sample.device_y, 32_767);
    assert!((sample.pressure - 1024.0 / 2047.0).abs() < 1e-6);
    assert_eq!(sample.tilt_x_degrees, 0.0);
}

#[test]
fn a_composite_interface_reads_only_its_pen_report() {
    let layout = generic_pen_layout(XP_PEN_STYLE_COMPOSITE).unwrap();
    assert_eq!(layout.reports.len(), 1);
    assert_eq!(layout.reports[0].report_id, 7);
    let mut pen = GenericPenReader::new(layout);
    // The mouse and the keyboard report never become pen samples.
    assert_eq!(pen.read(&[1, 0x01, 10, 10]), None);
    assert_eq!(pen.read(&[6, 0x02, 0, 4, 0, 0, 0, 0, 0]), None);
    // Eraser end in range (invert), then touching (eraser), then out.
    let pen_report = |switches: u8, pressure: u16, tilt: i8| {
        let mut report = vec![7, switches];
        report.extend_from_slice(&20_000_u16.to_le_bytes());
        report.extend_from_slice(&12_500_u16.to_le_bytes());
        report.extend_from_slice(&pressure.to_le_bytes());
        report.extend_from_slice(&[tilt.to_le_bytes()[0], 0]);
        report
    };
    let near = pen.read(&pen_report(0b1_1000, 0, 127)).unwrap();
    assert!(near.eraser && near.in_proximity && !near.touching);
    // Unitless ±127 tilt is spread over ±64°.
    assert!((near.tilt_x_degrees - 64.0).abs() < 1e-3);
    let rubbing = pen.read(&pen_report(0b1_0100, 16_383, -127)).unwrap();
    assert!(rubbing.eraser && rubbing.touching);
    assert_eq!(rubbing.pressure, 1.0);
    assert!((rubbing.tilt_x_degrees + 64.0).abs() < 1e-3);
    let gone = pen.read(&pen_report(0, 0, 0)).unwrap();
    assert!(!gone.in_proximity && gone.eraser);
    // The next proximity with the tip is a pen again.
    let tip = pen.read(&pen_report(0b1_0001, 100, 0)).unwrap();
    assert!(!tip.eraser && tip.touching);
}

#[test]
fn the_vendor_digitizer_convention_reads_like_the_standard_page() {
    let layout = generic_pen_layout(WACOM_GENERIC_STYLE_PEN).unwrap();
    let report = &layout.reports[0];
    assert_eq!(report.report_id, 0x10);
    assert_eq!((report.x.bit_offset, report.x.bits), (16, 32));
    assert_eq!(report.x.logical_max, 44_800);
    assert_eq!(report.pressure.map(|f| f.bit_offset), Some(80));
    assert!(report.twist.is_some() && report.invert.is_some() && report.eraser.is_some());
    let mut pen = GenericPenReader::new(layout);
    let mut bytes = vec![0x10, 0b10_0011, 0];
    bytes.extend_from_slice(&44_800_u32.to_le_bytes());
    bytes.extend_from_slice(&14_176_u32.to_le_bytes());
    bytes.extend_from_slice(&2048_u16.to_le_bytes());
    bytes.extend_from_slice(&(-4500_i16).to_le_bytes());
    bytes.extend_from_slice(&9000_i16.to_le_bytes());
    bytes.extend_from_slice(&270_u16.to_le_bytes());
    bytes.extend_from_slice(&0xDEAD_BEEF_u32.to_le_bytes());
    assert_eq!(bytes.len(), 23);
    let sample = pen.read(&bytes).unwrap();
    assert!(sample.touching && sample.in_proximity && !sample.eraser);
    assert_eq!(sample.barrel_buttons, 0b01);
    assert_eq!(sample.device_x, 44_800);
    assert!((sample.tilt_x_degrees + 45.0).abs() < 1e-3);
    assert!((sample.tilt_y_degrees - 90.0).abs() < 1e-3);
    assert!((sample.rotation_degrees - 270.0).abs() < 1e-3);
}

#[test]
fn the_windows_pen_layout_survives_push_and_pop_and_reads_twist_in_centidegrees() {
    let layout = generic_pen_layout(WINDOWS_STYLE_PEN).unwrap();
    let report = &layout.reports[0];
    assert_eq!(report.x.logical_max, 21_240);
    assert_eq!(report.y.logical_max, 15_980);
    // The pop restored the units the X and Y push changed.
    assert_eq!(
        report.pressure.map(|f| (f.unit, f.unit_exponent)),
        Some((0, 0))
    );
    let mut pen = GenericPenReader::new(layout);
    let mut bytes = vec![2, 0b1_0001];
    bytes.extend_from_slice(&10_620_u16.to_le_bytes());
    bytes.extend_from_slice(&7990_u16.to_le_bytes());
    bytes.extend_from_slice(&4095_u16.to_le_bytes());
    bytes.extend_from_slice(&0_i16.to_le_bytes());
    bytes.extend_from_slice(&(-9000_i16).to_le_bytes());
    bytes.extend_from_slice(&18_000_u32.to_le_bytes());
    let sample = pen.read(&bytes).unwrap();
    assert!(sample.touching);
    assert_eq!(sample.pressure, 1.0);
    assert!((sample.tilt_y_degrees + 90.0).abs() < 1e-3);
    assert!((sample.rotation_degrees - 180.0).abs() < 1e-3);
}

#[test]
fn opaque_vendor_pens_touch_pads_mice_keyboards_and_gamepads_have_no_pen_report() {
    for descriptor in [
        INTUOS5_STYLE_PEN_INTERFACE,
        crate::INTUOS5_PEN_REPORT_DESCRIPTOR,
        crate::INTUOS5_TOUCH_REPORT_DESCRIPTOR,
        crate::INTUOS5_MOUSE_REPORT_DESCRIPTOR,
        TOUCH_PAD_ONLY,
        BOOT_KEYBOARD,
        GAMEPAD,
    ] {
        assert_eq!(
            generic_pen_layout(descriptor),
            Err(GenericPenRefusal::NoPenReport)
        );
    }
}

#[test]
fn malformed_and_out_of_bounds_descriptors_are_refused() {
    let refused = |bytes: &[u8]| {
        assert_eq!(
            generic_pen_layout(bytes),
            Err(GenericPenRefusal::Malformed),
            "{bytes:02x?}"
        );
    };
    refused(&[]);
    refused(&vec![0x00; MAX_HID_REPORT_DESCRIPTOR_BYTES + 1]);
    refused(&[0x05]); // truncated item
    refused(&[0xFE, 0x10, 0x00]); // truncated long item
    refused(&[0xA1, 0x01]); // unterminated collection
    refused(&[0xC0]); // unbalanced
    refused(&[0x85, 0x00]); // report id 0
    refused(&[0x86, 0x00, 0x01]); // report id above 255
    refused(&[0xA4; MAX_GLOBAL_STACK_DEPTH + 1]); // push overflow
    refused(&[0xB4]); // pop underflow
    let deep: Vec<u8> = std::iter::repeat_n([0xA1, 0x00], MAX_COLLECTION_DEPTH + 1)
        .flatten()
        .chain(std::iter::repeat_n(0xC0, MAX_COLLECTION_DEPTH + 1))
        .collect();
    refused(&deep);
    let many_usages: Vec<u8> = std::iter::repeat_n([0x09, 0x30], MAX_LOCAL_USAGES + 1)
        .flatten()
        .collect();
    refused(&many_usages);
}

#[test]
fn fields_are_read_little_endian_bit_by_bit_and_sign_extended() {
    let field = |bit_offset, bits, logical_min| PenField {
        bit_offset,
        bits,
        logical_min,
        logical_max: 1,
        physical_min: 0,
        physical_max: 0,
        unit: 0,
        unit_exponent: 0,
    };
    let data = [0b1010_0000, 0xFF, 0x7F, 0x80, 0x01];
    assert_eq!(field(5, 1, 0).read(&data), Some(1));
    assert_eq!(field(6, 1, 0).read(&data), Some(0));
    assert_eq!(field(8, 16, 0).read(&data), Some(0x7FFF));
    assert_eq!(field(8, 8, -1).read(&data), Some(-1));
    assert_eq!(field(24, 8, -128).read(&data), Some(-128));
    assert_eq!(field(8, 32, 0).read(&data), Some(0x0180_7FFF));
    assert_eq!(field(8, 32, -1).read(&data), Some(0x0180_7FFF));
    assert_eq!(field(16, 32, 0).read(&data), None, "past the end");
    assert_eq!(field(0, 33, 0).read(&[0; 8]), None, "too wide");
    assert_eq!(field(0, 0, 0).read(&[0; 8]), None, "empty");
    assert_eq!(field(u32::MAX, 32, 0).read(&[0; 8]), None, "no overflow");
}

#[test]
fn the_route_prefers_a_dedicated_reader_then_any_readable_interface_then_refuses_by_id() {
    let claim = |interface, descriptor: &[u8]| InterfaceReportDescriptor {
        interface,
        descriptor: descriptor.to_vec(),
    };
    let intuos = UsbDeviceId {
        vendor_id: 0x056a,
        product_id: 0x0317,
        bcd_device: 0x0100,
    };
    let route = host_pen_route(intuos, &[claim(0, crate::INTUOS5_PEN_REPORT_DESCRIPTOR)]).unwrap();
    assert!(matches!(route, HostPenRoute::Dedicated(_)));
    assert_eq!(
        route.reader().mode_switch(),
        Some(crate::INTUOS_MODE_SWITCH)
    );

    let unknown = UsbDeviceId {
        vendor_id: 0x256c,
        product_id: 0x006d,
        bcd_device: 0x0001,
    };
    let route = host_pen_route(
        unknown,
        &[
            claim(0, INTUOS5_STYLE_PEN_INTERFACE),
            claim(1, HUION_STYLE_PEN),
        ],
    )
    .unwrap();
    assert_eq!(route.generic_interface(), Some(1));
    assert_eq!(route.kind(), "generic");
    assert_eq!(route.reader().mode_switch(), None);

    // An unknown Wacom model whose pen speaks the Intuos packet format.
    let other_intuos = UsbDeviceId {
        product_id: 0x0027,
        ..intuos
    };
    let refusal = host_pen_route(
        other_intuos,
        &[claim(0, crate::INTUOS5_PEN_REPORT_DESCRIPTOR)],
    )
    .unwrap_err();
    assert!(refusal.to_string().contains("056a:0027"), "{refusal}");
    assert!(refusal.to_string().contains("vendor format"), "{refusal}");
}

/// A small deterministic generator: the tests need reproducible noise, not
/// cryptographic randomness, and no new dependency.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn byte(&mut self) -> u8 {
        self.next().to_le_bytes()[0]
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound.max(1)).unwrap()).unwrap()
    }

    fn bytes(&mut self, length: usize) -> Vec<u8> {
        (0..length).map(|_| self.byte()).collect()
    }
}

fn assert_sane(sample: &HostPenSample) {
    assert!((0.0..1.0).contains(&sample.x), "{sample:?}");
    assert!((0.0..1.0).contains(&sample.y), "{sample:?}");
    assert!(sample.device_x <= sample.device_max_x, "{sample:?}");
    assert!(sample.device_y <= sample.device_max_y, "{sample:?}");
    assert!((0.0..=1.0).contains(&sample.pressure), "{sample:?}");
    assert!(
        (-90.0..=90.0).contains(&sample.tilt_x_degrees),
        "{sample:?}"
    );
    assert!(
        (-90.0..=90.0).contains(&sample.tilt_y_degrees),
        "{sample:?}"
    );
    assert!(
        (0.0..360.0).contains(&sample.rotation_degrees),
        "{sample:?}"
    );
    assert!(sample.barrel_buttons <= 0b11, "{sample:?}");
    assert!(!sample.touching || sample.in_proximity, "{sample:?}");
}

#[test]
fn fuzz_random_reports_never_panic_and_every_sample_is_in_range() {
    let mut noise = Noise(0x9E37_79B9_7F4A_7C15);
    for (label, descriptor) in GENERIC_PEN_FIXTURES {
        let layout = generic_pen_layout(descriptor).unwrap();
        let id = layout.reports[0].report_id;
        let mut pen = GenericPenReader::new(layout.clone());
        let mut produced = 0;
        for round in 0..20_000 {
            let length = noise.below(40);
            let mut report = noise.bytes(length);
            if layout.report_ids && !report.is_empty() && round % 2 == 0 {
                report[0] = id;
            }
            if let Some(sample) = pen.read(&report) {
                produced += 1;
                assert_sane(&sample);
            }
        }
        assert!(produced > 100, "{label}: {produced}");
    }
}

#[test]
fn fuzz_random_and_mutated_descriptors_never_panic_and_layouts_stay_bounded() {
    let mut noise = Noise(0xD1B5_4A32_D192_ED03);
    let check = |noise: &mut Noise, descriptor: &[u8]| {
        if let Ok(layout) = generic_pen_layout(descriptor) {
            assert!(!layout.reports.is_empty() && layout.reports.len() <= MAX_PEN_REPORTS);
            for report in &layout.reports {
                for field in [report.x, report.y] {
                    assert!(field.bits >= 1 && field.bits <= MAX_PEN_FIELD_BITS);
                    assert!(field.bit_offset + field.bits <= MAX_PEN_REPORT_BITS);
                }
            }
            let mut pen = GenericPenReader::new(layout);
            for _ in 0..64 {
                let length = noise.below(64);
                if let Some(sample) = pen.read(&noise.bytes(length)) {
                    assert_sane(&sample);
                }
            }
        }
    };
    for _ in 0..20_000 {
        let length = noise.below(256);
        let bytes = noise.bytes(length);
        check(&mut noise, &bytes);
    }
    for (_, descriptor) in GENERIC_PEN_FIXTURES {
        for _ in 0..4_000 {
            let mut bytes = descriptor.to_vec();
            match noise.below(4) {
                0 => {
                    let at = noise.below(bytes.len());
                    bytes[at] ^= 1 << noise.below(8);
                }
                1 => bytes.truncate(noise.below(bytes.len())),
                2 => {
                    let at = noise.below(bytes.len() + 1);
                    let value = noise.byte();
                    bytes.insert(at, value);
                }
                _ => {
                    let at = noise.below(bytes.len());
                    bytes[at] = noise.byte();
                }
            }
            check(&mut noise, &bytes);
        }
    }
}

#[test]
fn degenerate_ranges_never_divide_by_zero() {
    // X and Y with an empty range are not positions: no layout.
    let flat = [
        0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95,
        0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x05,
        0x25, 0x05, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0xC0,
    ];
    assert_eq!(
        generic_pen_layout(&flat),
        Err(GenericPenRefusal::NoPenReport)
    );
    // A pressure with an empty range is dropped; tip alone gives full
    // pressure while touching.
    let flat_pressure = [
        0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95,
        0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x26, 0xFF,
        0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0x05, 0x0D, 0x09, 0x30, 0x15, 0x00, 0x25, 0x00,
        0x95, 0x01, 0x81, 0x02, 0xC0,
    ];
    let layout = generic_pen_layout(&flat_pressure).unwrap();
    assert!(layout.reports[0].pressure.is_none());
    let sample = GenericPenReader::new(layout).read(&[1, 0, 0, 7]).unwrap();
    assert_eq!(sample.pressure, 1.0);
}

#[test]
fn a_collection_names_a_pen_on_the_digitizers_page_or_its_vendor_mirror() {
    use crate::names_pen_digitizer;
    for (page, usage) in [
        (0x0D, 0x01),
        (0x0D, 0x02),
        (0x0D, 0x03),
        (0xFF0D, 0x01),
        (0xFF0D, 0x02),
    ] {
        assert!(names_pen_digitizer(page, usage), "{page:04x}:{usage:02x}");
    }
    for (page, usage) in [
        (0x0D, 0x04),
        (0x0D, 0x05),
        (0x01, 0x02),
        (0xFF00, 0x01),
        (0xFF0D, 0x04),
    ] {
        assert!(!names_pen_digitizer(page, usage), "{page:04x}:{usage:02x}");
    }
}

/// A pen with tip, invert, X, Y and pressure but no In Range (one byte of
/// switches, 8-bit X and Y, 8-bit pressure) and a unitless 0..359 twist.
const NO_IN_RANGE_PEN: &[u8] = &[
    0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x42, 0x09, 0x3C, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
    0x95, 0x02, 0x81, 0x02, 0x95, 0x06, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x26, 0xFF,
    0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0x05, 0x0D, 0x09, 0x30, 0x95, 0x01, 0x81, 0x02, 0x09,
    0x41, 0x26, 0x67, 0x01, 0x75, 0x10, 0x81, 0x02, 0xC0,
];

#[test]
fn a_pen_without_in_range_leaves_proximity_when_it_goes_quiet() {
    let layout = generic_pen_layout(NO_IN_RANGE_PEN).unwrap();
    assert!(layout.reports[0].in_range.is_none());
    let mut pen = GenericPenReader::new(layout);
    let start = std::time::Instant::now();
    let report = |switches: u8, twist: u16| {
        let mut report = vec![switches, 10, 20, 128];
        report.extend_from_slice(&twist.to_le_bytes());
        report
    };
    let eraser = pen.read_at(&report(0b10, 0), start).unwrap();
    assert!(eraser.in_proximity && eraser.eraser);
    assert_eq!(pen.expire(start + GENERIC_QUIET_EXIT / 2), None);
    let gone = pen.expire(start + GENERIC_QUIET_EXIT).unwrap();
    assert!(!gone.in_proximity && gone.eraser);
    assert_eq!(pen.expire(start + GENERIC_QUIET_EXIT * 4), None, "one exit");
    // The next approach is a pen tip again, not a stuck eraser.
    let later = start + GENERIC_QUIET_EXIT * 5;
    let tip = pen.read_at(&report(0b01, 0), later).unwrap();
    assert!(tip.in_proximity && tip.touching && !tip.eraser);
    // A pen that reports In Range never times out.
    let mut huion = reader(HUION_STYLE_PEN);
    huion
        .read_at(&huion_report(HUION_IN_RANGE, 1, 1, 0, (0, 0)), start)
        .unwrap();
    assert_eq!(huion.expire(start + GENERIC_QUIET_EXIT * 10), None);
}

#[test]
fn a_pen_without_in_range_held_still_on_the_surface_stays_down() {
    let mut pen = GenericPenReader::new(generic_pen_layout(NO_IN_RANGE_PEN).unwrap());
    let start = std::time::Instant::now();
    let report = |switches: u8| vec![switches, 10, 20, 128, 0, 0];
    assert!(pen.read_at(&report(0b01), start).unwrap().touching);
    // Silence while touching (a device honouring SET_IDLE 0) is not an exit.
    assert_eq!(pen.expire(start + GENERIC_QUIET_EXIT * 20), None);
    // Lifting is a report; then silence while hovering is.
    let lifted = start + GENERIC_QUIET_EXIT * 21;
    let hover = pen.read_at(&report(0b00), lifted).unwrap();
    assert!(hover.in_proximity && !hover.touching);
    let gone = pen.expire(lifted + GENERIC_QUIET_EXIT).unwrap();
    assert!(!gone.in_proximity && !gone.touching);
}

fn huion_report(switches: u8, x: u16, y: u16, pressure: u16, tilt: (i8, i8)) -> Vec<u8> {
    huion(switches, x, y, pressure, tilt)
}

#[test]
fn a_unitless_twist_spans_one_full_turn() {
    let mut pen = GenericPenReader::new(generic_pen_layout(NO_IN_RANGE_PEN).unwrap());
    let rotation = |pen: &mut GenericPenReader, twist: u16| {
        let mut report = vec![0b01, 10, 20, 128];
        report.extend_from_slice(&twist.to_le_bytes());
        pen.read(&report).unwrap().rotation_degrees
    };
    assert!(rotation(&mut pen, 0).abs() < 1e-3);
    assert!((rotation(&mut pen, 90) - 90.0).abs() < 1e-3);
    assert!((rotation(&mut pen, 180) - 180.0).abs() < 1e-3);
    assert!((rotation(&mut pen, 359) - 359.0).abs() < 1e-3);
}
