// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! S1 and S2 of ADR 0019 §9: the exporter engine driven end to end through a
//! synthetic tablet, and every exit path of a session counted for releases.

use super::*;
use crate::{
    AttachmentGeneration, ExportedCompletion, ExporterError, PenSwitch, PenSwitches, TransferKind,
    UrbStatus, UrbSubmission, UsbExporter,
};
use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};

const NOTHING: Duration = Duration::from_millis(30);

fn generation() -> AttachmentGeneration {
    AttachmentGeneration::new(NonZeroU64::new(1).unwrap())
}

fn urb(value: u32) -> UrbId {
    UrbId::new(NonZeroU32::new(value).unwrap())
}

fn bridge() -> (UsbExporter, SyntheticHandle) {
    let (device, handle) = SyntheticDevice::capture().unwrap();
    (UsbExporter::new(Box::new(device)).unwrap(), handle)
}

fn control_in(
    id: u32,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
) -> UrbSubmission {
    UrbSubmission {
        generation: generation(),
        urb_id: urb(id),
        endpoint: EndpointAddress(0x80),
        transfer_kind: TransferKind::Control,
        timeout_ms: 1_000,
        declared_length: u32::from(length),
        setup: Some(SetupPacket {
            request_type,
            request,
            value,
            index,
            length,
        }),
    }
}

fn control_out(id: u32, request_type: u8, request: u8, value: u16, index: u16) -> UrbSubmission {
    UrbSubmission {
        endpoint: EndpointAddress(0x00),
        declared_length: 0,
        ..control_in(id, request_type, request, value, index, 0)
    }
}

fn interrupt(id: u32, endpoint: u8, length: u32) -> UrbSubmission {
    UrbSubmission {
        generation: generation(),
        urb_id: urb(id),
        endpoint: EndpointAddress(endpoint),
        transfer_kind: TransferKind::Interrupt,
        timeout_ms: 1_000,
        declared_length: length,
        setup: None,
    }
}

fn completion(id: u32, status: UrbStatus, data: &[u8]) -> ExportedCompletion {
    ExportedCompletion {
        generation: generation(),
        urb_id: urb(id),
        status,
        data: data.to_vec(),
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

const DEVICE_DESCRIPTOR: [u8; 18] = [
    18, 1, 0x00, 0x02, 0, 0, 0, 64, 0xff, 0xff, 0xce, 0xa2, 0x00, 0x01, 1, 2, 0, 1,
];
const CONFIGURATION_DESCRIPTOR: [u8; 34] = [
    9, 2, 34, 0, 1, 1, 0, 0x80, 50, // configuration
    9, 4, 0, 0, 1, 3, 0, 0, 0, // HID interface
    9, 0x21, 0x11, 0x01, 0, 1, 0x22, 0x58, 0x00, // HID, 88-byte report descriptor
    7, 5, 0x81, 3, 16, 0, 1, // interrupt IN 1
];

/// S1, golden: the enumeration a Linux host's USB core and HID driver send
/// for the synthetic tablet, then a pen report, a cancelled poll and a
/// configuration the bridge refuses. Pins what the peer receives byte for byte
/// and what reaches the device, in order.
#[test]
fn golden_enumeration_report_and_cancel() {
    let (exporter, handle) = bridge();
    let mut peer = Vec::new();
    let mut step = |submission: UrbSubmission| step_after(&exporter, &mut peer, submission);
    step(control_in(1, 0x80, 6, 0x0100, 0, 64));
    step(control_out(2, 0x00, 5, 7, 0));
    step(control_in(3, 0x80, 6, 0x0100, 0, 18));
    step(control_in(4, 0x80, 6, 0x0200, 0, 9));
    step(control_in(5, 0x80, 6, 0x0200, 0, 34));
    step(control_in(6, 0x80, 6, 0x0300, 0, 255));
    step(control_out(7, 0x00, 9, 1, 0));
    step(control_out(8, 0x21, 0x0a, 0, 0));
    step(control_in(9, 0x81, 6, 0x2200, 0, 88));
    step(control_in(10, 0x80, 6, 0x0600, 0, 10));

    // The HID driver's first poll is held: an idle tablet sends nothing, and
    // nothing is reported, not even a timeout.
    exporter.submit(interrupt(11, 0x81, 16), &[]).unwrap();
    assert_eq!(exporter.next_completion_within(NOTHING), Ok(None));
    assert_eq!(handle.held(), 1);
    handle.push_sample(PenSample {
        x: 0.5,
        y: 0.25,
        pressure: 1.0,
        tilt_x_degrees: 30.0,
        tilt_y_degrees: -45.0,
        switches: PenSwitches::default()
            .with(PenSwitch::InRange, true)
            .with(PenSwitch::Touching, true),
    });
    peer.push(exporter.next_completion().unwrap());

    // The next poll is cancelled; the peer gets exactly one Cancelled.
    exporter.submit(interrupt(12, 0x81, 16), &[]).unwrap();
    peer.push(exporter.cancel(generation(), urb(12)).unwrap());
    assert_eq!(exporter.next_completion_within(NOTHING), Ok(None));

    // A configuration other than the active one stalls without reaching it.
    step_after(&exporter, &mut peer, control_out(13, 0x00, 9, 2, 0));

    assert_eq!(
        peer,
        vec![
            completion(1, UrbStatus::Success, &DEVICE_DESCRIPTOR),
            completion(2, UrbStatus::Success, &[]),
            completion(3, UrbStatus::Success, &DEVICE_DESCRIPTOR),
            completion(4, UrbStatus::Success, &CONFIGURATION_DESCRIPTOR[..9]),
            completion(5, UrbStatus::Success, &CONFIGURATION_DESCRIPTOR),
            completion(6, UrbStatus::Success, &[4, 3, 0x09, 0x04]),
            completion(7, UrbStatus::Success, &[]),
            completion(8, UrbStatus::Success, &[]),
            completion(
                9,
                UrbStatus::Success,
                crate::SYNTHETIC_TABLET_REPORT_DESCRIPTOR
            ),
            completion(10, UrbStatus::Stall, &[]),
            completion(
                11,
                UrbStatus::Success,
                &[1, 3, 0x00, 0x80, 0x00, 0x40, 0xff, 0x03, 0x1e, 0xd3]
            ),
            completion(12, UrbStatus::Cancelled, &[]),
            completion(13, UrbStatus::Stall, &[]),
        ]
    );
    // SET_ADDRESS and both SET_CONFIGURATIONs never reached the device.
    assert_eq!(
        handle.ops(),
        vec![
            SyntheticOp::Control(setup(0x80, 6, 0x0100, 0, 64)),
            SyntheticOp::Control(setup(0x80, 6, 0x0100, 0, 18)),
            SyntheticOp::Control(setup(0x80, 6, 0x0200, 0, 9)),
            SyntheticOp::Control(setup(0x80, 6, 0x0200, 0, 34)),
            SyntheticOp::Control(setup(0x80, 6, 0x0300, 0, 255)),
            SyntheticOp::Control(setup(0x21, 0x0a, 0, 0, 0)),
            SyntheticOp::Control(setup(0x81, 6, 0x2200, 0, 88)),
            SyntheticOp::Control(setup(0x80, 6, 0x0600, 0, 10)),
            SyntheticOp::InterruptIn {
                endpoint: SYNTHETIC_INTERRUPT_IN,
                length: 16
            },
            SyntheticOp::InterruptIn {
                endpoint: SYNTHETIC_INTERRUPT_IN,
                length: 16
            },
            SyntheticOp::Cancel(urb(12)),
        ]
    );
    assert_eq!(handle.tablet().configuration(), 0);
    assert_eq!(handle.tablet().address(), 0);
}

fn step_after(
    exporter: &UsbExporter,
    peer: &mut Vec<ExportedCompletion>,
    submission: UrbSubmission,
) {
    exporter.submit(submission, &[]).unwrap();
    peer.push(exporter.next_completion().unwrap());
}

/// S1, golden: standard requests the bridge hands to the platform's own APIs,
/// and what an unknown target turns into.
#[test]
fn golden_interface_halt_and_unknown_targets() {
    let (exporter, handle) = bridge();
    let mut peer = Vec::new();
    step_after(&exporter, &mut peer, control_out(1, 0x01, 11, 0, 0));
    step_after(&exporter, &mut peer, control_out(2, 0x01, 11, 1, 0));
    step_after(&exporter, &mut peer, control_out(3, 0x02, 1, 0, 0x81));
    step_after(&exporter, &mut peer, control_out(4, 0x02, 1, 0, 0x82));
    step_after(&exporter, &mut peer, interrupt(5, 0x82, 8));
    exporter.submit(interrupt(6, 0x02, 2), &[1, 2]).unwrap();
    peer.push(exporter.next_completion().unwrap());
    assert_eq!(
        peer.iter().map(|done| done.status).collect::<Vec<_>>(),
        vec![
            UrbStatus::Success,
            UrbStatus::Stall,
            UrbStatus::Success,
            UrbStatus::Stall,
            UrbStatus::Protocol,
            UrbStatus::Protocol,
        ]
    );
    assert_eq!(
        handle.ops()[..4],
        [
            SyntheticOp::SelectAlternateSetting {
                interface: InterfaceNumber(0),
                alternate: AlternateSetting(0)
            },
            SyntheticOp::SelectAlternateSetting {
                interface: InterfaceNumber(0),
                alternate: AlternateSetting(1)
            },
            SyntheticOp::ClearHalt(EndpointAddress(0x81)),
            SyntheticOp::ClearHalt(EndpointAddress(0x82)),
        ]
    );
}

/// S1: a report that arrives before anyone polls is delivered to the next
/// poll, truncated to what that poll asked for.
#[test]
fn a_report_waits_for_the_next_poll_and_is_bounded_by_it() {
    let (exporter, handle) = bridge();
    handle.push_report(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    exporter.submit(interrupt(1, 0x81, 4), &[]).unwrap();
    assert_eq!(
        exporter.next_completion().unwrap(),
        completion(1, UrbStatus::Success, &[1, 2, 3, 4])
    );
}

/// S1: shutdown drains the generation: every held poll is cancelled at the
/// device before it is released, and nothing more reaches the peer.
#[test]
fn shutdown_drains_every_held_transfer_before_release() {
    let (exporter, handle) = bridge();
    for id in 1..=3 {
        exporter.submit(interrupt(id, 0x81, 16), &[]).unwrap();
    }
    assert_eq!(handle.held(), 3);
    assert_eq!(
        exporter.release(ReleaseReason::SessionEnded),
        Some((ReleaseReason::SessionEnded, ReleaseOutcome::Restored))
    );
    let ops = handle.ops();
    let tail = &ops[ops.len() - 4..];
    assert!(
        tail[..3]
            .iter()
            .all(|op| matches!(op, SyntheticOp::Cancel(_))),
        "{ops:?}"
    );
    assert_eq!(tail[3], SyntheticOp::Release(ReleaseReason::SessionEnded));
    assert_eq!(handle.held(), 0);
    assert_eq!(exporter.next_completion(), Err(ExporterError::Stopped));
}

/// S1: the in-flight bound holds with real held transfers.
#[test]
fn the_in_flight_bound_holds_with_held_polls() {
    let (exporter, handle) = bridge();
    for id in 1..=u32::try_from(crate::MAX_IN_FLIGHT_URBS).unwrap() {
        exporter.submit(interrupt(id, 0x81, 16), &[]).unwrap();
    }
    assert_eq!(
        exporter.submit(interrupt(10_000, 0x81, 16), &[]),
        Err(ExporterError::InFlightLimit)
    );
    assert_eq!(handle.held(), crate::MAX_IN_FLIGHT_URBS);
}

/// S1, race: reports and cancellations racing on the same polls. Each URB
/// reaches the peer through the pump at most once, a cancelled URB's late
/// completion never does, and no tombstone leaks.
#[test]
fn cancellation_racing_reports_never_double_delivers() {
    let (exporter, handle) = bridge();
    let exporter = Arc::new(exporter);
    // At most half the polls stay held, inside the in-flight bound.
    let rounds = 200_u32;
    let pusher = {
        let handle = handle.clone();
        std::thread::spawn(move || {
            for round in 0..rounds {
                handle.push_report(vec![1, round.to_le_bytes()[0]]);
                std::thread::yield_now();
            }
        })
    };
    let mut cancelled = Vec::new();
    let mut delivered: BTreeMap<UrbId, usize> = BTreeMap::new();
    let mut record = |done: ExportedCompletion| {
        assert_ne!(done.status, UrbStatus::Cancelled, "a late cancel leaked");
        *delivered.entry(done.urb_id).or_default() += 1;
    };
    for id in 1..=rounds {
        exporter.submit(interrupt(id, 0x81, 16), &[]).unwrap();
        if id % 2 == 0 {
            exporter.cancel(generation(), urb(id)).unwrap();
            cancelled.push(urb(id));
        }
        // Drain whatever already completed, so tombstones are cleared.
        while let Some(done) = exporter.next_completion_within(Duration::ZERO).unwrap() {
            record(done);
        }
    }
    pusher.join().unwrap();
    // Feed the remaining polls so every uncancelled URB completes.
    for _ in 0..rounds {
        handle.push_report(vec![2]);
    }
    while let Some(done) = exporter.next_completion_within(NOTHING).unwrap() {
        record(done);
    }
    assert!(delivered.values().all(|count| *count == 1), "{delivered:?}");
    for id in (1..=rounds).filter(|id| id % 2 == 1) {
        assert_eq!(
            delivered.get(&urb(id)),
            Some(&1),
            "poll {id} never completed"
        );
    }
    // Every cancelled id is reusable again: its tombstone was cleared.
    for id in cancelled {
        let reuse = UrbSubmission {
            urb_id: id,
            ..interrupt(1, 0x81, 16)
        };
        assert_eq!(
            exporter.submit(reuse, &[]),
            Ok(()),
            "tombstone for {id:?} leaked"
        );
    }
}

// --- S2: release accounting -----------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Fault {
    Refuse(BackendRefusal),
    DropCompletions,
    PanicOnSubmit,
}

/// A fault-injecting backend over the synthetic tablet. Releases are counted
/// by the synthetic device itself, whether by `release` or by drop.
#[derive(Debug)]
struct Faulty {
    inner: SyntheticDevice,
    fault: Fault,
    explicit_releases: Arc<AtomicUsize>,
}

impl Faulty {
    fn intercept(&self, completion: PendingCompletion) -> Option<PendingCompletion> {
        match self.fault {
            Fault::Refuse(refusal) => {
                completion.refuse(refusal);
                None
            }
            Fault::DropCompletions => {
                drop(completion);
                None
            }
            Fault::PanicOnSubmit => panic!("backend fault injected"),
        }
    }
}

impl CapturedUsbDevice for Faulty {
    fn facts(&self) -> &CapturedDeviceFacts {
        self.inner.facts()
    }

    fn submit_control(
        &self,
        operation: ControlOperation,
        timeout: Duration,
        completion: PendingCompletion,
    ) {
        if let Some(completion) = self.intercept(completion) {
            self.inner.submit_control(operation, timeout, completion);
        }
    }

    fn submit_interrupt_in(
        &self,
        endpoint: EndpointAddress,
        length: usize,
        completion: PendingCompletion,
    ) {
        if let Some(completion) = self.intercept(completion) {
            self.inner.submit_interrupt_in(endpoint, length, completion);
        }
    }

    fn submit_interrupt_out(
        &self,
        endpoint: EndpointAddress,
        data: Vec<u8>,
        timeout: Duration,
        completion: PendingCompletion,
    ) {
        if let Some(completion) = self.intercept(completion) {
            self.inner
                .submit_interrupt_out(endpoint, data, timeout, completion);
        }
    }

    fn cancel(&self, urb: UrbId) {
        self.inner.cancel(urb);
    }

    fn release(self: Box<Self>, reason: ReleaseReason) -> ReleaseOutcome {
        self.explicit_releases.fetch_add(1, Ordering::AcqRel);
        Box::new(self.inner).release(reason)
    }
}

struct Counted {
    exporter: UsbExporter,
    handle: SyntheticHandle,
    explicit: Arc<AtomicUsize>,
}

fn faulty(fault: Fault) -> Counted {
    let (inner, handle) = SyntheticDevice::capture().unwrap();
    let explicit = Arc::new(AtomicUsize::new(0));
    let device = Faulty {
        inner,
        fault,
        explicit_releases: Arc::clone(&explicit),
    };
    Counted {
        exporter: UsbExporter::new(Box::new(device)).unwrap(),
        handle,
        explicit,
    }
}

fn last_release(handle: &SyntheticHandle) -> Option<SyntheticOp> {
    handle
        .ops()
        .into_iter()
        .rev()
        .find(|op| matches!(op, SyntheticOp::Release(_) | SyntheticOp::ReleasedByDrop))
}

#[test]
fn s2_drop_releases_once() {
    let (exporter, handle) = bridge();
    exporter.submit(interrupt(1, 0x81, 16), &[]).unwrap();
    drop(exporter);
    assert_eq!(handle.releases(), 1);
    assert_eq!(
        last_release(&handle),
        Some(SyntheticOp::Release(ReleaseReason::SessionEnded))
    );
}

#[test]
fn s2_a_session_error_releases_once() {
    let (exporter, handle) = bridge();
    exporter.submit(interrupt(1, 0x81, 16), &[]).unwrap();
    let mut stale = interrupt(2, 0x81, 16);
    stale.generation = AttachmentGeneration::new(NonZeroU64::new(9).unwrap());
    let error = exporter.submit(stale, &[]).unwrap_err();
    assert_eq!(error, ExporterError::GenerationChanged);
    assert!(exporter.release(ReleaseReason::Error).is_some());
    drop(exporter);
    assert_eq!(handle.releases(), 1);
    assert_eq!(
        last_release(&handle),
        Some(SyntheticOp::Release(ReleaseReason::Error))
    );
}

#[test]
fn s2_peer_detach_releases_once() {
    let (exporter, handle) = bridge();
    assert_eq!(
        exporter.release(ReleaseReason::PeerDetached),
        Some((ReleaseReason::PeerDetached, ReleaseOutcome::Restored))
    );
    assert_eq!(exporter.release(ReleaseReason::PeerDetached), None);
    drop(exporter);
    assert_eq!(handle.releases(), 1);
}

#[test]
fn s2_requested_close_and_user_switch_release_once_with_their_reason() {
    for reason in [ReleaseReason::RequestedByOs, ReleaseReason::UserSwitch] {
        let (exporter, handle) = bridge();
        exporter.submit(interrupt(1, 0x81, 16), &[]).unwrap();
        exporter.end_signal().signal(reason);
        assert_eq!(
            exporter.next_completion(),
            Err(ExporterError::DeviceEnded(reason))
        );
        drop(exporter);
        assert_eq!(handle.releases(), 1);
        assert_eq!(last_release(&handle), Some(SyntheticOp::Release(reason)));
    }
}

#[test]
fn s2_unplug_completes_held_polls_and_releases_once() {
    let (exporter, handle) = bridge();
    exporter.submit(interrupt(1, 0x81, 16), &[]).unwrap();
    handle.unplug();
    exporter.end_signal().signal(ReleaseReason::Unplugged);
    assert_eq!(
        exporter.next_completion().unwrap(),
        completion(1, UrbStatus::Disconnected, &[])
    );
    assert_eq!(
        exporter.next_completion(),
        Err(ExporterError::DeviceEnded(ReleaseReason::Unplugged))
    );
    drop(exporter);
    assert_eq!(handle.releases(), 1);
    assert_eq!(
        last_release(&handle),
        Some(SyntheticOp::Release(ReleaseReason::Unplugged))
    );
}

#[test]
fn s2_a_panic_holding_the_exporter_releases_once() {
    let (exporter, handle) = bridge();
    let exporter = Arc::new(exporter);
    let held = Arc::clone(&exporter);
    drop(exporter);
    let joined = std::thread::spawn(move || {
        held.submit(interrupt(1, 0x81, 16), &[]).ok();
        panic!("session thread failed");
    })
    .join();
    assert!(joined.is_err());
    assert_eq!(handle.releases(), 1);
    assert_eq!(
        last_release(&handle),
        Some(SyntheticOp::Release(ReleaseReason::SessionEnded))
    );
}

#[test]
fn s2_a_panicking_backend_still_releases_once() {
    let counted = faulty(Fault::PanicOnSubmit);
    let submitted = catch_unwind(AssertUnwindSafe(|| {
        counted.exporter.submit(interrupt(1, 0x81, 16), &[])
    }));
    assert!(submitted.is_err());
    // The transfer the panic abandoned still reached the peer.
    assert_eq!(
        counted.exporter.next_completion().unwrap(),
        completion(1, UrbStatus::Io, &[])
    );
    assert!(counted.exporter.release(ReleaseReason::Error).is_some());
    drop(counted.exporter);
    assert_eq!(counted.handle.releases(), 1);
    assert_eq!(counted.explicit.load(Ordering::Acquire), 1);
}

#[test]
fn s2_refused_and_dropped_transfers_complete_once_and_the_session_goes_on() {
    for (fault, status) in [
        (
            Fault::Refuse(BackendRefusal::Disconnected),
            UrbStatus::Disconnected,
        ),
        (
            Fault::Refuse(BackendRefusal::UnknownTarget),
            UrbStatus::Protocol,
        ),
        (Fault::Refuse(BackendRefusal::Exhausted), UrbStatus::Io),
        (Fault::DropCompletions, UrbStatus::Io),
    ] {
        let counted = faulty(fault);
        for id in 1..=3 {
            counted
                .exporter
                .submit(interrupt(id, 0x81, 16), &[])
                .unwrap();
            assert_eq!(
                counted.exporter.next_completion().unwrap(),
                completion(id, status, &[]),
                "{fault:?}"
            );
        }
        assert_eq!(counted.exporter.next_completion_within(NOTHING), Ok(None));
        drop(counted.exporter);
        assert_eq!(counted.handle.releases(), 1, "{fault:?}");
        assert_eq!(counted.explicit.load(Ordering::Acquire), 1, "{fault:?}");
    }
}

#[test]
fn s2_concurrent_releases_release_once() {
    let (exporter, handle) = bridge();
    let exporter = Arc::new(exporter);
    let threads: Vec<_> = [ReleaseReason::PeerDetached, ReleaseReason::Error]
        .into_iter()
        .map(|reason| {
            let exporter = Arc::clone(&exporter);
            std::thread::spawn(move || exporter.release(reason).is_some())
        })
        .collect();
    let released: usize = threads
        .into_iter()
        .map(|thread| usize::from(thread.join().unwrap()))
        .sum();
    assert_eq!(released, 1);
    drop(exporter);
    assert_eq!(handle.releases(), 1);
}

#[test]
fn s2_a_device_dropped_outside_an_exporter_releases_itself() {
    let (device, handle) = SyntheticDevice::capture().unwrap();
    drop(device);
    assert_eq!(handle.releases(), 1);
    assert_eq!(handle.ops(), vec![SyntheticOp::ReleasedByDrop]);
}
