// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One X11 desktop per host.
//!
//! A dedicated Xorg desktop is a logind session on `seat0` at virtual
//! terminal 1, because that is where an X11 login gets its seat devices and
//! its active-session rights. logind keeps one session per seat position: a
//! second `CreateSession` for the same VT while a non-greeter session still
//! holds it is answered `Already occupied by a session`, and `pam_systemd`
//! treats that answer as "already in a session" — the PAM transaction
//! succeeds, but no logind session (no `XDG_SESSION_ID`) is created.
//!
//! The holder can be another Pier's desktop on the same host (a second Pier
//! on another port, or a desktop still closing). The launcher cannot serve
//! that sign-in, and must say so in words the Deck user can act on rather
//! than as a protocol failure. This module holds the decision: which
//! sessions occupy the position, read from logind's own properties, and the
//! refusal the Deck shows.

/// The VT position a dedicated Xorg desktop asks logind for on `seat0`.
pub const X11_DESKTOP_VT: u32 = 1;

/// What the Deck is told when another desktop holds the host's X11 seat.
/// It travels in a QUIC/WebSocket close reason, so it stays well under the
/// 120-byte budget the Pier allows there.
pub const DECK_REASON_SEAT_OCCUPIED: &str = "Another remote desktop is active on this host (one X11 desktop per host). Sign out of it first.";

/// What the Deck is told when another remote-desktop product's own desktop
/// holds the host's display: a second X server must not start beside it on
/// the same GPU.
pub const DECK_REASON_SEAT_OTHER_PRODUCT: &str = "Another remote-desktop product holds this host's display (one X11 desktop per host). Sign out of it first.";

/// [`DECK_REASON_SEAT_OTHER_PRODUCT`] naming the service the host reports
/// (when it fits the close-frame budget).
#[must_use]
pub fn deck_reason_other_product(
    service: Option<&crate::console_session::ForeignService>,
) -> String {
    let head = "Another remote-desktop service (";
    let tail = ") holds this host's display. Sign out of it first.";
    let budget = crate::console_session::foreign_service::DECK_REASON_MAX
        .saturating_sub(head.len() + tail.len());
    service
        .and_then(|service| service.label_within(budget))
        .map_or_else(
            || DECK_REASON_SEAT_OTHER_PRODUCT.to_owned(),
            |label| format!("{head}{label}{tail}"),
        )
}

/// Who holds the console before a dedicated desktop would start: any
/// user's graphical session on `seat0`, on any virtual terminal (another
/// product's desktop runs its own Xorg on another VT, not VT 1), and
/// whether the display manager started it.
#[must_use]
pub fn x11_seat_holder(
    sessions: &[crate::console_session::ConsoleSessionFact],
) -> Option<(
    &crate::console_session::ConsoleSessionFact,
    crate::console_session::ConsoleOrigin,
)> {
    sessions
        .iter()
        .filter(|session| session.is_console_desktop())
        .map(|session| (session, session.origin()))
        .max_by_key(|(_, origin)| *origin == crate::console_session::ConsoleOrigin::OtherProduct)
}

/// The facts logind reports for one session (`loginctl show-session -p Id
/// -p Seat -p VTNr -p Class -p State -p Type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatSessionFact {
    pub id: String,
    pub seat: String,
    pub vtnr: u32,
    pub class: String,
    pub state: String,
    pub session_type: String,
}

impl SeatSessionFact {
    /// Parses `key=value` lines as printed by `loginctl show-session`.
    /// Unknown keys are ignored; a missing `Id` is a parse failure because a
    /// fact that cannot be named cannot be reported.
    #[must_use]
    pub fn from_properties(properties: &str) -> Option<Self> {
        let mut fact = Self {
            id: String::new(),
            seat: String::new(),
            vtnr: 0,
            class: String::new(),
            state: String::new(),
            session_type: String::new(),
        };
        for line in properties.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key {
                "Id" => value.clone_into(&mut fact.id),
                "Seat" => value.clone_into(&mut fact.seat),
                "VTNr" => fact.vtnr = value.parse().unwrap_or(0),
                "Class" => value.clone_into(&mut fact.class),
                "State" => value.clone_into(&mut fact.state),
                "Type" => value.clone_into(&mut fact.session_type),
                _ => {}
            }
        }
        (!fact.id.is_empty()).then_some(fact)
    }

    /// Whether logind counts this session as holding `seat`'s position
    /// `vtnr` against a new session: the same seat and VT, and not a
    /// greeter (logind lets a login replace its own greeter). A closing
    /// session still holds the position until logind frees it.
    #[must_use]
    pub fn occupies(&self, seat: &str, vtnr: u32) -> bool {
        vtnr != 0 && self.seat == seat && self.vtnr == vtnr && self.class != "greeter"
    }
}

/// The session that holds the X11 desktop position on `seat0`, if any.
#[must_use]
pub fn x11_seat_occupant(sessions: &[SeatSessionFact]) -> Option<&SeatSessionFact> {
    sessions
        .iter()
        .find(|session| session.occupies("seat0", X11_DESKTOP_VT))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(id: &str, seat: &str, vtnr: u32, class: &str) -> SeatSessionFact {
        SeatSessionFact {
            id: id.to_owned(),
            seat: seat.to_owned(),
            vtnr,
            class: class.to_owned(),
            state: "active".to_owned(),
            session_type: "x11".to_owned(),
        }
    }

    #[test]
    fn another_desktop_on_seat0_vt1_occupies_the_x11_seat() {
        let sessions = [
            fact("c2", "seat0", 1, "greeter"),
            fact("3107", "", 0, "user"),
            fact("1917", "seat0", 1, "user"),
        ];
        assert_eq!(
            x11_seat_occupant(&sessions).map(|s| s.id.as_str()),
            Some("1917")
        );
    }

    #[test]
    fn a_greeter_ssh_logins_and_seatless_wayland_desktops_do_not() {
        let sessions = [
            fact("c2", "seat0", 1, "greeter"),
            fact("3107", "", 0, "user"),
            fact("88", "", 0, "user"),
            fact("90", "seat0", 2, "user"),
        ];
        assert_eq!(x11_seat_occupant(&sessions), None);
    }

    #[test]
    fn a_closing_desktop_still_occupies_the_seat() {
        let mut closing = fact("1969", "seat0", 1, "user");
        closing.state = "closing".to_owned();
        assert!(x11_seat_occupant(std::slice::from_ref(&closing)).is_some());
    }

    #[test]
    fn loginctl_properties_are_parsed_and_an_unnamed_session_is_refused() {
        let parsed = SeatSessionFact::from_properties(
            "Id=1917\nSeat=seat0\nVTNr=1\nClass=user\nState=active\nType=x11\nRemote=yes\n",
        )
        .expect("named session");
        assert_eq!(parsed, fact("1917", "seat0", 1, "user"));
        let no_seat = SeatSessionFact::from_properties("Id=3107\nSeat=\nVTNr=0\nClass=user\n")
            .expect("named session");
        assert!(!no_seat.occupies("seat0", X11_DESKTOP_VT));
        assert_eq!(
            SeatSessionFact::from_properties("Seat=seat0\nVTNr=1\n"),
            None
        );
        assert_eq!(
            SeatSessionFact::from_properties("Id=5\nVTNr=garbage\n").map(|s| s.vtnr),
            Some(0)
        );
    }

    #[test]
    fn any_console_desktop_on_any_vt_holds_the_seat_and_another_product_is_named() {
        use crate::console_session::{ConsoleOrigin, ConsoleSessionFact};
        let parse = |text: &str| ConsoleSessionFact::from_properties(text).expect("fact");
        let greeter = parse(
            "Id=c1\nUser=42\nSeat=seat0\nVTNr=1\nClass=greeter\nType=x11\nState=active\nActive=yes\nService=gdm-launch-environment\n",
        );
        let ssh = parse(
            "Id=5\nUser=0\nSeat=\nClass=user\nType=tty\nState=active\nRemote=yes\nService=sshd\n",
        );
        let arcen = parse(
            "Id=6\nUser=1003\nSeat=seat0\nVTNr=1\nTTY=arcen\nClass=user\nType=x11\nState=active\nRemote=yes\nService=login\n",
        );
        assert!(x11_seat_holder(&[greeter.clone(), ssh.clone(), arcen]).is_none());
        let product = parse(
            "Id=81063\nUser=1001\nSeat=seat0\nVTNr=7\nClass=user\nType=x11\nState=active\nActive=yes\nRemote=yes\nService=vendor-session\n",
        );
        let gdm = parse(
            "Id=9\nUser=1002\nSeat=seat0\nVTNr=2\nClass=user\nType=x11\nState=online\nService=gdm-password\n",
        );
        let all = [greeter, gdm.clone(), product, ssh];
        let (holder, origin) = x11_seat_holder(&all).expect("held");
        assert_eq!(holder.id, "81063");
        assert_eq!(origin, ConsoleOrigin::OtherProduct);
        assert_eq!(
            x11_seat_holder(&[gdm]).map(|(s, o)| (s.id.as_str(), o)),
            Some(("9", ConsoleOrigin::DisplayManager))
        );
        assert!(DECK_REASON_SEAT_OTHER_PRODUCT.len() <= 120);
    }

    #[test]
    fn the_other_product_reason_names_the_service_within_budget() {
        let service = crate::console_session::ForeignService::new(
            "labrdp-session",
            Some("labrdp.service"),
            None,
        );
        let reason = deck_reason_other_product(Some(&service));
        assert!(
            reason.contains("labrdp-session, labrdp.service"),
            "{reason}"
        );
        assert!(reason.len() <= 120);
        assert_eq!(
            deck_reason_other_product(None),
            DECK_REASON_SEAT_OTHER_PRODUCT
        );
    }

    #[test]
    fn the_deck_reason_fits_a_close_frame() {
        assert!(DECK_REASON_SEAT_OCCUPIED.len() <= 120);
        assert!(DECK_REASON_SEAT_OCCUPIED.contains("one X11 desktop per host"));
    }
}
