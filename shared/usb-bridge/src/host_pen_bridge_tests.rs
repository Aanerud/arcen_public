// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use super::*;
use crate::{
    ControlDispatch, ControlResponse, EmulatedUsbFunction, EndpointAddress, GENERIC_PEN_IN,
    GenericPenState, GenericPenTablet, INTUOS5_PEN_IN, IntuosPenModel, TransferKind, UrbStatus,
    WacomIntuos5, control_dispatch, generic_pen_scripted_stroke, host_pen_route,
    intuos5_scripted_stroke,
};
use std::num::NonZeroU64;

fn generation() -> AttachmentGeneration {
    AttachmentGeneration::new(NonZeroU64::new(0x5150).unwrap())
}

/// The Deck side, as the exporter and an emulated tablet answer it, without
/// a network: control transfers are answered at once, interrupt transfers
/// held until a test gives them a report.
struct Deck<F: EmulatedUsbFunction> {
    function: F,
    held: Vec<HidHostUrb>,
    seen: Vec<crate::SetupPacket>,
}

impl<F: EmulatedUsbFunction> Deck<F> {
    fn new(function: F) -> Self {
        Self {
            function,
            held: Vec::new(),
            seen: Vec::new(),
        }
    }

    fn bridge(&self, policy: HidPublishPolicy) -> Result<HostPenBridge, NoPenInterface> {
        let facts = self.function.captured_facts().unwrap();
        let route = host_pen_route(facts.snapshot.id, &facts.report_descriptors).unwrap();
        HostPenBridge::new(
            facts.snapshot.id,
            facts.report_descriptors,
            &route,
            policy,
            generation(),
        )
    }

    fn answer(&mut self, urb: &HidHostUrb) -> (UrbStatus, Vec<u8>) {
        let setup = urb.setup.unwrap();
        self.seen.push(setup);
        let response = match control_dispatch(setup, 1) {
            ControlDispatch::Ack => ControlResponse::Ack,
            ControlDispatch::Stall => ControlResponse::Stall,
            _ => self.function.control(setup, &urb.payload),
        };
        match response {
            ControlResponse::Ack => (UrbStatus::Success, Vec::new()),
            ControlResponse::Data(mut data) => {
                data.truncate(usize::from(setup.length));
                (UrbStatus::Success, data)
            }
            ControlResponse::Stall => (UrbStatus::Stall, Vec::new()),
        }
    }

    /// Feeds the bridge's outputs back until only held transfers remain;
    /// returns everything that was not a submit.
    fn run(
        &mut self,
        bridge: &mut HostPenBridge,
        outputs: Vec<HostPenBridgeOutput>,
    ) -> Vec<HostPenBridgeOutput> {
        let mut pending: VecDeque<HostPenBridgeOutput> = outputs.into();
        let mut rest = Vec::new();
        while let Some(output) = pending.pop_front() {
            match output {
                HostPenBridgeOutput::Submit(urb) if urb.transfer_kind == TransferKind::Control => {
                    let (status, data) = self.answer(&urb);
                    pending.extend(
                        bridge
                            .on_completion(urb.urb_id, status, &data, Instant::now())
                            .unwrap(),
                    );
                }
                HostPenBridgeOutput::Submit(urb) => self.held.push(urb),
                other => rest.push(other),
            }
        }
        rest
    }

    fn report(
        &mut self,
        bridge: &mut HostPenBridge,
        endpoint: EndpointAddress,
        report: &[u8],
    ) -> Vec<HostPenBridgeOutput> {
        let position = self
            .held
            .iter()
            .position(|urb| urb.endpoint == endpoint)
            .expect("an interrupt transfer is held on the endpoint");
        let urb = self.held.remove(position);
        let outputs = bridge
            .on_completion(urb.urb_id, UrbStatus::Success, report, Instant::now())
            .unwrap();
        self.run(bridge, outputs)
    }
}

fn read_only_intuos5() -> (HostPenBridge, Deck<WacomIntuos5>) {
    let mut deck = Deck::new(WacomIntuos5::new());
    let mut bridge = deck.bridge(HidPublishPolicy::Nothing).unwrap();
    let start = bridge.start(Instant::now()).unwrap();
    let rest = deck.run(&mut bridge, start);
    assert!(
        rest.is_empty(),
        "nothing to publish, nothing to answer: {rest:?}"
    );
    (bridge, deck)
}

#[test]
fn a_host_that_publishes_nothing_reads_only_the_pen_and_switches_the_intuos_itself() {
    let (mut bridge, mut deck) = read_only_intuos5();
    assert_eq!(bridge.phase(), HidHostPhase::Running);
    assert_eq!(bridge.published(), 0);
    assert_eq!(bridge.reader_kind(), "dedicated");
    // Only the pen's endpoint is polled: touch and the boot mouse never are.
    assert_eq!(
        deck.held.iter().map(|urb| urb.endpoint).collect::<Vec<_>>(),
        vec![INTUOS5_PEN_IN]
    );
    // The mode was read, then the tablet left mouse mode.
    assert!(deck.function.wacom_mode());
    assert_eq!(bridge.mode(), HostPenMode::Switched(Some(0)));
    let notes = bridge.take_notes();
    assert!(matches!(
        notes.as_slice(),
        [
            HostPenNote::ModeRead {
                original: Some(0),
                failure: None
            },
            HostPenNote::ModeSwitched {
                accepted: true,
                failure: None
            }
        ]
    ));
    // Every pen packet of a scripted stroke becomes a sample, in order.
    let mut samples = Vec::new();
    for step in intuos5_scripted_stroke() {
        let rest = deck.report(&mut bridge, INTUOS5_PEN_IN, &step.report.encode());
        assert!(
            rest.is_empty(),
            "a pen report is never a device's: {rest:?}"
        );
        samples.extend(bridge.take_samples());
    }
    assert!(samples.len() > 20);
    assert!(samples.iter().any(|sample| sample.touching));
    assert!(samples.iter().any(|sample| sample.barrel_buttons == 1));
    assert!(!samples.last().unwrap().in_proximity);
    assert_eq!(bridge.pen_counts().2, 0, "nothing dropped");
}

#[test]
fn a_generic_pen_is_read_in_its_power_on_mode() {
    let mut deck = Deck::new(GenericPenTablet::default());
    let mut bridge = deck.bridge(HidPublishPolicy::Nothing).unwrap();
    let start = bridge.start(Instant::now()).unwrap();
    assert!(deck.run(&mut bridge, start).is_empty());
    assert_eq!(bridge.reader_kind(), "generic");
    assert_eq!(bridge.mode(), HostPenMode::Untouched);
    assert!(
        deck.seen
            .iter()
            .all(|setup| setup.request_type & 0x60 != 0x20 || setup.request != 0x09),
        "no SET_REPORT reaches a generic pen"
    );
    let mut samples = Vec::new();
    for step in generic_pen_scripted_stroke() {
        assert!(
            deck.report(&mut bridge, GENERIC_PEN_IN, &step.state.encode())
                .is_empty()
        );
        samples.extend(bridge.take_samples());
    }
    assert!(samples.iter().any(|sample| sample.touching));
    assert!(samples.iter().all(|sample| sample.pressure <= 1.0));
}

#[test]
fn the_end_lifts_a_pen_left_in_proximity_and_puts_the_mode_back() {
    let (mut bridge, mut deck) = read_only_intuos5();
    let stroke = intuos5_scripted_stroke();
    // Stop while the pen still hovers.
    for step in stroke.iter().take(3) {
        deck.report(&mut bridge, INTUOS5_PEN_IN, &step.report.encode());
    }
    let owed = bridge.release(true);
    let exit = owed.last().unwrap();
    assert!(!exit.in_proximity && !exit.touching && exit.barrel_buttons == 0);
    assert_eq!(exit.pressure, 0.0);
    assert!(bridge.release(true).is_empty(), "lifted once");
    // A console that changed hands gets nothing, not even a release.
    deck.report(&mut bridge, INTUOS5_PEN_IN, &stroke[0].report.encode());
    assert!(bridge.release(false).is_empty());

    let (outputs, keep) = bridge.restore_mode(Instant::now());
    let keep = keep.expect("the restore is sent");
    let [HostPenBridgeOutput::Submit(restore)] = outputs.as_slice() else {
        panic!("one SET_REPORT: {outputs:?}");
    };
    assert_eq!(restore.payload, vec![2, 0]);
    let cancels = bridge.shutdown(Some(keep));
    assert!(!cancels.contains(&keep));
    assert_eq!(cancels.len(), 1, "the pen poll");
    assert!(bridge.restore_mode(Instant::now()).1.is_none());
}

#[test]
fn a_published_device_never_gets_the_pen_and_cannot_borrow_the_bridges_tokens() {
    let mut deck = Deck::new(WacomIntuos5::new());
    let mut bridge = deck.bridge(HidPublishPolicy::PenAndVendorOnly).unwrap();
    let start = bridge.start(Instant::now()).unwrap();
    let rest = deck.run(&mut bridge, start);
    let [HostPenBridgeOutput::Publish(plan)] = rest.as_slice() else {
        panic!("the plan to publish: {rest:?}");
    };
    assert_eq!(plan.len(), 1, "the pen interface");
    assert_eq!(bridge.pen_index(), Some(0));
    let created = bridge.devices_created(Instant::now()).unwrap();
    assert!(deck.run(&mut bridge, created).is_empty());
    assert!(deck.function.wacom_mode());
    for step in intuos5_scripted_stroke().iter().take(3) {
        assert!(
            deck.report(&mut bridge, INTUOS5_PEN_IN, &step.report.encode())
                .is_empty()
        );
    }
    assert!(!bridge.take_samples().is_empty());
    let request = |index| HidReportRequest::Get {
        index,
        kind: HidReportKind::Feature,
        report_id: 2,
        length: 2,
    };
    for (token, index) in [(HOST_PEN_TOKENS, 0), (7, 1)] {
        assert_eq!(
            bridge.device_request(token, request(index), Instant::now()),
            vec![HostPenBridgeOutput::DeviceReply {
                token,
                result: Err(HidReportFailure::UnknownDevice)
            }]
        );
    }
    let asked = bridge.device_request(7, request(0), Instant::now());
    assert_eq!(
        deck.run(&mut bridge, asked),
        vec![HostPenBridgeOutput::DeviceReply {
            token: 7,
            result: Ok(vec![2, 2])
        }]
    );
}

#[test]
fn a_dedicated_route_needs_an_interface_that_sends_its_packet() {
    let facts = WacomIntuos5::new().captured_facts().unwrap();
    let without_pen: Vec<_> = facts
        .report_descriptors
        .into_iter()
        .filter(|claim| claim.interface != 0)
        .collect();
    let route = HostPenRoute::Dedicated(IntuosPenModel {
        ..crate::host_decoded_intuos_model(0x056a, 0x0317).unwrap()
    });
    assert_eq!(
        HostPenBridge::new(
            facts.snapshot.id,
            without_pen,
            &route,
            HidPublishPolicy::Nothing,
            generation(),
        )
        .unwrap_err(),
        NoPenInterface
    );
}

#[test]
fn a_host_that_falls_behind_loses_the_oldest_samples_and_counts_them() {
    let mut deck = Deck::new(GenericPenTablet::default());
    let mut bridge = deck.bridge(HidPublishPolicy::Nothing).unwrap();
    let start = bridge.start(Instant::now()).unwrap();
    deck.run(&mut bridge, start);
    let hover = |x| GenericPenState {
        x,
        y: 100,
        in_range: true,
        ..GenericPenState::default()
    };
    for x in 0..u16::try_from(HOST_PEN_BACKLOG + 4).unwrap() {
        deck.report(&mut bridge, GENERIC_PEN_IN, &hover(x).encode());
    }
    let samples = bridge.take_samples();
    assert_eq!(samples.len(), HOST_PEN_BACKLOG);
    assert_eq!(samples[0].device_x, 4);
    assert_eq!(bridge.pen_counts().2, 4);
}

#[test]
fn the_mode_read_back_is_bounded_and_names_only_report_two() {
    assert_eq!(read_pen_mode(&[2, 0]), Some(0));
    assert_eq!(read_pen_mode(&[2, 2, 9]), Some(2));
    assert_eq!(read_pen_mode(&[3, 0]), None);
    assert_eq!(read_pen_mode(&[2]), None);
    assert_eq!(read_pen_mode(&[]), None);
}

#[test]
fn a_dedicated_readers_interface_is_found_by_its_packet_not_its_number() {
    let route = HostPenRoute::Dedicated(crate::host_decoded_intuos_model(0x056a, 0x0317).unwrap());
    let claim = |interface: u8, of: u8| InterfaceReportDescriptor {
        interface,
        descriptor: WacomIntuos5::report_descriptor(of).unwrap().to_vec(),
    };
    assert_eq!(route.read_interface(&[claim(0, 1), claim(1, 0)]), Some(1));
    assert_eq!(route.read_interface(&[claim(0, 1), claim(1, 2)]), None);
}
