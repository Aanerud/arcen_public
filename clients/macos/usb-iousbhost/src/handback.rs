// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Handing a captured device back to macOS so it looks like a real unplug
//! and replug.
//!
//! `destroy` after `DeviceCapture` resets the device. The kernel terminates
//! the old `IOUSBHostDevice` and registers a new one "if the reset is
//! successful and the previous object has finished terminating"
//! (`IOUSBHostDevice.h`, `-resetWithError:` / kernel `reset()`). Without
//! anything holding the old object, the new one arrives about 50 ms after
//! the old one went away. Some vendor drivers cannot handle that: Wacom's
//! `WacomTouchDriver` (6.4.9) is still tearing down the old device when the
//! new one arrives, logs `NotificationManager.cpp:206: failed assertion:
//! _CallbackMap[obj_I] == nullptr`, and drops the arrival, so the tablet stays
//! dead on the Mac until its agent restarts. A person unplugging and
//! replugging never produces a gap that short.
//!
//! The hand-back therefore keeps the capture's `IOUSBHostInterface` objects
//! open while the device is destroyed. The interfaces are clients of the old
//! device, and a client that still has its provider open defers the
//! provider's stop (`IOService::didTerminate`), so the old device cannot
//! finish terminating and the port stays empty. Once the old device is seen
//! terminated, the hold lasts [`HANDBACK_HOLD`]; then the interfaces are
//! destroyed, termination completes and the device re-enumerates.
//!
//! This module decides and measures, without touching the operating system:
//! which hand-back a release gets and what the observed timeline means. The
//! timings are logged so a hand-back that did not hold is visible.

use crate::lifecycle::{DestroyMode, ReleasePlan};
use crate::matching::RegistryUsbDevice;
use arcen_usb_bridge::ReleaseReason;
use std::fmt::{Display, Formatter};
use std::time::Duration;

/// How long the port stays empty after the old device terminated, before the
/// held interfaces are let go. Long enough for a user-space driver to finish
/// handling the removal; short against a person's replug.
pub const HANDBACK_HOLD: Duration = Duration::from_millis(1_500);
/// How long the old device may take to be seen terminated after `destroy`.
pub const TERMINATION_WAIT: Duration = Duration::from_secs(2);
/// How long the device may take to re-enumerate once nothing holds it.
pub const REENUMERATION_WAIT: Duration = Duration::from_secs(4);
/// How often the registry is read while a hand-back is under way.
pub const HANDBACK_POLL: Duration = Duration::from_millis(10);
/// A detachment shorter than this is reported as not held.
pub const MIN_DETACHED: Duration = Duration::from_secs(1);

/// How a release hands the device back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handback {
    /// Reset with the interfaces held open, so the old device stays
    /// terminating and the port empty for [`HANDBACK_HOLD`].
    HeldReset,
    /// Reset without a hold: nothing is open to hold the old device with.
    /// The re-enumeration is still observed and logged.
    PlainReset,
    /// Nothing re-enumerates: the device is surrendered to the client that
    /// asked for it, or it is already gone.
    NoReenumeration,
}

impl Handback {
    /// The hand-back `plan` gets when `open_interfaces` interface objects are
    /// still open.
    #[must_use]
    pub const fn for_plan(plan: ReleasePlan, open_interfaces: usize) -> Self {
        match (plan.destroy, plan.reason) {
            (DestroyMode::Surrender, _) | (_, ReleaseReason::Unplugged) => Self::NoReenumeration,
            (DestroyMode::ResetAndRematch, _) if open_interfaces > 0 => Self::HeldReset,
            (DestroyMode::ResetAndRematch, _) => Self::PlainReset,
        }
    }
}

/// What the hand-back saw of the device, as offsets from the `destroy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandbackTimeline {
    old_registry_id: u64,
    location_id: Option<u32>,
    vendor_id: u16,
    product_id: u16,
    terminated: Option<Duration>,
    hold_released: Option<Duration>,
    reenumerated: Option<(Duration, u64)>,
}

/// What a finished hand-back amounted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandbackVerdict {
    /// The old device never went away.
    NotTerminated,
    /// The old device went away and no new one came back in time.
    NotReenumerated { terminated: Duration },
    /// The device came back after `detached` with the port empty.
    Reenumerated {
        registry_id: u64,
        detached: Duration,
        /// Whether it stayed away for at least [`MIN_DETACHED`].
        held: bool,
    },
}

impl HandbackTimeline {
    /// Starts the timeline of handing back `device`.
    #[must_use]
    pub const fn new(device: &RegistryUsbDevice) -> Self {
        Self {
            old_registry_id: device.registry_id,
            location_id: device.location_id,
            vendor_id: device.vendor_id,
            product_id: device.product_id,
            terminated: None,
            hold_released: None,
            reenumerated: None,
        }
    }

    /// Records one read of the registry `at` an offset from the `destroy`.
    ///
    /// The old device is terminated once matching no longer returns its
    /// registry entry. The new device is the same vendor and product at the
    /// same port (`locationID`) under a new registry entry. A new device that
    /// is seen before the old one was seen gone also proves it gone.
    pub fn observe(&mut self, at: Duration, devices: &[RegistryUsbDevice]) {
        let old_present = devices
            .iter()
            .any(|device| device.registry_id == self.old_registry_id);
        let successor = devices.iter().find(|device| {
            device.registry_id != self.old_registry_id
                && device.vendor_id == self.vendor_id
                && device.product_id == self.product_id
                && device.location_id == self.location_id
        });
        if self.terminated.is_none() && (!old_present || successor.is_some()) {
            self.terminated = Some(at);
        }
        if self.reenumerated.is_none() {
            if let Some(device) = successor {
                self.reenumerated = Some((at, device.registry_id));
            }
        }
    }

    /// The hold was let go `at`.
    pub fn note_hold_released(&mut self, at: Duration) {
        if self.hold_released.is_none() {
            self.hold_released = Some(at);
        }
    }

    #[must_use]
    pub const fn terminated(&self) -> Option<Duration> {
        self.terminated
    }

    #[must_use]
    pub const fn reenumerated(&self) -> Option<(Duration, u64)> {
        self.reenumerated
    }

    /// When the hold may end: [`HANDBACK_HOLD`] after the old device was seen
    /// terminated. `None` while it has not been.
    #[must_use]
    pub fn hold_until(&self) -> Option<Duration> {
        self.terminated.map(|terminated| terminated + HANDBACK_HOLD)
    }

    /// Whether the device came back while it was meant to be held away: the
    /// kernel did not wait for the held interfaces.
    #[must_use]
    pub fn came_back_during_hold(&self) -> bool {
        match (self.reenumerated, self.hold_released) {
            (Some((at, _)), Some(released)) => at < released,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    #[must_use]
    pub fn verdict(&self) -> HandbackVerdict {
        match (self.terminated, self.reenumerated) {
            (None, _) => HandbackVerdict::NotTerminated,
            (Some(terminated), None) => HandbackVerdict::NotReenumerated { terminated },
            (Some(terminated), Some((at, registry_id))) => {
                let detached = at.saturating_sub(terminated);
                HandbackVerdict::Reenumerated {
                    registry_id,
                    detached,
                    held: detached >= MIN_DETACHED,
                }
            }
        }
    }
}

fn millis(duration: Duration) -> u128 {
    duration.as_millis()
}

fn offset(value: Option<Duration>) -> String {
    value.map_or_else(|| "-".to_owned(), |at| format!("+{}ms", millis(at)))
}

impl Display for HandbackTimeline {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:04x}:{:04x} old registry={:#x} location={} destroyed=+0ms terminated={} \
             hold_released={} reenumerated=",
            self.vendor_id,
            self.product_id,
            self.old_registry_id,
            self.location_id
                .map_or_else(|| "-".to_owned(), |location| format!("{location:#010x}")),
            offset(self.terminated),
            offset(self.hold_released),
        )?;
        match self.reenumerated {
            Some((at, registry_id)) => {
                write!(formatter, "+{}ms registry={registry_id:#x}", millis(at))
            }
            None => formatter.write_str("-"),
        }
    }
}

impl Display for HandbackVerdict {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotTerminated => formatter.write_str("the old device was never seen terminated"),
            Self::NotReenumerated { terminated } => write!(
                formatter,
                "the old device terminated at +{}ms and nothing re-enumerated in time",
                millis(*terminated)
            ),
            Self::Reenumerated {
                registry_id,
                detached,
                held: true,
            } => write!(
                formatter,
                "re-enumerated as registry={registry_id:#x} after {}ms detached",
                millis(*detached)
            ),
            Self::Reenumerated {
                registry_id,
                detached,
                held: false,
            } => write!(
                formatter,
                "re-enumerated as registry={registry_id:#x} after only {}ms detached (wanted \
                 at least {}ms): drivers may miss the arrival",
                millis(*detached),
                millis(MIN_DETACHED)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::DestroyMode;

    const OLD: u64 = 0x1_0002_b941;
    const NEW: u64 = 0x1_0002_b9f0;

    fn tablet(registry_id: u64) -> RegistryUsbDevice {
        RegistryUsbDevice {
            registry_id,
            location_id: Some(0x0011_2000),
            vendor_id: 0x056a,
            product_id: 0x0317,
            bcd_device: None,
            device_class: None,
            speed: None,
            exclusive_owner: None,
        }
    }

    fn other(registry_id: u64) -> RegistryUsbDevice {
        RegistryUsbDevice {
            location_id: Some(0x0021_1000),
            vendor_id: 0x2bd9,
            product_id: 0x0011,
            ..tablet(registry_id)
        }
    }

    const fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn plan(reason: ReleaseReason) -> ReleasePlan {
        ReleasePlan {
            reason,
            destroy: DestroyMode::for_reason(reason),
        }
    }

    #[test]
    fn a_session_end_holds_the_reset_while_interfaces_are_open() {
        for reason in [
            ReleaseReason::SessionEnded,
            ReleaseReason::PeerDetached,
            ReleaseReason::UserSwitch,
            ReleaseReason::Error,
        ] {
            assert_eq!(Handback::for_plan(plan(reason), 3), Handback::HeldReset);
            assert_eq!(Handback::for_plan(plan(reason), 0), Handback::PlainReset);
        }
        assert_eq!(
            Handback::for_plan(plan(ReleaseReason::RequestedByOs), 3),
            Handback::NoReenumeration
        );
        assert_eq!(
            Handback::for_plan(plan(ReleaseReason::Unplugged), 3),
            Handback::NoReenumeration
        );
    }

    #[test]
    fn a_held_hand_back_keeps_the_port_empty_for_the_hold() {
        let mut timeline = HandbackTimeline::new(&tablet(OLD));
        timeline.observe(ms(0), &[other(7), tablet(OLD)]);
        assert_eq!(timeline.terminated(), None);
        assert_eq!(timeline.hold_until(), None);
        timeline.observe(ms(40), &[other(7)]);
        assert_eq!(timeline.terminated(), Some(ms(40)));
        assert_eq!(timeline.hold_until(), Some(ms(40) + HANDBACK_HOLD));
        timeline.observe(ms(900), &[other(7)]);
        timeline.note_hold_released(ms(1_540));
        timeline.observe(ms(1_600), &[other(7), tablet(NEW)]);
        assert!(!timeline.came_back_during_hold());
        assert_eq!(
            timeline.verdict(),
            HandbackVerdict::Reenumerated {
                registry_id: NEW,
                detached: ms(1_560),
                held: true,
            }
        );
        assert_eq!(
            timeline.to_string(),
            "056a:0317 old registry=0x10002b941 location=0x00112000 destroyed=+0ms \
             terminated=+40ms hold_released=+1540ms reenumerated=+1600ms registry=0x10002b9f0"
        );
    }

    #[test]
    fn a_device_back_within_fifty_ms_is_reported_as_not_held() {
        let mut timeline = HandbackTimeline::new(&tablet(OLD));
        timeline.observe(ms(5), &[]);
        timeline.observe(ms(55), &[tablet(NEW)]);
        assert!(timeline.came_back_during_hold());
        let verdict = timeline.verdict();
        assert_eq!(
            verdict,
            HandbackVerdict::Reenumerated {
                registry_id: NEW,
                detached: ms(50),
                held: false,
            }
        );
        assert!(verdict.to_string().contains("only 50ms detached"));
    }

    #[test]
    fn a_successor_seen_first_also_proves_the_old_device_gone() {
        let mut timeline = HandbackTimeline::new(&tablet(OLD));
        timeline.observe(ms(30), &[tablet(OLD), tablet(NEW)]);
        assert_eq!(timeline.terminated(), Some(ms(30)));
        assert_eq!(timeline.reenumerated(), Some((ms(30), NEW)));
    }

    #[test]
    fn the_same_product_at_another_port_is_not_the_successor() {
        let mut timeline = HandbackTimeline::new(&tablet(OLD));
        let elsewhere = RegistryUsbDevice {
            location_id: Some(0x0021_2000),
            ..tablet(NEW)
        };
        timeline.observe(ms(20), &[elsewhere]);
        assert_eq!(timeline.terminated(), Some(ms(20)));
        assert_eq!(timeline.reenumerated(), None);
        assert_eq!(
            timeline.verdict(),
            HandbackVerdict::NotReenumerated { terminated: ms(20) }
        );
    }

    #[test]
    fn a_device_that_never_left_is_reported() {
        let mut timeline = HandbackTimeline::new(&tablet(OLD));
        timeline.observe(ms(2_000), &[tablet(OLD)]);
        assert_eq!(timeline.verdict(), HandbackVerdict::NotTerminated);
        assert!(!timeline.came_back_during_hold());
    }
}
