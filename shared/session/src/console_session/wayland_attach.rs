// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Attaching a Deck to the account's own GNOME/Mutter Wayland session (ADR
//! 0025 §9, GAP-042, S4b), once [`super::wayland_join`] chose and proved it.
//!
//! The session is joined, never replaced: the Pier's agent runs as the
//! account on the session's own user bus, records the session's own
//! monitors (`ScreenCast.RecordMonitor`) on a `RemoteDesktop` session paired
//! with it, and injects input through that session. It never starts a bus
//! or a compositor, never changes the session's monitors, and never ends
//! the session. This module holds every decision of that attachment that
//! does not need the operating system:
//!
//! - [`joined_outputs`]: which of the session's monitors are recorded, at
//!   the size, refresh, place and scale the session runs them;
//! - [`desk_privacy`]: whether the desk can be kept from seeing the joined
//!   session (a Wayland compositor's monitors cannot be blanked from
//!   outside it without changing its state, so a session showing on a
//!   physical monitor is refused while desk privacy is required);
//! - [`JoinRouteProof`] and [`decide_serve`]: the Deck is served only when
//!   the whole route was proved on the running session — capture, input
//!   and lock — and refused with a typed code otherwise;
//! - [`LockStep`] and [`after_unlock`]: GNOME Shell closes screen sharing
//!   and refuses new sharing sessions while the session is locked, so a
//!   locked session is unlocked (the account's password was just proven,
//!   as for an X11 session behind its login screen) before it is joined,
//!   and refused if it does not unlock;
//! - [`AFTER_DECK_LEFT`]: when the Deck leaves, the session is locked and
//!   the attachment ends. It is not held for a resume: the lock closes the
//!   sharing sessions, so a returning Deck signs in again and the same
//!   session is joined afresh.

use super::detached_hold::AfterDeckLeft;
use super::{ConsoleRefusal, wayland_join::WaylandJoin};

/// The product names Mutter gives the monitors it makes itself: a headless
/// session's virtual monitor and a `ScreenCast.RecordVirtual` one.
pub const VIRTUAL_MONITOR_PRODUCTS: [&str; 2] = ["MetaVirtualMonitor", "Virtual remote monitor"];

/// Whether a monitor of `product` is one the compositor made (nothing at
/// a desk shows it).
#[must_use]
pub fn is_virtual_product(product: &str) -> bool {
    VIRTUAL_MONITOR_PRODUCTS.contains(&product)
}

/// Whether the session may show on a monitor at the desk: an enabled
/// monitor (one in a logical monitor) that the compositor did not make,
/// or no enabled monitor at all (nothing proves the desk dark). Read
/// before anything of the session is brought forward or unlocked, so a
/// join that desk privacy refuses never shows the session at the desk.
#[must_use]
pub fn monitors_at_desk(monitors: &[JoinMonitor]) -> bool {
    let mut enabled = monitors
        .iter()
        .filter(|monitor| monitor.placement.is_some())
        .peekable();
    enabled.peek().is_none() || enabled.any(|monitor| !monitor.virtual_monitor)
}

/// One monitor of the joined session, as its compositor reports it
/// (`DisplayConfig.GetCurrentState`).
#[derive(Debug, Clone, PartialEq)]
pub struct JoinMonitor {
    /// Connector name (`DP-1`, `Meta-0`…).
    pub connector: String,
    /// A monitor the compositor made (a headless session's virtual
    /// monitor, a `RecordVirtual` monitor): nothing at the desk shows it.
    pub virtual_monitor: bool,
    /// The current mode: width, height, refresh in millihertz.
    pub mode: Option<(u32, u32, u32)>,
    /// The place of its logical monitor, when it is enabled.
    pub placement: Option<(i32, i32)>,
    /// Its logical monitor's UI scale.
    pub scale: f64,
}

/// A monitor the attachment records, at the size the session runs it.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinedOutput {
    pub connector: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    /// Shown on a monitor at the desk.
    pub at_desk: bool,
}

/// The session's enabled monitors, in the compositor's order: each is
/// recorded as it is; nothing is reconfigured.
///
/// # Errors
///
/// [`ConsoleRefusal::SessionUnreachable`] when the session shows nothing
/// (no enabled monitor with a mode).
pub fn joined_outputs(monitors: &[JoinMonitor]) -> Result<Vec<JoinedOutput>, ConsoleRefusal> {
    let outputs: Vec<JoinedOutput> = monitors
        .iter()
        .filter_map(|monitor| {
            let (width, height, millihertz) = monitor.mode?;
            let (x, y) = monitor.placement?;
            (width > 0 && height > 0).then(|| JoinedOutput {
                connector: monitor.connector.clone(),
                width,
                height,
                refresh_hz: millihertz.saturating_add(500).div_euclid(1000).max(1),
                x,
                y,
                scale: if monitor.scale.is_finite() && monitor.scale > 0.0 {
                    monitor.scale
                } else {
                    1.0
                },
                at_desk: !monitor.virtual_monitor,
            })
        })
        .collect();
    if outputs.is_empty() {
        Err(ConsoleRefusal::SessionUnreachable)
    } else {
        Ok(outputs)
    }
}

/// Whether the desk can be kept from seeing the joined session.
///
/// The desk's input is disabled under the compositor, at the kernel, as
/// for an X11 session. Its monitors are another matter: blanking them
/// would change the user's compositor (its power-save mode or its colour
/// state), which a joined session never is, and Mutter stops painting a
/// monitor it powered down. So while desk privacy is required
/// (`desk_private`), a session showing on a monitor at the desk is refused
/// `console_privacy_unavailable`; one whose monitors are all the
/// compositor's own (a headless session) shows nothing at the desk.
///
/// `at_desk`: a recorded monitor is shown at the desk ([`shown_at_desk`]).
///
/// # Errors
///
/// [`ConsoleRefusal::PrivacyUnavailable`] as above.
pub const fn desk_privacy(at_desk: bool, desk_private: bool) -> Result<(), ConsoleRefusal> {
    if desk_private && at_desk {
        Err(ConsoleRefusal::PrivacyUnavailable)
    } else {
        Ok(())
    }
}

/// Whether any of `outputs` is shown on a monitor at the desk.
#[must_use]
pub fn shown_at_desk(outputs: &[JoinedOutput]) -> bool {
    outputs.iter().any(|output| output.at_desk)
}

/// What a joined attachment does when its compositor reports that the
/// session's monitors changed (`DisplayConfig.MonitorsChanged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorChangeStep {
    /// Nothing the attachment recorded or judged for the desk changed.
    Keep,
    /// The recorded monitors changed: the attachment ends, and its
    /// hand-back locks the session as when the Deck leaves.
    End,
    /// The session now shows, or may show, on a monitor at the desk where
    /// it did not when joined: it is locked at once, from inside the
    /// session, and then the attachment ends.
    LockNow,
}

/// The step for a monitor change: `recorded_same` when the enabled
/// monitors are still the recorded ones, `at_desk_when_joined` as judged
/// at the join, and `at_desk_now` as read after the change (`None` when it
/// cannot be read).
#[must_use]
pub const fn monitor_change_step(
    recorded_same: bool,
    at_desk_when_joined: bool,
    at_desk_now: Option<bool>,
) -> MonitorChangeStep {
    match at_desk_now {
        None => MonitorChangeStep::LockNow,
        Some(true) if !at_desk_when_joined => MonitorChangeStep::LockNow,
        Some(now) if recorded_same && now == at_desk_when_joined => MonitorChangeStep::Keep,
        Some(_) => MonitorChangeStep::End,
    }
}

/// What the agent proved on the running session before the Deck is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinRouteProof {
    /// Every recorded monitor announced a `PipeWire` stream.
    pub capture: bool,
    /// The `RemoteDesktop` session started, is paired with the screen
    /// cast, takes keyboard and pointer, and accepted an absolute pointer
    /// motion addressed to a stream.
    pub input: bool,
    /// The session's own shell locks it when logind asks (the compositor's
    /// connection is GNOME Shell's, and the account's settings allow
    /// locking), so the hand-back can lock it; the hand-back still checks
    /// that it did, and keeps the desk private while it has not.
    pub lock: bool,
}

/// The first part of the route a joined session did not prove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteGap {
    Capture,
    Input,
    Lock,
}

impl RouteGap {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Capture => "capture_unproven",
            Self::Input => "input_unproven",
            Self::Lock => "lock_unproven",
        }
    }
}

impl JoinRouteProof {
    /// The first gap, or `None` when the whole route is proved.
    #[must_use]
    pub const fn gap(&self) -> Option<RouteGap> {
        if !self.capture {
            Some(RouteGap::Capture)
        } else if !self.input {
            Some(RouteGap::Input)
        } else if !self.lock {
            Some(RouteGap::Lock)
        } else {
            None
        }
    }
}

/// Whether the Deck is served: only when capture, input and lock were all
/// proved on this session. A join is never advertised on less.
///
/// # Errors
///
/// [`ConsoleRefusal::SessionUnreachable`] with the first [`RouteGap`].
pub const fn decide_serve(proof: JoinRouteProof) -> Result<(), (ConsoleRefusal, RouteGap)> {
    match proof.gap() {
        None => Ok(()),
        Some(gap) => Err((ConsoleRefusal::SessionUnreachable, gap)),
    }
}

/// What the launcher does with a proven join before its agent starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStep {
    /// The session is unlocked: join it.
    Attach,
    /// The session is locked, and GNOME shares nothing of a locked
    /// session: unlock it through logind first (the account's password was
    /// just proven), with the desk's input already disabled.
    UnlockFirst,
}

/// The lock step of a proven join, or `None` for an outcome that is not a
/// join.
#[must_use]
pub const fn lock_step(join: &WaylandJoin) -> Option<LockStep> {
    match join {
        WaylandJoin::Join { .. } => Some(LockStep::Attach),
        WaylandJoin::HandleLock { .. } => Some(LockStep::UnlockFirst),
        WaylandJoin::Refuse { .. } | WaylandJoin::AdapterUnavailable { .. } => None,
    }
}

/// After asking logind to unlock: join only a session whose shell reported
/// it unlocked (`LockedHint` cleared).
///
/// # Errors
///
/// [`ConsoleRefusal::SessionUnreachable`] when it stayed locked.
pub const fn after_unlock(still_locked: bool) -> Result<(), ConsoleRefusal> {
    if still_locked {
        Err(ConsoleRefusal::SessionUnreachable)
    } else {
        Ok(())
    }
}

/// What a joined Wayland session does when its Deck leaves: it is handed
/// back locked and the attachment ends ([`AfterDeckLeft::LeaveConsole`]),
/// even after a lost transport. GNOME closes the sharing sessions on lock,
/// so nothing could be held for a resume; the returning Deck signs in
/// again and joins the same session.
pub const AFTER_DECK_LEFT: AfterDeckLeft = AfterDeckLeft::LeaveConsole;

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(connector: &str, virtual_monitor: bool, placed: bool) -> JoinMonitor {
        JoinMonitor {
            connector: connector.to_owned(),
            virtual_monitor,
            mode: Some((1920, 1080, 59_951)),
            placement: placed.then_some((0, 0)),
            scale: 1.0,
        }
    }

    #[test]
    fn the_sessions_own_enabled_monitors_are_recorded_as_they_run() {
        let mut right = monitor("DP-2", false, true);
        right.placement = Some((1920, 0));
        right.mode = Some((2560, 1440, 143_912));
        right.scale = 1.25;
        let outputs = joined_outputs(&[
            monitor("DP-1", false, true),
            right,
            monitor("HDMI-1", false, false),
        ])
        .expect("outputs");
        assert_eq!(outputs.len(), 2, "a disabled monitor is not recorded");
        assert_eq!(
            (outputs[0].width, outputs[0].height, outputs[0].refresh_hz),
            (1920, 1080, 60)
        );
        assert_eq!(
            (outputs[1].x, outputs[1].refresh_hz, outputs[1].scale),
            (1920, 144, 1.25)
        );
        assert!(outputs.iter().all(|output| output.at_desk));
        let headless = joined_outputs(&[monitor("Meta-0", true, true)]).expect("headless");
        assert!(!headless[0].at_desk);
    }

    #[test]
    fn the_desk_is_judged_from_the_enabled_monitors_before_anything_is_shown() {
        let mut physical = monitor("DP-1", false, true);
        physical.mode = None;
        assert!(monitors_at_desk(&[physical]));
        assert!(!monitors_at_desk(&[
            monitor("Meta-0", true, true),
            monitor("HDMI-1", false, false)
        ]));
        assert!(monitors_at_desk(&[]));
        assert!(monitors_at_desk(&[monitor("Meta-0", true, false)]));
        assert!(is_virtual_product("MetaVirtualMonitor"));
        assert!(!is_virtual_product("DELL U2720Q"));
    }

    #[test]
    fn a_monitor_appearing_at_the_desk_locks_the_session_at_once() {
        use MonitorChangeStep::{End, Keep, LockNow};
        // A desk monitor plugged in, or the monitors unreadable after a
        // change: locked at once, not after the attachment winds down.
        assert_eq!(monitor_change_step(false, false, Some(true)), LockNow);
        assert_eq!(monitor_change_step(true, false, Some(true)), LockNow);
        assert_eq!(monitor_change_step(false, false, None), LockNow);
        assert_eq!(monitor_change_step(true, false, None), LockNow);
        // Any other change ends the attachment; the hand-back locks.
        assert_eq!(monitor_change_step(false, false, Some(false)), End);
        assert_eq!(monitor_change_step(false, true, Some(true)), End);
        assert_eq!(monitor_change_step(true, true, Some(false)), End);
        assert_eq!(monitor_change_step(true, false, Some(false)), Keep);
        assert_eq!(monitor_change_step(true, true, Some(true)), Keep);
    }

    #[test]
    fn a_session_that_shows_nothing_is_unreachable() {
        assert_eq!(joined_outputs(&[]), Err(ConsoleRefusal::SessionUnreachable));
        let mut modeless = monitor("DP-1", false, true);
        modeless.mode = None;
        assert_eq!(
            joined_outputs(&[modeless, monitor("DP-2", false, false)]),
            Err(ConsoleRefusal::SessionUnreachable)
        );
    }

    #[test]
    fn a_session_on_a_monitor_at_the_desk_is_refused_while_the_desk_must_be_private() {
        let physical = shown_at_desk(&joined_outputs(&[monitor("DP-1", false, true)]).unwrap());
        let headless = shown_at_desk(&joined_outputs(&[monitor("Meta-0", true, true)]).unwrap());
        assert_eq!(
            desk_privacy(physical, true),
            Err(ConsoleRefusal::PrivacyUnavailable)
        );
        assert_eq!(desk_privacy(physical, false), Ok(()));
        assert_eq!(desk_privacy(headless, true), Ok(()));
    }

    #[test]
    fn the_deck_is_served_only_on_a_fully_proved_route() {
        let full = JoinRouteProof {
            capture: true,
            input: true,
            lock: true,
        };
        assert_eq!(decide_serve(full), Ok(()));
        for (proof, gap) in [
            (
                JoinRouteProof {
                    capture: false,
                    ..full
                },
                RouteGap::Capture,
            ),
            (
                JoinRouteProof {
                    input: false,
                    ..full
                },
                RouteGap::Input,
            ),
            (
                JoinRouteProof {
                    lock: false,
                    ..full
                },
                RouteGap::Lock,
            ),
        ] {
            assert_eq!(
                decide_serve(proof),
                Err((ConsoleRefusal::SessionUnreachable, gap)),
                "{}",
                gap.token()
            );
        }
    }

    #[test]
    fn a_locked_session_is_unlocked_before_it_is_joined_and_refused_if_it_stays_locked() {
        let join = WaylandJoin::Join {
            session: "7".to_owned(),
            activate: false,
            compositor: 42,
        };
        let locked = WaylandJoin::HandleLock {
            session: "7".to_owned(),
            activate: false,
            compositor: 42,
        };
        assert_eq!(lock_step(&join), Some(LockStep::Attach));
        assert_eq!(lock_step(&locked), Some(LockStep::UnlockFirst));
        assert_eq!(
            lock_step(&WaylandJoin::Refuse {
                refusal: ConsoleRefusal::SessionUnreachable,
                proof: super::super::wayland_join::CompositorProof::Verified { pid: 42 },
            }),
            None
        );
        assert_eq!(after_unlock(false), Ok(()));
        assert_eq!(after_unlock(true), Err(ConsoleRefusal::SessionUnreachable));
    }
}
