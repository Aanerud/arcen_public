// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Native tablet on a joined console desktop (ADR 0025): which route a
//! session takes, and the proof that the Pier's own pen device reaches that
//! desktop's display server alone.
//!
//! Native tablet has two routes ([`NativeTabletRoute`]):
//!
//! - **Host driver.** The Deck's tablet becomes a real USB device on the
//!   host and the host's own driver reads it. Only a desktop Arcen can give
//!   the device to alone takes it ([`crate::bridge_seat`]): a dedicated Xorg
//!   desktop or a desktop on its own Arcen seat.
//! - **Host decoded.** The Pier reads the pen itself
//!   ([`crate::HostPenBridge`] with [`crate::HidPublishPolicy::Nothing`]:
//!   no USB or HID device appears on the host) and posts it through its own
//!   typed pen device, the one Tablet support uses.
//!
//! A joined console's display server is not one Arcen starts, so it never
//! admits a raw bridged device tagged for it alone, and the host-driver
//! route stays refused there
//! ([`crate::HardUsbSessionDenial::JoinedConsole`]). It does admit the
//! Pier's typed pen device, as a console display server admits any device on
//! its seat, so the host-decoded route is taken only while that device is
//! proved to reach the joined desktop's display server alone
//! (`arcen_input::joined_input`, which reduces its gap to a
//! [`JoinedPenRefusal`]).
//!
//! Policy only: the platform adapter proves the device and acts on the
//! decision.

use crate::{HardUsbSessionDenial, HardUsbSessionSeat, admit_hard_usb_session};

/// How a Native tablet reaches a session's desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeTabletRoute {
    /// The tablet is a real USB device on the host, read by the host's
    /// driver ([`crate::bridge_seat`]).
    HostDriver,
    /// The Pier reads the pen and posts it through its own typed pen
    /// device; nothing is published on the host.
    HostDecoded,
}

impl NativeTabletRoute {
    /// A short name for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::HostDriver => "host_driver",
            Self::HostDecoded => "host_decoded",
        }
    }
}

/// Why a joined console's typed pen device does not carry the host-decoded
/// route, as far as the Deck is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinedPenRefusal {
    /// There is no typed pen device.
    NoDevice,
    /// Another display server or process also reads the device.
    NotConfined,
    /// The joined desktop's display server has not taken the device, could
    /// not be identified, or is not in front of its seat.
    NotReady,
}

impl JoinedPenRefusal {
    /// The denial a Deck is shown.
    #[must_use]
    pub const fn denial(self) -> HardUsbSessionDenial {
        match self {
            Self::NoDevice => HardUsbSessionDenial::JoinedConsoleNoPenDevice,
            Self::NotConfined => HardUsbSessionDenial::JoinedConsolePenNotConfined,
            Self::NotReady => HardUsbSessionDenial::JoinedConsolePenNotReady,
        }
    }
}

/// Decides the Native tablet route for a session on `seat`. A joined
/// console takes the host-decoded route when its typed pen device is
/// proved confined (`typed_pen`);
/// sessions Arcen can give a bridged device to keep the host-driver route;
/// every other session is refused as before.
///
/// # Errors
///
/// The denial the Deck is shown.
pub fn native_tablet_route(
    seat: HardUsbSessionSeat,
    typed_pen: &Result<(), JoinedPenRefusal>,
) -> Result<NativeTabletRoute, HardUsbSessionDenial> {
    match seat {
        HardUsbSessionSeat::JoinedConsole => typed_pen
            .as_ref()
            .map(|()| NativeTabletRoute::HostDecoded)
            .map_err(|refusal| refusal.denial()),
        seat => admit_hard_usb_session(seat).map(|()| NativeTabletRoute::HostDriver),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mode table: every seat, with and without a confined typed pen.
    #[test]
    fn a_joined_console_takes_the_host_decoded_route_and_never_the_host_driver() {
        let proved = Ok(());
        let unconfined = Err(JoinedPenRefusal::NotConfined);
        let absent = Err(JoinedPenRefusal::NoDevice);
        let unready = Err(JoinedPenRefusal::NotReady);
        let table = [
            (
                HardUsbSessionSeat::JoinedConsole,
                &proved,
                Ok(NativeTabletRoute::HostDecoded),
            ),
            (
                HardUsbSessionSeat::JoinedConsole,
                &unconfined,
                Err(HardUsbSessionDenial::JoinedConsolePenNotConfined),
            ),
            (
                HardUsbSessionSeat::JoinedConsole,
                &absent,
                Err(HardUsbSessionDenial::JoinedConsoleNoPenDevice),
            ),
            (
                HardUsbSessionSeat::JoinedConsole,
                &unready,
                Err(HardUsbSessionDenial::JoinedConsolePenNotReady),
            ),
            (
                HardUsbSessionSeat::X11Desktop,
                &absent,
                Ok(NativeTabletRoute::HostDriver),
            ),
            (
                HardUsbSessionSeat::ArcenSeat,
                &proved,
                Ok(NativeTabletRoute::HostDriver),
            ),
            (
                HardUsbSessionSeat::Seatless,
                &proved,
                Err(HardUsbSessionDenial::SeatlessSession),
            ),
            (
                HardUsbSessionSeat::ConsoleSeat,
                &proved,
                Err(HardUsbSessionDenial::ConsoleSeatSession),
            ),
        ];
        for (seat, typed_pen, expected) in table {
            assert_eq!(
                native_tablet_route(seat, typed_pen),
                expected,
                "{seat:?} {typed_pen:?}"
            );
        }
        // The raw bridge stays refused on a joined console whatever the
        // typed pen proves.
        assert_eq!(
            admit_hard_usb_session(HardUsbSessionSeat::JoinedConsole),
            Err(HardUsbSessionDenial::JoinedConsole)
        );
        assert_eq!(NativeTabletRoute::HostDecoded.token(), "host_decoded");
    }
}
