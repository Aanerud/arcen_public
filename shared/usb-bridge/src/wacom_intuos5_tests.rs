// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The emulated Intuos5 against its descriptor fixture, against the Linux
//! `wacom` driver's packet layout (decoded here independently, straight from
//! the driver's formulas), and through the shared exporter as a host would
//! enumerate it, with the host's descriptor guard checking every answer.

use super::*;
use crate::{
    AttachmentGeneration, BridgedDeviceGuard, ExportedCompletion, ReleaseReason, SyntheticDevice,
    SyntheticHandle, TransferKind, UrbId, UrbStatus, UrbSubmission, UsbExporter,
    verify_captured_tablet,
};
use std::num::{NonZeroU32, NonZeroU64};

const WAIT: Duration = Duration::from_secs(2);
const NOTHING: Duration = Duration::from_millis(30);

/// One pen packet as `wacom_intuos_inout` / `wacom_intuos_general` read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxView {
    Enter {
        id: u32,
        serial: u32,
    },
    Exit,
    /// "In range": while reporting, the driver releases the tip.
    InRange,
    Pen {
        x: u32,
        y: u32,
        distance: u32,
        pressure: u32,
        tilt_x: i32,
        tilt_y: i32,
        stylus: bool,
        stylus2: bool,
        touch: bool,
    },
    Ignored,
}

fn linux_view(data: &[u8]) -> LinuxView {
    assert_eq!(data.len(), 10);
    let d = |index: usize| u32::from(data[index]);
    if data[0] != 2 {
        return LinuxView::Ignored;
    }
    if data[1] & 0xfc == 0xc0 {
        return LinuxView::Enter {
            serial: ((d(3) & 0x0f) << 28) + (d(4) << 20) + (d(5) << 12) + (d(6) << 4) + (d(7) >> 4),
            id: (d(2) << 4) | (d(3) >> 4) | ((d(7) & 0x0f) << 16) | ((d(8) & 0xf0) << 8),
        };
    }
    if data[1] & 0xfe == 0x20 {
        return LinuxView::InRange;
    }
    if data[1] & 0xfe == 0x80 {
        return LinuxView::Exit;
    }
    if (data[1] >> 1) & 0x0f > 3 {
        return LinuxView::Ignored;
    }
    let pressure = (d(6) << 3) | ((d(7) & 0xc0) >> 5) | (d(1) & 1);
    LinuxView::Pen {
        x: (((d(2) << 8) | d(3)) << 1) | ((d(9) >> 1) & 1),
        y: (((d(4) << 8) | d(5)) << 1) | (d(9) & 1),
        distance: d(9) >> 2,
        pressure,
        tilt_x: i32::try_from(((d(7) << 1) & 0x7e) | (d(8) >> 7)).unwrap() - 64,
        tilt_y: i32::try_from(d(8) & 0x7f).unwrap() - 64,
        stylus: data[1] & 2 != 0,
        stylus2: data[1] & 4 != 0,
        touch: pressure > 10,
    }
}

#[test]
fn the_fixture_is_the_tablet_the_linux_driver_expects() {
    let facts = WacomIntuos5::new().captured_facts().unwrap();
    assert_eq!(
        facts.snapshot.id,
        UsbDeviceId {
            vendor_id: WACOM_VENDOR_ID,
            product_id: INTUOS5_TOUCH_L_PRODUCT_ID,
            bcd_device: 0x0100,
        }
    );
    assert_eq!(facts.speed, UsbSpeed::Full);
    // iManufacturer 1, iProduct 2, no serial string, one configuration — as
    // the host logged the real tablet.
    assert_eq!(&INTUOS5_DEVICE_DESCRIPTOR[14..], &[1, 2, 0, 1]);
    let interfaces = &facts.snapshot.configuration.interfaces;
    assert_eq!(interfaces.len(), 3);
    let endpoints: Vec<u8> = interfaces
        .iter()
        .flat_map(|interface| {
            interface
                .endpoints
                .iter()
                .map(|endpoint| endpoint.address.0)
        })
        .collect();
    assert_eq!(endpoints, [0x81, 0x82, 0x83]);
    for interface in 0..3 {
        let hid = hid_class_descriptor(interface).unwrap();
        let advertised = usize::from(u16::from_le_bytes([hid[7], hid[8]]));
        assert_eq!(
            advertised,
            WacomIntuos5::report_descriptor(interface).unwrap().len()
        );
    }

    // `wacom_compute_pktlen`: the longest input report of an interface. Ten
    // bytes is an Intuos pen interface; 64 (`WACOM_PKGLEN_BBTOUCH3`) makes
    // the driver treat interface 1 as the touch sensor.
    let pen_inputs = declared_report_lengths(INTUOS5_PEN_REPORT_DESCRIPTOR, HidReportKind::Input);
    assert_eq!(pen_inputs.get(&INTUOS5_PEN_REPORT_ID), Some(&10));
    assert_eq!(pen_inputs.values().max(), Some(&INTUOS5_PEN_REPORT_LENGTH));
    let touch_inputs =
        declared_report_lengths(INTUOS5_TOUCH_REPORT_DESCRIPTOR, HidReportKind::Input);
    assert_eq!(touch_inputs.values().max(), Some(&64));
    // The mode switch writes and reads back a two-byte feature report 2.
    let features = declared_report_lengths(INTUOS5_PEN_REPORT_DESCRIPTOR, HidReportKind::Feature);
    assert_eq!(features.get(&INTUOS5_MODE_REPORT_ID), Some(&2));

    // The shared admission policy takes it exactly like a captured tablet.
    verify_captured_tablet(&facts).unwrap();
    let (device, _handle) = SyntheticDevice::emulate(WacomIntuos5::new()).unwrap();
    assert!(UsbExporter::new(Box::new(device)).is_ok());
}

#[test]
fn pen_packets_decode_as_the_linux_driver_reads_them() {
    let enter = Intuos5PenReport::Enter {
        tool_id: INTUOS5_GRIP_PEN_TOOL_ID,
        serial: 0xfedc_ba98,
    }
    .encode();
    assert_eq!(
        linux_view(&enter),
        LinuxView::Enter {
            id: INTUOS5_GRIP_PEN_TOOL_ID,
            serial: 0xfedc_ba98
        }
    );
    let wide_tool = Intuos5PenReport::Enter {
        tool_id: 0x8_d802,
        serial: 1,
    }
    .encode();
    assert_eq!(
        linux_view(&wide_tool),
        LinuxView::Enter {
            id: 0x8_d802,
            serial: 1
        }
    );
    assert_eq!(
        linux_view(&Intuos5PenReport::Exit.encode()),
        LinuxView::Exit
    );

    let cases = [
        (Intuos5PenState::default(), (0, 0, 0, 0, 0, 0)),
        (
            Intuos5PenState {
                x: INTUOS5_MAX_X - 1,
                y: INTUOS5_MAX_Y - 1,
                pressure: INTUOS5_MAX_PRESSURE,
                distance: INTUOS5_MAX_DISTANCE,
                tilt_x: 63,
                tilt_y: -64,
                stylus: true,
                stylus2: true,
            },
            (INTUOS5_MAX_X - 1, INTUOS5_MAX_Y - 1, 63, 2_047, 63, -64),
        ),
        (
            Intuos5PenState {
                x: 12_345,
                y: 6_789,
                pressure: 1_025,
                distance: 7,
                tilt_x: -17,
                tilt_y: 29,
                stylus: false,
                stylus2: true,
            },
            (12_345, 6_789, 7, 1_025, -17, 29),
        ),
        // Out of range values are clamped to what the driver accepts.
        (
            Intuos5PenState {
                x: u32::MAX,
                y: u32::MAX,
                pressure: u16::MAX,
                distance: u8::MAX,
                tilt_x: i8::MIN,
                tilt_y: i8::MAX,
                stylus: true,
                stylus2: false,
            },
            (INTUOS5_MAX_X, INTUOS5_MAX_Y, 63, 2_047, -64, 63),
        ),
    ];
    for (state, (x, y, distance, pressure, tilt_x, tilt_y)) in cases {
        let report = Intuos5PenReport::Pen(state).encode();
        assert_eq!(report.len(), INTUOS5_PEN_REPORT_LENGTH);
        assert_eq!(
            linux_view(&report),
            LinuxView::Pen {
                x,
                y,
                distance,
                pressure,
                tilt_x,
                tilt_y,
                stylus: state.stylus,
                stylus2: state.stylus2,
                touch: pressure > 10,
            },
            "{state:?}"
        );
    }
}

#[test]
fn the_scripted_stroke_is_a_complete_known_stroke() {
    let script = intuos5_scripted_stroke();
    let views: Vec<_> = script
        .iter()
        .map(|step| linux_view(&step.report.encode()))
        .collect();
    assert_eq!(
        views.first(),
        Some(&LinuxView::Enter {
            id: INTUOS5_GRIP_PEN_TOOL_ID,
            serial: SCRIPTED_PEN_SERIAL
        })
    );
    assert_eq!(views.last(), Some(&LinuxView::Exit));
    let pens: Vec<_> = views[1..views.len() - 1]
        .iter()
        .map(|view| match *view {
            LinuxView::Pen {
                x,
                y,
                distance,
                pressure,
                stylus,
                touch,
                ..
            } => (x, y, distance, pressure, stylus, touch),
            other => panic!("expected a pen packet, got {other:?}"),
        })
        .collect();

    // The barrel button is pressed and released while hovering.
    let pressed: Vec<_> = pens.iter().filter(|pen| pen.4).collect();
    assert_eq!(pressed.len(), 3);
    assert!(
        pressed
            .iter()
            .all(|pen| !pen.5 && pen.2 == u32::from(SCRIPTED_HOVER_DISTANCE))
    );

    // Touch-down at the start with a strictly rising ramp to the maximum.
    let first_touch = pens.iter().position(|pen| pen.5).unwrap();
    assert_eq!((pens[first_touch].0, pens[first_touch].1), SCRIPTED_START);
    let ramp: Vec<u32> = script
        .iter()
        .zip(&views)
        .filter(|(step, _)| step.phase == "pressure-ramp")
        .map(|(_, view)| match view {
            LinuxView::Pen { pressure, .. } => *pressure,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(ramp.len(), usize::from(SCRIPTED_RAMP_STEPS));
    assert!(ramp.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(ramp.last(), Some(&u32::from(INTUOS5_MAX_PRESSURE)));

    // The known pattern: through the corner to the end, touching all the way.
    let touching: Vec<_> = pens.iter().filter(|pen| pen.5).collect();
    assert!(touching.iter().any(|pen| (pen.0, pen.1) == SCRIPTED_CORNER));
    let last_touch = touching.last().unwrap();
    assert_eq!((last_touch.0, last_touch.1), SCRIPTED_END);
    assert!(
        touching
            .iter()
            .all(|pen| pen.2 == 0 && (pen.1 == SCRIPTED_START.1 || pen.0 == SCRIPTED_END.0))
    );

    // Lift-off: the pressure reaches zero before the pen leaves proximity.
    let after: Vec<_> = pens
        .iter()
        .skip_while(|pen| !pen.5)
        .skip_while(|pen| pen.5)
        .collect();
    assert!(after.iter().all(|pen| !pen.5));
    assert!(
        after
            .iter()
            .any(|pen| pen.3 == 0 && (pen.0, pen.1) == SCRIPTED_END)
    );
}

#[test]
fn the_landing_stroke_presses_in_its_first_pen_packet() {
    let views: Vec<_> = intuos5_landing_stroke()
        .iter()
        .map(|step| linux_view(&step.report.encode()))
        .collect();
    assert!(matches!(views.first(), Some(LinuxView::Enter { .. })));
    assert_eq!(views.last(), Some(&LinuxView::Exit));
    let pens: Vec<_> = views[1..views.len() - 1]
        .iter()
        .map(|view| match *view {
            LinuxView::Pen {
                x,
                y,
                distance,
                pressure,
                touch,
                ..
            } => (x, y, distance, pressure, touch),
            other => panic!("expected a pen packet, got {other:?}"),
        })
        .collect();
    assert_eq!(
        pens[0],
        (
            SCRIPTED_START.0,
            SCRIPTED_START.1,
            0,
            u32::from(SCRIPTED_MOVE_PRESSURE),
            true
        )
    );
    let lift = pens.iter().position(|pen| !pen.4).unwrap();
    assert!(
        pens[..lift]
            .iter()
            .any(|pen| (pen.0, pen.1) == SCRIPTED_CORNER)
    );
    assert_eq!((pens[lift - 1].0, pens[lift - 1].1), SCRIPTED_END);
    assert!(pens[lift..].iter().all(|pen| !pen.4 && pen.3 == 0));
}

#[test]
fn the_realistic_stroke_comes_in_range_lands_pressing_and_tilts() {
    let views: Vec<_> = intuos5_realistic_stroke()
        .iter()
        .map(|step| linux_view(&step.report.encode()))
        .collect();
    assert_eq!(&views[..2], &[LinuxView::InRange, LinuxView::InRange]);
    assert_eq!(
        views[2],
        LinuxView::Enter {
            id: INTUOS_PRO_GRIP_PEN_TOOL_ID,
            serial: SCRIPTED_PEN_SERIAL
        }
    );
    assert_eq!(
        &views[views.len() - 2..],
        &[LinuxView::InRange, LinuxView::Exit]
    );
    let pens: Vec<_> = views[3..views.len() - 2]
        .iter()
        .map(|view| match *view {
            LinuxView::Pen {
                x,
                y,
                distance,
                pressure,
                tilt_x,
                tilt_y,
                stylus,
                stylus2,
                touch,
            } => {
                assert!(!stylus && !stylus2, "no barrel button");
                assert_eq!((tilt_x, tilt_y), (25, -11));
                (x, y, distance, pressure, touch)
            }
            other => panic!("expected a pen packet, got {other:?}"),
        })
        .collect();
    let landing = pens.iter().position(|pen| pen.4).unwrap();
    assert!(landing > 0, "hovers before it lands");
    assert!(pens[..landing].iter().all(|pen| pen.3 == 0 && pen.2 > 0));
    // The landing packet already presses (one poll per round trip).
    let max = f64::from(INTUOS5_MAX_PRESSURE);
    assert!((f64::from(pens[landing].3) / max - 0.40).abs() < 0.01);
    let lift = landing + pens[landing..].iter().position(|pen| !pen.4).unwrap();
    let peak = pens[landing..lift].iter().map(|pen| pen.3).max().unwrap();
    assert!((f64::from(peak) / max - 0.78).abs() < 0.01);
    assert!(pens[lift..].iter().all(|pen| pen.3 == 0 && pen.2 > 0));
    // Every packet moves, as a hand does.
    assert!(
        pens.windows(2)
            .all(|pair| (pair[0].0, pair[0].1) != (pair[1].0, pair[1].1))
    );
}

#[test]
fn a_resting_stroke_ends_with_the_pen_still_down_or_hovering() {
    for pressing in [true, false] {
        let views: Vec<_> = intuos5_resting_stroke(pressing)
            .iter()
            .map(|step| linux_view(&step.report.encode()))
            .collect();
        assert!(matches!(views.first(), Some(LinuxView::Enter { .. })));
        assert!(!views.contains(&LinuxView::Exit), "never leaves proximity");
        assert!(
            views
                .iter()
                .any(|view| matches!(view, LinuxView::Pen { touch: true, .. }))
        );
        match views.last() {
            Some(LinuxView::Pen { touch, x, y, .. }) => {
                assert_eq!(*touch, pressing);
                assert_eq!((*x, *y), SCRIPTED_CORNER);
            }
            other => panic!("expected a pen packet last, got {other:?}"),
        }
    }
}

#[test]
fn the_upper_switch_is_held_in_every_pen_packet_and_nothing_else_changes() {
    let plain = intuos5_realistic_stroke();
    let held = with_upper_switch_held(plain.clone());
    assert_eq!(plain.len(), held.len());
    for (a, b) in plain.iter().zip(&held) {
        match (
            linux_view(&a.report.encode()),
            linux_view(&b.report.encode()),
        ) {
            (
                LinuxView::Pen { stylus2: false, .. },
                LinuxView::Pen {
                    stylus2,
                    x,
                    y,
                    pressure,
                    ..
                },
            ) => {
                assert!(stylus2);
                assert!(
                    matches!(linux_view(&a.report.encode()), LinuxView::Pen { x: ax, y: ay, pressure: ap, .. } if (ax, ay, ap) == (x, y, pressure))
                );
            }
            (left, right) => assert_eq!(left, right),
        }
    }
}

fn generation() -> AttachmentGeneration {
    AttachmentGeneration::new(NonZeroU64::new(7).unwrap())
}

fn urb(value: u32) -> UrbId {
    UrbId::new(NonZeroU32::new(value).unwrap())
}

fn control(id: u32, setup: SetupPacket) -> UrbSubmission {
    let direction_in = setup.request_type & 0x80 != 0;
    UrbSubmission {
        generation: generation(),
        urb_id: urb(id),
        endpoint: EndpointAddress(if direction_in { 0x80 } else { 0 }),
        transfer_kind: TransferKind::Control,
        timeout_ms: 1_000,
        declared_length: if direction_in {
            u32::from(setup.length)
        } else {
            0
        },
        setup: Some(setup),
    }
}

fn poll(id: u32, endpoint: EndpointAddress, length: u32) -> UrbSubmission {
    UrbSubmission {
        generation: generation(),
        urb_id: urb(id),
        endpoint,
        transfer_kind: TransferKind::Interrupt,
        timeout_ms: 1_000,
        declared_length: length,
        setup: None,
    }
}

const fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> SetupPacket {
    SetupPacket {
        request_type,
        request,
        value,
        index,
        length,
    }
}

/// A host's side of the bridge: submits through the exporter, and checks every
/// descriptor answer with the guard the Linux Pier applies.
struct Host {
    exporter: UsbExporter,
    guard: BridgedDeviceGuard,
    next: u32,
}

impl Host {
    fn new() -> (Self, SyntheticHandle<WacomIntuos5>) {
        let (device, handle) = SyntheticDevice::emulate(WacomIntuos5::new()).unwrap();
        let exporter = UsbExporter::new(Box::new(device)).unwrap();
        let facts = exporter.facts().clone();
        let guard = BridgedDeviceGuard::new(facts.snapshot.id, facts.report_descriptors);
        (
            Self {
                exporter,
                guard,
                next: 1,
            },
            handle,
        )
    }

    fn request(&mut self, setup: SetupPacket, payload: &[u8]) -> ExportedCompletion {
        let id = self.next;
        self.next += 1;
        self.guard.observe_submit(urb(id), Some(setup)).unwrap();
        self.exporter.submit(control(id, setup), payload).unwrap();
        let completion = self.exporter.next_completion_within(WAIT).unwrap().unwrap();
        assert_eq!(completion.urb_id, urb(id));
        self.guard
            .check_completion(
                urb(id),
                completion.status == UrbStatus::Success,
                &completion.data,
            )
            .unwrap();
        completion
    }

    fn data(&mut self, setup: SetupPacket) -> Vec<u8> {
        let completion = self.request(setup, &[]);
        assert_eq!(completion.status, UrbStatus::Success, "{setup:?}");
        completion.data
    }

    fn status(&mut self, setup: SetupPacket, payload: &[u8]) -> UrbStatus {
        self.request(setup, payload).status
    }
}

/// The USB core's enumeration and `usbhid`'s per-interface setup.
fn enumerate_like_linux(host: &mut Host) {
    assert_eq!(
        host.data(setup(0x80, 6, 0x0100, 0, 8)),
        &INTUOS5_DEVICE_DESCRIPTOR[..8]
    );
    assert_eq!(
        host.status(setup(0x00, 5, 9, 0, 0), &[]),
        UrbStatus::Success
    );
    assert_eq!(
        host.data(setup(0x80, 6, 0x0100, 0, 18)),
        INTUOS5_DEVICE_DESCRIPTOR
    );
    assert_eq!(
        host.data(setup(0x80, 6, 0x0200, 0, 9)),
        &INTUOS5_CONFIGURATION_DESCRIPTOR[..9]
    );
    assert_eq!(
        host.data(setup(0x80, 6, 0x0200, 0, 84)),
        INTUOS5_CONFIGURATION_DESCRIPTOR
    );
    assert_eq!(
        host.data(setup(0x80, 6, 0x0300, 0, 255)),
        [4, 3, 0x09, 0x04]
    );
    let product = host.data(setup(0x80, 6, 0x0302, 0x0409, 255));
    let product: Vec<u16> = product[2..]
        .chunks(2)
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .collect();
    assert_eq!(String::from_utf16(&product).unwrap(), "Intuos5 touch L");
    // A full-speed USB 2.0 device has no device qualifier.
    assert_eq!(
        host.status(setup(0x80, 6, 0x0600, 0, 10), &[]),
        UrbStatus::Stall
    );
    assert_eq!(
        host.status(setup(0x00, 9, 1, 0, 0), &[]),
        UrbStatus::Success
    );

    for interface in 0..3_u16 {
        assert_eq!(
            host.status(setup(0x21, 0x0a, 0, interface, 0), &[]),
            UrbStatus::Success
        );
        let descriptor = WacomIntuos5::report_descriptor(u8::try_from(interface).unwrap()).unwrap();
        let length = u16::try_from(descriptor.len()).unwrap();
        assert_eq!(
            host.data(setup(0x81, 6, 0x2200, interface, length)),
            descriptor
        );
    }
}

/// What the Linux USB core, `usbhid` and the `wacom` driver send while
/// binding the tablet, then the pen's first reports, answered through the
/// shared exporter exactly as a captured device would answer them.
#[test]
fn a_linux_host_enumerates_switches_mode_and_reads_pen_reports() {
    let (mut host, handle) = Host::new();
    enumerate_like_linux(&mut host);

    // `wacom_set_device_mode`: SET_REPORT(Feature 2) = [2, 2], then
    // GET_REPORT until the tablet echoes the mode.
    assert!(!handle.inspect(WacomIntuos5::wacom_mode));
    assert_eq!(host.data(setup(0xa1, 0x01, 0x0302, 0, 2)), [2, 0]);
    assert_eq!(
        host.status(setup(0x21, 0x09, 0x0302, 0, 2), &[2, 2]),
        UrbStatus::Success
    );
    assert!(handle.inspect(WacomIntuos5::wacom_mode));
    assert_eq!(host.data(setup(0xa1, 0x01, 0x0302, 0, 2)), [2, 2]);
    // LED control (`WAC_CMD_LED_CONTROL`, report 0x20) is acknowledged and
    // read back like any feature report the host sets.
    let led = [0x20, 0x04, 0x7f, 0x7f, 0, 0, 0, 0, 0];
    assert_eq!(
        host.status(setup(0x21, 0x09, 0x0320, 0, 9), &led),
        UrbStatus::Success
    );
    assert_eq!(host.data(setup(0xa1, 0x01, 0x0320, 0, 9)), led);
    // A report whose first byte contradicts its id, an interface the device
    // lacks, and a report never declared nor set all stall.
    assert_eq!(
        host.status(setup(0x21, 0x09, 0x0302, 0, 2), &[3, 2]),
        UrbStatus::Stall
    );
    assert_eq!(
        host.status(setup(0x21, 0x09, 0x0302, 5, 2), &[2, 2]),
        UrbStatus::Stall
    );
    assert_eq!(
        host.status(setup(0xa1, 0x01, 0x0377, 0, 9), &[]),
        UrbStatus::Stall
    );
    assert_eq!(handle.inspect(WacomIntuos5::set_reports), 2);

    // Pen reports go to the pen endpoint's poll; the touch poll stays held.
    host.exporter
        .submit(poll(100, INTUOS5_TOUCH_IN, 64), &[])
        .unwrap();
    host.exporter
        .submit(poll(101, INTUOS5_PEN_IN, 10), &[])
        .unwrap();
    let script = intuos5_scripted_stroke();
    let report = |index: usize| script[index].report.encode().to_vec();
    assert!(handle.deliver_when_polled(INTUOS5_PEN_IN, report(0), WAIT));
    let first = host.exporter.next_completion_within(WAIT).unwrap().unwrap();
    assert_eq!(first.urb_id, urb(101));
    assert_eq!(first.data, report(0));
    // Nobody polls the pen endpoint now: the next report waits, then times out.
    assert!(!handle.deliver_when_polled(INTUOS5_PEN_IN, report(1), NOTHING));
    assert_eq!(handle.held_on(INTUOS5_TOUCH_IN), 1);
    assert_eq!(host.exporter.next_completion_within(NOTHING).unwrap(), None);
    host.exporter
        .submit(poll(102, INTUOS5_PEN_IN, 10), &[])
        .unwrap();
    assert!(handle.deliver_when_polled(INTUOS5_PEN_IN, report(1), WAIT));
    let second = host.exporter.next_completion_within(WAIT).unwrap().unwrap();
    assert_eq!(second.urb_id, urb(102));
    assert_eq!(linux_view(&second.data), linux_view(&report(1)));

    // A released device takes no more reports.
    let _ = host.exporter.release(ReleaseReason::SessionEnded);
    assert!(!handle.deliver_when_polled(INTUOS5_PEN_IN, report(2), NOTHING));
}

/// The HID-level calls a `uhid` presentation uses answer like the USB path.
#[test]
fn hid_level_reports_match_the_usb_answers() {
    let mut tablet = WacomIntuos5::new();
    assert_eq!(WacomIntuos5::report_descriptor(3), None);
    assert_eq!(
        tablet.get_report(0, HidReportKind::Feature, INTUOS5_MODE_REPORT_ID),
        Some(vec![2, 0])
    );
    assert_eq!(
        tablet
            .get_report(1, HidReportKind::Feature, 0x12)
            .map(|report| report.len()),
        Some(64)
    );
    assert_eq!(tablet.get_report(2, HidReportKind::Feature, 0), None);
    assert!(tablet.set_report(0, HidReportKind::Feature, 2, &[2, 2]));
    assert!(tablet.wacom_mode());
    assert_eq!(
        tablet.control(setup(0xa1, 0x01, 0x0302, 0, 2), &[]),
        ControlResponse::Data(vec![2, 2])
    );
    assert_eq!(
        tablet.control(setup(0xa1, 0x01, 0x0302, 0, 1), &[]),
        ControlResponse::Data(vec![2])
    );
    assert_eq!(
        tablet.control(setup(0xa1, 0x03, 0, 2, 1), &[]),
        ControlResponse::Data(vec![1])
    );
    assert_eq!(
        tablet.control(setup(0x21, 0x0b, 0, 2, 0), &[]),
        ControlResponse::Ack
    );
    assert_eq!(
        tablet.control(setup(0xa1, 0x03, 0, 2, 1), &[]),
        ControlResponse::Data(vec![0])
    );
    assert_eq!(
        tablet.control(setup(0x21, 0x0a, 0x0400, 9, 0), &[]),
        ControlResponse::Stall
    );
    assert_eq!(
        tablet.control(setup(0xc0, 0x01, 0, 0, 4), &[]),
        ControlResponse::Stall
    );
}
