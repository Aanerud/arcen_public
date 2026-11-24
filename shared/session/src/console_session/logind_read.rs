// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Reading logind's sessions: completely, or not at all (ADR 0025 §2, §10).
//!
//! Every console decision starts from "which sessions exist". A read that
//! logind did not answer is not "nobody is signed in": on a host without a
//! display manager that answer starts the account's login session, a
//! second one for an account that may already hold one. So a read is
//! either complete (every session logind listed, each described and
//! consistent with the list) or [`SessionsUnreadable`], and every decision
//! refuses on the second ([`super::ConsoleRefusal::SessionsUnreadable`]).
//! Only a complete read can prove that the account has no session.
//!
//! The adapter reports what `loginctl` printed (or that it failed or timed
//! out); this module decides what that proves.

use super::ConsoleSessionFact;

/// What logind said about every session: all of them, or why the answer
/// cannot be used.
pub type SessionRead = Result<Vec<ConsoleSessionFact>, SessionsUnreadable>;

/// Why logind's sessions could not be read. Every decision refuses on it;
/// none treats it as an empty list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionsUnreadable {
    /// The session list could not be read (`loginctl` failed, timed out,
    /// or could not be started).
    ListFailed,
    /// The session list came back in a form that is not a session list (a
    /// line without a session id and a numeric user, or not UTF-8).
    ListGarbled,
    /// logind listed a session but would not describe it, and still lists
    /// it: whose session it is, and what it is, is unknown.
    SessionUndescribed,
    /// The answer contradicts itself: a session listed twice, or a
    /// description that names another session or another user than the
    /// list, or that names none.
    Contradictory,
}

impl SessionsUnreadable {
    /// Stable token for the Pier's log.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::ListFailed => "list_failed",
            Self::ListGarbled => "list_garbled",
            Self::SessionUndescribed => "session_undescribed",
            Self::Contradictory => "contradictory",
        }
    }
}

/// One line of `loginctl list-sessions --no-legend`: the session and the
/// user logind lists it for. Every systemd release Arcen supports prints
/// these two columns first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    pub id: String,
    pub uid: u32,
}

/// A logind session id Arcen passes to `loginctl` (letters, digits, `-`
/// and `_`; at most 64 bytes).
#[must_use]
pub fn safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 64
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Parses `loginctl list-sessions --no-legend --no-pager` output. No lines
/// is a list of no sessions (logind answered, and nobody has one).
///
/// # Errors
///
/// [`SessionsUnreadable::ListGarbled`] for a line that is not a session id
/// and a numeric user; [`SessionsUnreadable::Contradictory`] for a session
/// listed twice.
pub fn parse_session_list(text: &str) -> Result<Vec<ListedSession>, SessionsUnreadable> {
    let mut listed: Vec<ListedSession> = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut columns = line.split_whitespace();
        let (Some(id), Some(uid)) = (columns.next(), columns.next()) else {
            return Err(SessionsUnreadable::ListGarbled);
        };
        let Ok(uid) = uid.parse::<u32>() else {
            return Err(SessionsUnreadable::ListGarbled);
        };
        if !safe_session_id(id) {
            return Err(SessionsUnreadable::ListGarbled);
        }
        if listed.iter().any(|session| session.id == id) {
            return Err(SessionsUnreadable::Contradictory);
        }
        listed.push(ListedSession {
            id: id.to_owned(),
            uid,
        });
    }
    Ok(listed)
}

/// Every session of `listed`, from `described` (in the same order: what
/// `loginctl show-session` printed for it, or `None` when that failed or
/// timed out). `relisted` is the list read again after a description
/// failed: a session it no longer lists has ended, which is an answer; one
/// it still lists (or no second list) leaves the read incomplete.
///
/// # Errors
///
/// [`SessionsUnreadable::SessionUndescribed`] for a listed session that
/// could not be described and has not ended;
/// [`SessionsUnreadable::Contradictory`] for a description that does not
/// describe the listed session of the listed user.
pub fn assemble(
    listed: &[ListedSession],
    described: &[Option<String>],
    relisted: Option<&[ListedSession]>,
) -> SessionRead {
    if listed.len() != described.len() {
        return Err(SessionsUnreadable::Contradictory);
    }
    let mut facts = Vec::with_capacity(listed.len());
    for (session, description) in listed.iter().zip(described) {
        let Some(description) = description else {
            let ended = relisted.is_some_and(|again| again.iter().all(|s| s.id != session.id));
            if ended {
                continue;
            }
            return Err(SessionsUnreadable::SessionUndescribed);
        };
        let fact = ConsoleSessionFact::from_properties(description)
            .ok_or(SessionsUnreadable::Contradictory)?;
        if fact.id != session.id || fact.uid != session.uid {
            return Err(SessionsUnreadable::Contradictory);
        }
        facts.push(fact);
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::super::{
        ConsoleDecision, ConsoleHost, ConsoleRefusal, LoginScreen, decide_on_read,
        decide_reattach_read,
        detached_hold::{DetachedHold, HoldEnd},
        wayland_join::{BeforeCreating, decide_before_creating_read},
    };
    use super::*;

    const TIM: u32 = 1001;
    const NO_DISPLAY_MANAGER: ConsoleHost = ConsoleHost {
        display_manager: false,
        login_screen: LoginScreen::Shown,
    };
    const DISPLAY_MANAGER: ConsoleHost = ConsoleHost {
        display_manager: true,
        login_screen: LoginScreen::Shown,
    };

    fn listed(id: &str, uid: u32) -> ListedSession {
        ListedSession {
            id: id.to_owned(),
            uid,
        }
    }

    /// `loginctl show-session` for an X11 console session.
    fn x11_session(id: &str, uid: u32, vt: u32) -> String {
        format!(
            "Id={id}\nUser={uid}\nName=u{uid}\nSeat=seat0\nVTNr={vt}\nTTY=tty{vt}\nClass=user\n\
             Type=x11\nState=active\nActive=yes\nRemote=no\nService=arcen-login\n\
             LockedHint=no\nLeader=4242\nDesktop=gnome\n"
        )
    }

    /// `loginctl show-session` for an SSH session.
    fn ssh_session(id: &str, uid: u32) -> String {
        format!(
            "Id={id}\nUser={uid}\nName=u{uid}\nSeat=\nVTNr=0\nTTY=pts/0\nClass=user\n\
             Type=tty\nState=active\nActive=no\nRemote=yes\nService=sshd\nLockedHint=no\n"
        )
    }

    #[test]
    fn the_list_parses_as_every_supported_systemd_prints_it() {
        // systemd 252 (EL9): SESSION UID USER SEAT TTY.
        assert_eq!(
            parse_session_list("     2 1001 tim   seat0 tty2\n     5    0 root        pts/0\n"),
            Ok(vec![listed("2", TIM), listed("5", 0)])
        );
        // systemd 257 (EL10): SESSION UID USER SEAT LEADER CLASS TTY IDLE SINCE.
        assert_eq!(
            parse_session_list(
                "c1 0 root -    1234 manager-early -     no  -\n 3 1001 tim seat0 2201 user tty7 no -\n"
            ),
            Ok(vec![listed("c1", 0), listed("3", TIM)])
        );
        // logind answered: nobody has a session.
        assert_eq!(parse_session_list(""), Ok(Vec::new()));
        assert_eq!(parse_session_list("\n  \n"), Ok(Vec::new()));
    }

    #[test]
    fn a_list_that_is_not_a_session_list_is_garbled_not_empty() {
        for garbage in [
            "No sessions.\n",
            "Failed to connect to bus: No such file or directory\n",
            "2\n",
            "2 tim seat0\n",
            "../x 1001 tim\n",
            "2 1001 tim\n<html>\n",
        ] {
            assert_eq!(
                parse_session_list(garbage),
                Err(SessionsUnreadable::ListGarbled),
                "{garbage:?}"
            );
        }
        assert_eq!(
            parse_session_list("2 1001 tim\n2 1001 tim\n"),
            Err(SessionsUnreadable::Contradictory)
        );
    }

    #[test]
    fn a_complete_read_describes_every_listed_session() {
        let list = [listed("2", TIM), listed("5", 0)];
        let read = assemble(
            &list,
            &[Some(x11_session("2", TIM, 7)), Some(ssh_session("5", 0))],
            None,
        )
        .expect("complete");
        assert_eq!(
            read.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            ["2", "5"]
        );
    }

    #[test]
    fn a_listed_session_logind_would_not_describe_leaves_the_read_incomplete() {
        let list = [listed("2", TIM), listed("5", 0)];
        let described = [None, Some(ssh_session("5", 0))];
        // Still listed when asked again, or never asked again: unknown.
        assert_eq!(
            assemble(&list, &described, Some(&list)),
            Err(SessionsUnreadable::SessionUndescribed)
        );
        assert_eq!(
            assemble(&list, &described, None),
            Err(SessionsUnreadable::SessionUndescribed)
        );
        // Gone from the second list: it ended between the two reads.
        let read = assemble(&list, &described, Some(&[listed("5", 0)])).expect("ended");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].id, "5");
    }

    #[test]
    fn a_description_that_contradicts_the_list_is_refused() {
        let list = [listed("2", TIM)];
        for description in [
            x11_session("3", TIM, 7),      // another session
            x11_session("2", 1002, 7),     // another user
            "Id=2\nName=tim\n".to_owned(), // no user
            "garbage".to_owned(),
            String::new(),
        ] {
            assert_eq!(
                assemble(&list, &[Some(description.clone())], None),
                Err(SessionsUnreadable::Contradictory),
                "{description:?}"
            );
        }
        assert_eq!(
            assemble(&list, &[], None),
            Err(SessionsUnreadable::Contradictory)
        );
    }

    #[test]
    fn only_proven_absence_starts_a_login_session() {
        // Proven absent: logind listed only root's SSH session.
        let absent = assemble(&[listed("5", 0)], &[Some(ssh_session("5", 0))], None);
        assert_eq!(
            decide_on_read(&absent, TIM, NO_DISPLAY_MANAGER),
            ConsoleDecision::StartLoginSession { vt: 7 }
        );
        // With a display manager, absence is a failure, as before.
        assert_eq!(
            decide_on_read(&absent, TIM, DISPLAY_MANAGER),
            ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession)
        );
        // Unknown: never a second login session.
        for unreadable in [
            SessionsUnreadable::ListFailed,
            SessionsUnreadable::ListGarbled,
            SessionsUnreadable::SessionUndescribed,
            SessionsUnreadable::Contradictory,
        ] {
            for host in [NO_DISPLAY_MANAGER, DISPLAY_MANAGER] {
                assert_eq!(
                    decide_on_read(&Err(unreadable), TIM, host),
                    ConsoleDecision::Refuse(ConsoleRefusal::SessionsUnreadable),
                    "{unreadable:?}"
                );
            }
        }
    }

    #[test]
    fn a_partly_readable_console_never_starts_a_second_session() {
        // The account's own session is listed but logind would not describe
        // it; what was described (root's SSH session) proves nothing.
        let list = [listed("2", TIM), listed("5", 0)];
        let partial = assemble(&list, &[None, Some(ssh_session("5", 0))], Some(&list));
        assert_eq!(
            decide_on_read(&partial, TIM, NO_DISPLAY_MANAGER),
            ConsoleDecision::Refuse(ConsoleRefusal::SessionsUnreadable)
        );
        // A description of another user's session under the account's id.
        let contradictory = assemble(&[listed("2", TIM)], &[Some(x11_session("2", 0, 7))], None);
        assert_eq!(
            decide_on_read(&contradictory, TIM, NO_DISPLAY_MANAGER),
            ConsoleDecision::Refuse(ConsoleRefusal::SessionsUnreadable)
        );
        // Read completely, the account's session is joined, not duplicated.
        let complete = assemble(
            &list,
            &[Some(x11_session("2", TIM, 7)), Some(ssh_session("5", 0))],
            None,
        );
        assert!(matches!(
            decide_on_read(&complete, TIM, NO_DISPLAY_MANAGER),
            ConsoleDecision::Attach { ref session, .. } if session == "2"
        ));
    }

    #[test]
    fn every_other_decision_refuses_or_concludes_nothing_on_an_unreadable_console() {
        let unreadable: SessionRead = Err(SessionsUnreadable::ListFailed);
        assert_eq!(
            decide_reattach_read(&unreadable, TIM, "2", NO_DISPLAY_MANAGER),
            Err(ConsoleRefusal::SessionsUnreadable)
        );
        assert_eq!(
            decide_before_creating_read(&unreadable, TIM),
            BeforeCreating::Refuse(ConsoleRefusal::SessionsUnreadable)
        );
        assert_eq!(
            decide_before_creating_read(&Ok(Vec::new()), TIM),
            BeforeCreating::Create
        );
        // A held console: an unreadable look ends nothing and sets no
        // baseline; a guest console's own product's client still ends it.
        let mut hold = DetachedHold::from_read("2", false, &unreadable);
        assert_eq!(hold.observe_read(&unreadable, false), None);
        let mut guest = DetachedHold::from_read("2", true, &unreadable);
        assert_eq!(
            guest.observe_read(&unreadable, true),
            Some(HoldEnd::OtherProductClient)
        );
    }
}
