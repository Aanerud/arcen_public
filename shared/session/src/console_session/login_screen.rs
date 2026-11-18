// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A display manager's login screen that never reached its virtual terminal.
//!
//! When a console session ends, GDM 40 opens a new login screen session on
//! virtual terminal 1 and, before it starts the X server, switches the
//! console to that terminal and waits for the switch (`VT_ACTIVATE`, then
//! `VT_WAITACTIVE`). The switch is asked of the ending session's terminal,
//! which logind still holds in process mode. When the login screen opens
//! quickly — the display manager's account still has a running user
//! manager, kept alive by a login screen that is still closing (an
//! `ibus-daemon` a crashed greeter shell left behind, or the one that just
//! took a sign-in) — logind restores that terminal to automatic mode
//! before it answers the request, and restoring the mode drops the pending
//! switch. GDM then waits forever: its login screen session is `online`,
//! never `active`, with no X server and no shell; the console shows an
//! empty text terminal; and every remote-first sign-in finds no login
//! screen to hand to.
//!
//! Switching the console to that terminal ends the wait: GDM starts the X
//! server and the shell as it always does. This module decides when the
//! Pier does that ([`step`], [`StalledLoginScreens`]): only for a login
//! screen on the console seat that is not in front and has none of its own
//! processes running (the display manager's worker, which runs as root,
//! is all there is), and only while no other session is in front of the
//! seat — Arcen never switches anybody away. The Linux Pier gathers the
//! facts (logind sessions, the login screen session's processes) and asks
//! logind to activate the session.

use std::time::{Duration, Instant};

use super::{CONSOLE_SEAT, ConsoleSessionFact};

/// What a remote-first sign-in does next with the login screen it chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginScreenStep {
    /// Its shell runs: hand it the sign-in.
    Ready,
    /// Waiting for its virtual terminal with nothing in front of the seat:
    /// activate this session (its own id).
    BringToTerminal(String),
    /// Starting, or behind another session: wait.
    Wait,
    /// The session is gone or no longer a login screen.
    Gone,
}

/// Decides the next step for the login screen session `greeter`, from
/// every logind session and whether the login screen's own processes
/// (its display server, its shell) run in it.
#[must_use]
pub fn step(
    sessions: &[ConsoleSessionFact],
    greeter: &str,
    shell_running: bool,
) -> LoginScreenStep {
    let Some(fact) = sessions.iter().find(|s| s.id == greeter) else {
        return LoginScreenStep::Gone;
    };
    if !fact.is_console_greeter() {
        return LoginScreenStep::Gone;
    }
    if shell_running {
        return LoginScreenStep::Ready;
    }
    if waiting_for_terminal(sessions, fact) {
        LoginScreenStep::BringToTerminal(fact.id.clone())
    } else {
        LoginScreenStep::Wait
    }
}

/// A login screen that is not in front while nothing else on the console
/// seat is (no active session at all). The caller knows its own processes
/// do not run.
fn waiting_for_terminal(sessions: &[ConsoleSessionFact], greeter: &ConsoleSessionFact) -> bool {
    greeter.state == "online"
        && !greeter.active
        && !sessions
            .iter()
            .any(|s| s.seat == CONSOLE_SEAT && s.active && s.id != greeter.id)
}

/// How long a login screen must have been waiting for its terminal before
/// the Pier brings it there without a sign-in: GDM itself needs well under
/// a second between opening the session and the switch.
pub const STALL_GRACE: Duration = Duration::from_secs(5);

/// How many times the Pier brings one stalled login screen to its terminal
/// before it leaves it to the display manager.
pub const MAX_ATTEMPTS: u32 = 3;

/// Between two attempts for the same login screen.
pub const RETRY_AFTER: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct Seen {
    id: String,
    since: Instant,
    attempts: u32,
    last_attempt: Option<Instant>,
    /// The virtual terminal in front when the stall was first seen (the
    /// ended session's): one the person at the desk switched to since (a
    /// text console's login prompt) is never pulled away from.
    foreground_vt: u32,
}

/// Watches the console for a login screen left waiting for its terminal
/// while no Deck asks for it, so the person at the desk gets the login
/// screen back after a session ends.
#[derive(Debug, Clone, Default)]
pub struct StalledLoginScreens {
    seen: Vec<Seen>,
}

impl StalledLoginScreens {
    /// Records one look at the console (`without_shell`: the login screen
    /// sessions whose own processes do not run; `foreground_vt`: the
    /// virtual terminal in front) and returns the login screen to bring to
    /// its terminal now, if any. Once the terminal in front changes while a
    /// login screen waits, somebody at the desk chose it: that login screen
    /// is left to the next sign-in.
    pub fn observe(
        &mut self,
        sessions: &[ConsoleSessionFact],
        without_shell: &[String],
        foreground_vt: u32,
        now: Instant,
    ) -> Option<String> {
        let waiting: Vec<&ConsoleSessionFact> = sessions
            .iter()
            .filter(|s| {
                s.is_console_greeter()
                    && without_shell.contains(&s.id)
                    && waiting_for_terminal(sessions, s)
            })
            .collect();
        self.seen
            .retain(|seen| waiting.iter().any(|fact| fact.id == seen.id));
        for fact in &waiting {
            if !self.seen.iter().any(|seen| seen.id == fact.id) {
                self.seen.push(Seen {
                    id: fact.id.clone(),
                    since: now,
                    attempts: 0,
                    last_attempt: None,
                    foreground_vt,
                });
            }
        }
        for seen in &mut self.seen {
            if seen.foreground_vt != foreground_vt {
                seen.attempts = MAX_ATTEMPTS;
            }
        }
        let due = self.seen.iter_mut().find(|seen| {
            seen.attempts < MAX_ATTEMPTS
                && now.saturating_duration_since(seen.since) >= STALL_GRACE
                && seen
                    .last_attempt
                    .is_none_or(|last| now.saturating_duration_since(last) >= RETRY_AFTER)
        })?;
        due.attempts += 1;
        due.last_attempt = Some(now);
        Some(due.id.clone())
    }

    /// How many times the login screen `id` has been brought to its
    /// terminal while it kept waiting.
    #[must_use]
    pub fn attempts(&self, id: &str) -> u32 {
        self.seen
            .iter()
            .find(|seen| seen.id == id)
            .map_or(0, |seen| seen.attempts)
    }
}

/// Whether the login screen session `greeter`, which took a remote-first
/// sign-in, is finished but kept by processes it left behind: closing, its
/// leader (the display manager's worker) gone. Ending it removes the
/// leftovers that would keep the display manager's account manager running
/// and let the next login screen lose its terminal switch.
#[must_use]
pub fn finished_but_lingering(fact: &ConsoleSessionFact, leader_alive: bool) -> bool {
    fact.seat == CONSOLE_SEAT && fact.class == "greeter" && fact.state == "closing" && !leader_alive
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, class: &str, state: &str, active: bool) -> ConsoleSessionFact {
        ConsoleSessionFact {
            id: id.into(),
            uid: if class == "greeter" { 42 } else { 1000 },
            user: if class == "greeter" { "gdm" } else { "ana" }.into(),
            seat: CONSOLE_SEAT.into(),
            vtnr: if class == "greeter" { 1 } else { 7 },
            class: class.into(),
            session_type: "x11".into(),
            state: state.into(),
            active,
            service: if class == "greeter" {
                "gdm-launch-environment"
            } else {
                "gdm-password"
            }
            .into(),
            ..ConsoleSessionFact::default()
        }
    }

    fn ssh() -> ConsoleSessionFact {
        ConsoleSessionFact {
            id: "40".into(),
            uid: 0,
            user: "root".into(),
            class: "user".into(),
            session_type: "tty".into(),
            state: "active".into(),
            active: true,
            service: "sshd".into(),
            ..ConsoleSessionFact::default()
        }
    }

    #[test]
    fn a_running_login_screen_takes_the_sign_in() {
        let sessions = [session("c5", "greeter", "active", true)];
        assert_eq!(step(&sessions, "c5", true), LoginScreenStep::Ready);
        // Behind a text terminal the desk switched to, its shell still runs.
        let sessions = [session("c5", "greeter", "online", false)];
        assert_eq!(step(&sessions, "c5", true), LoginScreenStep::Ready);
    }

    #[test]
    fn a_login_screen_that_lost_its_terminal_switch_is_brought_there() {
        // The lab's state after a session ended: the old login screen still
        // closing, the new one online without X, nothing in front; an ssh
        // login elsewhere does not count.
        let sessions = [
            session("c149", "greeter", "closing", false),
            session("c150", "greeter", "online", false),
            ssh(),
        ];
        assert_eq!(
            step(&sessions, "c150", false),
            LoginScreenStep::BringToTerminal("c150".into())
        );
    }

    #[test]
    fn nobody_in_front_of_the_seat_is_switched_away() {
        let sessions = [
            session("c7", "greeter", "online", false),
            session("12", "user", "active", true),
        ];
        assert_eq!(step(&sessions, "c7", false), LoginScreenStep::Wait);
    }

    #[test]
    fn a_starting_login_screen_in_front_is_waited_for() {
        let sessions = [session("c7", "greeter", "active", true)];
        assert_eq!(step(&sessions, "c7", false), LoginScreenStep::Wait);
    }

    #[test]
    fn a_vanished_or_closing_login_screen_is_gone() {
        let sessions = [session("c7", "greeter", "closing", false)];
        assert_eq!(step(&sessions, "c7", false), LoginScreenStep::Gone);
        assert_eq!(step(&sessions, "c8", false), LoginScreenStep::Gone);
        let mut remote = session("c9", "greeter", "online", false);
        remote.seat = String::new();
        assert_eq!(step(&[remote], "c9", false), LoginScreenStep::Gone);
    }

    #[test]
    fn the_desk_gets_its_login_screen_back_after_the_grace_only() {
        let start = Instant::now();
        let sessions = [session("c151", "greeter", "online", false), ssh()];
        let stalled = vec!["c151".to_owned()];
        let mut watch = StalledLoginScreens::default();
        assert_eq!(watch.observe(&sessions, &stalled, 7, start), None);
        assert_eq!(
            watch.observe(&sessions, &stalled, 7, start + Duration::from_secs(2)),
            None
        );
        assert_eq!(
            watch.observe(&sessions, &stalled, 7, start + STALL_GRACE),
            Some("c151".into())
        );
        // Not again at once; again after the retry interval, a bounded
        // number of times.
        assert_eq!(
            watch.observe(
                &sessions,
                &stalled,
                7,
                start + STALL_GRACE + Duration::from_secs(1)
            ),
            None
        );
        let mut at = start + STALL_GRACE;
        for _ in 1..MAX_ATTEMPTS {
            at += RETRY_AFTER;
            assert_eq!(
                watch.observe(&sessions, &stalled, 7, at),
                Some("c151".into())
            );
        }
        at += RETRY_AFTER;
        assert_eq!(watch.observe(&sessions, &stalled, 7, at), None);
        assert_eq!(watch.attempts("c151"), MAX_ATTEMPTS);
    }

    #[test]
    fn a_login_screen_that_starts_or_has_company_is_forgotten() {
        let start = Instant::now();
        let mut watch = StalledLoginScreens::default();
        let waiting = [session("c3", "greeter", "online", false)];
        let stalled = vec!["c3".to_owned()];
        assert_eq!(watch.observe(&waiting, &stalled, 7, start), None);
        // Its shell started: nothing to do, and the clock starts again.
        assert_eq!(
            watch.observe(&waiting, &[], 7, start + Duration::from_secs(3)),
            None
        );
        assert_eq!(
            watch.observe(&waiting, &stalled, 7, start + Duration::from_secs(6)),
            None
        );
        // A session came to the front: never switched away.
        let with_user = [
            session("c3", "greeter", "online", false),
            session("21", "user", "active", true),
        ];
        assert_eq!(
            watch.observe(&with_user, &stalled, 7, start + Duration::from_secs(20)),
            None
        );
        assert_eq!(watch.attempts("c3"), 0);
    }

    #[test]
    fn a_text_console_the_desk_switched_to_is_never_left() {
        let start = Instant::now();
        let mut watch = StalledLoginScreens::default();
        let sessions = [session("c9", "greeter", "online", false)];
        let stalled = vec!["c9".to_owned()];
        assert_eq!(watch.observe(&sessions, &stalled, 7, start), None);
        // Somebody pressed Ctrl+Alt+F3 for a text login prompt.
        assert_eq!(
            watch.observe(&sessions, &stalled, 3, start + STALL_GRACE),
            None
        );
        assert_eq!(
            watch.observe(&sessions, &stalled, 3, start + STALL_GRACE * 10),
            None
        );
    }

    #[test]
    fn only_a_finished_login_screen_without_its_worker_is_ended() {
        let closing = session("c149", "greeter", "closing", false);
        assert!(finished_but_lingering(&closing, false));
        assert!(!finished_but_lingering(&closing, true));
        assert!(!finished_but_lingering(
            &session("c150", "greeter", "online", false),
            false
        ));
        assert!(!finished_but_lingering(
            &session("12", "user", "closing", false),
            false
        ));
    }
}
