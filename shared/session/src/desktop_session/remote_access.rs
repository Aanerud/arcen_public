// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A desktop outlives its remote-access streams (ADR 0025: lock, never end).
//!
//! An Arcen GNOME desktop on a compositor that owns its outputs through
//! `RemoteDesktop`/`ScreenCast` streams loses those streams whenever GNOME
//! Shell inhibits remote access. The shell does that when it enters its
//! `unlock-dialog` session mode (the screen shield comes up: an idle lock,
//! Super+L, `loginctl lock-session`) on any compositor that is not
//! `--headless`, which includes a native-seat desktop. Mutter then closes
//! every remote-access session (`meta_dbus_session_manager_inhibit`) and
//! refuses new ones with "Session creation inhibited" until the shell pops
//! the mode again as the shield goes down. Mutter publishes no D-Bus signal
//! for the inhibition; its own refusal is the authoritative fact. The
//! shell's `org.gnome.ScreenSaver` `ActiveChanged(false)` and logind's
//! `LockedHint=no` follow the uninhibit (both are set when the shield's
//! hide animation completes), so they are a prompt to try again, not proof.
//!
//! [`StreamsLifetime`] keeps the two lifetimes apart: closed streams are
//! recorded again at once; a refusal Mutter attributes to the inhibition,
//! or any refusal while the shield is up, suspends capture and input and
//! keeps the desktop and its applications; the streams are recorded again
//! once the shield goes down or a periodic probe succeeds. Only a logout,
//! a compositor that exits, an orderly stop, or streams that cannot be
//! recorded again while nothing inhibits them end the desktop.
//!
//! While the streams are not live an attachment has nothing to capture or
//! to send input to. [`attach_wait`] decides how long an attachment that
//! has just unlocked the desktop waits for them before it is refused.

use std::time::Duration;

/// The text Mutter's refusal carries while remote access is inhibited
/// (`meta-dbus-session-manager.c`, Mutter 44 and later; Mutter 40–43 said
/// the same from `meta-remote-desktop.c`).
pub const INHIBITED_REFUSAL: &str = "Session creation inhibited";

/// Whether a `CreateSession` refusal is Mutter's remote-access inhibition.
#[must_use]
pub fn refusal_is_inhibited(message: &str) -> bool {
    message.contains(INHIBITED_REFUSAL)
}

/// How many times closed streams are recorded again, while nothing
/// inhibits remote access, before the desktop is given up.
pub const REPLACE_ATTEMPTS: u32 = 3;

/// How often a suspended desktop asks Mutter again, in case no shield
/// signal arrives (a shell that restarted, a shield that never animates
/// without a monitor).
pub const SUSPENDED_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// How long an attachment waits for a suspended desktop's streams after
/// it unlocked the desktop: the shield's hide animation, the probe
/// interval and recording every output again, with margin.
pub const ATTACH_RESUME_WAIT: Duration = Duration::from_secs(20);

/// How often an attachment asks again while it waits.
pub const ATTACH_RESUME_POLL: Duration = Duration::from_millis(250);

/// The delay before replacement attempt `attempt` (1-based).
#[must_use]
pub const fn replace_delay(attempt: u32) -> Duration {
    match attempt {
        0 | 1 => Duration::ZERO,
        2 => Duration::from_secs(1),
        _ => Duration::from_secs(2),
    }
}

/// Why capture and input are suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suspension {
    /// The screen shield is up: the desktop is locked or blanked.
    Locked,
    /// Mutter refuses remote access although no shield was seen.
    Inhibited,
}

impl Suspension {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::Inhibited => "remote_access_inhibited",
        }
    }
}

/// Why a desktop ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopEnd {
    /// The user logged out inside the desktop.
    LoggedOut,
    /// The compositor or another stack member exited.
    CompositorExited,
    /// The launcher or logind asked the desktop to stop.
    Stopped,
    /// The streams closed and could not be recorded again although nothing
    /// inhibited remote access, or the outputs cannot be recorded again.
    StreamsLost,
}

impl DesktopEnd {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::LoggedOut => "user_logout",
            Self::CompositorExited => "compositor_exited",
            Self::Stopped => "stopped",
            Self::StreamsLost => "streams_lost",
        }
    }
}

/// Where the streams are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamsState {
    /// Recorded and served.
    Live,
    /// Closed; attempt `attempt` to record them again is due or running.
    Replacing {
        /// 1-based attempt.
        attempt: u32,
        /// Set while the replacement is a suspended desktop's probe.
        suspended: Option<Suspension>,
    },
    /// Closed while remote access is inhibited; the desktop waits.
    Suspended(Suspension),
    /// The desktop ends.
    Ended(DesktopEnd),
}

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamsEvent {
    /// The compositor closed the live streams. `replaceable` is false when
    /// the session's outputs are fixed at start (startup monitors), whose
    /// new streams no attachment could find.
    Closed {
        /// Whether the outputs can be recorded again.
        replaceable: bool,
        /// The screen shield, when the shell answered.
        shield_active: Option<bool>,
    },
    /// The outputs were recorded again.
    Established,
    /// Recording them again was refused.
    Refused {
        /// Mutter said remote access is inhibited ([`refusal_is_inhibited`]).
        inhibited: bool,
        /// The screen shield, when the shell answered.
        shield_active: Option<bool>,
    },
    /// The shell's screen shield went up or down.
    ShieldChanged(bool),
    /// [`SUSPENDED_PROBE_INTERVAL`] passed.
    ProbeDue,
    /// The user logged out inside the desktop.
    LoggedOut,
    /// A stack member (the compositor) exited.
    CompositorExited,
    /// An orderly stop was requested.
    Stopped,
}

/// What to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamsAction {
    /// Nothing: keep serving or keep waiting as before.
    Continue,
    /// Record the outputs again after `after`.
    Establish {
        /// Delay first.
        after: Duration,
    },
    /// Keep the desktop, its applications and its private services;
    /// serve no capture and no input; ask again on the next shield change
    /// or [`SUSPENDED_PROBE_INTERVAL`].
    Suspend(Suspension),
    /// The streams are live again: serve the next output generation.
    Resume,
    /// End the desktop.
    End(DesktopEnd),
}

/// The streams' lifetime, apart from the desktop's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamsLifetime {
    state: StreamsState,
}

impl Default for StreamsLifetime {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamsLifetime {
    /// Live streams.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: StreamsState::Live,
        }
    }

    /// The current state.
    #[must_use]
    pub const fn state(&self) -> StreamsState {
        self.state
    }

    /// Whether an attachment finds nothing to capture or to send input to
    /// now: the streams are being recorded again or wait for an unlock.
    #[must_use]
    pub const fn streams_unavailable(&self) -> bool {
        matches!(
            self.state,
            StreamsState::Replacing { .. } | StreamsState::Suspended(_)
        )
    }

    /// Applies `event` and says what to do.
    pub fn on(&mut self, event: StreamsEvent) -> StreamsAction {
        let (state, action) = Self::next(self.state, event);
        self.state = state;
        action
    }

    fn next(state: StreamsState, event: StreamsEvent) -> (StreamsState, StreamsAction) {
        use StreamsAction as A;
        use StreamsState as S;

        let end = |end: DesktopEnd| (S::Ended(end), A::End(end));
        let replace = |attempt: u32, suspended: Option<Suspension>| {
            (
                S::Replacing { attempt, suspended },
                A::Establish {
                    after: replace_delay(attempt),
                },
            )
        };
        let suspend = |shield_active: Option<bool>| {
            let suspension = if shield_active == Some(true) {
                Suspension::Locked
            } else {
                Suspension::Inhibited
            };
            (S::Suspended(suspension), A::Suspend(suspension))
        };
        match (state, event) {
            (S::Ended(ended), _) => (state, A::End(ended)),
            (_, StreamsEvent::LoggedOut) => end(DesktopEnd::LoggedOut),
            (_, StreamsEvent::CompositorExited) => end(DesktopEnd::CompositorExited),
            (_, StreamsEvent::Stopped) => end(DesktopEnd::Stopped),

            (
                S::Live,
                StreamsEvent::Closed {
                    replaceable: false, ..
                },
            ) => end(DesktopEnd::StreamsLost),
            // The shield is already up: Mutter refuses until it goes down.
            (
                S::Live,
                StreamsEvent::Closed {
                    shield_active: Some(true),
                    ..
                },
            ) => suspend(Some(true)),
            // Usually the shield is not reported yet (the shell announces it
            // after its fade): ask Mutter, whose answer is the fact.
            (S::Live, StreamsEvent::Closed { .. }) => replace(1, None),

            (S::Replacing { .. }, StreamsEvent::Established) => (S::Live, A::Resume),
            (
                S::Replacing { .. },
                StreamsEvent::Refused {
                    inhibited,
                    shield_active,
                },
            ) if inhibited || shield_active == Some(true) => suspend(shield_active),
            // Refused for another reason with no shield up, a bounded number
            // of times (a suspended desktop's probes too): the streams are lost.
            (S::Replacing { attempt, suspended }, StreamsEvent::Refused { .. }) => {
                if attempt >= REPLACE_ATTEMPTS {
                    end(DesktopEnd::StreamsLost)
                } else {
                    replace(attempt + 1, suspended)
                }
            }
            (
                S::Suspended(suspension),
                StreamsEvent::ShieldChanged(false) | StreamsEvent::ProbeDue,
            ) => replace(1, Some(suspension)),
            (S::Suspended(Suspension::Inhibited), StreamsEvent::ShieldChanged(true)) => (
                S::Suspended(Suspension::Locked),
                A::Suspend(Suspension::Locked),
            ),
            // Live streams ignore shield and probe events; a replacement in
            // flight finishes first and its own answer decides; a suspended
            // desktop keeps waiting.
            _ => (state, A::Continue),
        }
    }
}

/// What an attachment does with a desktop whose streams are unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachWait {
    /// The streams are live: capture them.
    Proceed,
    /// Ask again after this long.
    Poll(Duration),
    /// Give up: the desktop stays locked (or keeps refusing remote access).
    Refuse,
}

/// Whether an attachment that has waited `waited` for a desktop whose
/// streams are (`unavailable`) waits on.
#[must_use]
pub fn attach_wait(unavailable: bool, waited: Duration) -> AttachWait {
    if !unavailable {
        AttachWait::Proceed
    } else if waited >= ATTACH_RESUME_WAIT {
        AttachWait::Refuse
    } else {
        AttachWait::Poll(ATTACH_RESUME_POLL.min(ATTACH_RESUME_WAIT.saturating_sub(waited)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(shield: Option<bool>) -> StreamsEvent {
        StreamsEvent::Closed {
            replaceable: true,
            shield_active: shield,
        }
    }

    fn refused(inhibited: bool, shield: Option<bool>) -> StreamsEvent {
        StreamsEvent::Refused {
            inhibited,
            shield_active: shield,
        }
    }

    #[test]
    fn mutters_refusal_text_marks_the_inhibition() {
        assert!(refusal_is_inhibited(
            "org.freedesktop.DBus.Error.Failed: Failed to create remote desktop session: Session creation inhibited"
        ));
        assert!(!refusal_is_inhibited(
            "org.freedesktop.DBus.Error.ServiceUnknown: name has no owner"
        ));
    }

    #[test]
    fn a_lock_suspends_the_streams_and_keeps_the_desktop_until_the_unlock() {
        let mut lifetime = StreamsLifetime::new();
        // The shield is not reported yet when the streams close: Mutter is
        // asked, and refuses because of the inhibition.
        assert_eq!(
            lifetime.on(closed(Some(false))),
            StreamsAction::Establish {
                after: Duration::ZERO
            }
        );
        assert!(lifetime.streams_unavailable());
        assert_eq!(
            lifetime.on(refused(true, Some(false))),
            StreamsAction::Suspend(Suspension::Inhibited)
        );
        assert_eq!(
            lifetime.on(StreamsEvent::ShieldChanged(true)),
            StreamsAction::Suspend(Suspension::Locked)
        );
        // Probes while locked are refused again and never end the desktop.
        for _ in 0..100 {
            assert_eq!(
                lifetime.on(StreamsEvent::ProbeDue),
                StreamsAction::Establish {
                    after: Duration::ZERO
                }
            );
            assert_eq!(
                lifetime.on(refused(true, Some(true))),
                StreamsAction::Suspend(Suspension::Locked)
            );
        }
        assert!(lifetime.streams_unavailable());
        // The unlock: the shield goes down and the outputs are recorded again.
        assert_eq!(
            lifetime.on(StreamsEvent::ShieldChanged(false)),
            StreamsAction::Establish {
                after: Duration::ZERO
            }
        );
        assert_eq!(
            lifetime.on(StreamsEvent::Established),
            StreamsAction::Resume
        );
        assert_eq!(lifetime.state(), StreamsState::Live);
        assert!(!lifetime.streams_unavailable());
    }

    #[test]
    fn streams_that_close_under_a_raised_shield_wait_without_asking() {
        let mut lifetime = StreamsLifetime::new();
        assert_eq!(
            lifetime.on(closed(Some(true))),
            StreamsAction::Suspend(Suspension::Locked)
        );
        // A refusal for another reason while the shield is up still waits.
        lifetime.on(StreamsEvent::ProbeDue);
        assert_eq!(
            lifetime.on(refused(false, Some(true))),
            StreamsAction::Suspend(Suspension::Locked)
        );
    }

    #[test]
    fn a_logout_ends_the_desktop_live_or_suspended() {
        let mut live = StreamsLifetime::new();
        assert_eq!(
            live.on(StreamsEvent::LoggedOut),
            StreamsAction::End(DesktopEnd::LoggedOut)
        );
        let mut locked = StreamsLifetime::new();
        locked.on(closed(Some(true)));
        assert_eq!(
            locked.on(StreamsEvent::LoggedOut),
            StreamsAction::End(DesktopEnd::LoggedOut)
        );
        // Ended is final.
        assert_eq!(
            locked.on(StreamsEvent::Established),
            StreamsAction::End(DesktopEnd::LoggedOut)
        );
        assert_eq!(locked.state(), StreamsState::Ended(DesktopEnd::LoggedOut));
    }

    #[test]
    fn a_compositor_that_exits_ends_the_desktop_even_while_locked() {
        let mut lifetime = StreamsLifetime::new();
        lifetime.on(closed(None));
        lifetime.on(refused(true, None));
        assert_eq!(
            lifetime.on(StreamsEvent::CompositorExited),
            StreamsAction::End(DesktopEnd::CompositorExited)
        );
        let mut stopped = StreamsLifetime::new();
        assert_eq!(
            stopped.on(StreamsEvent::Stopped),
            StreamsAction::End(DesktopEnd::Stopped)
        );
    }

    #[test]
    fn streams_that_fail_while_unlocked_are_replaced_a_bounded_number_of_times() {
        let mut replaced = StreamsLifetime::new();
        replaced.on(closed(Some(false)));
        assert_eq!(
            replaced.on(refused(false, Some(false))),
            StreamsAction::Establish {
                after: Duration::from_secs(1)
            }
        );
        assert_eq!(
            replaced.on(StreamsEvent::Established),
            StreamsAction::Resume
        );

        let mut lost = StreamsLifetime::new();
        lost.on(closed(Some(false)));
        let mut action = StreamsAction::Continue;
        for _ in 0..REPLACE_ATTEMPTS {
            action = lost.on(refused(false, Some(false)));
        }
        assert_eq!(action, StreamsAction::End(DesktopEnd::StreamsLost));
    }

    #[test]
    fn a_suspended_desktop_whose_probes_keep_failing_unlocked_is_given_up() {
        let mut lifetime = StreamsLifetime::new();
        lifetime.on(closed(Some(true)));
        lifetime.on(StreamsEvent::ShieldChanged(false));
        lifetime.on(refused(false, Some(false)));
        lifetime.on(refused(false, Some(false)));
        assert_eq!(
            lifetime.on(refused(false, Some(false))),
            StreamsAction::End(DesktopEnd::StreamsLost)
        );
    }

    #[test]
    fn fixed_outputs_cannot_be_recorded_again() {
        let mut lifetime = StreamsLifetime::new();
        assert_eq!(
            lifetime.on(StreamsEvent::Closed {
                replaceable: false,
                shield_active: Some(true),
            }),
            StreamsAction::End(DesktopEnd::StreamsLost)
        );
    }

    #[test]
    fn shield_and_probe_events_change_nothing_while_live() {
        let mut lifetime = StreamsLifetime::new();
        assert_eq!(
            lifetime.on(StreamsEvent::ShieldChanged(true)),
            StreamsAction::Continue
        );
        assert_eq!(lifetime.on(StreamsEvent::ProbeDue), StreamsAction::Continue);
        assert_eq!(lifetime.state(), StreamsState::Live);
    }

    #[test]
    fn an_attachment_waits_a_bounded_time_for_suspended_streams() {
        assert_eq!(attach_wait(false, Duration::ZERO), AttachWait::Proceed);
        assert_eq!(
            attach_wait(true, Duration::ZERO),
            AttachWait::Poll(ATTACH_RESUME_POLL)
        );
        let almost = ATTACH_RESUME_WAIT.saturating_sub(Duration::from_millis(100));
        assert_eq!(almost + Duration::from_millis(100), ATTACH_RESUME_WAIT);
        assert_eq!(
            attach_wait(true, almost),
            AttachWait::Poll(Duration::from_millis(100))
        );
        assert_eq!(attach_wait(true, ATTACH_RESUME_WAIT), AttachWait::Refuse);
        assert_eq!(
            attach_wait(false, ATTACH_RESUME_WAIT * 2),
            AttachWait::Proceed
        );
    }
}
