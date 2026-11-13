// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
use super::*;
use crate::wacom_intuos5::{
    INTUOS5_GRIP_PEN_TOOL_ID, Intuos5PenReport, SCRIPTED_PEN_SERIAL, intuos5_scripted_stroke,
};

fn intuos5() -> IntuosPenModel {
    host_decoded_intuos_model(WACOM_VENDOR_ID, INTUOS5_TOUCH_L_PRODUCT_ID).expect("Intuos5 L")
}

#[test]
fn every_packet_the_emulated_tablet_sends_reads_back_exactly() {
    let states = [
        Intuos5PenState::default(),
        Intuos5PenState {
            x: INTUOS5_MAX_X,
            y: INTUOS5_MAX_Y,
            pressure: INTUOS5_MAX_PRESSURE,
            distance: 63,
            tilt_x: 63,
            tilt_y: -64,
            stylus: true,
            stylus2: true,
        },
        Intuos5PenState {
            x: 12_345,
            y: 6_789,
            pressure: 1_023,
            distance: 0,
            tilt_x: -17,
            tilt_y: 5,
            stylus: false,
            stylus2: true,
        },
    ];
    for state in states {
        let report = Intuos5PenReport::Pen(state).encode();
        assert_eq!(
            decode_intuos_pen_report(&report),
            Some(IntuosPenPacket::Pen(state))
        );
    }
    let enter = Intuos5PenReport::Enter {
        tool_id: 0x0008_080a,
        serial: SCRIPTED_PEN_SERIAL,
    };
    assert_eq!(
        decode_intuos_pen_report(&enter.encode()),
        Some(IntuosPenPacket::Enter {
            tool_id: 0x0008_080a,
            serial: SCRIPTED_PEN_SERIAL
        })
    );
    assert_eq!(
        decode_intuos_pen_report(&Intuos5PenReport::Exit.encode()),
        Some(IntuosPenPacket::Exit)
    );
}

#[test]
fn reports_that_are_not_pen_packets_are_left_alone() {
    let mut mouse_mode = Intuos5PenReport::Exit.encode();
    mouse_mode[0] = 1;
    assert_eq!(decode_intuos_pen_report(&mouse_mode), None);
    assert_eq!(decode_intuos_pen_report(&[2, 0xe0, 0, 0]), None);
    let mut puck = Intuos5PenReport::Pen(Intuos5PenState::default()).encode();
    puck[1] = 0xa8;
    assert_eq!(
        decode_intuos_pen_report(&puck),
        Some(IntuosPenPacket::Other)
    );
    let mut reader = IntuosPenReader::new(intuos5());
    assert_eq!(reader.read(&mouse_mode), None);
    assert_eq!(reader.read(&puck), None);
    assert_eq!(reader.counts(), (1, 2));
}

#[test]
fn only_known_models_are_read_by_the_host() {
    assert!(host_decoded_intuos_model(WACOM_VENDOR_ID, 0x0001).is_none());
    assert!(host_decoded_intuos_model(0x1234, INTUOS5_TOUCH_L_PRODUCT_ID).is_none());
    assert_eq!(INTUOS_MODE_SWITCH, (2, [2, 2]));
}

#[test]
fn the_scripted_stroke_becomes_hover_pressure_and_a_clean_exit() {
    let mut reader = IntuosPenReader::new(intuos5());
    let samples: Vec<(&str, HostPenSample)> = intuos5_scripted_stroke()
        .into_iter()
        .filter_map(|step| {
            reader
                .read(&step.report.encode())
                .map(|sample| (step.phase, sample))
        })
        .collect();
    let first = samples.first().expect("samples").1;
    assert!(first.in_proximity && !first.touching && !first.eraser);
    assert!((first.x - 16_000.0 / f64::from(INTUOS5_MAX_X)).abs() < 1e-4);
    let barrel = samples
        .iter()
        .find(|(phase, _)| *phase == "barrel-down")
        .expect("barrel")
        .1;
    assert_eq!(barrel.barrel_buttons, 1);
    let ramp: Vec<f32> = samples
        .iter()
        .filter(|(phase, _)| *phase == "pressure-ramp")
        .map(|(_, sample)| sample.pressure)
        .collect();
    assert_eq!(ramp.len(), 16);
    assert!(ramp.windows(2).all(|pair| pair[0] < pair[1]));
    assert!((ramp[15] - 1.0).abs() < f32::EPSILON);
    let moving = samples
        .iter()
        .find(|(phase, _)| *phase == "move-right")
        .expect("move")
        .1;
    assert!(moving.touching);
    assert!((moving.pressure - 1_024.0 / 2_047.0).abs() < 1e-3);
    let last = samples.last().expect("exit").1;
    assert!(!last.in_proximity && !last.touching);
    assert!(last.pressure.abs() < f32::EPSILON);
    assert!((last.x - 48_000.0 / f64::from(INTUOS5_MAX_X)).abs() < 1e-4);
}

#[test]
fn the_eraser_end_is_its_own_tool_until_it_leaves() {
    let mut reader = IntuosPenReader::new(intuos5());
    let enter = |tool_id| Intuos5PenReport::Enter { tool_id, serial: 1 }.encode();
    let pen = Intuos5PenReport::Pen(Intuos5PenState {
        pressure: 500,
        ..Intuos5PenState::default()
    })
    .encode();
    assert_eq!(reader.read(&enter(0x080a)), None);
    assert!(reader.read(&pen).expect("eraser").eraser);
    assert!(
        reader
            .read(&Intuos5PenReport::Exit.encode())
            .expect("exit")
            .eraser
    );
    assert_eq!(reader.read(&enter(INTUOS5_GRIP_PEN_TOOL_ID)), None);
    assert!(!reader.read(&pen).expect("tip").eraser);
}

#[test]
fn an_exit_with_no_position_yet_produces_nothing() {
    let mut reader = IntuosPenReader::new(intuos5());
    assert_eq!(reader.read(&Intuos5PenReport::Exit.encode()), None);
}

/// Wire bytes written out by hand from the Intuos5 packet layout (the Linux
/// `wacom` driver's `wacom_intuos_general`/`wacom_intuos_inout`), not from
/// the emulator's encoder, so an encoder fault cannot hide a decoder fault.
#[test]
fn literal_intuos5_packets_decode_to_the_documented_fields() {
    let pen = [0x02, 0xe0, 0x12, 0x34, 0x0a, 0xbc, 0x80, 0x20, 0x40, 0x2a];
    assert_eq!(
        decode_intuos_pen_report(&pen),
        Some(IntuosPenPacket::Pen(Intuos5PenState {
            x: 0x2469,
            y: 0x1578,
            pressure: 1_024,
            distance: 10,
            tilt_x: 0,
            tilt_y: 0,
            stylus: false,
            stylus2: false,
        }))
    );
    // Both barrel buttons, the pressure LSB in the status byte, tilt
    // (+20, -30) around the 64 bias.
    let tilted = [0x02, 0xe7, 0x12, 0x34, 0x0a, 0xbc, 0xff, 0xea, 0x22, 0x00];
    let Some(IntuosPenPacket::Pen(state)) = decode_intuos_pen_report(&tilted) else {
        panic!("a pen packet");
    };
    assert!(state.stylus && state.stylus2);
    assert_eq!(state.pressure, INTUOS5_MAX_PRESSURE);
    assert_eq!((state.tilt_x, state.tilt_y), (20, -30));
    let mut reader = IntuosPenReader::new(intuos5());
    assert_eq!(
        reader.read(&[0x02, 0xc2, 0x80, 0x20, 0, 0, 0, 0, 0, 0]),
        None
    );
    let tip = reader.read(&tilted).expect("sample");
    assert!(!tip.eraser && tip.touching && tip.in_proximity);
    assert_eq!(tip.barrel_buttons, 0b11);
    assert!((tip.pressure - 1.0).abs() < f32::EPSILON);
    assert!((tip.tilt_x_degrees - 20.0).abs() < f32::EPSILON);
    let gone = reader
        .read(&[0x02, 0x80, 0, 0, 0, 0, 0, 0, 0, 0])
        .expect("exit");
    assert!(!gone.in_proximity && !gone.touching && gone.barrel_buttons == 0);
    // The eraser end of the same pen (tool id 0x80a).
    assert_eq!(
        reader.read(&[0x02, 0xc2, 0x80, 0xa0, 0, 0, 0, 0, 0, 0]),
        None
    );
    assert!(reader.read(&pen).expect("eraser").eraser);
}

/// Any bytes at all, of any length: no panic, and every sample stays in
/// the documented ranges.
#[test]
fn malformed_and_arbitrary_reports_never_panic_or_leave_the_ranges() {
    let mut reader = IntuosPenReader::new(intuos5());
    let mut seed = 0x2545_f491_u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed.to_le_bytes()[0]
    };
    for length in 0..=64_usize {
        for status in 0..=255_u8 {
            let mut report: Vec<u8> = (0..length).map(|_| next()).collect();
            if let Some(first) = report.first_mut() {
                *first = INTUOS5_PEN_REPORT_ID;
            }
            if let Some(second) = report.get_mut(1) {
                *second = status;
            }
            let _ = decode_intuos_pen_report(&report);
            if let Some(sample) = reader.read(&report) {
                assert!((0.0..=1.0).contains(&sample.x));
                assert!((0.0..=1.0).contains(&sample.y));
                assert!((0.0..=1.0).contains(&sample.pressure));
                assert!((-64.0..=63.0).contains(&sample.tilt_x_degrees));
                assert!((-64.0..=63.0).contains(&sample.tilt_y_degrees));
                assert!(sample.barrel_buttons <= 0b11);
                assert!(!sample.touching || sample.in_proximity);
            }
            if length < INTUOS5_PEN_REPORT_LENGTH {
                assert_eq!(decode_intuos_pen_report(&report), None);
            }
        }
    }
}

#[test]
fn erasers_are_known_by_bit_three_of_the_tool_id_as_the_linux_driver_knows_them() {
    for eraser in [0x080a, 0x080c, 0x1_080c, 0x082b, 0x081b, 0x091b, 0x0_085a] {
        assert!(intuos_tool_is_eraser(eraser), "{eraser:#x}");
    }
    for tip in [
        INTUOS5_GRIP_PEN_TOOL_ID,
        0x0802,
        0x0804,
        0x0812,
        0x1_2802,
        0x0832,
        0x0902,
        0x1_0902,
        0x0912,
        0x0806,
        0x009c,
        0x0096,
    ] {
        assert!(!intuos_tool_is_eraser(tip), "{tip:#x}");
    }
}
