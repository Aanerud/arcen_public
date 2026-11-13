// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use super::*;
use crate::{
    ControlDispatch, ControlResponse, EmulatedUsbFunction, INTUOS5_MOUSE_IN, INTUOS5_PEN_IN,
    INTUOS5_TOUCH_IN, WacomIntuos5, authorize_hard_usb_tablet, control_dispatch,
};
use std::num::NonZeroU64;

fn generation() -> AttachmentGeneration {
    AttachmentGeneration::new(NonZeroU64::new(0x1234).unwrap())
}

fn claims() -> Vec<InterfaceReportDescriptor> {
    WacomIntuos5::new()
        .captured_facts()
        .unwrap()
        .report_descriptors
}

/// The Deck side of the bridge, as the exporter and the emulated tablet
/// answer it, without a network: control transfers are answered at once,
/// interrupt transfers are held until a test gives them a report.
struct Deck {
    tablet: WacomIntuos5,
    held: Vec<HidHostUrb>,
    /// Replaces the device's answer to a control request.
    tamper: Option<Box<dyn FnMut(SetupPacket, ControlResponse) -> ControlResponse>>,
    /// Control requests seen, in order.
    seen: Vec<SetupPacket>,
}

impl Deck {
    fn new() -> Self {
        Self {
            tablet: WacomIntuos5::new(),
            held: Vec::new(),
            tamper: None,
            seen: Vec::new(),
        }
    }

    fn answer(&mut self, urb: &HidHostUrb) -> (UrbStatus, Vec<u8>) {
        let setup = urb.setup.unwrap();
        self.seen.push(setup);
        let response = match control_dispatch(setup, 1) {
            ControlDispatch::Ack => ControlResponse::Ack,
            ControlDispatch::Stall => ControlResponse::Stall,
            _ => self.tablet.control(setup, &urb.payload),
        };
        let response = match response {
            ControlResponse::Data(mut data) => {
                data.truncate(usize::from(setup.length));
                ControlResponse::Data(data)
            }
            other => other,
        };
        let response = match self.tamper.as_mut() {
            Some(tamper) => tamper(setup, response),
            None => response,
        };
        match response {
            ControlResponse::Ack => (UrbStatus::Success, Vec::new()),
            ControlResponse::Data(data) => (UrbStatus::Success, data),
            ControlResponse::Stall => (UrbStatus::Stall, Vec::new()),
        }
    }

    /// Feeds the adapter's outputs back until only held transfers remain;
    /// returns everything that was not a submit.
    fn run(
        &mut self,
        adapter: &mut HidHostAdapter,
        outputs: Vec<HidHostOutput>,
    ) -> Result<Vec<HidHostOutput>, HidHostError> {
        let mut pending: VecDeque<HidHostOutput> = outputs.into();
        let mut rest = Vec::new();
        while let Some(output) = pending.pop_front() {
            match output {
                HidHostOutput::Submit(urb) if urb.transfer_kind == TransferKind::Control => {
                    let (status, data) = self.answer(&urb);
                    pending.extend(adapter.on_completion(
                        urb.urb_id,
                        status,
                        &data,
                        Instant::now(),
                    )?);
                }
                HidHostOutput::Submit(urb) => self.held.push(urb),
                other => rest.push(other),
            }
        }
        Ok(rest)
    }

    fn report(
        &mut self,
        adapter: &mut HidHostAdapter,
        endpoint: EndpointAddress,
        report: &[u8],
    ) -> Result<Vec<HidHostOutput>, HidHostError> {
        let position = self
            .held
            .iter()
            .position(|urb| urb.endpoint == endpoint)
            .expect("an interrupt transfer is held on the endpoint");
        let urb = self.held.remove(position);
        let outputs =
            adapter.on_completion(urb.urb_id, UrbStatus::Success, report, Instant::now())?;
        self.run(adapter, outputs)
    }
}

fn enumerated() -> (HidHostAdapter, Deck, Vec<BridgedHidDevice>) {
    let mut adapter = HidHostAdapter::new(WacomIntuos5::identity(), claims(), generation());
    let mut deck = Deck::new();
    let start = adapter.start(Instant::now()).unwrap();
    let rest = deck.run(&mut adapter, start).unwrap();
    let [HidHostOutput::CreateDevices(devices)] = rest.as_slice() else {
        panic!("expected the device plan, got {rest:?}");
    };
    (adapter, deck, devices.clone())
}

#[test]
fn an_emulated_intuos5_enumerates_into_one_device_per_hid_interface() {
    assert!(authorize_hard_usb_tablet(0, &claims()).is_ok());
    let (adapter, deck, devices) = enumerated();
    assert_eq!(adapter.phase(), HidHostPhase::Planned);
    let requests: Vec<(u8, u8, u16, u16)> = deck
        .seen
        .iter()
        .map(|setup| (setup.request_type, setup.request, setup.value, setup.index))
        .collect();
    assert_eq!(
        requests,
        vec![
            (0x80, 0x06, 0x0100, 0),      // device
            (0x80, 0x06, 0x0200, 0),      // configuration header
            (0x80, 0x06, 0x0200, 0),      // configuration
            (0x80, 0x06, 0x0301, 0x0409), // manufacturer
            (0x80, 0x06, 0x0302, 0x0409), // product
            (0x00, 0x09, 1, 0),           // SET_CONFIGURATION
            (0x21, 0x0A, 0, 0),           // SET_IDLE
            (0x81, 0x06, 0x2200, 0),      // report descriptor
            (0x21, 0x0A, 0, 1),
            (0x81, 0x06, 0x2200, 1),
            (0x21, 0x0A, 0, 2),
            (0x81, 0x06, 0x2200, 2),
        ]
    );
    assert_eq!(devices.len(), 3);
    for (index, device) in devices.iter().enumerate() {
        assert_eq!(usize::from(device.index), index);
        assert_eq!(usize::from(device.interface), index);
        assert_eq!((device.vendor_id, device.product_id), (0x056a, 0x0317));
        assert_eq!(device.version, 0x0100);
        assert_eq!(device.manufacturer, "Wacom Co.,Ltd.");
        assert_eq!(device.product, "Intuos5 touch L");
        assert_eq!(device.serial, format!("arcen-0000000000001234-{index}"));
        assert_eq!(
            device.location_id,
            0xA2C0_0000 | (0x234 << 8) | u32::try_from(index).unwrap()
        );
        assert!(crate::is_bridged_location_id(device.location_id));
        assert!(!crate::is_bridged_location_id(
            0x1410_0000 | device.location_id & 0xFFFF
        ));
        assert_eq!(
            device.report_descriptor,
            WacomIntuos5::report_descriptor(device.interface).unwrap()
        );
    }
    assert_eq!(
        (devices[0].primary_usage_page, devices[0].primary_usage),
        (0xff0d, 1)
    );
    assert_eq!(
        (devices[2].primary_usage_page, devices[2].primary_usage),
        (1, 2),
        "the boot mouse"
    );
    assert_eq!(
        [
            devices[0].input_endpoint,
            devices[1].input_endpoint,
            devices[2].input_endpoint
        ],
        [INTUOS5_PEN_IN, INTUOS5_TOUCH_IN, INTUOS5_MOUSE_IN]
    );
    // The pen's ten-byte reports fit its 16-byte packets; touch reports are
    // 64 bytes.
    assert_eq!(devices[0].input_length, 16);
    assert_eq!(devices[1].input_length, 64);
}

#[test]
fn reports_are_delivered_verbatim_and_each_poll_is_renewed() {
    let (mut adapter, mut deck, _) = enumerated();
    let start = adapter.devices_created(Instant::now()).unwrap();
    assert!(deck.run(&mut adapter, start).unwrap().is_empty());
    assert_eq!(deck.held.len(), 3, "one interrupt IN per interface");
    assert!(deck.held.iter().all(|urb| urb.setup.is_none()
        && urb.payload.is_empty()
        && urb.timeout_ms == HID_HOST_CONTROL_TIMEOUT_MS));
    let pen = [2, 0xe0, 0x3e, 0x80, 0x2e, 0xe0, 0x80, 0x00, 0x00, 0x00];
    for _ in 0..5 {
        let rest = deck.report(&mut adapter, INTUOS5_PEN_IN, &pen).unwrap();
        assert_eq!(
            rest,
            vec![HidHostOutput::Report {
                index: 0,
                report: pen.to_vec()
            }]
        );
        assert_eq!(deck.held.len(), 3, "the pen poll is renewed");
    }
    // An empty completion is not a report, and the poll is still renewed.
    assert!(
        deck.report(&mut adapter, INTUOS5_MOUSE_IN, &[])
            .unwrap()
            .is_empty()
    );
    assert_eq!(adapter.stats().reports, 5);
    assert_eq!(adapter.in_flight(), 3);
}

#[test]
fn the_wacom_mode_switch_and_its_read_back_cross_the_bridge_as_hid_requests() {
    let (mut adapter, mut deck, _) = enumerated();
    let start = adapter.devices_created(Instant::now()).unwrap();
    deck.run(&mut adapter, start).unwrap();
    assert!(!deck.tablet.wacom_mode());
    let set = adapter.request_report(
        41,
        HidReportRequest::Set {
            index: 0,
            kind: HidReportKind::Feature,
            report_id: 2,
            data: vec![2, 2],
        },
        Instant::now(),
    );
    let rest = deck.run(&mut adapter, set).unwrap();
    assert_eq!(
        rest,
        vec![HidHostOutput::ReportReply {
            token: 41,
            result: Ok(Vec::new())
        }]
    );
    let setup = *deck.seen.last().unwrap();
    assert_eq!(
        (
            setup.request_type,
            setup.request,
            setup.value,
            setup.index,
            setup.length
        ),
        (0x21, 0x09, 0x0302, 0, 2)
    );
    assert!(deck.tablet.wacom_mode(), "the tablet left mouse mode");
    let get = adapter.request_report(
        42,
        HidReportRequest::Get {
            index: 0,
            kind: HidReportKind::Feature,
            report_id: 2,
            length: 2,
        },
        Instant::now(),
    );
    assert_eq!(
        deck.run(&mut adapter, get).unwrap(),
        vec![HidHostOutput::ReportReply {
            token: 42,
            result: Ok(vec![2, 2])
        }]
    );
    // A report the device refuses is a stall, and the session goes on.
    let refused = adapter.request_report(
        43,
        HidReportRequest::Set {
            index: 0,
            kind: HidReportKind::Feature,
            report_id: 9,
            data: vec![2, 2],
        },
        Instant::now(),
    );
    assert_eq!(
        deck.run(&mut adapter, refused).unwrap(),
        vec![HidHostOutput::ReportReply {
            token: 43,
            result: Err(HidReportFailure::Stall)
        }]
    );
    assert_eq!(adapter.phase(), HidHostPhase::Running);
}

#[test]
fn report_requests_take_the_control_pipe_one_at_a_time_and_are_bounded() {
    let (mut adapter, mut deck, _) = enumerated();
    let start = adapter.devices_created(Instant::now()).unwrap();
    deck.run(&mut adapter, start).unwrap();
    let get = |index| HidReportRequest::Get {
        index,
        kind: HidReportKind::Feature,
        report_id: 2,
        length: 2,
    };
    let now = Instant::now();
    let mut submitted = Vec::new();
    for token in 0..=u32::try_from(MAX_QUEUED_REPORT_REQUESTS).unwrap() {
        submitted.extend(adapter.request_report(token, get(0), now));
    }
    assert_eq!(submitted.len(), 1, "one control transfer in flight");
    assert_eq!(
        adapter.request_report(99, get(0), now),
        vec![HidHostOutput::ReportReply {
            token: 99,
            result: Err(HidReportFailure::QueueFull)
        }]
    );
    let rest = deck.run(&mut adapter, submitted).unwrap();
    assert_eq!(
        rest.len(),
        MAX_QUEUED_REPORT_REQUESTS + 1,
        "all answered in order"
    );
    for (token, reply) in rest.iter().enumerate() {
        assert!(matches!(
            reply,
            HidHostOutput::ReportReply { token: answered, result: Ok(_) }
                if *answered == u32::try_from(token).unwrap()
        ));
    }
    // Out-of-contract requests are answered at once and reach nobody.
    for (request, failure) in [
        (get(3), HidReportFailure::UnknownDevice),
        (
            HidReportRequest::Set {
                index: 0,
                kind: HidReportKind::Input,
                report_id: 2,
                data: vec![2],
            },
            HidReportFailure::Invalid,
        ),
        (
            HidReportRequest::Get {
                index: 0,
                kind: HidReportKind::Feature,
                report_id: 2,
                length: 0,
            },
            HidReportFailure::Invalid,
        ),
    ] {
        assert_eq!(
            adapter.request_report(7, request, now),
            vec![HidHostOutput::ReportReply {
                token: 7,
                result: Err(failure)
            }]
        );
    }
}

#[test]
fn nothing_reaches_the_device_before_the_devices_exist() {
    let (mut adapter, _, _) = enumerated();
    assert_eq!(
        adapter.request_report(
            1,
            HidReportRequest::Get {
                index: 0,
                kind: HidReportKind::Feature,
                report_id: 2,
                length: 2
            },
            Instant::now()
        ),
        vec![HidHostOutput::ReportReply {
            token: 1,
            result: Err(HidReportFailure::NotRunning)
        }]
    );
    let mut fresh = HidHostAdapter::new(WacomIntuos5::identity(), claims(), generation());
    assert_eq!(
        fresh.devices_created(Instant::now()),
        Err(HidHostError::OutOfOrder("devices_created"))
    );
    fresh.start(Instant::now()).unwrap();
    assert_eq!(
        fresh.start(Instant::now()),
        Err(HidHostError::OutOfOrder("start"))
    );
}

fn enumerate_with(
    tamper: impl FnMut(SetupPacket, ControlResponse) -> ControlResponse + 'static,
) -> Result<Vec<HidHostOutput>, HidHostError> {
    let mut adapter = HidHostAdapter::new(WacomIntuos5::identity(), claims(), generation());
    let mut deck = Deck::new();
    deck.tamper = Some(Box::new(tamper));
    let start = adapter.start(Instant::now())?;
    let result = deck.run(&mut adapter, start);
    if result.is_err() {
        assert_eq!(adapter.phase(), HidHostPhase::Failed);
    }
    result
}

#[test]
fn a_device_that_contradicts_its_authorization_creates_nothing() {
    // Another product id in the device descriptor.
    let other_product = enumerate_with(|setup, response| match response {
        ControlResponse::Data(mut data) if setup.value == 0x0100 => {
            data[10] = 0x18;
            ControlResponse::Data(data)
        }
        other => other,
    });
    assert!(
        matches!(other_product, Err(HidHostError::Guard(_))),
        "{other_product:?}"
    );
    // A report descriptor other than the one authorized.
    let other_report = enumerate_with(|setup, response| match response {
        ControlResponse::Data(mut data) if setup.value == 0x2200 && setup.index == 2 => {
            data[3] ^= 0x01;
            ControlResponse::Data(data)
        }
        other => other,
    });
    assert!(
        matches!(other_report, Err(HidHostError::Guard(_))),
        "{other_report:?}"
    );
    // A configuration that turns into mass storage.
    let storage = enumerate_with(|setup, response| match response {
        ControlResponse::Data(mut data) if setup.value == 0x0200 && data.len() > 9 => {
            data[9 + 5] = 0x08;
            ControlResponse::Data(data)
        }
        other => other,
    });
    assert!(
        matches!(storage, Err(HidHostError::Guard(_))),
        "{storage:?}"
    );
    // More data than asked for.
    let oversized = enumerate_with(|setup, response| match response {
        ControlResponse::Data(mut data) if setup.value == 0x0100 => {
            data.push(0);
            ControlResponse::Data(data)
        }
        other => other,
    });
    assert!(
        matches!(oversized, Err(HidHostError::Oversized(_))),
        "{oversized:?}"
    );
}

#[test]
fn optional_requests_may_stall_and_required_ones_may_not() {
    // SET_IDLE and the strings are optional.
    let tolerant = enumerate_with(|setup, response| {
        if setup.request == HID_SET_IDLE || setup.value >> 8 == 3 {
            ControlResponse::Stall
        } else {
            response
        }
    })
    .unwrap();
    let [HidHostOutput::CreateDevices(devices)] = tolerant.as_slice() else {
        panic!("{tolerant:?}");
    };
    assert_eq!(devices[0].product, DEFAULT_PRODUCT_NAME);
    assert_eq!(devices[0].manufacturer, "");
    // The report descriptor is not.
    let refused = enumerate_with(|setup, response| {
        if setup.value == 0x2200 {
            ControlResponse::Stall
        } else {
            response
        }
    });
    assert_eq!(
        refused,
        Err(HidHostError::Enumeration {
            step: "GET_DESCRIPTOR(Report)",
            status: UrbStatus::Stall
        })
    );
}

#[test]
fn silence_is_bounded() {
    let mut adapter = HidHostAdapter::new(WacomIntuos5::identity(), claims(), generation());
    let now = Instant::now();
    let start = adapter.start(now).unwrap();
    assert_eq!(start.len(), 1);
    assert!(
        adapter
            .poll(now + HID_HOST_CONTROL_DEADLINE)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        adapter.poll(now + HID_HOST_CONTROL_DEADLINE + Duration::from_millis(1)),
        Err(HidHostError::ControlTimeout("GET_DESCRIPTOR(Device)"))
    );
    // The whole enumeration has a deadline of its own.
    let mut slow = HidHostAdapter::new(WacomIntuos5::identity(), claims(), generation());
    slow.start(now).unwrap();
    assert_eq!(
        slow.poll(now + HID_HOST_ENUMERATION_DEADLINE + Duration::from_millis(1)),
        Err(HidHostError::EnumerationTimeout)
    );

    // An unanswered report request fails alone, is cancelled, and its late
    // answer is ignored.
    let (mut adapter, mut deck, _) = enumerated();
    let start = adapter.devices_created(now).unwrap();
    deck.run(&mut adapter, start).unwrap();
    let submitted = adapter.request_report(
        5,
        HidReportRequest::Get {
            index: 0,
            kind: HidReportKind::Feature,
            report_id: 2,
            length: 2,
        },
        now,
    );
    let [HidHostOutput::Submit(lost)] = submitted.as_slice() else {
        panic!("{submitted:?}");
    };
    let late = now + HID_HOST_CONTROL_DEADLINE + Duration::from_millis(1);
    assert_eq!(
        adapter.poll(late).unwrap(),
        vec![
            HidHostOutput::Cancel(lost.urb_id),
            HidHostOutput::ReportReply {
                token: 5,
                result: Err(HidReportFailure::TimedOut)
            }
        ]
    );
    assert!(
        adapter
            .on_completion(lost.urb_id, UrbStatus::Cancelled, &[], late)
            .unwrap()
            .is_empty()
    );
    assert_eq!(adapter.phase(), HidHostPhase::Running);
}

#[test]
fn a_confused_peer_and_a_lost_device_end_the_attachment() {
    let (mut adapter, mut deck, _) = enumerated();
    let start = adapter.devices_created(Instant::now()).unwrap();
    deck.run(&mut adapter, start).unwrap();
    let stranger = UrbId::new(NonZeroU32::new(9_999).unwrap());
    assert_eq!(
        adapter.on_completion(stranger, UrbStatus::Success, &[1], Instant::now()),
        Err(HidHostError::UnknownCompletion(stranger))
    );
    assert_eq!(adapter.phase(), HidHostPhase::Failed);

    let (mut adapter, mut deck, _) = enumerated();
    let start = adapter.devices_created(Instant::now()).unwrap();
    deck.run(&mut adapter, start).unwrap();
    let pen = deck.held.remove(0);
    assert_eq!(
        adapter.on_completion(pen.urb_id, UrbStatus::Disconnected, &[], Instant::now()),
        Err(HidHostError::Disconnected)
    );

    // Repeated failures on one interface, each renewed until the bound.
    let (mut adapter, mut deck, _) = enumerated();
    let start = adapter.devices_created(Instant::now()).unwrap();
    deck.run(&mut adapter, start).unwrap();
    let mut outcome = Ok(Vec::new());
    for _ in 0..MAX_INTERRUPT_ERRORS {
        let position = deck
            .held
            .iter()
            .position(|urb| urb.endpoint == INTUOS5_TOUCH_IN)
            .unwrap();
        let urb = deck.held.remove(position);
        outcome = adapter.on_completion(urb.urb_id, UrbStatus::Io, &[], Instant::now());
        if let Ok(outputs) = &outcome {
            deck.run(&mut adapter, outputs.clone()).unwrap();
        }
    }
    assert_eq!(
        outcome,
        Err(HidHostError::InterruptFailures { interface: 1 })
    );

    // A report longer than the poll asked for.
    let (mut adapter, mut deck, _) = enumerated();
    let start = adapter.devices_created(Instant::now()).unwrap();
    deck.run(&mut adapter, start).unwrap();
    assert!(matches!(
        deck.report(&mut adapter, INTUOS5_PEN_IN, &[0; 17]),
        Err(HidHostError::Oversized(_))
    ));
}

#[test]
fn shutdown_cancels_every_transfer_and_answers_every_waiting_request() {
    let (mut adapter, mut deck, _) = enumerated();
    let start = adapter.devices_created(Instant::now()).unwrap();
    deck.run(&mut adapter, start).unwrap();
    let get = HidReportRequest::Get {
        index: 0,
        kind: HidReportKind::Feature,
        report_id: 2,
        length: 2,
    };
    let in_flight = adapter.request_report(1, get.clone(), Instant::now());
    let _ = adapter.request_report(2, get, Instant::now());
    let out = adapter.shutdown();
    let cancels = out
        .iter()
        .filter(|output| matches!(output, HidHostOutput::Cancel(_)))
        .count();
    assert_eq!(cancels, 4, "three polls and the control transfer: {out:?}");
    let refused: Vec<u32> = out
        .iter()
        .filter_map(|output| match output {
            HidHostOutput::ReportReply {
                token,
                result: Err(HidReportFailure::NotRunning),
            } => Some(*token),
            _ => None,
        })
        .collect();
    assert_eq!(refused, vec![1, 2]);
    assert_eq!(adapter.phase(), HidHostPhase::Stopped);
    assert_eq!(adapter.in_flight(), 0);
    let HidHostOutput::Submit(urb) = &in_flight[0] else {
        panic!();
    };
    assert!(
        adapter
            .on_completion(urb.urb_id, UrbStatus::Success, &[2, 2], Instant::now())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn string_properties_are_bounded_and_clean() {
    let encode = |text: &str| {
        let mut bytes = vec![0, DESCRIPTOR_STRING];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes[0] = u8::try_from(bytes.len()).unwrap_or(u8::MAX);
        bytes
    };
    assert_eq!(
        string_descriptor(&encode(" Wacom\u{7}\n ")),
        Some("Wacom".to_owned())
    );
    assert_eq!(string_descriptor(&encode("")), None);
    assert_eq!(string_descriptor(&[4, 2, 0x41, 0]), None, "not a string");
    assert_eq!(string_descriptor(&[8, 3, 0x41, 0]), None, "truncated");
    let long = string_descriptor(&encode(&"x".repeat(127))).unwrap();
    assert_eq!(long.chars().count(), MAX_PROPERTY_CHARS);
    assert_eq!(
        hid_country_codes(crate::INTUOS5_CONFIGURATION_DESCRIPTOR),
        [(0, 0), (1, 0), (2, 0)].into_iter().collect()
    );
}

#[derive(Default)]
struct Seen {
    devices: Vec<BridgedHidDevice>,
    reports: Vec<(u8, Vec<u8>)>,
    replies: Vec<(u32, Result<Vec<u8>, HidReportFailure>)>,
}

/// The adapter against the Deck's real exporter and the emulated tablet, as
/// a Deck run with `--usb-emulate wacom-intuos5` serves it.
#[test]
#[allow(clippy::too_many_lines)]
fn the_deck_exporter_serves_the_adapter_end_to_end() {
    use crate::{ReleaseReason, SyntheticDevice, UrbSubmission, UsbExporter};

    let (device, handle) = SyntheticDevice::emulate(WacomIntuos5::new()).unwrap();
    let exporter = UsbExporter::new(Box::new(device))
        .map_err(|(refusal, _)| refusal)
        .unwrap();
    let facts = exporter.facts();
    let profile = authorize_hard_usb_tablet(facts.snapshot.device_class, &facts.report_descriptors);
    assert!(profile.is_ok());
    let mut adapter = HidHostAdapter::new(
        facts.snapshot.id,
        facts.report_descriptors.clone(),
        generation(),
    );
    let mut seen = Seen::default();
    let step = |adapter: &mut HidHostAdapter, seen: &mut Seen, outputs: Vec<HidHostOutput>| {
        let mut pending: VecDeque<HidHostOutput> = outputs.into();
        let mut control_outstanding = 0;
        while let Some(output) = pending.pop_front() {
            match output {
                HidHostOutput::Submit(urb) => {
                    exporter
                        .submit(
                            UrbSubmission {
                                generation: adapter.generation(),
                                urb_id: urb.urb_id,
                                endpoint: urb.endpoint,
                                transfer_kind: urb.transfer_kind,
                                timeout_ms: urb.timeout_ms,
                                declared_length: urb.declared_length,
                                setup: urb.setup,
                            },
                            &urb.payload,
                        )
                        .unwrap();
                    if urb.transfer_kind == TransferKind::Control {
                        control_outstanding += 1;
                    }
                }
                HidHostOutput::CreateDevices(plan) => seen.devices = plan,
                HidHostOutput::Report { index, report } => seen.reports.push((index, report)),
                HidHostOutput::ReportReply { token, result } => seen.replies.push((token, result)),
                HidHostOutput::Cancel(urb) => {
                    let cancelled = exporter.cancel(adapter.generation(), urb).unwrap();
                    pending.extend(
                        adapter
                            .on_completion(
                                cancelled.urb_id,
                                cancelled.status,
                                &cancelled.data,
                                Instant::now(),
                            )
                            .unwrap(),
                    );
                }
            }
            if pending.is_empty() && control_outstanding > 0 {
                let completion = exporter
                    .next_completion_within(Duration::from_secs(2))
                    .unwrap()
                    .expect("the control transfer completes");
                control_outstanding -= 1;
                pending.extend(
                    adapter
                        .on_completion(
                            completion.urb_id,
                            completion.status,
                            &completion.data,
                            Instant::now(),
                        )
                        .unwrap(),
                );
            }
        }
    };
    let start = adapter.start(Instant::now()).unwrap();
    step(&mut adapter, &mut seen, start);
    assert_eq!(seen.devices.len(), 3);
    let polls = adapter.devices_created(Instant::now()).unwrap();
    step(&mut adapter, &mut seen, polls);
    let mode = adapter.request_report(
        1,
        HidReportRequest::Set {
            index: 0,
            kind: HidReportKind::Feature,
            report_id: 2,
            data: vec![2, 2],
        },
        Instant::now(),
    );
    step(&mut adapter, &mut seen, mode);
    assert!(handle.inspect(WacomIntuos5::wacom_mode));
    let pen = vec![2, 0xc2, 0x3e, 0x80, 0x2e, 0xe0, 0x80, 0x00, 0x00, 0x00];
    handle.push_report_to(INTUOS5_PEN_IN, pen.clone());
    let completion = exporter
        .next_completion_within(Duration::from_secs(2))
        .unwrap()
        .expect("the held pen poll completes");
    let delivered = adapter
        .on_completion(
            completion.urb_id,
            completion.status,
            &completion.data,
            Instant::now(),
        )
        .unwrap();
    step(&mut adapter, &mut seen, delivered);
    assert_eq!(seen.reports, vec![(0, pen)]);
    let ending = adapter.shutdown();
    step(&mut adapter, &mut seen, ending);
    assert!(
        seen.replies.iter().all(|(_, result)| result.is_ok()),
        "{:?}",
        seen.replies
    );
    let _ = exporter.release(ReleaseReason::SessionEnded);
}

#[test]
fn short_device_and_configuration_answers_end_the_attachment_without_a_panic() {
    // Each is a successful completion with fewer bytes than the descriptor
    // has: an answer the guard alone accepts as a prefix.
    for length in [0, 1, 8, 14, 16, 17] {
        let short_device = enumerate_with(move |setup, response| match response {
            ControlResponse::Data(mut data) if setup.value == 0x0100 => {
                data.truncate(length);
                ControlResponse::Data(data)
            }
            other => other,
        });
        assert!(
            matches!(short_device, Err(HidHostError::Descriptor(_))),
            "{length}-byte device descriptor: {short_device:?}"
        );
    }
    for length in [0, 1, 3, 4, 8] {
        let short_header = enumerate_with(move |setup, response| match response {
            ControlResponse::Data(mut data) if setup.value == 0x0200 && setup.length == 9 => {
                data.truncate(length);
                ControlResponse::Data(data)
            }
            other => other,
        });
        assert!(
            matches!(short_header, Err(HidHostError::Descriptor(_))),
            "{length}-byte configuration header: {short_header:?}"
        );
    }
    // A complete descriptor naming another release is refused (the guard
    // catches it first; the adapter never publishes a served identity).
    let other_release = enumerate_with(|setup, response| match response {
        ControlResponse::Data(mut data) if setup.value == 0x0100 => {
            data[12] ^= 0x01;
            ControlResponse::Data(data)
        }
        other => other,
    });
    assert!(other_release.is_err(), "{other_release:?}");
}

#[test]
fn a_host_without_seats_publishes_only_the_pen_and_vendor_interfaces() {
    let mut adapter = HidHostAdapter::new(WacomIntuos5::identity(), claims(), generation())
        .with_publish_policy(HidPublishPolicy::PenAndVendorOnly);
    let mut deck = Deck::new();
    let start = adapter.start(Instant::now()).unwrap();
    let rest = deck.run(&mut adapter, start).unwrap();
    let [HidHostOutput::CreateDevices(devices)] = rest.as_slice() else {
        panic!("expected the device plan, got {rest:?}");
    };
    let interfaces: Vec<u8> = devices.iter().map(|device| device.interface).collect();
    assert_eq!(
        interfaces,
        vec![0],
        "the touch interface (Apple's 0xFF00 page) and the boot mouse are withheld"
    );
    assert_eq!(
        adapter.withheld(),
        &[
            (
                1,
                PublishRefusal::ApplePage {
                    usage_page: 0xFF00,
                    usage: 0x01
                }
            ),
            (
                2,
                PublishRefusal::Collection {
                    usage_page: 0x01,
                    usage: 0x02
                }
            )
        ]
    );
    // Withheld interfaces are never configured for polling or polled.
    assert!(
        !deck
            .seen
            .iter()
            .any(|setup| setup.index == 1 || setup.index == 2)
    );
    let outputs = adapter.devices_created(Instant::now()).unwrap();
    deck.run(&mut adapter, outputs).unwrap();
    let polled: Vec<EndpointAddress> = deck.held.iter().map(|urb| urb.endpoint).collect();
    assert_eq!(polled, vec![INTUOS5_PEN_IN]);

    // A device with nothing publishable ends before anything exists: here
    // every authorized interface is described as the boot mouse.
    let mouse = WacomIntuos5::report_descriptor(2).unwrap();
    let all_mice = (0..3)
        .map(|interface| InterfaceReportDescriptor {
            interface,
            descriptor: mouse.to_vec(),
        })
        .collect();
    let mut adapter = HidHostAdapter::new(WacomIntuos5::identity(), all_mice, generation())
        .with_publish_policy(HidPublishPolicy::PenAndVendorOnly);
    let ControlResponse::Data(configuration) = WacomIntuos5::new().control(
        SetupPacket {
            request_type: 0x80,
            request: 0x06,
            value: 0x0200,
            index: 0,
            length: 4096,
        },
        &[],
    ) else {
        panic!("the emulated tablet serves its configuration");
    };
    assert_eq!(
        adapter.absorb_configuration(&configuration),
        Err(HidHostError::NothingPublishable)
    );
    assert_eq!(adapter.withheld().len(), 3);
}

#[test]
fn an_interrupt_transfer_that_went_silent_is_cancelled_and_renewed() {
    let (mut adapter, mut deck, _) = enumerated();
    let now = Instant::now();
    let outputs = adapter.devices_created(now).unwrap();
    deck.run(&mut adapter, outputs).unwrap();
    let first: Vec<HidHostUrb> = std::mem::take(&mut deck.held);
    assert_eq!(first.len(), 3);
    // Held transfers are normal for an idle tablet: nothing happens before
    // the watchdog period.
    assert!(
        adapter
            .poll(
                now + INTERRUPT_IDLE_RESUBMIT
                    .checked_sub(Duration::from_millis(1))
                    .unwrap()
            )
            .unwrap()
            .is_empty()
    );
    // Then every one is cancelled and a fresh one submitted, as if the
    // submits had been lost on the way to the Deck.
    let renewed = adapter.poll(now + INTERRUPT_IDLE_RESUBMIT).unwrap();
    for old in &first {
        assert!(renewed.contains(&HidHostOutput::Cancel(old.urb_id)));
    }
    let fresh: Vec<HidHostUrb> = renewed
        .iter()
        .filter_map(|output| match output {
            HidHostOutput::Submit(urb) => Some(urb.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        fresh.iter().map(|urb| urb.endpoint).collect::<Vec<_>>(),
        first.iter().map(|urb| urb.endpoint).collect::<Vec<_>>()
    );
    assert_eq!(adapter.stats().interrupt_resubmits, 3);
    assert_eq!(adapter.in_flight(), 3);
    // The cancellations' answers (the Deck answers a cancel even for a
    // transfer it never saw) are ignored, and the renewed poll delivers.
    let later = now + INTERRUPT_IDLE_RESUBMIT;
    for old in &first {
        assert!(
            adapter
                .on_completion(old.urb_id, UrbStatus::Cancelled, &[], later)
                .unwrap()
                .is_empty()
        );
    }
    deck.held = fresh;
    let report = [0x02, 0xE0, 0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00];
    let delivered = deck.report(&mut adapter, INTUOS5_PEN_IN, &report).unwrap();
    assert!(delivered.contains(&HidHostOutput::Report {
        index: 0,
        report: report.to_vec()
    }));
    assert_eq!(adapter.phase(), HidHostPhase::Running);
}

/// Runs an attachment to polling and lets the watchdog renew every interrupt
/// transfer once; returns the renewed (old) and the fresh transfers.
fn renewed() -> (
    HidHostAdapter,
    Deck,
    Instant,
    Vec<HidHostUrb>,
    Vec<HidHostUrb>,
) {
    let (mut adapter, mut deck, _) = enumerated();
    let now = Instant::now();
    let outputs = adapter.devices_created(now).unwrap();
    deck.run(&mut adapter, outputs).unwrap();
    let old = std::mem::take(&mut deck.held);
    let later = now + INTERRUPT_IDLE_RESUBMIT;
    let fresh = adapter
        .poll(later)
        .unwrap()
        .into_iter()
        .filter_map(|output| match output {
            HidHostOutput::Submit(urb) => Some(urb),
            _ => None,
        })
        .collect();
    (adapter, deck, later, old, fresh)
}

const PEN_REPORT: [u8; 10] = [0x02, 0xE0, 0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00];

#[test]
fn a_report_that_races_the_watchdog_is_delivered_and_its_cancel_absorbed() {
    // The pen re-enters proximity just as the watchdog renews its transfer:
    // the Deck completes the old transfer with the report, then answers the
    // cancel with Cancelled.
    let (mut adapter, _, later, old, fresh) = renewed();
    let pen = old
        .iter()
        .find(|urb| urb.endpoint == INTUOS5_PEN_IN)
        .unwrap();
    let delivered = adapter
        .on_completion(pen.urb_id, UrbStatus::Success, &PEN_REPORT, later)
        .unwrap();
    assert_eq!(
        delivered,
        vec![HidHostOutput::Report {
            index: 0,
            report: PEN_REPORT.to_vec()
        }],
        "the report is input, delivered once, and the renewed transfer is not doubled"
    );
    assert_eq!(adapter.stats().late_reports, 1);
    assert!(
        adapter
            .on_completion(pen.urb_id, UrbStatus::Cancelled, &[], later)
            .unwrap()
            .is_empty(),
        "the cancel's own answer is absorbed, not an unknown completion"
    );
    assert_eq!(adapter.phase(), HidHostPhase::Running);
    // The renewed transfer carries the next report.
    let next = fresh
        .iter()
        .find(|urb| urb.endpoint == INTUOS5_PEN_IN)
        .unwrap();
    let outputs = adapter
        .on_completion(next.urb_id, UrbStatus::Success, &PEN_REPORT, later)
        .unwrap();
    assert!(outputs.contains(&HidHostOutput::Report {
        index: 0,
        report: PEN_REPORT.to_vec()
    }));
    assert_eq!(adapter.in_flight(), 3);
    // Any other status before the Cancelled keeps the tombstone too.
    let touch = old
        .iter()
        .find(|urb| urb.endpoint == INTUOS5_TOUCH_IN)
        .unwrap();
    for status in [UrbStatus::Stall, UrbStatus::TimedOut, UrbStatus::Success] {
        assert!(
            adapter
                .on_completion(touch.urb_id, status, &[], later)
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        adapter
            .on_completion(touch.urb_id, UrbStatus::Cancelled, &[], later)
            .unwrap()
            .is_empty()
    );
    assert_eq!(adapter.phase(), HidHostPhase::Running);
}

#[test]
fn a_cancel_answered_first_retires_the_tombstone_and_polling_goes_on() {
    // The usual order for a held transfer: Cancelled first; the renewed
    // transfer then brings the report.
    let (mut adapter, _, later, old, fresh) = renewed();
    for urb in &old {
        assert!(
            adapter
                .on_completion(urb.urb_id, UrbStatus::Cancelled, &[], later)
                .unwrap()
                .is_empty()
        );
    }
    let pen = fresh
        .iter()
        .find(|urb| urb.endpoint == INTUOS5_PEN_IN)
        .unwrap();
    let outputs = adapter
        .on_completion(pen.urb_id, UrbStatus::Success, &PEN_REPORT, later)
        .unwrap();
    assert!(outputs.contains(&HidHostOutput::Report {
        index: 0,
        report: PEN_REPORT.to_vec()
    }));
    assert_eq!(adapter.stats().late_reports, 0);
    // A retired transfer's own completion may still follow its Cancelled
    // (the Deck's helper sends them from two threads): it is ignored here
    // because nothing came of it, and the attachment goes on.
    let stale = old
        .iter()
        .find(|urb| urb.endpoint == INTUOS5_TOUCH_IN)
        .unwrap();
    assert!(
        adapter
            .on_completion(stale.urb_id, UrbStatus::Stall, &[], later)
            .unwrap()
            .is_empty()
    );
    assert_eq!(adapter.phase(), HidHostPhase::Running);
}

#[test]
fn a_report_that_follows_its_cancel_answer_is_delivered_once() {
    // The Deck's drain thread had the report (the transfer completed) while
    // its request thread answered the cancel; the Cancelled went out first.
    let (mut adapter, _, later, old, fresh) = renewed();
    let pen = old
        .iter()
        .find(|urb| urb.endpoint == INTUOS5_PEN_IN)
        .unwrap();
    assert!(
        adapter
            .on_completion(pen.urb_id, UrbStatus::Cancelled, &[], later)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        adapter
            .on_completion(pen.urb_id, UrbStatus::Success, &PEN_REPORT, later)
            .unwrap(),
        vec![HidHostOutput::Report {
            index: 0,
            report: PEN_REPORT.to_vec()
        }],
        "valid input, and still ahead of anything the renewed transfer brings"
    );
    assert_eq!(adapter.stats().late_reports, 1);
    // Delivered once: a repeat is ignored, a repeated Cancelled too.
    assert!(
        adapter
            .on_completion(pen.urb_id, UrbStatus::Success, &PEN_REPORT, later)
            .unwrap()
            .is_empty()
    );
    assert!(
        adapter
            .on_completion(pen.urb_id, UrbStatus::Cancelled, &[], later)
            .unwrap()
            .is_empty()
    );
    assert_eq!(adapter.stats().late_reports, 1);
    // The renewed transfer goes on as usual.
    let next = fresh
        .iter()
        .find(|urb| urb.endpoint == INTUOS5_PEN_IN)
        .unwrap();
    let outputs = adapter
        .on_completion(next.urb_id, UrbStatus::Success, &PEN_REPORT, later)
        .unwrap();
    assert!(outputs.contains(&HidHostOutput::Report {
        index: 0,
        report: PEN_REPORT.to_vec()
    }));
    assert_eq!(adapter.phase(), HidHostPhase::Running);
}

#[test]
fn a_report_request_answered_after_its_cancel_answer_is_dropped() {
    let (mut adapter, mut deck, _) = enumerated();
    let now = Instant::now();
    let outputs = adapter.devices_created(now).unwrap();
    deck.run(&mut adapter, outputs).unwrap();
    let get = HidReportRequest::Get {
        index: 0,
        kind: HidReportKind::Feature,
        report_id: 2,
        length: 2,
    };
    let submitted = adapter.request_report(9, get.clone(), now);
    let [HidHostOutput::Submit(lost)] = submitted.as_slice() else {
        panic!("{submitted:?}");
    };
    let late = now + HID_HOST_CONTROL_DEADLINE + Duration::from_millis(1);
    adapter.poll(late).unwrap();
    assert!(
        adapter
            .on_completion(lost.urb_id, UrbStatus::Cancelled, &[], late)
            .unwrap()
            .is_empty()
    );
    // The request already failed with TimedOut: its answer is dropped, not
    // an unknown completion, and produces no second reply.
    assert!(
        adapter
            .on_completion(lost.urb_id, UrbStatus::Success, &[2, 2], late)
            .unwrap()
            .is_empty()
    );
    assert_eq!(adapter.phase(), HidHostPhase::Running);
    let again = adapter.request_report(10, get, late);
    assert!(matches!(again.as_slice(), [HidHostOutput::Submit(_)]));
}

#[test]
fn a_timed_out_report_request_keeps_its_tombstone_until_the_cancel_answer() {
    let (mut adapter, mut deck, _) = enumerated();
    let now = Instant::now();
    let outputs = adapter.devices_created(now).unwrap();
    deck.run(&mut adapter, outputs).unwrap();
    let get = HidReportRequest::Get {
        index: 0,
        kind: HidReportKind::Feature,
        report_id: 2,
        length: 2,
    };
    let submitted = adapter.request_report(7, get.clone(), now);
    let [HidHostOutput::Submit(lost)] = submitted.as_slice() else {
        panic!("{submitted:?}");
    };
    let late = now + HID_HOST_CONTROL_DEADLINE + Duration::from_millis(1);
    assert!(
        adapter
            .poll(late)
            .unwrap()
            .contains(&HidHostOutput::Cancel(lost.urb_id))
    );
    // The answer was on its way: it is dropped (the request already failed),
    // and the cancel's own answer is still absorbed after it.
    assert!(
        adapter
            .on_completion(lost.urb_id, UrbStatus::Success, &[2, 2], late)
            .unwrap()
            .is_empty()
    );
    assert!(
        adapter
            .on_completion(lost.urb_id, UrbStatus::Cancelled, &[], late)
            .unwrap()
            .is_empty()
    );
    assert_eq!(adapter.phase(), HidHostPhase::Running);
    // The control pipe is free again.
    let again = adapter.request_report(8, get, late);
    assert!(matches!(again.as_slice(), [HidHostOutput::Submit(_)]));
}
