//! Whether a host session can receive a Hard USB device at all.
//!
//! A bridged device appears as a real device on the host: the host kernel's
//! driver claims it and udev assigns its input nodes to a logind seat (on a
//! single-seat machine, `seat0`). Only a desktop whose compositor reads input
//! from that seat ever sees the device. That is true of an Xorg session on
//! the seat, but not of a seatless session: a headless compositor with no
//! seat opens no input devices at all (Mutter runs headless without
//! libinput). Bridging into such a session would deliver the tablet to
//! whatever runs on the physical seat instead (a greeter or another user's
//! desktop) while the session the Deck is looking at receives nothing.
//!
//! The decision is policy, not platform, so it lives here: a product adapter
//! states which kind of session it serves and refuses Hard USB before any
//! virtual host controller or helper is touched.

use std::fmt::{Display, Formatter};

/// How the session a Deck is attached to receives physical input devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardUsbSessionSeat {
    /// The session's display server reads input devices from the seat that a
    /// bridged device is attached to (an Xorg session on the machine's seat).
    SeatBound,
    /// The session has no seat: its compositor reads no input devices, and a
    /// bridged device would reach the machine's physical seat instead (a
    /// headless Wayland session).
    Seatless,
}

/// Why Hard USB was refused for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardUsbSessionDenial {
    /// The session is seatless; see [`HardUsbSessionSeat::Seatless`].
    SeatlessSession,
}

impl HardUsbSessionDenial {
    /// Operator- and user-facing explanation, naming the alternative.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::SeatlessSession => {
                "Native tablet (USB bridged) is not available in a Wayland session: its \
                 headless compositor opens no input devices, so a bridged tablet would reach \
                 this machine's physical seat instead. Use Tablet support instead."
            }
        }
    }
}

impl Display for HardUsbSessionDenial {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.reason())
    }
}

impl std::error::Error for HardUsbSessionDenial {}

/// Decides whether a Hard USB device may be bridged into a session.
///
/// # Errors
///
/// [`HardUsbSessionDenial::SeatlessSession`] for a seatless session.
pub const fn admit_hard_usb_session(seat: HardUsbSessionSeat) -> Result<(), HardUsbSessionDenial> {
    match seat {
        HardUsbSessionSeat::SeatBound => Ok(()),
        HardUsbSessionSeat::Seatless => Err(HardUsbSessionDenial::SeatlessSession),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_bound_session_may_receive_a_bridged_device() {
        assert_eq!(
            admit_hard_usb_session(HardUsbSessionSeat::SeatBound),
            Ok(())
        );
    }

    #[test]
    fn a_seatless_session_is_refused_with_a_reason_naming_the_alternative() {
        let denial = admit_hard_usb_session(HardUsbSessionSeat::Seatless).unwrap_err();
        assert_eq!(denial, HardUsbSessionDenial::SeatlessSession);
        let reason = denial.to_string();
        assert!(reason.contains("Wayland session"));
        assert!(reason.contains("physical seat"));
        assert!(reason.contains("Tablet support"));
        // Fits the protocol's tablet-mode reason bound (240 bytes).
        assert!(reason.len() <= 240, "{} bytes", reason.len());
    }
}
