// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether a host session can receive a Hard USB device at all.
//!
//! A bridged device appears as a real device on the host: the host kernel's
//! driver claims it and udev assigns its input nodes to a logind seat. On a
//! single-seat machine that is `seat0`, whose display manager (the login
//! screen) would take it. So the host adapter gives every bridged device to
//! one session before it exists ([`crate::bridge_seat`]), and only a session
//! that can be given a device that way may receive one:
//!
//! - a native Wayland desktop on its own seat takes it through logind;
//! - a dedicated Xorg desktop is a `seat0` server: the device stays on
//!   `seat0`, tagged for that desktop alone; every other X server ignores
//!   it and the desktop hot-adds it on the first connection;
//! - a headless Wayland session reads no input devices at all and is
//!   refused.
//!
//! The decision is policy, not platform, so it lives here: a product adapter
//! states which kind of session it serves and refuses Hard USB before any
//! virtual host controller or helper is touched.

use std::fmt::{Display, Formatter};

/// How the session a Deck is attached to receives physical input devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardUsbSessionSeat {
    /// The session has no seat: its compositor reads no input devices, and a
    /// bridged device would reach the machine's physical seat instead (a
    /// headless Wayland session).
    Seatless,
    /// The session's server reads input only from the machine's console
    /// seat (`seat0`) and cannot be given a device on a seat of its own: a
    /// bridged device would reach the console seat (the login screen)
    /// instead.
    ConsoleSeat,
    /// A dedicated Xorg desktop: the bridged device stays on `seat0`, tagged
    /// for this desktop's X server, which hot-adds it; every other X server
    /// ignores it (see [`crate::bridge_seat`]).
    X11Desktop,
    /// A native Wayland desktop on its own Arcen seat, whose compositor takes
    /// the bridged device through logind.
    ArcenSeat,
}

/// Why Hard USB was refused for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardUsbSessionDenial {
    /// The session is seatless; see [`HardUsbSessionSeat::Seatless`].
    SeatlessSession,
    /// The session reads only the console seat; see
    /// [`HardUsbSessionSeat::ConsoleSeat`].
    ConsoleSeatSession,
    /// The tablet cannot be used by a dedicated Xorg desktop: no pen node of
    /// it is served by the kernel `wacom` driver with xf86-input-wacom
    /// installed.
    X11TabletUnsupported,
    /// A bridged device would not be confined to the desktop: the host's
    /// Xorg ignore class is missing, or an X server (such as the login
    /// screen) started before it was installed.
    X11IsolationNotInForce,
    /// The desktop's X server was started by an earlier Pier and does not
    /// admit a bridged device.
    X11DesktopPredatesBridge,
    /// The tablet's input nodes did not prove that the bridge rule applied.
    X11DeviceNotVerified,
    /// The tablet presents a keyboard (a keyboard interface, or keyboard
    /// keys on a node), which on `seat0` would reach the machine's console.
    X11KeyboardInterface,
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
            Self::ConsoleSeatSession => {
                "Native tablet (USB bridged) is not available here: this session reads input \
                 from the machine's console seat, so a bridged tablet would reach the login \
                 screen instead of your desktop. Use Tablet support instead."
            }
            Self::X11TabletUnsupported => {
                "Native tablet on an X11 desktop needs a tablet the host's wacom driver serves, \
                 with xf86-input-wacom installed (installing it takes effect at the next \
                 connection). Use Tablet support."
            }
            Self::X11IsolationNotInForce => {
                "Native tablet is not ready on this host: an X server (such as the login \
                 screen) started before Arcen's input isolation was installed. The host's \
                 administrator restarts the display manager. Use Tablet support."
            }
            Self::X11DesktopPredatesBridge => {
                "Native tablet is not available in this desktop: an earlier Arcen version \
                 started it. Desktops this version starts take the tablet. Use Tablet support."
            }
            Self::X11KeyboardInterface => {
                "Native tablet on an X11 desktop takes pens, pads and touch only: this tablet \
                 also presents a keyboard, which would reach this machine's console. Use \
                 Tablet support."
            }
            Self::X11DeviceNotVerified => {
                "Native tablet stopped: the tablet's devices on this host did not prove they \
                 belong to your desktop alone. Use Tablet support."
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
/// [`HardUsbSessionDenial::SeatlessSession`] for a seatless session and
/// [`HardUsbSessionDenial::ConsoleSeatSession`] for a console-seat one.
pub const fn admit_hard_usb_session(seat: HardUsbSessionSeat) -> Result<(), HardUsbSessionDenial> {
    match seat {
        HardUsbSessionSeat::X11Desktop | HardUsbSessionSeat::ArcenSeat => Ok(()),
        HardUsbSessionSeat::Seatless => Err(HardUsbSessionDenial::SeatlessSession),
        HardUsbSessionSeat::ConsoleSeat => Err(HardUsbSessionDenial::ConsoleSeatSession),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_desktops_and_arcen_seats_may_receive_a_bridged_device() {
        assert_eq!(
            admit_hard_usb_session(HardUsbSessionSeat::X11Desktop),
            Ok(())
        );
        assert_eq!(
            admit_hard_usb_session(HardUsbSessionSeat::ArcenSeat),
            Ok(())
        );
    }

    #[test]
    fn seatless_and_console_seat_sessions_are_refused_with_a_reason_naming_the_alternative() {
        let denial = admit_hard_usb_session(HardUsbSessionSeat::Seatless).unwrap_err();
        assert_eq!(denial, HardUsbSessionDenial::SeatlessSession);
        let reason = denial.to_string();
        assert!(reason.contains("Wayland session"));
        assert!(reason.contains("physical seat"));
        let console = admit_hard_usb_session(HardUsbSessionSeat::ConsoleSeat).unwrap_err();
        assert!(console.reason().contains("login screen"));
        for denial in [
            denial,
            console,
            HardUsbSessionDenial::X11TabletUnsupported,
            HardUsbSessionDenial::X11IsolationNotInForce,
            HardUsbSessionDenial::X11DesktopPredatesBridge,
            HardUsbSessionDenial::X11DeviceNotVerified,
            HardUsbSessionDenial::X11KeyboardInterface,
        ] {
            assert!(denial.reason().contains("Tablet support"));
            // Fits the protocol's tablet-mode reason bound (240 bytes).
            assert!(
                denial.reason().len() <= 240,
                "{} bytes",
                denial.reason().len()
            );
            // Never a sign-out instruction: Native tablet works on the first
            // connection or says why not.
            assert!(!denial.reason().contains("Sign out"), "{denial:?}");
        }
    }
}
