// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use super::*;
use crate::{InterfaceDescriptor, ParsedConfiguration, SyntheticTabletDevice, UsbDeviceId};
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::atomic::AtomicUsize;

/// A backend that records what reaches it and holds every transfer until the
/// test completes it, or until it is cancelled, which completes it the way a
/// platform abort does.
struct Scripted {
    facts: CapturedDeviceFacts,
    log: Arc<Mutex<Vec<String>>>,
    held: Arc<Mutex<Vec<PendingCompletion>>>,
    releases: Arc<AtomicUsize>,
    reasons: Arc<Mutex<Vec<ReleaseReason>>>,
    stuck: Arc<AtomicBool>,
    released: bool,
}

#[derive(Clone, Default)]
struct Probe {
    log: Arc<Mutex<Vec<String>>>,
    held: Arc<Mutex<Vec<PendingCompletion>>>,
    releases: Arc<AtomicUsize>,
    reasons: Arc<Mutex<Vec<ReleaseReason>>>,
    /// When set, cancellation does not complete held transfers: a platform
    /// call that cannot be aborted.
    stuck: Arc<AtomicBool>,
}

impl Probe {
    fn device(&self) -> Box<dyn CapturedUsbDevice> {
        self.device_with(SyntheticTabletDevice::captured_facts().unwrap())
    }

    fn device_with(&self, facts: CapturedDeviceFacts) -> Box<dyn CapturedUsbDevice> {
        Box::new(Scripted {
            facts,
            log: Arc::clone(&self.log),
            held: Arc::clone(&self.held),
            releases: Arc::clone(&self.releases),
            reasons: Arc::clone(&self.reasons),
            stuck: Arc::clone(&self.stuck),
            released: false,
        })
    }

    fn exporter(&self) -> UsbExporter {
        UsbExporter::new(self.device()).unwrap()
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn releases(&self) -> usize {
        self.releases.load(Ordering::Acquire)
    }

    fn take_held(&self) -> Vec<PendingCompletion> {
        std::mem::take(&mut *self.held.lock().unwrap())
    }
}

impl CapturedUsbDevice for Scripted {
    fn facts(&self) -> &CapturedDeviceFacts {
        &self.facts
    }

    fn submit_control(
        &self,
        operation: ControlOperation,
        _timeout: Duration,
        completion: PendingCompletion,
    ) {
        self.log.lock().unwrap().push(format!(
            "control {:?} {operation:?}",
            completion.urb().get()
        ));
        self.held.lock().unwrap().push(completion);
    }

    fn submit_interrupt_in(
        &self,
        endpoint: EndpointAddress,
        length: usize,
        completion: PendingCompletion,
    ) {
        self.log.lock().unwrap().push(format!(
            "in {} ep={:#04x} len={length}",
            completion.urb().get(),
            endpoint.0
        ));
        self.held.lock().unwrap().push(completion);
    }

    fn submit_interrupt_out(
        &self,
        endpoint: EndpointAddress,
        data: Vec<u8>,
        _timeout: Duration,
        completion: PendingCompletion,
    ) {
        self.log.lock().unwrap().push(format!(
            "out {} ep={:#04x} {data:?}",
            completion.urb().get(),
            endpoint.0
        ));
        self.held.lock().unwrap().push(completion);
    }

    fn cancel(&self, urb: UrbId) {
        self.log
            .lock()
            .unwrap()
            .push(format!("cancel {}", urb.get()));
        if self.stuck.load(Ordering::Acquire) {
            return;
        }
        let mut held = self.held.lock().unwrap();
        if let Some(index) = held.iter().position(|pending| pending.urb() == urb) {
            let pending = held.remove(index);
            assert!(
                pending.is_cancelled(),
                "cancel reached the backend before the flag"
            );
            pending.complete(BackendStatus::Cancelled, Vec::new());
        }
    }

    fn release(mut self: Box<Self>, reason: ReleaseReason) -> ReleaseOutcome {
        self.released = true;
        self.releases.fetch_add(1, Ordering::AcqRel);
        self.reasons.lock().unwrap().push(reason);
        ReleaseOutcome::Restored
    }
}

impl Drop for Scripted {
    fn drop(&mut self) {
        if !self.released {
            self.releases.fetch_add(1, Ordering::AcqRel);
        }
    }
}

fn generation() -> AttachmentGeneration {
    AttachmentGeneration::new(NonZeroU64::new(7).unwrap())
}

fn urb(value: u32) -> UrbId {
    UrbId::new(NonZeroU32::new(value).unwrap())
}

fn interrupt_in(urb_id: u32) -> UrbSubmission {
    UrbSubmission {
        generation: generation(),
        urb_id: urb(urb_id),
        endpoint: EndpointAddress(0x81),
        transfer_kind: TransferKind::Interrupt,
        timeout_ms: 100,
        declared_length: 10,
        setup: None,
    }
}

fn control(urb_id: u32, setup: SetupPacket, declared_length: u32) -> UrbSubmission {
    UrbSubmission {
        generation: generation(),
        urb_id: urb(urb_id),
        endpoint: EndpointAddress(if setup.request_type & 0x80 == 0 {
            0x00
        } else {
            0x80
        }),
        transfer_kind: TransferKind::Control,
        timeout_ms: 500,
        declared_length,
        setup: Some(setup),
    }
}

fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> SetupPacket {
    SetupPacket {
        request_type,
        request,
        value,
        index,
        length,
    }
}

// --- control_dispatch ------------------------------------------------------

#[test]
fn set_address_is_acknowledged_locally() {
    let request = setup(0, USB_REQUEST_SET_ADDRESS, 3, 0, 0);
    assert_eq!(control_dispatch(request, 1), ControlDispatch::Ack);
}

#[test]
fn the_active_configuration_is_acknowledged_and_any_other_stalls() {
    let matching = setup(0, USB_REQUEST_SET_CONFIGURATION, 1, 0, 0);
    assert_eq!(control_dispatch(matching, 1), ControlDispatch::Ack);
    let mismatched = setup(0, USB_REQUEST_SET_CONFIGURATION, 7, 0, 0);
    assert_eq!(control_dispatch(mismatched, 1), ControlDispatch::Stall);
}

#[test]
fn endpoint_halt_maps_to_clear_halt() {
    let request = setup(
        0x02,
        USB_REQUEST_CLEAR_FEATURE,
        USB_FEATURE_ENDPOINT_HALT,
        0x81,
        0,
    );
    assert_eq!(
        control_dispatch(request, 1),
        ControlDispatch::ClearHalt(0x81)
    );
}

#[test]
fn set_interface_and_class_requests_reach_the_device() {
    let set_interface = setup(0x01, USB_REQUEST_SET_INTERFACE, 2, 1, 0);
    assert_eq!(
        control_dispatch(set_interface, 1),
        ControlDispatch::SetInterface {
            interface: 1,
            alternate: 2
        }
    );
    // HID SET_IDLE: class request to an interface.
    let set_idle = setup(0x21, 0x0a, 0, 0, 0);
    assert_eq!(control_dispatch(set_idle, 1), ControlDispatch::Transfer);
    // SET_ADDRESS sent to an interface is not the standard device request.
    let odd = setup(0x01, USB_REQUEST_SET_ADDRESS, 3, 0, 0);
    assert_eq!(control_dispatch(odd, 1), ControlDispatch::Transfer);
}

// --- admission policy ------------------------------------------------------

fn snapshot_with(
    product_id: u16,
    device_class: u8,
    interfaces: &[(u8, u8, u8, u8)],
) -> DeviceSnapshot {
    DeviceSnapshot {
        id: UsbDeviceId {
            vendor_id: 0x1234,
            product_id,
            bcd_device: 0x0100,
        },
        device_class,
        configuration: ParsedConfiguration {
            configuration_value: 1,
            interfaces: interfaces
                .iter()
                .map(|&(number, class, subclass, protocol)| InterfaceDescriptor {
                    number: InterfaceNumber(number),
                    alternate_setting: AlternateSetting(0),
                    class,
                    subclass,
                    protocol,
                    endpoints: Vec::new(),
                })
                .collect(),
        },
    }
}

#[test]
fn the_measured_three_interface_tablet_is_still_admissible() {
    // The device the bridge was developed against, kept as a regression.
    let snapshot = snapshot_with(
        0x0317,
        0x00,
        &[
            (0, 0x03, 0x00, 0x00),
            (1, 0x03, 0x00, 0x00),
            (2, 0x03, 0x01, 0x02),
        ],
    );
    assert_eq!(admissible_tablet(&snapshot), Ok(()));
    assert!(evaluate_profile(true, &tablet_profile_for(&snapshot), &snapshot).is_ok());
}

#[test]
fn a_different_tablet_model_is_admitted_rather_than_pinned_out() {
    for (product_id, interfaces) in [
        (0x033c_u16, &[(0_u8, 0x03_u8, 0x00_u8, 0x00_u8)][..]),
        (0x0357, &[(0, 0x03, 0x00, 0x00), (1, 0x03, 0x00, 0x00)][..]),
        (0x03aa, &[(0, 0x03, 0x01, 0x02)][..]),
    ] {
        let snapshot = snapshot_with(product_id, 0x00, interfaces);
        assert_eq!(admissible_tablet(&snapshot), Ok(()));
        assert!(evaluate_profile(true, &tablet_profile_for(&snapshot), &snapshot).is_ok());
    }
}

#[test]
fn a_non_hid_interface_is_refused_whatever_vendor_id_it_claims() {
    for class in [0x08_u8, 0x09, 0x0b, 0x01, 0xe0] {
        let snapshot = snapshot_with(
            0x0317,
            0x00,
            &[(0, 0x03, 0x00, 0x00), (1, class, 0x00, 0x00)],
        );
        assert!(
            admissible_tablet(&snapshot).is_err(),
            "interface class {class:#04x} should be refused"
        );
    }
}

#[test]
fn express_key_keyboards_and_boot_mice_are_admissible_hid_interfaces() {
    let with_keyboard = snapshot_with(
        0x0317,
        0x00,
        &[(0, 0x03, 0x00, 0x00), (1, 0x03, 0x01, 0x01)],
    );
    assert_eq!(admissible_tablet(&with_keyboard), Ok(()));
    let with_mouse = snapshot_with(
        0x0317,
        0x00,
        &[(0, 0x03, 0x00, 0x00), (1, 0x03, 0x01, 0x02)],
    );
    assert_eq!(admissible_tablet(&with_mouse), Ok(()));
}

#[test]
fn a_non_hid_device_class_or_no_interfaces_is_refused() {
    assert!(admissible_tablet(&snapshot_with(0x0317, 0x08, &[(0, 0x03, 0x00, 0x00)])).is_err());
    assert!(admissible_tablet(&snapshot_with(0x0317, 0x00, &[])).is_err());
}

#[test]
fn prohibited_classes_are_still_refused_by_the_shared_policy() {
    // `tablet_profile_for` is derived from the device, so this proves the
    // prohibited-class rules remain in the path and are not made vacuous.
    let snapshot = snapshot_with(0x0317, 0x09, &[(0, 0x03, 0x00, 0x00)]);
    assert!(evaluate_profile(true, &tablet_profile_for(&snapshot), &snapshot).is_err());
}

#[test]
fn the_synthetic_tablet_passes_verification() {
    let facts = SyntheticTabletDevice::captured_facts().unwrap();
    assert!(verify_captured_tablet(&facts).is_ok());
}

#[test]
fn verification_refuses_what_the_backend_misdescribes() {
    let mut not_a_tablet = SyntheticTabletDevice::captured_facts().unwrap();
    // A boot mouse report descriptor.
    not_a_tablet.report_descriptors[0].descriptor = vec![
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29,
        0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
        0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95,
        0x02, 0x81, 0x06, 0xc0, 0xc0,
    ];
    assert!(matches!(
        verify_captured_tablet(&not_a_tablet),
        Err(CaptureRefusal::NotPenTablet(_))
    ));

    let mut phantom_interface = SyntheticTabletDevice::captured_facts().unwrap();
    phantom_interface.report_descriptors[0].interface = 5;
    assert!(matches!(
        verify_captured_tablet(&phantom_interface),
        Err(CaptureRefusal::Inadmissible(_))
    ));

    let mut storage_beside = SyntheticTabletDevice::captured_facts().unwrap();
    storage_beside.snapshot.configuration.interfaces[0].class = 0x08;
    assert!(matches!(
        verify_captured_tablet(&storage_beside),
        Err(CaptureRefusal::Inadmissible(_))
    ));
}

#[test]
fn a_refused_device_is_released_at_once() {
    let probe = Probe::default();
    let mut facts = SyntheticTabletDevice::captured_facts().unwrap();
    facts.snapshot.device_class = 0x08;
    let (refusal, outcome) = UsbExporter::new(probe.device_with(facts)).unwrap_err();
    assert!(matches!(refusal, CaptureRefusal::Denied(_)), "{refusal}");
    assert_eq!(outcome, ReleaseOutcome::Restored);
    assert_eq!(probe.releases(), 1);
    assert_eq!(*probe.reasons.lock().unwrap(), vec![ReleaseReason::Error]);
}

// --- URB admission (moved from the macOS helper) ---------------------------

#[test]
fn an_in_flight_urb_id_is_not_admitted_twice() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    exporter.submit(interrupt_in(1), &[]).unwrap();
    assert_eq!(
        exporter.submit(interrupt_in(1), &[]),
        Err(ExporterError::DuplicateUrb)
    );
}

/// After a cancellation the id is gone from the ledger, so the ceiling alone
/// would re-admit it while its transfer still runs. The tombstone refuses it
/// until the transfer's late completion arrives.
#[test]
fn a_cancelled_urb_id_is_not_readmitted_until_its_transfer_reports() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    exporter.submit(interrupt_in(1), &[]).unwrap();
    let cancelled = exporter.cancel(generation(), urb(1)).unwrap();
    assert_eq!(cancelled.status, UrbStatus::Cancelled);
    assert_eq!(
        exporter.submit(interrupt_in(1), &[]),
        Err(ExporterError::CancelledUrbReused)
    );

    // The backend saw the cancel and completed the transfer late; that late
    // completion is still queued, so the tombstone still stands.
    assert!(probe.log().contains(&"cancel 1".to_owned()));
    assert!(probe.take_held().is_empty());

    // Delivering the queue swallows the late completion and clears it.
    exporter.submit(interrupt_in(2), &[]).unwrap();
    probe
        .take_held()
        .into_iter()
        .next()
        .unwrap()
        .complete(BackendStatus::Completed, vec![9]);
    let delivered = exporter.next_completion().unwrap();
    assert_eq!(
        delivered.urb_id,
        urb(2),
        "the cancelled URB's completion leaked"
    );
    exporter.submit(interrupt_in(1), &[]).unwrap();
}

/// A peer that submits and immediately cancels with a fresh id every time is
/// stopped by the tombstone ceiling, because every cancelled transfer keeps
/// its tombstone until it reports back.
#[test]
fn a_submit_cancel_loop_cannot_start_unbounded_transfers() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    let mut live = 0_usize;
    for urb_id in 1..=(u32::try_from(MAX_IN_FLIGHT_URBS).unwrap() * 8) {
        if exporter.submit(interrupt_in(urb_id), &[]).is_err() {
            break;
        }
        live += 1;
        if exporter.cancel(generation(), urb(urb_id)).is_err() {
            break;
        }
    }
    assert!(
        live <= MAX_IN_FLIGHT_URBS + 1,
        "a submit/cancel loop left {live} transfers running"
    );
}

#[test]
fn a_submit_cancel_loop_on_one_id_starts_exactly_one_transfer() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    let mut admitted = 0_usize;
    for _ in 0..1_000 {
        if exporter.submit(interrupt_in(1), &[]).is_err() {
            continue;
        }
        admitted += 1;
        exporter.cancel(generation(), urb(1)).unwrap();
    }
    assert_eq!(admitted, 1);
}

#[test]
fn the_in_flight_ceiling_is_enforced() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    for urb_id in 1..=u32::try_from(MAX_IN_FLIGHT_URBS).unwrap() {
        exporter.submit(interrupt_in(urb_id), &[]).unwrap();
    }
    assert_eq!(
        exporter.submit(interrupt_in(9_999), &[]),
        Err(ExporterError::InFlightLimit)
    );
}

#[test]
fn a_second_generation_is_refused() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    exporter.submit(interrupt_in(1), &[]).unwrap();
    let mut other = interrupt_in(2);
    other.generation = AttachmentGeneration::new(NonZeroU64::new(8).unwrap());
    assert_eq!(
        exporter.submit(other, &[]),
        Err(ExporterError::GenerationChanged)
    );
    assert_eq!(
        exporter.cancel(other.generation, urb(1)),
        Err(ExporterError::GenerationChanged)
    );
}

#[test]
fn an_oversized_transfer_is_refused_before_admission() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    let mut big = interrupt_in(1);
    big.declared_length = u32::try_from(MAX_TRANSFER_BYTES).unwrap() + 1;
    assert_eq!(
        exporter.submit(big, &[]),
        Err(ExporterError::TransferTooLarge)
    );
    assert!(probe.log().is_empty());
}

// --- dispatch and completion ----------------------------------------------

#[test]
fn locally_answered_requests_never_reach_the_device() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    exporter
        .submit(
            control(1, setup(0, USB_REQUEST_SET_ADDRESS, 4, 0, 0), 0),
            &[],
        )
        .unwrap();
    exporter
        .submit(
            control(2, setup(0, USB_REQUEST_SET_CONFIGURATION, 2, 0, 0), 0),
            &[],
        )
        .unwrap();
    let first = exporter.next_completion().unwrap();
    let second = exporter.next_completion().unwrap();
    assert_eq!((first.urb_id, first.status), (urb(1), UrbStatus::Success));
    assert_eq!((second.urb_id, second.status), (urb(2), UrbStatus::Stall));
    assert!(probe.log().is_empty(), "{:?}", probe.log());
}

#[test]
fn device_requests_carry_their_direction_and_length() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    exporter
        .submit(control(1, setup(0x81, 6, 0x2200, 0, 64), 64), &[])
        .unwrap();
    exporter
        .submit(control(2, setup(0x21, 0x0a, 0, 0, 0), 0), &[])
        .unwrap();
    exporter
        .submit(
            control(3, setup(0x01, USB_REQUEST_SET_INTERFACE, 1, 0, 0), 0),
            &[],
        )
        .unwrap();
    let mut out = interrupt_in(4);
    out.endpoint = EndpointAddress(0x02);
    out.declared_length = 2;
    exporter.submit(out, &[5, 6]).unwrap();
    assert_eq!(
        probe.log(),
        vec![
            "control 1 Transfer { setup: SetupPacket { request_type: 129, request: 6, value: 8704, index: 0, length: 64 }, data: [], in_length: 64 }".to_owned(),
            "control 2 Transfer { setup: SetupPacket { request_type: 33, request: 10, value: 0, index: 0, length: 0 }, data: [], in_length: 0 }".to_owned(),
            "control 3 SelectAlternateSetting { interface: InterfaceNumber(0), alternate: AlternateSetting(1) }".to_owned(),
            "out 4 ep=0x02 [5, 6]".to_owned(),
        ]
    );
}

#[test]
fn a_backend_returning_more_than_was_asked_completes_as_protocol() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    exporter.submit(interrupt_in(1), &[]).unwrap();
    probe
        .take_held()
        .into_iter()
        .next()
        .unwrap()
        .complete(BackendStatus::Completed, vec![0; 11]);
    let completion = exporter.next_completion().unwrap();
    assert_eq!(completion.status, UrbStatus::Protocol);
    assert!(completion.data.is_empty());
}

#[test]
fn a_dropped_backend_completion_reaches_the_peer_as_io() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    exporter.submit(interrupt_in(1), &[]).unwrap();
    drop(probe.take_held());
    let completion = exporter.next_completion().unwrap();
    assert_eq!(
        (completion.urb_id, completion.status),
        (urb(1), UrbStatus::Io)
    );
}

// --- end and release -------------------------------------------------------

#[test]
fn dropping_the_exporter_releases_once() {
    let probe = Probe::default();
    drop(probe.exporter());
    assert_eq!(probe.releases(), 1);
    assert_eq!(
        *probe.reasons.lock().unwrap(),
        vec![ReleaseReason::SessionEnded]
    );
}

#[test]
fn release_is_idempotent_and_refuses_later_transfers() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    exporter.submit(interrupt_in(1), &[]).unwrap();
    // Shutdown cancels the held transfer, which the backend aborts.
    assert_eq!(
        exporter.release(ReleaseReason::PeerDetached),
        Some((ReleaseReason::PeerDetached, ReleaseOutcome::Restored))
    );
    assert_eq!(exporter.release(ReleaseReason::Error), None);
    assert!(exporter.is_released());
    assert_eq!(
        exporter.submit(interrupt_in(2), &[]),
        Err(ExporterError::Stopped)
    );
    assert_eq!(exporter.next_completion(), Err(ExporterError::Stopped));
    drop(exporter);
    assert_eq!(probe.releases(), 1);
    assert!(probe.log().contains(&"cancel 1".to_owned()));
}

#[test]
fn an_ended_device_delivers_what_it_queued_then_ends_with_its_reason() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    exporter.submit(interrupt_in(1), &[]).unwrap();
    let signal = exporter.end_signal();
    probe
        .take_held()
        .into_iter()
        .next()
        .unwrap()
        .complete(BackendStatus::Disconnected, Vec::new());
    signal.signal(ReleaseReason::RequestedByOs);
    signal.signal(ReleaseReason::Unplugged);
    assert_eq!(
        exporter.next_completion().unwrap().status,
        UrbStatus::Disconnected
    );
    assert_eq!(
        exporter.next_completion(),
        Err(ExporterError::DeviceEnded(ReleaseReason::RequestedByOs))
    );
    assert_eq!(
        exporter.submit(interrupt_in(2), &[]),
        Err(ExporterError::DeviceEnded(ReleaseReason::RequestedByOs))
    );
    // The OS asked for it: the release surrenders rather than ends a session.
    assert_eq!(
        exporter.release(ReleaseReason::SessionEnded),
        Some((ReleaseReason::RequestedByOs, ReleaseOutcome::Restored))
    );
    assert_eq!(probe.releases(), 1);
}

#[test]
fn error_texts_are_the_ones_the_helper_always_sent() {
    assert_eq!(
        ExporterError::GenerationChanged.to_string(),
        "physical USB attachment generation changed"
    );
    assert_eq!(
        ExporterError::CancelledUrbReused.to_string(),
        "physical USB submit reused a cancelled URB id"
    );
    assert_eq!(
        ExporterError::DeviceEnded(ReleaseReason::UserSwitch).to_string(),
        "physical USB device ended: user_switch"
    );
}

#[test]
fn one_exporter_serves_a_request_thread_and_a_completion_thread() {
    fn shareable<T: Send + Sync>() {}
    shareable::<UsbExporter>();
    shareable::<EndSignal>();
}

#[test]
fn a_stuck_transfer_delays_the_end_of_a_session_once() {
    let probe = Probe::default();
    let exporter = probe.exporter();
    probe.stuck.store(true, Ordering::Release);
    exporter.submit(interrupt_in(1), &[]).unwrap();
    let started = Instant::now();
    exporter.shutdown();
    assert!(started.elapsed() >= SHUTDOWN_DRAIN);
    let again = Instant::now();
    assert!(exporter.release(ReleaseReason::SessionEnded).is_some());
    assert!(
        again.elapsed() < SHUTDOWN_DRAIN / 2,
        "the release waited again"
    );
    // The stuck transfer was flagged, so a polling backend would still stop.
    assert!(probe.take_held()[0].is_cancelled());
}
