// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The capture state machine of one `IOUSBHost` device.
//!
//! It decides, without touching the operating system, what the platform code
//! does next: whether transfers may start, what an I/O Kit interest message
//! means, how the device is destroyed, and that it is destroyed only once.

use arcen_usb_bridge::ReleaseReason;
use std::fmt::{Display, Formatter};

/// `kUSBHostMessageDeviceIsRequestingClose`: another client is seizing the
/// device and asks the current owner to close it.
pub const USB_HOST_MESSAGE_DEVICE_IS_REQUESTING_CLOSE: u32 = 0xe000_5003;
/// `kIOMessageServiceIsTerminated`: the service is going away (unplug).
pub const IO_MESSAGE_SERVICE_IS_TERMINATED: u32 = 0xe000_0010;
/// `kIOMessageServiceIsRequestingClose`: the generic I/O Kit close request.
pub const IO_MESSAGE_SERVICE_IS_REQUESTING_CLOSE: u32 = 0xe000_0100;

/// Where a capture is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Captured, being configured; no transfer may start.
    Capturing,
    /// Interfaces and pipes are open; transfers flow.
    Active,
    /// Abort, drain and destroy are running on the release thread.
    Releasing,
    /// The device is back with the operating system (or with whoever asked
    /// for it).
    Released,
}

/// How the device object is destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestroyMode {
    /// `destroy`: the device is reset and its drivers re-match. The ordinary
    /// way a captured device goes back to macOS.
    ResetAndRematch,
    /// `destroyWithOptions: DeviceSurrender`: no reset, no re-match, so the
    /// client that asked for the device gets it as it is.
    Surrender,
}

impl DestroyMode {
    /// The destroy a release for `reason` must use.
    #[must_use]
    pub const fn for_reason(reason: ReleaseReason) -> Self {
        match reason {
            ReleaseReason::RequestedByOs => Self::Surrender,
            ReleaseReason::SessionEnded
            | ReleaseReason::PeerDetached
            | ReleaseReason::UserSwitch
            | ReleaseReason::Unplugged
            | ReleaseReason::Error => Self::ResetAndRematch,
        }
    }
}

/// What one interest message asks the backend to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterestAction {
    /// Not about ownership; nothing to do.
    Ignore,
    /// Another client asks for the device: surrender it now.
    Surrender,
    /// The device is gone.
    Unplugged,
}

/// Classifies an I/O Kit general-interest message.
#[must_use]
pub const fn interest_action(message: u32) -> InterestAction {
    match message {
        USB_HOST_MESSAGE_DEVICE_IS_REQUESTING_CLOSE | IO_MESSAGE_SERVICE_IS_REQUESTING_CLOSE => {
            InterestAction::Surrender
        }
        IO_MESSAGE_SERVICE_IS_TERMINATED => InterestAction::Unplugged,
        _ => InterestAction::Ignore,
    }
}

/// The one release a capture gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasePlan {
    pub reason: ReleaseReason,
    pub destroy: DestroyMode,
}

/// A transition the state machine refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleError {
    /// Only a capture still being set up can become active.
    NotCapturing(Phase),
}

impl Display for LifecycleError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCapturing(phase) => {
                write!(formatter, "the capture cannot become active from {phase:?}")
            }
        }
    }
}

impl std::error::Error for LifecycleError {}

/// The state of one capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lifecycle {
    phase: Phase,
    ended: Option<ReleaseReason>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: Phase::Capturing,
            ended: None,
        }
    }

    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// The reason the operating system ended the capture, if it did.
    #[must_use]
    pub const fn ended(&self) -> Option<ReleaseReason> {
        self.ended
    }

    /// Interfaces and pipes are open.
    ///
    /// # Errors
    ///
    /// Refuses unless the capture is still being set up.
    pub fn activate(&mut self) -> Result<(), LifecycleError> {
        if self.phase != Phase::Capturing {
            return Err(LifecycleError::NotCapturing(self.phase));
        }
        self.phase = Phase::Active;
        Ok(())
    }

    /// Whether a new transfer may be started on the device.
    #[must_use]
    pub const fn admits_transfers(&self) -> bool {
        matches!(self.phase, Phase::Active) && self.ended.is_none()
    }

    /// Records an interest message. Returns the reason to release with when
    /// the message ends the capture and nothing is releasing it yet.
    ///
    /// The first ending wins: an unplug after a requesting-close does not
    /// turn a surrender into a reset.
    pub fn note_interest(&mut self, message: u32) -> Option<ReleaseReason> {
        let reason = match interest_action(message) {
            InterestAction::Ignore => return None,
            InterestAction::Surrender => ReleaseReason::RequestedByOs,
            InterestAction::Unplugged => ReleaseReason::Unplugged,
        };
        if matches!(self.phase, Phase::Releasing | Phase::Released) || self.ended.is_some() {
            return None;
        }
        self.ended = Some(reason);
        Some(reason)
    }

    /// Starts the one release. Returns `None` when a release already started.
    ///
    /// An ending the operating system reported takes precedence over the
    /// reason the caller gives, so a device another client asked for is
    /// surrendered, not reset.
    pub fn begin_release(&mut self, requested: ReleaseReason) -> Option<ReleasePlan> {
        if matches!(self.phase, Phase::Releasing | Phase::Released) {
            return None;
        }
        let reason = self.ended.unwrap_or(requested);
        self.phase = Phase::Releasing;
        Some(ReleasePlan {
            reason,
            destroy: DestroyMode::for_reason(reason),
        })
    }

    /// The release finished.
    pub fn finish_release(&mut self) {
        self.phase = Phase::Released;
    }
}

/// Whether a pipe may be aborted to honour a cancellation.
///
/// `IOUSBHost` aborts every transfer pending on a pipe, not one. Aborting is
/// therefore allowed only when every transfer still pending on that pipe was
/// itself cancelled; otherwise the cancelled transfer is left to finish and
/// the shared engine discards its late completion.
#[must_use]
pub fn pipe_may_abort(pending_cancelled: &[bool]) -> bool {
    !pending_cancelled.is_empty() && pending_cancelled.iter().all(|cancelled| *cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_REASONS: [ReleaseReason; 6] = [
        ReleaseReason::SessionEnded,
        ReleaseReason::PeerDetached,
        ReleaseReason::RequestedByOs,
        ReleaseReason::UserSwitch,
        ReleaseReason::Unplugged,
        ReleaseReason::Error,
    ];

    #[test]
    fn only_a_requesting_close_surrenders() {
        for reason in ALL_REASONS {
            let expected = if reason == ReleaseReason::RequestedByOs {
                DestroyMode::Surrender
            } else {
                DestroyMode::ResetAndRematch
            };
            assert_eq!(DestroyMode::for_reason(reason), expected, "{reason}");
        }
    }

    #[test]
    fn interest_messages_are_classified() {
        assert_eq!(
            interest_action(USB_HOST_MESSAGE_DEVICE_IS_REQUESTING_CLOSE),
            InterestAction::Surrender
        );
        assert_eq!(
            interest_action(IO_MESSAGE_SERVICE_IS_REQUESTING_CLOSE),
            InterestAction::Surrender
        );
        assert_eq!(
            interest_action(IO_MESSAGE_SERVICE_IS_TERMINATED),
            InterestAction::Unplugged
        );
        // kIOMessageServiceWasClosed, kIOMessageServiceBusyStateChange.
        assert_eq!(interest_action(0xe000_0110), InterestAction::Ignore);
        assert_eq!(interest_action(0xe000_0120), InterestAction::Ignore);
    }

    #[test]
    fn transfers_start_only_while_active_and_not_ended() {
        let mut lifecycle = Lifecycle::new();
        assert!(!lifecycle.admits_transfers());
        lifecycle.activate().unwrap();
        assert!(lifecycle.admits_transfers());
        assert_eq!(
            lifecycle.activate(),
            Err(LifecycleError::NotCapturing(Phase::Active))
        );
        lifecycle.note_interest(USB_HOST_MESSAGE_DEVICE_IS_REQUESTING_CLOSE);
        assert!(!lifecycle.admits_transfers());
    }

    #[test]
    fn a_requesting_close_makes_the_release_a_surrender() {
        let mut lifecycle = Lifecycle::new();
        lifecycle.activate().unwrap();
        assert_eq!(
            lifecycle.note_interest(USB_HOST_MESSAGE_DEVICE_IS_REQUESTING_CLOSE),
            Some(ReleaseReason::RequestedByOs)
        );
        // An unplug racing in behind it neither re-triggers nor downgrades it.
        assert_eq!(
            lifecycle.note_interest(IO_MESSAGE_SERVICE_IS_TERMINATED),
            None
        );
        assert_eq!(
            lifecycle.begin_release(ReleaseReason::SessionEnded),
            Some(ReleasePlan {
                reason: ReleaseReason::RequestedByOs,
                destroy: DestroyMode::Surrender,
            })
        );
    }

    #[test]
    fn an_unplug_releases_with_a_plain_destroy() {
        let mut lifecycle = Lifecycle::new();
        lifecycle.activate().unwrap();
        assert_eq!(
            lifecycle.note_interest(IO_MESSAGE_SERVICE_IS_TERMINATED),
            Some(ReleaseReason::Unplugged)
        );
        assert_eq!(
            lifecycle.begin_release(ReleaseReason::PeerDetached),
            Some(ReleasePlan {
                reason: ReleaseReason::Unplugged,
                destroy: DestroyMode::ResetAndRematch,
            })
        );
    }

    #[test]
    fn the_device_is_released_exactly_once() {
        for reason in ALL_REASONS {
            let mut lifecycle = Lifecycle::new();
            lifecycle.activate().unwrap();
            let plan = lifecycle.begin_release(reason).unwrap();
            assert_eq!(plan.reason, reason);
            assert_eq!(lifecycle.phase(), Phase::Releasing);
            assert_eq!(lifecycle.begin_release(ReleaseReason::Error), None);
            assert_eq!(
                lifecycle.note_interest(USB_HOST_MESSAGE_DEVICE_IS_REQUESTING_CLOSE),
                None
            );
            lifecycle.finish_release();
            assert_eq!(lifecycle.phase(), Phase::Released);
            assert_eq!(lifecycle.begin_release(reason), None);
            assert!(!lifecycle.admits_transfers());
        }
    }

    #[test]
    fn a_capture_that_fails_during_setup_still_releases_once() {
        let mut lifecycle = Lifecycle::new();
        let plan = lifecycle.begin_release(ReleaseReason::Error).unwrap();
        assert_eq!(plan.destroy, DestroyMode::ResetAndRematch);
        assert_eq!(
            lifecycle.activate(),
            Err(LifecycleError::NotCapturing(Phase::Releasing))
        );
        assert_eq!(lifecycle.begin_release(ReleaseReason::Error), None);
    }

    #[test]
    fn a_requesting_close_during_setup_surrenders_too() {
        let mut lifecycle = Lifecycle::new();
        assert_eq!(
            lifecycle.note_interest(IO_MESSAGE_SERVICE_IS_REQUESTING_CLOSE),
            Some(ReleaseReason::RequestedByOs)
        );
        assert_eq!(
            lifecycle
                .begin_release(ReleaseReason::Error)
                .map(|plan| plan.destroy),
            Some(DestroyMode::Surrender)
        );
    }

    #[test]
    fn a_pipe_is_aborted_only_when_everything_on_it_was_cancelled() {
        assert!(!pipe_may_abort(&[]));
        assert!(pipe_may_abort(&[true]));
        assert!(pipe_may_abort(&[true, true]));
        assert!(!pipe_may_abort(&[true, false]));
        assert!(!pipe_may_abort(&[false]));
    }
}
