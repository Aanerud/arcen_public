// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The exporter engine (ADR 0019 §2): everything between a peer's URB
//! requests and a captured device that does not depend on the operating
//! system.
//!
//! [`UsbExporter`] is generic over [`CapturedUsbDevice`], so the same engine
//! runs in the macOS root helper, in a Deck process that captured through
//! Accessory Access, in a future non-Apple client, and in tests. It owns:
//!
//! - re-running the shared admission policy on the descriptors the backend
//!   read itself ([`verify_captured_tablet`]);
//! - generation binding and URB admission against [`MAX_IN_FLIGHT_URBS`] and
//!   [`MAX_TRANSFER_BYTES`];
//! - cancellation bookkeeping in [`InFlightLedger`], with tombstones so a
//!   cancelled transfer's id cannot start a second transfer while the first
//!   still runs;
//! - the standard requests the bridge answers locally ([`control_dispatch`]);
//! - status mapping and completion bounds;
//! - releasing the device exactly once, whichever way the session ends.
//!
//! It spawns no threads and performs no I/O; completions arrive from the
//! backend through a bounded queue.

use crate::backend::{
    BackendRefusal, BackendStatus, CapturedDeviceFacts, CapturedUsbDevice, CompletionSink,
    ControlOperation, PendingCompletion, ReleaseOutcome, ReleaseReason,
};
use crate::{
    AlternateSetting, AttachmentGeneration, DeviceProfile, DeviceSnapshot, EndpointAddress,
    HardUsbTabletDenial, InFlightLedger, InterfaceNumber, InterfaceRule, LedgerError,
    MAX_IN_FLIGHT_URBS, MAX_TRANSFER_BYTES, NotPenTablet, PenTabletProfile, PolicyDenial,
    SetupPacket, TransferDirection, TransferKind, UrbId, UrbRecord, UrbStatus,
    authorize_hard_usb_tablet, evaluate_profile,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Interface class every bridged interface must report.
///
/// A vendor id is only a claim — anything can assert any vendor's — so
/// admissibility rests on what the device *is*: every interface must be HID,
/// and the HID report descriptors must classify as a pen tablet
/// ([`crate::classify_pen_tablet`]). Combined with [`evaluate_profile`]'s
/// permanently prohibited class list, that keeps storage, hubs, smart card and
/// wireless-controller devices, and every keyboard, mouse and gamepad, out of
/// the bridge whatever identity they present.
pub const USB_CLASS_HID: u8 = 0x03;

/// How long [`UsbExporter::shutdown`] waits for running transfers to report.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_millis(1_500);

const USB_REQUEST_CLEAR_FEATURE: u8 = 1;
const USB_REQUEST_SET_ADDRESS: u8 = 5;
const USB_REQUEST_SET_CONFIGURATION: u8 = 9;
const USB_REQUEST_SET_INTERFACE: u8 = 11;
const USB_REQUEST_TYPE_STANDARD: u8 = 0;
const USB_REQUEST_TYPE_MASK: u8 = 0x60;
const USB_RECIPIENT_DEVICE: u8 = 0;
const USB_RECIPIENT_ENDPOINT: u8 = 2;
const USB_RECIPIENT_INTERFACE: u8 = 1;
const USB_RECIPIENT_MASK: u8 = 0x1f;
const USB_FEATURE_ENDPOINT_HALT: u16 = 0;
/// How long one wait for a completion lasts before the stop flag is rechecked.
const COMPLETION_POLL: Duration = Duration::from_millis(100);
/// How long one drain wait lasts during shutdown.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

/// How one control request is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlDispatch {
    /// Answered locally with success; the device never sees it.
    Ack,
    /// `SET_INTERFACE`, performed through the platform's interface API.
    SetInterface { interface: u8, alternate: u8 },
    /// `CLEAR_FEATURE(ENDPOINT_HALT)`, performed through the platform's pipe API.
    ClearHalt(u8),
    /// Answered locally with a stall; the device never sees it.
    Stall,
    /// Sent to the device unchanged.
    Transfer,
}

/// Decides how one control request reaches a captured device.
///
/// The device is already addressed and configured by the local operating
/// system, so `SET_ADDRESS` is acknowledged without reaching it, and a
/// `SET_CONFIGURATION` for the active configuration is acknowledged while any
/// other value stalls rather than reconfiguring the device under the capture.
#[must_use]
pub const fn control_dispatch(setup: SetupPacket, configuration_value: u8) -> ControlDispatch {
    let standard = setup.request_type & USB_REQUEST_TYPE_MASK == USB_REQUEST_TYPE_STANDARD;
    let recipient = setup.request_type & USB_RECIPIENT_MASK;
    if standard && recipient == USB_RECIPIENT_DEVICE && setup.request == USB_REQUEST_SET_ADDRESS {
        return ControlDispatch::Ack;
    }
    if standard
        && recipient == USB_RECIPIENT_DEVICE
        && setup.request == USB_REQUEST_SET_CONFIGURATION
    {
        return if setup.value == configuration_value as u16 {
            ControlDispatch::Ack
        } else {
            ControlDispatch::Stall
        };
    }
    if standard
        && recipient == USB_RECIPIENT_INTERFACE
        && setup.request == USB_REQUEST_SET_INTERFACE
    {
        return ControlDispatch::SetInterface {
            interface: setup.index.to_le_bytes()[0],
            alternate: setup.value.to_le_bytes()[0],
        };
    }
    if standard
        && recipient == USB_RECIPIENT_ENDPOINT
        && setup.request == USB_REQUEST_CLEAR_FEATURE
        && setup.value == USB_FEATURE_ENDPOINT_HALT
    {
        return ControlDispatch::ClearHalt(setup.index.to_le_bytes()[0]);
    }
    ControlDispatch::Transfer
}

/// Rejects a device that is not wholly a HID device.
///
/// # Errors
///
/// Returns a message naming the first non-HID class, or an empty
/// configuration.
pub fn admissible_tablet(snapshot: &DeviceSnapshot) -> Result<(), String> {
    if snapshot.configuration.interfaces.is_empty() {
        return Err("device exposes no interfaces".to_owned());
    }
    // 0x00 means "class is declared per interface", which the loop below then
    // checks. Any other device-level class must itself be HID.
    if snapshot.device_class != 0x00 && snapshot.device_class != USB_CLASS_HID {
        return Err(format!(
            "device class {:#04x} is not a HID device",
            snapshot.device_class
        ));
    }
    for interface in &snapshot.configuration.interfaces {
        if interface.class != USB_CLASS_HID {
            return Err(format!(
                "interface {} class {:#04x} is not HID",
                interface.number.0, interface.class
            ));
        }
    }
    Ok(())
}

/// The exact profile of the device being examined.
///
/// Derived from the device itself, so it cannot reject anything on its own; it
/// keeps [`evaluate_profile`]'s prohibited-class and internal-consistency
/// checks in the path. Admissibility is decided by [`admissible_tablet`] and
/// the report-descriptor classification.
#[must_use]
pub fn tablet_profile_for(snapshot: &DeviceSnapshot) -> DeviceProfile {
    DeviceProfile {
        name: format!(
            "pen-tablet-{:04x}-{:04x}",
            snapshot.id.vendor_id, snapshot.id.product_id
        ),
        vendor_id: snapshot.id.vendor_id,
        product_id: snapshot.id.product_id,
        minimum_bcd_device: snapshot.id.bcd_device,
        maximum_bcd_device: snapshot.id.bcd_device,
        interfaces: snapshot
            .configuration
            .interfaces
            .iter()
            .map(|interface| InterfaceRule {
                number: interface.number,
                alternate_setting: interface.alternate_setting,
                class: interface.class,
                subclass: interface.subclass,
                protocol: interface.protocol,
            })
            .collect(),
    }
}

/// Why a captured device may not be bridged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRefusal {
    /// The report descriptors do not describe a pen tablet. Not a failure: the
    /// device is simply not a candidate.
    NotPenTablet(NotPenTablet),
    /// A pen tablet that fails the shared Hard USB tablet authorization.
    Denied(HardUsbTabletDenial),
    /// A pen tablet whose descriptors break a bridge invariant.
    Inadmissible(String),
    /// The shared profile policy refused it.
    DeniedByProfile(PolicyDenial),
}

impl Display for CaptureRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPenTablet(reason) => write!(formatter, "not a pen tablet: {reason}"),
            Self::Denied(denial) => Display::fmt(denial, formatter),
            Self::Inadmissible(reason) => formatter.write_str(reason),
            Self::DeniedByProfile(denial) => write!(formatter, "denied by profile: {denial}"),
        }
    }
}

impl std::error::Error for CaptureRefusal {}

/// Runs every shared admission check against what a backend read itself.
///
/// The same authorization the host applies ([`authorize_hard_usb_tablet`]),
/// then [`admissible_tablet`] and [`evaluate_profile`]. Run in the process
/// that holds the device, so a less trusted process cannot talk it into
/// bridging an unapproved device (ADR 0011).
///
/// # Errors
///
/// Returns the first [`CaptureRefusal`].
pub fn verify_captured_tablet(
    facts: &CapturedDeviceFacts,
) -> Result<PenTabletProfile, CaptureRefusal> {
    let profile = authorize_hard_usb_tablet(facts.snapshot.device_class, &facts.report_descriptors)
        .map_err(|denial| match denial {
            HardUsbTabletDenial::NotPenTablet(reason) => CaptureRefusal::NotPenTablet(reason),
            other => CaptureRefusal::Denied(other),
        })?;
    for described in &facts.report_descriptors {
        let present = facts
            .snapshot
            .configuration
            .interfaces
            .iter()
            .any(|interface| interface.number.0 == described.interface);
        if !present {
            return Err(CaptureRefusal::Inadmissible(format!(
                "a report descriptor names interface {}, which the configuration does not have",
                described.interface
            )));
        }
    }
    admissible_tablet(&facts.snapshot).map_err(CaptureRefusal::Inadmissible)?;
    evaluate_profile(true, &tablet_profile_for(&facts.snapshot), &facts.snapshot)
        .map_err(CaptureRefusal::DeniedByProfile)?;
    Ok(profile)
}

/// One URB request as the peer sent it, already decoded by the transport.
///
/// Mirrors the wire header (`arcen_protocol::UsbUrbSubmitHeader`) without
/// depending on the protocol crate, which depends on this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrbSubmission {
    pub generation: AttachmentGeneration,
    pub urb_id: UrbId,
    pub endpoint: EndpointAddress,
    pub transfer_kind: TransferKind,
    pub timeout_ms: u32,
    pub declared_length: u32,
    pub setup: Option<SetupPacket>,
}

/// One terminal URB outcome ready for the transport to encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedCompletion {
    pub generation: AttachmentGeneration,
    pub urb_id: UrbId,
    pub status: UrbStatus,
    pub data: Vec<u8>,
}

/// Why the exporter refused a request or stopped.
///
/// Every variant ends the bridge session; the texts are sent to the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExporterError {
    GenerationChanged,
    DuplicateUrb,
    CancelledUrbReused,
    InFlightLimit,
    TransferTooLarge,
    CancellationGenerationMismatch,
    TombstoneLimit,
    Stopped,
    QueueClosed,
    StatePoisoned,
    CompletionsPoisoned,
    /// The device ended under the bridge (see [`EndSignal`]).
    DeviceEnded(ReleaseReason),
}

impl Display for ExporterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::GenerationChanged => "physical USB attachment generation changed",
            Self::DuplicateUrb => "physical USB submit reused an in-flight URB id",
            Self::CancelledUrbReused => "physical USB submit reused a cancelled URB id",
            Self::InFlightLimit => "physical USB in-flight URB limit exceeded",
            Self::TransferTooLarge => "physical USB transfer exceeds the bridge limit",
            Self::CancellationGenerationMismatch => "physical USB cancellation generation mismatch",
            Self::TombstoneLimit => "physical USB cancellation tombstone limit exceeded",
            Self::Stopped => "physical USB completion pump stopped",
            Self::QueueClosed => "physical USB completion worker ended",
            Self::StatePoisoned => "physical USB state lock poisoned",
            Self::CompletionsPoisoned => "physical USB completion lock poisoned",
            Self::DeviceEnded(reason) => {
                return write!(formatter, "physical USB device ended: {reason}");
            }
        };
        formatter.write_str(text)
    }
}

impl std::error::Error for ExporterError {}

/// Lets a backend report that the device ended under the bridge — the
/// operating system asked for it back, the user switched away, it was
/// unplugged.
///
/// The first reason wins. Queued completions are still delivered; after them
/// [`UsbExporter::next_completion`] returns [`ExporterError::DeviceEnded`], new
/// submissions are refused, and the release uses this reason.
#[derive(Debug, Clone)]
pub struct EndSignal {
    ended: Arc<Mutex<Option<ReleaseReason>>>,
}

impl EndSignal {
    pub fn signal(&self, reason: ReleaseReason) {
        let mut ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
        if ended.is_none() {
            *ended = Some(reason);
        }
    }
}

struct RawCompletion {
    urb: UrbId,
    status: BackendStatus,
    data: Vec<u8>,
}

/// The engine's end of every [`PendingCompletion`].
struct QueueSink {
    sender: SyncSender<RawCompletion>,
    /// Admitted transfers whose completion has not been queued yet.
    outstanding: AtomicUsize,
}

impl CompletionSink for QueueSink {
    fn complete(&self, urb: UrbId, status: BackendStatus, data: Vec<u8>) {
        // The receiver outlives every legitimate sender; after the exporter is
        // gone a late completion has nobody to tell.
        let _ = self.sender.send(RawCompletion { urb, status, data });
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

/// In-flight bookkeeping. Held only briefly; nothing blocks under it.
#[derive(Default)]
struct LedgerState {
    generation: Option<AttachmentGeneration>,
    ledger: InFlightLedger,
    /// Cancellation flag of every in-flight transfer, shared with its
    /// [`PendingCompletion`].
    flags: BTreeMap<UrbId, Arc<AtomicBool>>,
    /// Cancelled transfers whose late completion has not arrived yet.
    tombstones: BTreeSet<UrbId>,
}

impl LedgerState {
    fn bind_generation(&mut self, generation: AttachmentGeneration) -> Result<(), ExporterError> {
        match self.generation {
            None => {
                self.generation = Some(generation);
                Ok(())
            }
            Some(expected) if expected == generation => Ok(()),
            Some(_) => Err(ExporterError::GenerationChanged),
        }
    }

    /// Decides whether one submission may start a transfer, and records it.
    ///
    /// Together with [`Self::record_cancellation`] this bounds how many
    /// transfers a connected peer can have running — in the macOS helper,
    /// inside a root process.
    fn admit(
        &mut self,
        submission: &UrbSubmission,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<(), ExporterError> {
        self.bind_generation(submission.generation)?;
        if self.ledger.contains(submission.urb_id) {
            return Err(ExporterError::DuplicateUrb);
        }
        // A cancelled URB keeps a tombstone until its late completion arrives
        // and clears it. Accepting the same id again inside that window is what
        // let a submit/cancel loop start transfers without bound: cancellation
        // drops the id from the ledger, so the ceiling below stopped
        // constraining anything while every cancelled transfer still ran.
        if self.tombstones.contains(&submission.urb_id) {
            return Err(ExporterError::CancelledUrbReused);
        }
        if self.ledger.len() >= MAX_IN_FLIGHT_URBS {
            return Err(ExporterError::InFlightLimit);
        }
        let declared_length = usize::try_from(submission.declared_length)
            .map_err(|_| ExporterError::TransferTooLarge)?;
        self.ledger
            .submit(
                submission.urb_id,
                UrbRecord {
                    generation: submission.generation,
                    endpoint: submission.endpoint,
                    transfer_kind: submission.transfer_kind,
                    declared_length,
                    deadline_millis: submission.timeout_ms,
                },
            )
            .map_err(|error| match error {
                LedgerError::Capacity => ExporterError::InFlightLimit,
                _ => ExporterError::DuplicateUrb,
            })?;
        self.flags.insert(submission.urb_id, Arc::clone(cancelled));
        Ok(())
    }

    /// Records one cancellation. Returns whether the transfer was in flight.
    fn record_cancellation(
        &mut self,
        generation: AttachmentGeneration,
        urb_id: UrbId,
    ) -> Result<bool, ExporterError> {
        self.bind_generation(generation)?;
        match self.ledger.cancel(generation, urb_id) {
            Ok(_) => {
                if let Some(flag) = self.flags.remove(&urb_id) {
                    flag.store(true, Ordering::Release);
                }
                if self.tombstones.len() >= MAX_IN_FLIGHT_URBS {
                    return Err(ExporterError::TombstoneLimit);
                }
                self.tombstones.insert(urb_id);
                Ok(true)
            }
            // Not in flight: the peer still gets its terminal completion.
            Err(LedgerError::Unknown(_)) => Ok(false),
            Err(_) => Err(ExporterError::CancellationGenerationMismatch),
        }
    }

    /// Accepts one backend completion, or returns `None` for a late
    /// completion of a cancelled or drained transfer.
    fn accept(&mut self, raw: RawCompletion) -> Option<ExportedCompletion> {
        let generation = self.generation?;
        let Ok(record) = self.ledger.complete(generation, raw.urb) else {
            self.tombstones.remove(&raw.urb);
            return None;
        };
        self.flags.remove(&raw.urb);
        // A backend may never return more than the peer asked for.
        let (status, data) = if raw.data.len() > record.declared_length {
            (UrbStatus::Protocol, Vec::new())
        } else {
            (raw.status.urb_status(), raw.data)
        };
        Some(ExportedCompletion {
            generation,
            urb_id: raw.urb,
            status,
            data,
        })
    }

    /// Marks every in-flight transfer cancelled and forgets it.
    fn drain(&mut self) -> Vec<UrbId> {
        let ids: Vec<UrbId> = self.flags.keys().copied().collect();
        for flag in self.flags.values() {
            flag.store(true, Ordering::Release);
        }
        self.flags.clear();
        if let Some(generation) = self.generation {
            let _ = self.ledger.drain_generation(generation);
        }
        ids
    }
}

/// One captured device bridged to one peer.
///
/// Safe to share between a request thread (`submit`, `cancel`) and a
/// completion thread (`next_completion`). The device is released exactly
/// once: by [`Self::release`], or on drop.
pub struct UsbExporter {
    device: Mutex<Option<Box<dyn CapturedUsbDevice>>>,
    facts: CapturedDeviceFacts,
    profile: PenTabletProfile,
    sink: Arc<QueueSink>,
    completions: Mutex<Receiver<RawCompletion>>,
    stopping: AtomicBool,
    ended: Arc<Mutex<Option<ReleaseReason>>>,
    state: Mutex<LedgerState>,
}

impl std::fmt::Debug for UsbExporter {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsbExporter")
            .field("id", &self.facts.snapshot.id)
            .field("released", &self.is_released())
            .finish_non_exhaustive()
    }
}

impl UsbExporter {
    /// Takes ownership of a captured device after verifying it with the shared
    /// policy.
    ///
    /// # Errors
    ///
    /// When [`verify_captured_tablet`] refuses the device, it is released at
    /// once (fail closed) with [`ReleaseReason::Error`], and the refusal is
    /// returned with the release outcome.
    pub fn new(
        device: Box<dyn CapturedUsbDevice>,
    ) -> Result<Self, (CaptureRefusal, ReleaseOutcome)> {
        let facts = device.facts().clone();
        let profile = match verify_captured_tablet(&facts) {
            Ok(profile) => profile,
            Err(refusal) => return Err((refusal, device.release(ReleaseReason::Error))),
        };
        // Bounded. At most `MAX_IN_FLIGHT_URBS` live transfers plus that many
        // cancelled-but-still-running ones can exist, and each completes
        // exactly once, so correct operation never fills it; it stops a peer
        // that no longer drains from growing the queue without limit.
        let (sender, receiver) =
            std::sync::mpsc::sync_channel(MAX_IN_FLIGHT_URBS.saturating_mul(2));
        Ok(Self {
            device: Mutex::new(Some(device)),
            facts,
            profile,
            sink: Arc::new(QueueSink {
                sender,
                outstanding: AtomicUsize::new(0),
            }),
            completions: Mutex::new(receiver),
            stopping: AtomicBool::new(false),
            ended: Arc::new(Mutex::new(None)),
            state: Mutex::new(LedgerState::default()),
        })
    }

    /// What the backend read from the device.
    #[must_use]
    pub const fn facts(&self) -> &CapturedDeviceFacts {
        &self.facts
    }

    /// The pen-tablet classification the device was admitted with.
    #[must_use]
    pub const fn profile(&self) -> &PenTabletProfile {
        &self.profile
    }

    /// A handle the backend uses to report that the device ended.
    #[must_use]
    pub fn end_signal(&self) -> EndSignal {
        EndSignal {
            ended: Arc::clone(&self.ended),
        }
    }

    /// The reason the device ended under the bridge, if it did.
    #[must_use]
    pub fn ended(&self) -> Option<ReleaseReason> {
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the device has been released.
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.device
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
    }

    /// Admits one URB and starts it on the device, or answers it locally.
    ///
    /// # Errors
    ///
    /// Returns an [`ExporterError`] on generation change, duplicate or
    /// tombstoned URB id, the in-flight bound, an oversized transfer, or a
    /// device that ended. Each ends the session.
    pub fn submit(&self, submission: UrbSubmission, payload: &[u8]) -> Result<(), ExporterError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(ExporterError::Stopped);
        }
        if let Some(reason) = self.ended() {
            return Err(ExporterError::DeviceEnded(reason));
        }
        let oversized = usize::try_from(submission.declared_length)
            .map_or(true, |declared| declared > MAX_TRANSFER_BYTES);
        if oversized || payload.len() > MAX_TRANSFER_BYTES {
            return Err(ExporterError::TransferTooLarge);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.lock_state()?.admit(&submission, &cancelled)?;
        self.sink.outstanding.fetch_add(1, Ordering::AcqRel);
        let completion = PendingCompletion::new(
            submission.urb_id,
            cancelled,
            Arc::clone(&self.sink) as Arc<dyn CompletionSink>,
        );
        self.dispatch(&submission, payload, completion);
        Ok(())
    }

    fn dispatch(&self, submission: &UrbSubmission, payload: &[u8], completion: PendingCompletion) {
        let timeout = Duration::from_millis(u64::from(submission.timeout_ms.max(1)));
        match submission.transfer_kind {
            TransferKind::Control => {
                let Some(setup) = submission.setup else {
                    completion.complete(BackendStatus::Protocol, Vec::new());
                    return;
                };
                let configuration_value = self.facts.snapshot.configuration.configuration_value;
                let operation = match control_dispatch(setup, configuration_value) {
                    ControlDispatch::Ack => {
                        completion.complete(BackendStatus::Completed, Vec::new());
                        return;
                    }
                    ControlDispatch::Stall => {
                        completion.complete(BackendStatus::Stall, Vec::new());
                        return;
                    }
                    ControlDispatch::SetInterface {
                        interface,
                        alternate,
                    } => ControlOperation::SelectAlternateSetting {
                        interface: InterfaceNumber(interface),
                        alternate: AlternateSetting(alternate),
                    },
                    ControlDispatch::ClearHalt(endpoint) => ControlOperation::ClearHalt {
                        endpoint: EndpointAddress(endpoint),
                    },
                    ControlDispatch::Transfer if setup.request_type & 0x80 != 0 => {
                        let Ok(in_length) = usize::try_from(submission.declared_length) else {
                            completion.complete(BackendStatus::Protocol, Vec::new());
                            return;
                        };
                        ControlOperation::Transfer {
                            setup,
                            data: Vec::new(),
                            in_length,
                        }
                    }
                    ControlDispatch::Transfer => ControlOperation::Transfer {
                        setup,
                        data: payload.to_vec(),
                        in_length: 0,
                    },
                };
                self.with_device(completion, |device, completion| {
                    device.submit_control(operation, timeout, completion);
                });
            }
            TransferKind::Interrupt if submission.endpoint.direction() == TransferDirection::In => {
                let Ok(length) = usize::try_from(submission.declared_length) else {
                    completion.complete(BackendStatus::Protocol, Vec::new());
                    return;
                };
                let endpoint = submission.endpoint;
                self.with_device(completion, |device, completion| {
                    device.submit_interrupt_in(endpoint, length, completion);
                });
            }
            TransferKind::Interrupt => {
                let endpoint = submission.endpoint;
                let data = payload.to_vec();
                self.with_device(completion, |device, completion| {
                    device.submit_interrupt_out(endpoint, data, timeout, completion);
                });
            }
        }
    }

    fn with_device(
        &self,
        completion: PendingCompletion,
        start: impl FnOnce(&dyn CapturedUsbDevice, PendingCompletion),
    ) {
        let device = self.device.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(device) = device.as_deref() {
            start(device, completion);
            return;
        }
        drop(device);
        completion.refuse(BackendRefusal::Disconnected);
    }

    /// Cancels one URB and returns its terminal completion.
    ///
    /// The peer always receives exactly one `Cancelled` completion for a
    /// cancel, whether or not the URB was still in flight; the transfer's own
    /// late completion is discarded by its tombstone.
    ///
    /// # Errors
    ///
    /// Returns an [`ExporterError`] on generation mismatch or tombstone
    /// exhaustion.
    pub fn cancel(
        &self,
        generation: AttachmentGeneration,
        urb_id: UrbId,
    ) -> Result<ExportedCompletion, ExporterError> {
        let recorded = self.lock_state()?.record_cancellation(generation, urb_id);
        // Tombstone exhaustion ends the session, but the transfer it refused
        // to tombstone was in flight and is already flagged: abort it too.
        let in_flight = matches!(recorded, Ok(true) | Err(ExporterError::TombstoneLimit));
        if in_flight {
            let device = self.device.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(device) = device.as_deref() {
                device.cancel(urb_id);
            }
        }
        recorded?;
        Ok(ExportedCompletion {
            generation,
            urb_id,
            status: UrbStatus::Cancelled,
            data: Vec::new(),
        })
    }

    /// Blocks for the next completion, skipping late completions of cancelled
    /// transfers.
    ///
    /// # Errors
    ///
    /// Returns [`ExporterError::Stopped`] once [`Self::shutdown`] has run, and
    /// [`ExporterError::DeviceEnded`] once the device ended and every queued
    /// completion was delivered.
    pub fn next_completion(&self) -> Result<ExportedCompletion, ExporterError> {
        loop {
            if let Some(completion) = self.next_completion_within(COMPLETION_POLL)? {
                return Ok(completion);
            }
        }
    }

    /// Like [`Self::next_completion`], but returns `Ok(None)` when nothing
    /// completed within `wait` — for a pump that has other work, and for
    /// proving a transfer is still held.
    ///
    /// # Errors
    ///
    /// As [`Self::next_completion`].
    pub fn next_completion_within(
        &self,
        wait: Duration,
    ) -> Result<Option<ExportedCompletion>, ExporterError> {
        let deadline = Instant::now() + wait;
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Err(ExporterError::Stopped);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let raw = {
                let receiver = self
                    .completions
                    .lock()
                    .map_err(|_| ExporterError::CompletionsPoisoned)?;
                match receiver.recv_timeout(remaining.min(COMPLETION_POLL)) {
                    Ok(raw) => raw,
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(reason) = self.ended() {
                            return Err(ExporterError::DeviceEnded(reason));
                        }
                        if Instant::now() >= deadline {
                            return Ok(None);
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(ExporterError::QueueClosed);
                    }
                }
            };
            if let Some(completion) = self.lock_state()?.accept(raw) {
                return Ok(Some(completion));
            }
        }
    }

    /// Stops the completion pump, cancels every in-flight transfer and waits
    /// up to [`SHUTDOWN_DRAIN`] for running transfers to report.
    ///
    /// Only the first call waits; later calls (the release after an explicit
    /// shutdown) return at once, so a transfer stuck in the platform delays a
    /// session's end by one drain period, not two. Whatever is still running
    /// then holds the device until it ends ([`ReleaseOutcome::Deferred`]).
    pub fn shutdown(&self) {
        // Release the pump before waiting for transfers, or joining it deadlocks.
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        let drained = self
            .state
            .lock()
            .map(|mut state| state.drain())
            .unwrap_or_default();
        if !drained.is_empty() {
            let device = self.device.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(device) = device.as_deref() {
                for urb in drained {
                    device.cancel(urb);
                }
            }
        }
        let deadline = Instant::now() + SHUTDOWN_DRAIN;
        while self.sink.outstanding.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            if let Ok(receiver) = self.completions.lock() {
                let _ = receiver.recv_timeout(SHUTDOWN_POLL);
            }
        }
    }

    /// Shuts down and returns the device to the local operating system.
    ///
    /// Returns `None` if it was already released. When the backend signalled
    /// an end ([`EndSignal`]), that reason is used instead of `reason`, so a
    /// device the operating system asked for is surrendered, not reset.
    pub fn release(&self, reason: ReleaseReason) -> Option<(ReleaseReason, ReleaseOutcome)> {
        self.shutdown();
        let device = self
            .device
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        let reason = self.ended().unwrap_or(reason);
        Some((reason, device.release(reason)))
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, LedgerState>, ExporterError> {
        self.state.lock().map_err(|_| ExporterError::StatePoisoned)
    }
}

impl Drop for UsbExporter {
    fn drop(&mut self) {
        // Every exit path, including a panic unwinding through the session,
        // must give the tablet back, or it stays dead to the local machine.
        let _ = self.release(ReleaseReason::SessionEnded);
    }
}

#[cfg(test)]
#[path = "exporter_tests.rs"]
mod tests;
