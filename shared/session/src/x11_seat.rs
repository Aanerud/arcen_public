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
    fn the_deck_reason_fits_a_close_frame() {
        assert!(DECK_REASON_SEAT_OCCUPIED.len() <= 120);
        assert!(DECK_REASON_SEAT_OCCUPIED.contains("one X11 desktop per host"));
    }
}
