// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Which console session a sign-in concerns, from logind's sessions: the
//! logind half of [`crate::session_handover::ConsoleOccupant`] (ADR 0025
//! §13).
//!
//! A change of account signs out exactly one session, so only a console
//! held by one session of one account names one. The requester's own
//! session is named whatever else is there ([`super::decide_on`] decides
//! what joining it means).

use super::ConsoleSessionFact;

/// Who holds the console, as far as a change of account is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleHolder<'a> {
    /// No account's desktop is on the console (a login screen may be).
    Nobody,
    /// The requester's own desktop, or the one desktop of one other
    /// account.
    Session(&'a ConsoleSessionFact),
    /// Several desktops of other accounts, or several of one other account:
    /// no single session can be named.
    Several,
}

/// The console desktop `requester_uid`'s sign-in concerns in `sessions`.
///
/// The requester's own desktop comes first (the one [`super::decide_on`]
/// would join: active, then X11, then online). Otherwise exactly one
/// desktop of another account is named; more than one is
/// [`ConsoleHolder::Several`], and none is [`ConsoleHolder::Nobody`].
/// Sessions that are closing are not desktops.
#[must_use]
pub fn console_holder(sessions: &[ConsoleSessionFact], requester_uid: u32) -> ConsoleHolder<'_> {
    let desktops = || sessions.iter().filter(|s| s.is_console_desktop());
    if let Some(own) = desktops()
        .filter(|s| s.uid == requester_uid)
        .min_by_key(|s| (!s.active, s.is_wayland(), s.state != "online"))
    {
        return ConsoleHolder::Session(own);
    }
    let mut others = desktops();
    match (others.next(), others.next()) {
        (None, _) => ConsoleHolder::Nobody,
        (Some(only), None) => ConsoleHolder::Session(only),
        (Some(_), Some(_)) => ConsoleHolder::Several,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop(id: &str, uid: u32, active: bool) -> ConsoleSessionFact {
        ConsoleSessionFact {
            id: id.to_owned(),
            uid,
            user: format!("user{uid}"),
            seat: "seat0".to_owned(),
            vtnr: 2,
            class: "user".to_owned(),
            session_type: "x11".to_owned(),
            state: if active { "active" } else { "online" }.to_owned(),
            active,
            service: "gdm-password".to_owned(),
            ..ConsoleSessionFact::default()
        }
    }

    fn greeter() -> ConsoleSessionFact {
        ConsoleSessionFact {
            class: "greeter".to_owned(),
            user: "gdm".to_owned(),
            ..desktop("c1", 42, true)
        }
    }

    #[test]
    fn a_login_screen_alone_is_nobody() {
        assert_eq!(console_holder(&[greeter()], 1000), ConsoleHolder::Nobody);
        assert_eq!(console_holder(&[], 1000), ConsoleHolder::Nobody);
    }

    #[test]
    fn one_other_accounts_desktop_is_named() {
        let sessions = [greeter(), desktop("7", 1001, false)];
        assert_eq!(
            console_holder(&sessions, 1000),
            ConsoleHolder::Session(&sessions[1])
        );
    }

    #[test]
    fn the_requesters_own_desktop_comes_first() {
        let sessions = [desktop("7", 1001, true), desktop("8", 1000, false)];
        assert_eq!(
            console_holder(&sessions, 1000),
            ConsoleHolder::Session(&sessions[1])
        );
    }

    #[test]
    fn several_other_desktops_name_none() {
        let two_accounts = [desktop("7", 1001, true), desktop("8", 1002, false)];
        assert_eq!(console_holder(&two_accounts, 1000), ConsoleHolder::Several);
        let one_account_twice = [desktop("7", 1001, true), desktop("8", 1001, false)];
        assert_eq!(
            console_holder(&one_account_twice, 1000),
            ConsoleHolder::Several
        );
    }

    #[test]
    fn a_closing_session_is_no_desktop() {
        let mut closing = desktop("7", 1001, false);
        closing.state = "closing".to_owned();
        assert_eq!(console_holder(&[closing], 1000), ConsoleHolder::Nobody);
    }
}
