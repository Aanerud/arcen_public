// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use super::*;
use arcen_protocol::messages::UsbHidReportDescriptorMsg;
use arcen_usb_bridge::descriptor_fixtures::{
    GAOMON_STYLE_PEN, GENERIC_PEN_FIXTURES, HUION_STYLE_PEN, XP_PEN_STYLE_COMPOSITE,
};
use arcen_usb_bridge::{
    EmulatedUsbFunction, GENERIC_PEN_IN, GenericPenState, GenericPenTablet, INTUOS5_PEN_IN,
    SyntheticDevice, SyntheticHandle, UrbSubmission, UsbExporter, WacomIntuos5,
};
use std::num::NonZeroU64;

const NOTICES: HostPenNotices = HostPenNotices {
    dedicated: "dedicated reader",
    generic: "generic reader",
};

fn intuos5() -> UsbHardDeviceMsg {
    UsbHardDeviceMsg {
        vendor_id: 0x056a,
        product_id: 0x0317,
        bcd_device: 0x0100,
        device_class: 0,
        speed: arcen_usb_bridge::UsbSpeed::Full,
        hid_report_descriptors: (0..3)
            .map(|interface| {
                UsbHidReportDescriptorMsg::new(
                    interface,
                    WacomIntuos5::report_descriptor(interface).unwrap(),
                )
            })
            .collect(),
    }
}

fn generic(vendor_id: u16, descriptor: &[u8]) -> UsbHardDeviceMsg {
    UsbHardDeviceMsg {
        vendor_id,
        product_id: 0x0042,
        hid_report_descriptors: vec![UsbHidReportDescriptorMsg::new(0, descriptor)],
        ..intuos5()
    }
}

fn requested_native(accepted: bool) -> TabletModeResultMsg {
    TabletModeResultMsg {
        requested: TabletModeMsg::UsbTabletBridge,
        active: if accepted {
            TabletModeMsg::UsbTabletBridge
        } else {
            TabletModeMsg::DisabledMouseCompat
        },
        accepted,
        ..TabletModeResultMsg::default()
    }
}

#[test]
fn the_deck_device_is_authorized_by_what_it_is_not_by_its_vendor() {
    for policy in [
        HidPublishPolicy::PenAndVendorOnly,
        HidPublishPolicy::Nothing,
    ] {
        let tablet = authorize(Some(&intuos5()), policy, false).unwrap();
        assert_eq!(tablet.identity.vendor_id, 0x056a);
        assert_eq!(tablet.report_descriptors.len(), 3);
        assert_eq!(tablet.pen_interface, 0);
        // A keyboard claiming Wacom's vendor id is not a tablet.
        let keyboard = UsbHardDeviceMsg {
            hid_report_descriptors: vec![UsbHidReportDescriptorMsg::new(
                0,
                &[
                    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15,
                    0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xC0,
                ],
            )],
            ..intuos5()
        };
        assert!(
            authorize(Some(&keyboard), policy, false)
                .unwrap_err()
                .contains("refused")
        );
        assert!(authorize(None, policy, false).is_err());
        let legacy = UsbHardDeviceMsg {
            hid_report_descriptors: Vec::new(),
            ..intuos5()
        };
        assert!(authorize(Some(&legacy), policy, false).is_err());
        let storage = UsbHardDeviceMsg {
            device_class: 0x08,
            ..intuos5()
        };
        assert!(authorize(Some(&storage), policy, false).is_err());
    }
}

#[test]
fn a_host_that_publishes_nothing_withholds_every_interface_and_still_reads_the_pen() {
    let tablet = authorize(Some(&intuos5()), HidPublishPolicy::Nothing, false).unwrap();
    assert!(tablet.published.is_empty());
    assert_eq!(
        tablet.withheld,
        (0..3)
            .map(|interface| (interface, PublishRefusal::HostPublishesNone))
            .collect::<Vec<_>>()
    );
    for (label, descriptor) in GENERIC_PEN_FIXTURES {
        let tablet = authorize(
            Some(&generic(0x256c, descriptor)),
            HidPublishPolicy::Nothing,
            false,
        )
        .unwrap_or_else(|why| panic!("{label}: {why}"));
        assert_eq!(tablet.pen_interface, 0, "{label}");
        assert_eq!(tablet.host_pen.kind(), "generic", "{label}");
    }
    let composite = authorize(
        Some(&generic(0x28bd, XP_PEN_STYLE_COMPOSITE)),
        HidPublishPolicy::Nothing,
        false,
    )
    .unwrap();
    assert_eq!(composite.pen_interface, 0);
}

#[test]
fn any_tablet_is_read_and_a_known_model_by_its_dedicated_reader() {
    let policy = HidPublishPolicy::PenAndVendorOnly;
    let tablet = authorize(Some(&intuos5()), policy, false).unwrap();
    assert!(
        matches!(tablet.host_pen, HostPenRoute::Dedicated(model) if model.product_id == 0x0317)
    );
    let huion = authorize(Some(&generic(0x256c, HUION_STYLE_PEN)), policy, false).unwrap();
    assert_eq!(
        huion.published,
        vec![0],
        "a pen-only interface is published"
    );
    let composite = authorize(
        Some(&generic(0x28bd, XP_PEN_STYLE_COMPOSITE)),
        policy,
        false,
    )
    .unwrap();
    assert!(composite.published.is_empty());
    assert_eq!(composite.host_pen.generic_interface(), Some(0));
}

#[test]
fn only_a_pen_in_a_vendor_format_no_reader_knows_is_refused_and_says_so() {
    let unknown = UsbHardDeviceMsg {
        product_id: 0x0001,
        ..intuos5()
    };
    let refusal = authorize(Some(&unknown), HidPublishPolicy::Nothing, false).unwrap_err();
    assert!(refusal.contains("056a:0001") && refusal.contains("Tablet support"));
    assert!(refusal.contains("vendor format"));
    let result = tablet_result(
        requested_native(true),
        &Ok(()),
        Some(&Err(refusal)),
        NOTICES,
    );
    assert!(!result.accepted);
    assert!(String::from(result.reason).contains("056a:0001"));
}

#[test]
fn the_synthetic_lab_tablet_needs_the_host_to_enable_it() {
    let lab = UsbHardDeviceMsg {
        vendor_id: arcen_usb_bridge::ARCEN_LAB_VENDOR_ID,
        product_id: arcen_usb_bridge::ARCEN_LAB_PRODUCT_ID,
        ..intuos5()
    };
    assert!(
        authorize(Some(&lab), HidPublishPolicy::Nothing, false)
            .unwrap_err()
            .contains("synthetic")
    );
}

#[test]
fn the_deck_is_told_exactly_why_and_what_it_will_get() {
    let unavailable = tablet_result(
        requested_native(false),
        &Err("no pen device".to_owned()),
        None,
        NOTICES,
    );
    assert!(String::from(unavailable.reason).contains("unavailable: no pen device"));
    let refused = tablet_result(
        requested_native(true),
        &Ok(()),
        Some(&Err("not a pen tablet".to_owned())),
        NOTICES,
    );
    assert!(!refused.accepted && refused.reconnect_required);
    assert_eq!(refused.active, TabletModeMsg::DisabledMouseCompat);
    assert!(String::from(refused.reason).contains("not a pen tablet"));
    let tablet = authorize(Some(&intuos5()), HidPublishPolicy::Nothing, false).unwrap();
    let accepted = tablet_result(requested_native(true), &Ok(()), Some(&Ok(tablet)), NOTICES);
    assert!(accepted.accepted && !accepted.reconnect_required);
    assert_eq!(String::from(accepted.reason), NOTICES.dedicated);
    let pen = authorize(
        Some(&generic(0x256c, HUION_STYLE_PEN)),
        HidPublishPolicy::Nothing,
        false,
    )
    .unwrap();
    let accepted = tablet_result(requested_native(true), &Ok(()), Some(&Ok(pen)), NOTICES);
    assert_eq!(String::from(accepted.reason), NOTICES.generic);
    let typed = TabletModeResultMsg {
        requested: TabletModeMsg::LocalTermination,
        ..TabletModeResultMsg::default()
    };
    assert_eq!(
        tablet_result(typed.clone(), &Err("x".to_owned()), None, NOTICES),
        typed
    );
}

/// A pen read on the host claims only what its reader decodes and the
/// host's pen device carries: a generic pen without tilt or eraser fields
/// gets neither, a model reader gets its full set, and no host pen device
/// means no pen at all.
#[test]
fn a_host_decoded_pen_claims_only_what_is_decoded_and_carried() {
    use crate::pen_emulation::PenCapabilityTruth;
    let available = CapabilityAvailability::Available;
    let unavailable = CapabilityAvailability::Unavailable;
    let linux = PenCapabilityTruth::LINUX_UINPUT_TABLET;
    let gaomon = authorize(
        Some(&generic(0x256c, GAOMON_STYLE_PEN)),
        HidPublishPolicy::Nothing,
        false,
    )
    .unwrap();
    let truth = host_decoded_capabilities(&gaomon, linux);
    assert_eq!(truth.pen, available);
    assert_eq!(truth.pressure, available);
    assert_eq!(truth.tilt, unavailable, "no tilt field is declared");
    assert_eq!(truth.eraser, unavailable, "no eraser or invert field");
    assert_eq!(truth.proximity, available, "In Range is declared");
    assert_eq!(truth.barrel_buttons, available);
    assert_eq!(truth.rotation, unavailable);
    assert_eq!(
        pen_feature_words(truth),
        ["pressure", "hover", "barrel buttons"]
    );
    let notice = host_decoded_notice(&gaomon, truth, "Linux Pier");
    assert!(notice.contains("from its HID reports"), "{notice}");
    assert!(
        notice.contains("pressure, hover and barrel buttons."),
        "{notice}"
    );
    assert!(
        !notice.contains("tilt") && !notice.contains("eraser"),
        "{notice}"
    );
    assert!(notice.contains("Pad buttons, touch"), "{notice}");
    let intuos = authorize(Some(&intuos5()), HidPublishPolicy::Nothing, false).unwrap();
    let truth = host_decoded_capabilities(&intuos, linux);
    assert_eq!(
        pen_feature_words(truth),
        ["pressure", "tilt", "eraser", "hover", "barrel buttons"],
        "the device carries no rotation"
    );
    let notice = host_decoded_notice(&intuos, truth, "Linux Pier");
    assert!(notice.contains("with its model's reader"), "{notice}");
    assert!(notice.len() <= MAX_TABLET_MODE_REASON_BYTES);
    // Every axis a host could carry still fits the wire's reason bound.
    let every = PenCapabilityTruth {
        rotation: available,
        ..linux
    };
    let huion = authorize(
        Some(&generic(0x256c, HUION_STYLE_PEN)),
        HidPublishPolicy::Nothing,
        false,
    )
    .unwrap();
    assert!(host_decoded_notice(&huion, every, "Linux Pier").len() <= MAX_TABLET_MODE_REASON_BYTES);
    assert_eq!(
        host_decoded_capabilities(&intuos, PenCapabilityTruth::UNAVAILABLE),
        PenCapabilityTruth::UNAVAILABLE
    );
    assert!(
        host_decoded_notice(&intuos, PenCapabilityTruth::UNAVAILABLE, "Linux Pier")
            .contains("position and tip only")
    );
}

/// A joined desktop's host-decoded result: an unknown model is refused with
/// mouse compatibility and its reason, an accepted pen says what it gets.
#[test]
fn a_host_decoded_result_refuses_an_unknown_model_and_names_what_an_accepted_pen_gets() {
    use crate::pen_emulation::PenCapabilityTruth;
    let device = PenCapabilityTruth::LINUX_UINPUT_TABLET;
    let unknown = UsbHardDeviceMsg {
        product_id: 0x0001,
        ..intuos5()
    };
    let refused = authorize(Some(&unknown), HidPublishPolicy::Nothing, false);
    let result =
        host_decoded_tablet_result(requested_native(true), Some(&refused), device, "Linux Pier");
    assert!(!result.accepted && result.reconnect_required);
    assert_eq!(result.active, TabletModeMsg::DisabledMouseCompat);
    let reason = String::from(result.reason);
    assert!(
        reason.contains("refused") && reason.contains("vendor format"),
        "{reason}"
    );
    let gaomon = authorize(
        Some(&generic(0x256c, GAOMON_STYLE_PEN)),
        HidPublishPolicy::Nothing,
        false,
    );
    let result =
        host_decoded_tablet_result(requested_native(true), Some(&gaomon), device, "Linux Pier");
    assert!(result.accepted && !result.reconnect_required);
    assert_eq!(result.active, TabletModeMsg::UsbTabletBridge);
    let reason = String::from(result.reason);
    assert!(
        reason.starts_with("Native tablet is active: this Linux Pier"),
        "{reason}"
    );
    assert!(
        reason.contains("pressure, hover and barrel buttons."),
        "{reason}"
    );
    // Another requested mode is untouched.
    let typed = TabletModeResultMsg {
        requested: TabletModeMsg::LocalTermination,
        ..TabletModeResultMsg::default()
    };
    assert_eq!(
        host_decoded_tablet_result(typed.clone(), None, device, "Linux Pier"),
        typed
    );
}

#[test]
fn an_ended_attachment_gives_the_tablet_back_with_its_reason() {
    let notice: TabletModeResultMsg =
        serde_json::from_str(&ended_notice(&requested_native(true), "the desk\nlocked").unwrap())
            .unwrap();
    assert!(!notice.accepted && notice.reconnect_required);
    assert_eq!(notice.active, TabletModeMsg::DisabledMouseCompat);
    assert_eq!(
        String::from(notice.reason),
        "Native tablet (USB bridged) ended: the desklocked"
    );
    let long = "x".repeat(400);
    let notice: TabletModeResultMsg =
        serde_json::from_str(&ended_notice(&requested_native(true), &long).unwrap()).unwrap();
    assert!(String::from(notice.reason).len() <= MAX_TABLET_MODE_REASON_BYTES);
}

#[test]
fn the_pointer_a_pen_read_on_the_host_moves_goes_through_the_shared_pen_path() {
    use arcen_protocol::cursor_position::decode_cursor_position;
    assert!(HostPenPointer::new(0, 1080).is_none());
    assert!(HostPenPointer::new(70_000, 1080).is_none());
    let model = arcen_usb_bridge::host_decoded_intuos_model(0x056a, 0x0317).unwrap();
    let mut pointer = HostPenPointer::new(1920, 1080).unwrap();
    let start = Instant::now();
    let read = |x: u32, y: u32, in_proximity| {
        host_pen_sample(
            &HostPenSample {
                x: 0.0,
                y: 0.0,
                device_x: x,
                device_y: y,
                device_max_x: model.max_x,
                device_max_y: model.max_y,
                pressure: 0.0,
                tilt_x_degrees: 0.0,
                tilt_y_degrees: 0.0,
                rotation_degrees: 0.0,
                eraser: false,
                in_proximity,
                touching: false,
                barrel_buttons: 0,
            },
            start,
        )
    };
    // The first position is where the pointer rests, not a move.
    pointer.observe(&[read(16_000, 20_000, true)]);
    assert_eq!(pointer.due_at(), None);
    // A pen out of proximity moves nothing.
    pointer.observe(&[read(model.max_x, model.max_y, false)]);
    assert_eq!(pointer.due_at(), None);
    // Several moves in one step: only the newest is reported, at the
    // shared mapping's pixel (32_513 / 65_025 and 10_161 / 40_641 of the
    // tablet, as libinput and the X server scale it).
    pointer.observe(&[read(16_000, 20_000, true), read(32_513, 10_161, true)]);
    let record = decode_cursor_position(&pointer.take_record(start).unwrap()).unwrap();
    assert!(record.on_stream);
    assert_eq!((record.space_width, record.space_height), (1920, 1080));
    assert_eq!((record.x, record.y), (960, 270));
    assert_eq!(record.sequence, 1);
    // At most one record per 8.3 ms; the newest waits, then goes.
    pointer.observe(&[read(model.max_x, 0, true)]);
    assert!(pointer.take_record(start).is_none());
    let due = pointer.due_at().unwrap();
    let record = decode_cursor_position(&pointer.take_record(due).unwrap()).unwrap();
    assert_eq!((record.x, record.y, record.sequence), (1919, 0, 2));
    assert_eq!(pointer.reports(), 2);
    assert_eq!(pointer.due_at(), None);
}

#[test]
fn a_pen_read_on_the_host_becomes_the_same_tablet_event_a_deck_sends() {
    let model = arcen_usb_bridge::host_decoded_intuos_model(0x056a, 0x0317).unwrap();
    let mut reader = arcen_usb_bridge::IntuosPenReader::new(model);
    let events: Vec<_> = arcen_usb_bridge::intuos5_scripted_stroke()
        .iter()
        .filter_map(|step| reader.read(&step.report.encode()))
        .map(|sample| host_pen_event(&sample))
        .collect();
    assert!(events.iter().all(|event| event.validate().is_ok()));
    assert!(events.iter().all(|event| event.tool == PenToolMsg::Tip));
    assert!(events.iter().filter(|event| event.touching).count() > 20);
    assert!(events.iter().any(|event| event.pressure > 0.99));
    assert!(events.iter().any(|event| event.buttons == 1));
    let last = events.last().unwrap();
    assert!(!last.in_proximity && !last.touching);
    let eraser = host_pen_event(&HostPenSample {
        x: 2.0,
        y: -1.0,
        device_x: 0,
        device_y: 0,
        device_max_x: 1,
        device_max_y: 1,
        pressure: 0.5,
        tilt_x_degrees: 63.0,
        tilt_y_degrees: -64.0,
        rotation_degrees: 400.0,
        eraser: true,
        in_proximity: true,
        touching: true,
        barrel_buttons: 0,
    });
    assert_eq!(eraser.tool, PenToolMsg::Eraser);
    assert!(eraser.validate().is_ok(), "clamped into the wire's ranges");
}

/// Drives a session against the Deck's real exporter and an emulated tablet,
/// over real wire frames, until `pens` events arrived or 10 s passed.
fn drive<F: EmulatedUsbFunction>(
    exporter: &UsbExporter,
    session: &mut BridgedPenSession,
    first: BridgedPenStep,
    mut push: impl FnMut(&SyntheticHandle<F>) -> bool,
    handle: &SyntheticHandle<F>,
    pens: usize,
) -> Vec<PenEventMsg> {
    let mut frames = first.frames;
    let mut seen = first.pens;
    let mut pushed = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while seen.len() < pens && Instant::now() < deadline {
        for frame in std::mem::take(&mut frames) {
            if frame.first() == Some(&(FrameType::UsbBridgeUrbSubmit as u8)) {
                let (header, payload) = arcen_protocol::decode_usb_urb_submit(&frame).unwrap();
                exporter
                    .submit(
                        UrbSubmission {
                            generation: header.generation,
                            urb_id: header.urb_id,
                            endpoint: header.endpoint,
                            transfer_kind: header.transfer_kind,
                            timeout_ms: header.timeout_ms,
                            declared_length: header.declared_length,
                            setup: header.setup,
                        },
                        payload,
                    )
                    .unwrap();
            }
        }
        if !pushed {
            pushed = push(handle);
        }
        if let Ok(Some(done)) = exporter.next_completion_within(Duration::from_millis(20)) {
            let frame = arcen_protocol::encode_usb_urb_complete(
                arcen_protocol::UsbUrbCompletionHeader {
                    generation: done.generation,
                    urb_id: done.urb_id,
                    status: done.status,
                    actual_length: u32::try_from(done.data.len()).unwrap(),
                },
                &done.data,
            )
            .unwrap();
            assert!(BridgedPenSession::takes(&frame));
            let step = session.on_deck_frame(&frame, Instant::now()).unwrap();
            assert!(step.publish.is_none() && step.device_reports.is_empty());
            frames.extend(step.frames);
            seen.extend(step.pens);
        }
    }
    seen
}

fn claim(exporter: &UsbExporter) -> UsbHardDeviceMsg {
    let facts = exporter.facts();
    UsbHardDeviceMsg {
        vendor_id: facts.snapshot.id.vendor_id,
        product_id: facts.snapshot.id.product_id,
        bcd_device: facts.snapshot.id.bcd_device,
        device_class: facts.snapshot.device_class,
        speed: facts.speed,
        hid_report_descriptors: facts
            .report_descriptors
            .iter()
            .map(|r| UsbHidReportDescriptorMsg::new(r.interface, &r.descriptor))
            .collect(),
    }
}

fn exporter_for<F: EmulatedUsbFunction>(function: F) -> (UsbExporter, SyntheticHandle<F>) {
    let (device, handle) = SyntheticDevice::emulate(function).unwrap();
    let exporter = UsbExporter::new(Box::new(device))
        .map_err(|(refusal, _)| refusal)
        .unwrap();
    (exporter, handle)
}

fn generation() -> AttachmentGeneration {
    AttachmentGeneration::new(NonZeroU64::new(0x77).unwrap())
}

/// The emulated Intuos5 a Deck serves with `--usb-emulate wacom-intuos5`,
/// read by a host that publishes nothing (the Windows Pier): the tablet is
/// switched to Wacom mode, every pen packet becomes the tablet event a Deck
/// sends, and the end restores the mode and lifts the pen.
#[test]
fn an_emulated_intuos5_is_read_over_the_wire_without_publishing_anything() {
    let (exporter, handle) = exporter_for(WacomIntuos5::new());
    let tablet = authorize(Some(&claim(&exporter)), HidPublishPolicy::Nothing, false).unwrap();
    let (mut session, first) = BridgedPenSession::start(
        &tablet,
        HidPublishPolicy::Nothing,
        generation(),
        Instant::now(),
    )
    .unwrap();
    assert_eq!(first.frames.len(), 1, "GET_DESCRIPTOR(Device)");
    let stroke = arcen_usb_bridge::intuos5_scripted_stroke();
    let total = {
        let model = arcen_usb_bridge::host_decoded_intuos_model(0x056a, 0x0317).unwrap();
        let mut reader = arcen_usb_bridge::IntuosPenReader::new(model);
        stroke
            .iter()
            .filter(|step| reader.read(&step.report.encode()).is_some())
            .count()
    };
    let pens = drive(
        &exporter,
        &mut session,
        first,
        |handle| {
            if !handle.inspect(WacomIntuos5::wacom_mode) || handle.held_on(INTUOS5_PEN_IN) == 0 {
                return false;
            }
            for step in &stroke {
                handle.push_report_to(INTUOS5_PEN_IN, step.report.encode().to_vec());
            }
            true
        },
        &handle,
        total,
    );
    assert_eq!(pens.len(), total);
    assert!(pens.iter().all(|pen| pen.validate().is_ok()));
    assert!(pens.iter().any(|pen| pen.touching));
    assert!(!pens.last().unwrap().in_proximity);
    assert_eq!(session.bridge().published(), 0);
    let (frames, owed) = session.end(true, Instant::now());
    assert!(owed.is_empty(), "the stroke already left proximity");
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.first() == Some(&(FrameType::UsbBridgeUrbSubmit as u8)))
            .count(),
        1,
        "the mode restore"
    );
    assert!(session.end(true, Instant::now()).0.is_empty(), "once");
}

/// The generic pen a Deck serves with `--usb-emulate generic-pen`.
#[test]
fn an_emulated_generic_pen_is_read_over_the_wire_with_its_axes() {
    let (exporter, handle) = exporter_for(GenericPenTablet::default());
    let tablet = authorize(Some(&claim(&exporter)), HidPublishPolicy::Nothing, false).unwrap();
    let (mut session, first) = BridgedPenSession::start(
        &tablet,
        HidPublishPolicy::Nothing,
        generation(),
        Instant::now(),
    )
    .unwrap();
    let state = |tip: bool, pressure: u16, barrel: bool, in_range: bool| GenericPenState {
        x: 25_400,
        y: 15_918,
        pressure,
        tilt_x: 30,
        tilt_y: -20,
        tip,
        barrel,
        in_range,
        ..GenericPenState::default()
    };
    let pens = drive(
        &exporter,
        &mut session,
        first,
        |handle| {
            if handle.held_on(GENERIC_PEN_IN) == 0 {
                return false;
            }
            for report in [
                state(false, 0, false, true),
                state(true, 4_096, false, true),
                state(true, 4_096, true, true),
                state(false, 0, false, false),
            ] {
                handle.push_report_to(GENERIC_PEN_IN, report.encode().to_vec());
            }
            true
        },
        &handle,
        4,
    );
    assert_eq!(pens.len(), 4, "{pens:?}");
    assert!(pens[0].in_proximity && !pens[0].touching);
    assert!(pens[1].touching && (pens[1].pressure - 4_096.0 / 8_191.0).abs() < 1e-3);
    assert!((pens[1].tilt_x_degrees - 30.0).abs() < 0.5);
    assert!((pens[1].tilt_y_degrees + 20.0).abs() < 0.5);
    assert_eq!(pens[2].buttons, 1, "the barrel button");
    assert!(!pens[3].in_proximity);
    assert!((pens[1].x - 0.5).abs() < 0.001);
}

#[test]
fn an_attachment_that_breaks_ends_native_tablet_and_tells_the_deck_while_the_session_goes_on() {
    let tablet = authorize(Some(&intuos5()), HidPublishPolicy::Nothing, false).unwrap();
    let result = requested_native(true);
    let (mut attachment, first) = NativeTabletAttachment::start(
        Some(&tablet),
        &result,
        HidPublishPolicy::Nothing,
        generation(),
        Instant::now(),
    );
    assert!(attachment.is_active());
    assert_eq!(first.frames.len(), 1, "GET_DESCRIPTOR(Device)");
    assert!(attachment.takes(&[FrameType::UsbBridgeUrbComplete as u8]));
    assert!(!attachment.takes(&[FrameType::UsbBridgeUrbSubmit as u8]));
    let ended = attachment.on_deck_frame(&[FrameType::UsbBridgeUrbComplete as u8], Instant::now());
    assert!(!attachment.is_active());
    assert_eq!(
        ended.frames.len(),
        1,
        "the outstanding request is cancelled"
    );
    let notice: TabletModeResultMsg =
        serde_json::from_str(ended.notice.as_deref().unwrap()).unwrap();
    assert!(!notice.accepted && notice.reconnect_required);
    assert!(ended.ended.unwrap().0.contains("malformed"));
    assert!(!attachment.takes(&[FrameType::UsbBridgeUrbComplete as u8]));
    assert!(attachment.poll(Instant::now()).frames.is_empty());
    assert!(attachment.shutdown().is_none());

    let (mut idle, nothing) = NativeTabletAttachment::start(
        None,
        &result,
        HidPublishPolicy::Nothing,
        generation(),
        Instant::now(),
    );
    assert!(!idle.is_active() && nothing.frames.is_empty() && nothing.notice.is_none());
    assert!(idle.end("x", true, Instant::now()).notice.is_none());
}

#[test]
fn a_panic_in_the_adapter_becomes_an_error() {
    let panicked: Result<(), String> = contained(|| panic!("a bug"));
    assert!(panicked.unwrap_err().contains("internal fault"));
    assert_eq!(contained(|| Ok(7)), Ok(7));
}

#[test]
fn generations_are_never_zero_and_follow_the_counter() {
    let at = Duration::from_nanos(1_000);
    assert_ne!(fresh_generation(at, 1), fresh_generation(at, 3));
    assert_eq!(fresh_generation(Duration::ZERO, 0).get() & 1, 1);
}
