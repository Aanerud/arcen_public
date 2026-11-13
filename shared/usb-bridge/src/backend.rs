// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The boundary between the portable exporter engine and a platform's
//! physical-device I/O (ADR 0019 §2).
//!
//! A platform captures a device however its operating system allows — a root
//! helper with `IOUSBHost` `DeviceCapture`, an Accessory Access attachment,
//! libusb during migration, a synthetic device in tests — and presents the
//! result as a [`CapturedUsbDevice`]. Acquisition is deliberately not part of
//! the trait: capture-by-root and attach-by-user have different shapes.
//!
//! Everything a platform could get wrong in the same way twice stays on this
//! side: admission, cancellation bookkeeping, locally answered standard
//! requests, status mapping and release accounting live in
//! [`crate::exporter`].

use crate::{
    AlternateSetting, DeviceSnapshot, EndpointAddress, InterfaceNumber, InterfaceReportDescriptor,
    SetupPacket, UrbId, UrbStatus, UsbSpeed,
};
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// What a backend read from the device itself after capturing it.
///
/// The exporter re-runs the shared admission policy on these facts
/// ([`crate::verify_captured_tablet`]), so a platform cannot bridge a device
/// by describing it differently from what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedDeviceFacts {
    /// Identity, device class and the parsed active configuration.
    pub snapshot: DeviceSnapshot,
    /// Negotiated speed.
    pub speed: UsbSpeed,
    /// HID report descriptor of every HID interface, by interface number.
    pub report_descriptors: Vec<InterfaceReportDescriptor>,
}

/// Closed portable outcome of one backend transfer.
///
/// Shared code maps it to [`UrbStatus`] ([`BackendStatus::urb_status`]); a
/// platform never invents a wire status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendStatus {
    Completed,
    Stall,
    TimedOut,
    Cancelled,
    Disconnected,
    Protocol,
    Io,
}

impl BackendStatus {
    /// The wire status for this outcome.
    #[must_use]
    pub const fn urb_status(self) -> UrbStatus {
        match self {
            Self::Completed => UrbStatus::Success,
            Self::Stall => UrbStatus::Stall,
            Self::TimedOut => UrbStatus::TimedOut,
            Self::Cancelled => UrbStatus::Cancelled,
            Self::Disconnected => UrbStatus::Disconnected,
            Self::Protocol => UrbStatus::Protocol,
            Self::Io => UrbStatus::Io,
        }
    }
}

/// Why a backend could not start a transfer.
///
/// Reported through [`PendingCompletion::refuse`], so a refused transfer still
/// gets exactly one terminal completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendRefusal {
    /// The device is gone or already released.
    Disconnected,
    /// The request names an endpoint or interface the device does not have.
    UnknownTarget,
    /// The backend does not implement this kind of transfer.
    Unsupported,
    /// The platform could not allocate or queue the transfer.
    Exhausted,
}

impl BackendRefusal {
    /// The terminal status a refused transfer completes with.
    #[must_use]
    pub const fn status(self) -> BackendStatus {
        match self {
            Self::Disconnected => BackendStatus::Disconnected,
            Self::UnknownTarget | Self::Unsupported => BackendStatus::Protocol,
            Self::Exhausted => BackendStatus::Io,
        }
    }
}

/// Why a captured device goes back to the local operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReleaseReason {
    /// The bridge session ended normally.
    SessionEnded,
    /// The peer (Deck or Pier) detached the device.
    PeerDetached,
    /// The operating system asked for the device back (`IOUSBHost`
    /// `kUSBHostMessageDeviceIsRequestingClose`, an Accessory Access detach).
    RequestedByOs,
    /// Fast user switching or console logout.
    UserSwitch,
    /// The device was unplugged.
    Unplugged,
    /// The bridge failed and is releasing to fail closed.
    Error,
}

impl ReleaseReason {
    /// Stable snake-case name for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionEnded => "session_ended",
            Self::PeerDetached => "peer_detached",
            Self::RequestedByOs => "requested_by_os",
            Self::UserSwitch => "user_switch",
            Self::Unplugged => "unplugged",
            Self::Error => "error",
        }
    }
}

impl Display for ReleaseReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a release achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The device is back with the operating system's drivers.
    Restored,
    /// Transfers still running hold the device; the last one to finish
    /// releases it.
    Deferred,
    /// The release ran and the platform reported a failure.
    Failed(String),
}

/// One control-pipe operation the device itself must perform.
///
/// Standard requests the bridge answers locally (`SET_ADDRESS`, a matching
/// `SET_CONFIGURATION`) never reach a backend; see
/// [`crate::control_dispatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOperation {
    /// A request sent to the device on the default pipe. For an IN request
    /// `in_length` is the most the device may return and `data` is empty; for
    /// an OUT request `data` is the payload and `in_length` is zero.
    Transfer {
        setup: SetupPacket,
        data: Vec<u8>,
        in_length: usize,
    },
    /// `SET_INTERFACE`, performed through the platform's interface API.
    SelectAlternateSetting {
        interface: InterfaceNumber,
        alternate: AlternateSetting,
    },
    /// `CLEAR_FEATURE(ENDPOINT_HALT)`, performed through the platform's pipe API.
    ClearHalt { endpoint: EndpointAddress },
}

impl ControlOperation {
    /// Whether this operation returns data from the device.
    #[must_use]
    pub const fn is_in(&self) -> bool {
        matches!(self, Self::Transfer { setup, .. } if setup.request_type & 0x80 != 0)
    }
}

/// Receives terminal transfer outcomes from a backend, on whatever thread the
/// platform completes them.
pub trait CompletionSink: Send + Sync {
    fn complete(&self, urb: UrbId, status: BackendStatus, data: Vec<u8>);
}

/// The obligation to complete one admitted transfer exactly once.
///
/// [`PendingCompletion::complete`] consumes it, so a second completion does
/// not compile. Dropping it without completing — a backend bug, or a panic
/// unwinding a transfer thread — completes the transfer with
/// [`BackendStatus::Io`], so the peer never waits on a request nobody owns.
#[must_use = "every admitted transfer must be completed exactly once"]
pub struct PendingCompletion {
    urb: UrbId,
    cancelled: Arc<AtomicBool>,
    sink: Option<Arc<dyn CompletionSink>>,
}

impl std::fmt::Debug for PendingCompletion {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingCompletion")
            .field("urb", &self.urb)
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl PendingCompletion {
    /// Creates the obligation for `urb`, reporting to `sink`.
    ///
    /// `cancelled` is shared with whoever may cancel the transfer; polling
    /// backends read it through [`Self::is_cancelled`].
    pub fn new(urb: UrbId, cancelled: Arc<AtomicBool>, sink: Arc<dyn CompletionSink>) -> Self {
        Self {
            urb,
            cancelled,
            sink: Some(sink),
        }
    }

    /// The transfer this completion belongs to.
    #[must_use]
    pub const fn urb(&self) -> UrbId {
        self.urb
    }

    /// Whether the transfer was cancelled or the exporter is shutting down.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Reports the transfer's terminal outcome.
    pub fn complete(mut self, status: BackendStatus, data: Vec<u8>) {
        if let Some(sink) = self.sink.take() {
            sink.complete(self.urb, status, data);
        }
    }

    /// Completes a transfer the backend could not start.
    pub fn refuse(self, refusal: BackendRefusal) {
        self.complete(refusal.status(), Vec::new());
    }
}

impl Drop for PendingCompletion {
    fn drop(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.complete(self.urb, BackendStatus::Io, Vec::new());
        }
    }
}

/// One captured physical device.
///
/// Dropping it must release the device and return it to the local operating
/// system exactly as [`CapturedUsbDevice::release`] does; release must happen
/// exactly once whichever way the device goes.
///
/// Every `submit_*` call owns its [`PendingCompletion`] and must complete it
/// exactly once, from any thread, possibly before returning. A backend never
/// calls back into the exporter from inside a completion except through the
/// [`CompletionSink`], and never destroys the device from a completion handler
/// (`IOUSBHost` documents that it deadlocks).
pub trait CapturedUsbDevice: Send {
    /// Descriptors the backend read itself after capture.
    fn facts(&self) -> &CapturedDeviceFacts;

    /// Starts one control-pipe operation.
    fn submit_control(
        &self,
        operation: ControlOperation,
        timeout: Duration,
        completion: PendingCompletion,
    );

    /// Starts one interrupt IN transfer of at most `length` bytes.
    ///
    /// There is no deadline: the transfer stays pending until the device
    /// reports, the transfer is cancelled, or the device fails. Returning a
    /// timeout on every idle poll made a Linux host reset an otherwise
    /// correctly enumerated tablet after a few seconds. A backend whose
    /// platform call can only poll uses [`hold_until_report`].
    fn submit_interrupt_in(
        &self,
        endpoint: EndpointAddress,
        length: usize,
        completion: PendingCompletion,
    );

    /// Starts one interrupt OUT transfer.
    fn submit_interrupt_out(
        &self,
        endpoint: EndpointAddress,
        data: Vec<u8>,
        timeout: Duration,
        completion: PendingCompletion,
    );

    /// Asks the backend to abandon `urb` early.
    ///
    /// Advisory: the exporter has already marked the transfer cancelled and
    /// discards its late completion. A backend that cannot abort one transfer
    /// lets it finish.
    fn cancel(&self, urb: UrbId);

    /// Returns the device to the local operating system.
    fn release(self: Box<Self>, reason: ReleaseReason) -> ReleaseOutcome;
}

/// One poll of a backend that can only wait for an interrupt report with a
/// timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptPoll {
    /// The device sent a report.
    Report(Vec<u8>),
    /// The poll timed out with nothing to report.
    Idle,
    /// The transfer failed.
    Failed(BackendStatus),
}

/// Keeps an interrupt IN transfer pending across ordinary polling timeouts.
///
/// Polls until a report arrives, the transfer fails, or `completion` is
/// cancelled (which completes it as [`BackendStatus::Cancelled`]). An idle
/// poll is never reported as a timeout; see
/// [`CapturedUsbDevice::submit_interrupt_in`].
pub fn hold_until_report(
    completion: &PendingCompletion,
    mut poll: impl FnMut() -> InterruptPoll,
) -> (BackendStatus, Vec<u8>) {
    while !completion.is_cancelled() {
        match poll() {
            InterruptPoll::Report(data) => return (BackendStatus::Completed, data),
            InterruptPoll::Idle => {}
            InterruptPoll::Failed(status) => return (status, Vec::new()),
        }
    }
    (BackendStatus::Cancelled, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(UrbId, BackendStatus, Vec<u8>)>>);

    impl CompletionSink for Recorder {
        fn complete(&self, urb: UrbId, status: BackendStatus, data: Vec<u8>) {
            self.0.lock().unwrap().push((urb, status, data));
        }
    }

    fn urb(value: u32) -> UrbId {
        UrbId::new(NonZeroU32::new(value).unwrap())
    }

    fn pending(recorder: &Arc<Recorder>, flag: &Arc<AtomicBool>) -> PendingCompletion {
        PendingCompletion::new(urb(1), Arc::clone(flag), Arc::clone(recorder) as _)
    }

    #[test]
    fn every_backend_status_maps_to_one_wire_status() {
        let pairs = [
            (BackendStatus::Completed, UrbStatus::Success),
            (BackendStatus::Stall, UrbStatus::Stall),
            (BackendStatus::TimedOut, UrbStatus::TimedOut),
            (BackendStatus::Cancelled, UrbStatus::Cancelled),
            (BackendStatus::Disconnected, UrbStatus::Disconnected),
            (BackendStatus::Protocol, UrbStatus::Protocol),
            (BackendStatus::Io, UrbStatus::Io),
        ];
        for (backend, wire) in pairs {
            assert_eq!(backend.urb_status(), wire);
        }
    }

    #[test]
    fn a_dropped_completion_still_completes_once_as_io() {
        let recorder = Arc::new(Recorder::default());
        let flag = Arc::new(AtomicBool::new(false));
        drop(pending(&recorder, &flag));
        pending(&recorder, &flag).complete(BackendStatus::Completed, vec![7]);
        pending(&recorder, &flag).refuse(BackendRefusal::UnknownTarget);
        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec![
                (urb(1), BackendStatus::Io, Vec::new()),
                (urb(1), BackendStatus::Completed, vec![7]),
                (urb(1), BackendStatus::Protocol, Vec::new()),
            ]
        );
    }

    #[test]
    fn a_completion_dropped_by_a_panicking_transfer_thread_is_reported() {
        let recorder = Arc::new(Recorder::default());
        let flag = Arc::new(AtomicBool::new(false));
        let completion = pending(&recorder, &flag);
        let joined = std::thread::spawn(move || {
            let _owned = completion;
            panic!("transfer thread failed");
        })
        .join();
        assert!(joined.is_err());
        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec![(urb(1), BackendStatus::Io, Vec::new())]
        );
    }

    #[test]
    fn holding_skips_idle_polls_until_a_report() {
        let recorder = Arc::new(Recorder::default());
        let flag = Arc::new(AtomicBool::new(false));
        let completion = pending(&recorder, &flag);
        let mut polls = 0;
        let outcome = hold_until_report(&completion, || {
            polls += 1;
            if polls < 50 {
                InterruptPoll::Idle
            } else {
                InterruptPoll::Report(vec![1, 2, 3])
            }
        });
        assert_eq!(outcome, (BackendStatus::Completed, vec![1, 2, 3]));
        assert_eq!(polls, 50);
        completion.complete(outcome.0, outcome.1);
    }

    #[test]
    fn holding_ends_as_cancelled_not_timed_out() {
        let recorder = Arc::new(Recorder::default());
        let flag = Arc::new(AtomicBool::new(false));
        let completion = pending(&recorder, &flag);
        let mut polls = 0;
        let outcome = hold_until_report(&completion, || {
            polls += 1;
            if polls == 3 {
                flag.store(true, Ordering::Release);
            }
            InterruptPoll::Idle
        });
        assert_eq!(outcome, (BackendStatus::Cancelled, Vec::new()));
        let failed = hold_until_report(
            &pending(&recorder, &Arc::new(AtomicBool::new(false))),
            || InterruptPoll::Failed(BackendStatus::Disconnected),
        );
        assert_eq!(failed, (BackendStatus::Disconnected, Vec::new()));
    }

    #[test]
    fn refusals_complete_with_a_terminal_status() {
        assert_eq!(
            BackendRefusal::Disconnected.status(),
            BackendStatus::Disconnected
        );
        assert_eq!(
            BackendRefusal::UnknownTarget.status(),
            BackendStatus::Protocol
        );
        assert_eq!(
            BackendRefusal::Unsupported.status(),
            BackendStatus::Protocol
        );
        assert_eq!(BackendRefusal::Exhausted.status(), BackendStatus::Io);
    }
}
