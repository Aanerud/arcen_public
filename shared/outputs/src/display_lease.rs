// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What a session must leave behind: the display-lease policy every Pier
//! follows when a session ends.
//!
//! A Pier changes the host's displays to serve a Deck: it may create displays
//! (an `IddCx` monitor, an NVIDIA headless EDID, a `CGVirtualDisplay`, a
//! private Xorg or Mutter virtual monitor) and it may change displays it did
//! not create (activate, deactivate, re-position, re-mode a monitor, a dummy
//! dongle or a third-party virtual display). When the session ends the host
//! must return to a state its local user can use:
//!
//! - **When** ([`display_lease_action`]): never during a brief network loss
//!   that the session's restore lease covers; on an explicit disconnect, when
//!   that lease expires, when the owner dies, and — for a record left by an
//!   unclean exit — when the Pier next starts or the console changes hands.
//! - **What** ([`plan_teardown`] and [`verify_restore`]): displays Arcen
//!   created are removed; displays Arcen did not create are never removed,
//!   only returned to their pre-session state (active, primary, position,
//!   mode); a display the user plugged in during the session is left alone.
//! - **Windows** ([`plan_window_repatriation`]): when a session gathered the
//!   desktop onto a display that was not the local user's primary, windows
//!   that ended up there are moved back to where they were, or to the primary;
//!   windows that sit on no active display are moved to the primary.
//!
//! Display and window identities are opaque strings and numbers chosen by
//! the platform adapter (a GDI device name, a `CGDirectDisplayID`, an output
//! name; an `HWND`, a window number). Geometry is in one desktop coordinate
//! space chosen by the adapter.

use std::collections::{BTreeMap, BTreeSet};

/// A rectangle in desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopArea {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DesktopArea {
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn right(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    fn bottom(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    fn intersection_area(self, other: Self) -> u64 {
        let left = i64::from(self.x).max(i64::from(other.x));
        let top = i64::from(self.y).max(i64::from(other.y));
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= left || bottom <= top {
            return 0;
        }
        u64::try_from((right - left) * (bottom - top)).unwrap_or(0)
    }

    /// Whether `self` lies entirely inside `outer`.
    #[must_use]
    pub fn within(self, outer: Self) -> bool {
        self.x >= outer.x
            && self.y >= outer.y
            && self.right() <= outer.right()
            && self.bottom() <= outer.bottom()
    }
}

/// Who brought a display into the session's topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayOrigin {
    /// Arcen created it for the session (it did not exist before).
    CreatedByArcen,
    /// It belongs to the host: a monitor, a dummy dongle, a third-party
    /// virtual display driver. Arcen may change it, never remove it.
    Foreign,
}

/// One display's state as the topology reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayState {
    pub key: String,
    pub origin: DisplayOrigin,
    pub active: bool,
    pub primary: bool,
    pub area: DesktopArea,
    pub refresh_hz: u32,
}

/// What one display needs when the session's displays are returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownStep {
    /// Arcen created it: remove it.
    Remove { key: String },
    /// A host display Arcen changed: return it to its pre-session state.
    Restore { key: String },
    /// A host display already in its pre-session state.
    Keep { key: String },
    /// A host display that appeared during the session (plugged in by the
    /// user): Arcen never touches it.
    LeaveUntouched { key: String },
    /// A host display that was there before the session and is gone now
    /// (unplugged): nothing to restore, and an exact snapshot restore that
    /// names it cannot succeed.
    Departed { key: String },
}

/// The ordered work to return the displays, and whether the pre-session
/// snapshot can be applied as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownPlan {
    pub steps: Vec<TeardownStep>,
    /// Every display the pre-session snapshot names still exists, so the
    /// snapshot can be applied exactly. When false a host must restore what
    /// remains (its OS's remembered layout for the connected set) instead.
    pub exact_snapshot_applies: bool,
}

impl TeardownPlan {
    /// Whether nothing needs doing.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.steps.iter().all(|step| {
            matches!(
                step,
                TeardownStep::Keep { .. }
                    | TeardownStep::LeaveUntouched { .. }
                    | TeardownStep::Departed { .. }
            )
        })
    }
}

fn same_state(before: &DisplayState, now: &DisplayState) -> bool {
    before.active == now.active
        && (!before.active
            || (before.primary == now.primary
                && before.area == now.area
                && before.refresh_hz == now.refresh_hz))
}

/// Decides what each display needs when the session ends.
///
/// `pre` is the topology recorded before Arcen changed anything; `now` is the
/// topology at teardown. A display in `now` that `pre` does not name is
/// Arcen's only when its origin says so; otherwise the user plugged it in.
#[must_use]
pub fn plan_teardown(pre: &[DisplayState], now: &[DisplayState]) -> TeardownPlan {
    let before: BTreeMap<&str, &DisplayState> = pre
        .iter()
        .filter(|display| display.origin == DisplayOrigin::Foreign)
        .map(|display| (display.key.as_str(), display))
        .collect();
    let present: BTreeSet<&str> = now.iter().map(|display| display.key.as_str()).collect();
    let mut steps = Vec::new();
    for display in now {
        let key = display.key.clone();
        steps.push(match (display.origin, before.get(display.key.as_str())) {
            (DisplayOrigin::CreatedByArcen, _) => TeardownStep::Remove { key },
            (DisplayOrigin::Foreign, None) => TeardownStep::LeaveUntouched { key },
            (DisplayOrigin::Foreign, Some(before)) if same_state(before, display) => {
                TeardownStep::Keep { key }
            }
            (DisplayOrigin::Foreign, Some(_)) => TeardownStep::Restore { key },
        });
    }
    let mut exact_snapshot_applies = true;
    for key in before.keys() {
        if !present.contains(key) {
            exact_snapshot_applies = false;
            steps.push(TeardownStep::Departed {
                key: (*key).to_string(),
            });
        }
    }
    TeardownPlan {
        steps,
        exact_snapshot_applies,
    }
}

/// One way the topology after a restore differs from before the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divergence {
    /// A display Arcen created is still there.
    ArcenDisplayRemains { key: String },
    /// A host display that was active is not (or the reverse).
    Activation { key: String, expected: bool },
    /// The primary display is not the one it was.
    Primary {
        expected: Option<String>,
        actual: Option<String>,
    },
    /// An active display sits somewhere else.
    Position { key: String },
    /// An active display runs another mode.
    Mode { key: String },
}

impl Divergence {
    /// Whether this is a topology difference (what is shown where at all),
    /// as opposed to a layout detail (exact position or mode). A restore may
    /// tolerate a mode the display no longer offers; it never tolerates a
    /// topology difference.
    #[must_use]
    pub const fn is_topology(&self) -> bool {
        matches!(
            self,
            Self::ArcenDisplayRemains { .. } | Self::Activation { .. } | Self::Primary { .. }
        )
    }
}

/// The result of comparing the restored topology with the pre-session one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreVerdict {
    pub divergences: Vec<Divergence>,
}

impl RestoreVerdict {
    /// Every display is back exactly.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        self.divergences.is_empty()
    }

    /// The topology is back (active set, primary, no Arcen display left);
    /// only modes or positions of displays whose mode was withdrawn differ.
    #[must_use]
    pub fn topology_restored(&self) -> bool {
        !self.divergences.iter().any(Divergence::is_topology)
    }
}

fn primary_key(displays: &[DisplayState]) -> Option<String> {
    displays
        .iter()
        .find(|display| display.active && display.primary)
        .map(|display| display.key.clone())
}

/// Compares the topology after a restore with the pre-session one.
///
/// Displays that departed during the session and displays the user plugged
/// in are not divergences: neither can, nor may, be restored.
#[must_use]
pub fn verify_restore(pre: &[DisplayState], now: &[DisplayState]) -> RestoreVerdict {
    let before: BTreeMap<&str, &DisplayState> = pre
        .iter()
        .filter(|display| display.origin == DisplayOrigin::Foreign)
        .map(|display| (display.key.as_str(), display))
        .collect();
    let mut divergences = Vec::new();
    for display in now {
        if display.origin == DisplayOrigin::CreatedByArcen {
            divergences.push(Divergence::ArcenDisplayRemains {
                key: display.key.clone(),
            });
            continue;
        }
        let Some(before) = before.get(display.key.as_str()) else {
            continue;
        };
        if before.active != display.active {
            divergences.push(Divergence::Activation {
                key: display.key.clone(),
                expected: before.active,
            });
            continue;
        }
        if !display.active {
            continue;
        }
        if before.area.x != display.area.x || before.area.y != display.area.y {
            divergences.push(Divergence::Position {
                key: display.key.clone(),
            });
        }
        if before.area.width != display.area.width
            || before.area.height != display.area.height
            || before.refresh_hz != display.refresh_hz
        {
            divergences.push(Divergence::Mode {
                key: display.key.clone(),
            });
        }
    }
    let present: BTreeSet<&str> = now.iter().map(|display| display.key.as_str()).collect();
    let expected_primary = primary_key(pre)
        .filter(|key| present.contains(key.as_str()) && before.contains_key(key.as_str()));
    let actual_primary = primary_key(now);
    if expected_primary.is_some() && expected_primary != actual_primary {
        divergences.push(Divergence::Primary {
            expected: expected_primary,
            actual: actual_primary,
        });
    }
    RestoreVerdict { divergences }
}

/// Something that happens to a session's display lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayLeaseEvent {
    /// The transport was lost and the session's restore lease covers a
    /// reconnect (`arcen_session::direct_reconnect` holds restore leases).
    TransportLostWithinLease,
    /// The restore lease expired with no reconnect.
    LeaseExpired,
    /// The Deck disconnected on purpose, or the Pier is shutting the session.
    ExplicitDisconnect,
    /// The process that owns the display changes died.
    OwnerEnded,
    /// The Pier started.
    PierStarted,
    /// The console changed hands (sign-in, sign-out, unlock).
    ConsoleChanged,
}

/// What to do with the session's display changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayLeaseAction {
    /// Keep the session's layout: a reconnect must find it unchanged.
    Hold,
    /// Return the displays now (the owner restores, or its watchdog does).
    RestoreNow,
    /// Replay a recovery record left by an unclean exit, if there is one.
    RecoverPending,
    /// Nothing to do: a live session owns its record.
    Nothing,
}

/// Decides when a session's display changes are returned.
///
/// `session_live` says whether any session still owns display changes (an
/// attached or detached-within-lease session): a recovery record then belongs
/// to it and must not be replayed underneath it.
#[must_use]
pub const fn display_lease_action(
    event: DisplayLeaseEvent,
    session_live: bool,
) -> DisplayLeaseAction {
    match event {
        DisplayLeaseEvent::TransportLostWithinLease => DisplayLeaseAction::Hold,
        DisplayLeaseEvent::LeaseExpired
        | DisplayLeaseEvent::ExplicitDisconnect
        | DisplayLeaseEvent::OwnerEnded => DisplayLeaseAction::RestoreNow,
        DisplayLeaseEvent::PierStarted | DisplayLeaseEvent::ConsoleChanged => {
            if session_live {
                DisplayLeaseAction::Nothing
            } else {
                DisplayLeaseAction::RecoverPending
            }
        }
    }
}

/// A display as the local desktop sees it after the restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDisplay {
    pub key: String,
    /// The usable area (excluding task bars and docks).
    pub work_area: DesktopArea,
    pub primary: bool,
}

/// A top-level window recorded before the session changed the displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowBefore {
    pub id: u64,
    pub display: String,
    /// The window's normal (restored) rectangle.
    pub normal: DesktopArea,
}

/// A top-level window after the restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowNow {
    pub id: u64,
    /// The active display the window is on, or `None` when it intersects no
    /// active display (it is off-screen).
    pub display: Option<String>,
    /// The window's normal (restored) rectangle.
    pub normal: DesktopArea,
}

/// What the session left behind, for the window decision.
#[derive(Debug, Clone, Copy)]
pub struct RepatriationInput<'a> {
    /// The displays the session served (the desktop it gathered windows on).
    pub session_displays: &'a [String],
    /// The local user's primary display before the session.
    pub pre_session_primary: Option<&'a str>,
    pub windows_before: &'a [WindowBefore],
    pub displays_now: &'a [LocalDisplay],
    pub windows_now: &'a [WindowNow],
}

/// Why a window moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveReason {
    /// It sat on no active display.
    OffScreen,
    /// The session moved it onto its display; it goes back where it was.
    ReturnToPreSessionDisplay,
    /// It was opened during the session on the session's display, which is
    /// not the local user's primary.
    LeaveSessionDisplay,
}

/// One window move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowMove {
    pub id: u64,
    pub from: Option<String>,
    pub to: String,
    pub normal: DesktopArea,
    pub reason: MoveReason,
}

/// Places `window` inside `to`, keeping its offset from `from` when it fits,
/// shrinking it when it is larger than `to`, centring it when it had no
/// display.
#[must_use]
pub fn relocate(window: DesktopArea, from: Option<DesktopArea>, to: DesktopArea) -> DesktopArea {
    let width = window.width.min(to.width);
    let height = window.height.min(to.height);
    let (offset_x, offset_y) = match from {
        Some(from) => (
            i64::from(window.x) - i64::from(from.x),
            i64::from(window.y) - i64::from(from.y),
        ),
        None => (
            (i64::from(to.width) - i64::from(width)) / 2,
            (i64::from(to.height) - i64::from(height)) / 2,
        ),
    };
    let max_x = i64::from(to.width) - i64::from(width);
    let max_y = i64::from(to.height) - i64::from(height);
    let x = i64::from(to.x) + offset_x.clamp(0, max_x);
    let y = i64::from(to.y) + offset_y.clamp(0, max_y);
    DesktopArea {
        x: i32::try_from(x).unwrap_or(to.x),
        y: i32::try_from(y).unwrap_or(to.y),
        width,
        height,
    }
}

/// Decides which windows to move after a session's displays were returned.
///
/// The decision (recorded in `docs/architecture/display-lifecycle.md`):
/// - A window on no active display moves to the primary.
/// - A window on a session display that was recorded on a display the
///   session did not serve goes back there when that display is active.
///   A move between two displays the session served stays.
/// - A window that was on that display before the session stays: that was
///   the user's choice.
/// - A window opened during the session (or whose display is gone) stays on
///   a session display that was the local user's primary (a dummy dongle
///   alone, a monitor alone); otherwise it moves to the primary.
#[must_use]
pub fn plan_window_repatriation(input: &RepatriationInput<'_>) -> Vec<WindowMove> {
    let displays: BTreeMap<&str, &LocalDisplay> = input
        .displays_now
        .iter()
        .map(|display| (display.key.as_str(), display))
        .collect();
    let Some(primary) = input
        .displays_now
        .iter()
        .find(|display| display.primary)
        .or_else(|| input.displays_now.first())
    else {
        return Vec::new();
    };
    let session: BTreeSet<&str> = input.session_displays.iter().map(String::as_str).collect();
    let session_was_primary = input
        .pre_session_primary
        .is_some_and(|primary| session.len() == 1 && session.contains(primary));
    let before: BTreeMap<u64, &WindowBefore> = input
        .windows_before
        .iter()
        .map(|window| (window.id, window))
        .collect();
    let mut moves = Vec::new();
    for window in input.windows_now {
        let current = window
            .display
            .as_deref()
            .and_then(|key| displays.get(key).copied());
        let Some(current) = current else {
            moves.push(WindowMove {
                id: window.id,
                from: window.display.clone(),
                to: primary.key.clone(),
                normal: relocate(window.normal, None, primary.work_area),
                reason: MoveReason::OffScreen,
            });
            continue;
        };
        if !session.contains(current.key.as_str()) {
            continue;
        }
        let previous = before.get(&window.id);
        if previous.is_some_and(|previous| previous.display == current.key) {
            continue;
        }
        // A recorded home off the session's displays wins, whatever the
        // session display was: a window the session pulled off another
        // display goes back there. A move between two displays the session
        // served (a physical multi-monitor session) was the remote user's.
        let home = previous.and_then(|previous| {
            displays
                .get(previous.display.as_str())
                .copied()
                .filter(|display| !session.contains(display.key.as_str()))
                .map(|display| (display, previous.normal))
        });
        if home.is_none()
            && (session_was_primary || input.pre_session_primary == Some(current.key.as_str()))
        {
            continue;
        }
        let (target, normal, reason) = match home {
            Some((display, normal)) if normal.within(display.work_area) => {
                (display, normal, MoveReason::ReturnToPreSessionDisplay)
            }
            Some((display, _)) => (
                display,
                relocate(window.normal, Some(current.work_area), display.work_area),
                MoveReason::ReturnToPreSessionDisplay,
            ),
            None => {
                if primary.key == current.key {
                    continue;
                }
                (
                    primary,
                    relocate(window.normal, Some(current.work_area), primary.work_area),
                    MoveReason::LeaveSessionDisplay,
                )
            }
        };
        moves.push(WindowMove {
            id: window.id,
            from: Some(current.key.clone()),
            to: target.key.clone(),
            normal,
            reason,
        });
    }
    moves
}

/// Which active display a window belongs to: the one it overlaps most, or
/// `None` when it overlaps none.
#[must_use]
pub fn display_of(window: DesktopArea, displays: &[LocalDisplay]) -> Option<String> {
    displays
        .iter()
        .map(|display| (window.intersection_area(display.work_area), display))
        .filter(|(area, _)| *area > 0)
        .max_by_key(|(area, _)| *area)
        .map(|(_, display)| display.key.clone())
}

/// Where a top-level window is, as the window manager reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPlacement {
    pub id: u64,
    /// The client area, in desktop coordinates.
    pub area: DesktopArea,
    pub maximized: bool,
    pub fullscreen: bool,
}

/// The windows of a session whose own displays a Pier laid out for a Deck
/// and has now put back (the Linux Pier's joined console, ADR 0025 §6).
#[derive(Debug, Clone, Copy)]
pub struct WindowRestoreInput<'a> {
    /// Before the Pier changed the displays.
    pub before: &'a [WindowPlacement],
    /// Once the Deck's layout had settled; `None` when they could not be
    /// read then.
    pub after_layout: Option<&'a [WindowPlacement]>,
    /// As the Deck left, still in its layout.
    pub departing: &'a [WindowPlacement],
    /// With the session's own displays back.
    pub now: &'a [WindowPlacement],
    /// The session's own displays, back.
    pub displays_now: &'a [DesktopArea],
}

/// Decides which windows go back where they were before a Deck's layout.
///
/// A window manager moves windows off a display that a layout removes or
/// shrinks, but it does not move them back when the display returns. So
/// each window recorded before the layout goes back to its recorded place,
/// size, maximised and fullscreen state, when:
///
/// - it is not there now;
/// - nobody moved it while the Deck was attached. If it was placed
///   differently as the Deck left than once the layout had settled, the
///   Deck's user moved it, and that choice stays. With no reading after the
///   layout, the window goes back;
/// - its recorded place is on a display that is back.
///
/// A window opened while the Deck was attached has no recorded place and
/// stays where the window manager put it.
#[must_use]
pub fn plan_window_restore(input: &WindowRestoreInput<'_>) -> Vec<WindowPlacement> {
    let by_id = |list: &[WindowPlacement]| -> BTreeMap<u64, WindowPlacement> {
        list.iter().map(|window| (window.id, *window)).collect()
    };
    let before = by_id(input.before);
    let after_layout = input.after_layout.map(by_id);
    let departing = by_id(input.departing);
    let mut restore = Vec::new();
    for now in input.now {
        let Some(recorded) = before.get(&now.id) else {
            continue;
        };
        if recorded == now {
            continue;
        }
        let moved_by_deck = after_layout
            .as_ref()
            .and_then(|after| after.get(&now.id))
            .zip(departing.get(&now.id))
            .is_some_and(|(settled, left)| settled != left);
        if moved_by_deck {
            continue;
        }
        if !input
            .displays_now
            .iter()
            .any(|display| recorded.area.intersection_area(*display) > 0)
        {
            continue;
        }
        restore.push(*recorded);
    }
    restore
}

#[cfg(test)]
mod tests {
    use super::*;

    fn foreign(key: &str, active: bool, primary: bool, x: i32) -> DisplayState {
        DisplayState {
            key: key.to_string(),
            origin: DisplayOrigin::Foreign,
            active,
            primary,
            area: DesktopArea::new(x, 0, 1920, 1080),
            refresh_hz: 60,
        }
    }

    fn local(key: &str, x: i32, primary: bool) -> LocalDisplay {
        LocalDisplay {
            key: key.to_string(),
            work_area: DesktopArea::new(x, 0, 1920, 1040),
            primary,
        }
    }

    #[test]
    fn teardown_removes_only_what_arcen_created_and_leaves_new_displays_alone() {
        let pre = [
            foreign("monitor", true, true, 0),
            foreign("vdd", true, false, 1920),
        ];
        let mut arcen = foreign("arcen", true, true, 0);
        arcen.origin = DisplayOrigin::CreatedByArcen;
        let now = [
            foreign("monitor", false, false, 0),
            foreign("vdd", true, false, 1920),
            arcen,
            foreign("plugged-in", true, false, 3840),
        ];
        let plan = plan_teardown(&pre, &now);
        assert_eq!(
            plan.steps,
            vec![
                TeardownStep::Restore {
                    key: "monitor".into()
                },
                TeardownStep::Keep { key: "vdd".into() },
                TeardownStep::Remove {
                    key: "arcen".into()
                },
                TeardownStep::LeaveUntouched {
                    key: "plugged-in".into()
                },
            ]
        );
        assert!(plan.exact_snapshot_applies);
    }

    #[test]
    fn an_unplugged_display_makes_the_exact_snapshot_inapplicable() {
        let pre = [
            foreign("monitor", true, true, 0),
            foreign("vdd", true, false, 1920),
        ];
        let now = [foreign("vdd", true, true, 0)];
        let plan = plan_teardown(&pre, &now);
        assert!(!plan.exact_snapshot_applies);
        assert!(plan.steps.contains(&TeardownStep::Departed {
            key: "monitor".into()
        }));
    }

    #[test]
    fn an_unchanged_host_needs_nothing() {
        let pre = [foreign("dongle", true, true, 0)];
        assert!(plan_teardown(&pre, &pre).is_noop());
    }

    #[test]
    fn a_withdrawn_mode_is_tolerated_but_an_isolated_topology_is_not() {
        let pre = [
            foreign("monitor", true, true, 0),
            foreign("vdd", true, false, 1920),
        ];
        let mut moded = pre.clone();
        moded[1].area.width = 2560;
        let verdict = verify_restore(&pre, &moded);
        assert!(!verdict.is_exact());
        assert!(verdict.topology_restored());

        let isolated = [
            foreign("monitor", false, false, 0),
            foreign("vdd", true, true, 0),
        ];
        let verdict = verify_restore(&pre, &isolated);
        assert!(!verdict.topology_restored());
        assert!(verdict.divergences.contains(&Divergence::Activation {
            key: "monitor".into(),
            expected: true
        }));
        assert!(verdict.divergences.contains(&Divergence::Primary {
            expected: Some("monitor".into()),
            actual: Some("vdd".into())
        }));
    }

    #[test]
    fn a_left_over_arcen_display_fails_the_restore() {
        let pre = [foreign("monitor", true, true, 0)];
        let mut arcen = foreign("arcen", true, false, 1920);
        arcen.origin = DisplayOrigin::CreatedByArcen;
        let verdict = verify_restore(&pre, &[pre[0].clone(), arcen]);
        assert!(!verdict.topology_restored());
    }

    #[test]
    fn a_departed_primary_is_not_a_divergence() {
        let pre = [
            foreign("monitor", true, true, 0),
            foreign("vdd", true, false, 1920),
        ];
        let now = [foreign("vdd", true, true, 0)];
        let verdict = verify_restore(&pre, &now);
        assert!(verdict.topology_restored(), "{verdict:?}");
    }

    #[test]
    fn lease_holds_through_a_blip_and_recovery_never_runs_under_a_live_session() {
        use DisplayLeaseAction as A;
        use DisplayLeaseEvent as E;
        assert_eq!(
            display_lease_action(E::TransportLostWithinLease, true),
            A::Hold
        );
        for event in [E::LeaseExpired, E::ExplicitDisconnect, E::OwnerEnded] {
            assert_eq!(display_lease_action(event, true), A::RestoreNow);
        }
        for event in [E::PierStarted, E::ConsoleChanged] {
            assert_eq!(display_lease_action(event, true), A::Nothing);
            assert_eq!(display_lease_action(event, false), A::RecoverPending);
        }
    }

    #[test]
    fn windows_gathered_on_a_virtual_display_go_back_to_the_monitor() {
        // A reported case: VDD and monitor, the session isolated the VDD,
        // the restore re-activated the monitor, every window stayed on the VDD.
        let displays = [local("monitor", 0, true), local("vdd", 1920, false)];
        let before = [WindowBefore {
            id: 1,
            display: "monitor".into(),
            normal: DesktopArea::new(100, 100, 800, 600),
        }];
        let now = [
            WindowNow {
                id: 1,
                display: Some("vdd".into()),
                normal: DesktopArea::new(2020, 100, 800, 600),
            },
            WindowNow {
                id: 2,
                display: Some("vdd".into()),
                normal: DesktopArea::new(1920 + 300, 200, 640, 480),
            },
        ];
        let moves = plan_window_repatriation(&RepatriationInput {
            session_displays: &["vdd".into()],
            pre_session_primary: Some("monitor"),
            windows_before: &before,
            displays_now: &displays,
            windows_now: &now,
        });
        assert_eq!(
            moves,
            vec![
                WindowMove {
                    id: 1,
                    from: Some("vdd".into()),
                    to: "monitor".into(),
                    normal: DesktopArea::new(100, 100, 800, 600),
                    reason: MoveReason::ReturnToPreSessionDisplay,
                },
                WindowMove {
                    id: 2,
                    from: Some("vdd".into()),
                    to: "monitor".into(),
                    normal: DesktopArea::new(300, 200, 640, 480),
                    reason: MoveReason::LeaveSessionDisplay,
                },
            ]
        );
    }

    #[test]
    fn a_window_the_user_kept_on_the_virtual_display_stays() {
        let displays = [local("monitor", 0, true), local("vdd", 1920, false)];
        let before = [WindowBefore {
            id: 7,
            display: "vdd".into(),
            normal: DesktopArea::new(2000, 50, 400, 300),
        }];
        let now = [WindowNow {
            id: 7,
            display: Some("vdd".into()),
            normal: DesktopArea::new(2000, 50, 400, 300),
        }];
        let moves = plan_window_repatriation(&RepatriationInput {
            session_displays: &["vdd".into()],
            pre_session_primary: Some("monitor"),
            windows_before: &before,
            displays_now: &displays,
            windows_now: &now,
        });
        assert!(moves.is_empty());
    }

    #[test]
    fn a_primary_virtual_display_still_gives_the_monitor_its_windows_back() {
        // The VDD (or a dongle) is the primary and the session display; the
        // monitor beside it was deactivated and every window gathered on the
        // VDD. Recorded windows go home; one opened in the session stays.
        let displays = [local("vdd", 0, true), local("monitor", 1920, false)];
        let before = [WindowBefore {
            id: 1,
            display: "monitor".into(),
            normal: DesktopArea::new(2000, 100, 800, 600),
        }];
        let now = [
            WindowNow {
                id: 1,
                display: Some("vdd".into()),
                normal: DesktopArea::new(80, 100, 800, 600),
            },
            WindowNow {
                id: 2,
                display: Some("vdd".into()),
                normal: DesktopArea::new(300, 300, 640, 480),
            },
        ];
        let moves = plan_window_repatriation(&RepatriationInput {
            session_displays: &["vdd".into()],
            pre_session_primary: Some("vdd"),
            windows_before: &before,
            displays_now: &displays,
            windows_now: &now,
        });
        assert_eq!(
            moves,
            vec![WindowMove {
                id: 1,
                from: Some("vdd".into()),
                to: "monitor".into(),
                normal: DesktopArea::new(2000, 100, 800, 600),
                reason: MoveReason::ReturnToPreSessionDisplay,
            }]
        );
    }

    #[test]
    fn a_move_between_two_served_monitors_is_kept() {
        let displays = [local("left", 0, true), local("right", 1920, false)];
        let before = [WindowBefore {
            id: 4,
            display: "right".into(),
            normal: DesktopArea::new(2000, 100, 800, 600),
        }];
        let now = [WindowNow {
            id: 4,
            display: Some("left".into()),
            normal: DesktopArea::new(100, 100, 800, 600),
        }];
        let moves = plan_window_repatriation(&RepatriationInput {
            session_displays: &["left".into(), "right".into()],
            pre_session_primary: Some("left"),
            windows_before: &before,
            displays_now: &displays,
            windows_now: &now,
        });
        assert!(moves.is_empty());
    }

    #[test]
    fn a_dongle_or_monitor_alone_moves_nothing() {
        let displays = [local("dongle", 0, true)];
        let now = [WindowNow {
            id: 3,
            display: Some("dongle".into()),
            normal: DesktopArea::new(10, 10, 300, 200),
        }];
        let moves = plan_window_repatriation(&RepatriationInput {
            session_displays: &["dongle".into()],
            pre_session_primary: Some("dongle"),
            windows_before: &[],
            displays_now: &displays,
            windows_now: &now,
        });
        assert!(moves.is_empty());
    }

    #[test]
    fn an_off_screen_window_is_centred_on_the_primary_and_shrunk_to_fit() {
        let displays = [local("monitor", 0, true)];
        let now = [WindowNow {
            id: 9,
            display: None,
            normal: DesktopArea::new(5000, 5000, 4000, 300),
        }];
        let moves = plan_window_repatriation(&RepatriationInput {
            session_displays: &["monitor".into()],
            pre_session_primary: Some("monitor"),
            windows_before: &[],
            displays_now: &displays,
            windows_now: &now,
        });
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].reason, MoveReason::OffScreen);
        assert_eq!(moves[0].normal, DesktopArea::new(0, 370, 1920, 300));
    }

    #[test]
    fn a_window_whose_home_display_is_gone_goes_to_the_primary() {
        // Monitor unplugged during a session on the VDD: the VDD is all that
        // is left, so it is the primary and nothing moves.
        let displays = [local("vdd", 0, true)];
        let before = [WindowBefore {
            id: 1,
            display: "monitor".into(),
            normal: DesktopArea::new(100, 100, 800, 600),
        }];
        let now = [WindowNow {
            id: 1,
            display: Some("vdd".into()),
            normal: DesktopArea::new(100, 100, 800, 600),
        }];
        let moves = plan_window_repatriation(&RepatriationInput {
            session_displays: &["vdd".into()],
            pre_session_primary: Some("monitor"),
            windows_before: &before,
            displays_now: &displays,
            windows_now: &now,
        });
        assert!(moves.is_empty());
    }

    #[test]
    fn display_of_picks_the_largest_overlap() {
        let displays = [local("a", 0, true), local("b", 1920, false)];
        assert_eq!(
            display_of(DesktopArea::new(1800, 0, 400, 300), &displays).as_deref(),
            Some("b")
        );
        assert_eq!(
            display_of(DesktopArea::new(-900, 0, 400, 300), &displays),
            None
        );
    }

    fn placed(id: u64, x: i32, y: i32, maximized: bool) -> WindowPlacement {
        WindowPlacement {
            id,
            area: DesktopArea::new(x, y, 800, 600),
            maximized,
            fullscreen: false,
        }
    }

    /// Two 1920x1080 monitors side by side: a layout without the right one
    /// gathered its windows onto the left one.
    const MONITORS: [DesktopArea; 2] = [
        DesktopArea::new(0, 0, 1920, 1080),
        DesktopArea::new(1920, 0, 1920, 1080),
    ];

    #[test]
    fn windows_a_layout_moved_go_back_where_they_were() {
        let before = [placed(1, 2100, 100, false), placed(2, 100, 100, false)];
        let moved = [placed(1, 180, 100, false), placed(2, 100, 100, false)];
        let input = WindowRestoreInput {
            before: &before,
            after_layout: Some(&moved),
            departing: &moved,
            now: &moved,
            displays_now: &MONITORS,
        };
        assert_eq!(plan_window_restore(&input), vec![before[0]]);
        // Unreadable after the layout: the window still goes back.
        let input = WindowRestoreInput {
            after_layout: None,
            ..input
        };
        assert_eq!(plan_window_restore(&input), vec![before[0]]);
    }

    #[test]
    fn a_maximised_window_goes_back_maximised_on_its_own_monitor() {
        let before = [WindowPlacement {
            id: 7,
            area: DesktopArea::new(1920, 27, 1920, 1053),
            maximized: true,
            fullscreen: false,
        }];
        let now = [WindowPlacement {
            id: 7,
            area: DesktopArea::new(0, 27, 1920, 1053),
            maximized: true,
            fullscreen: false,
        }];
        let input = WindowRestoreInput {
            before: &before,
            after_layout: Some(&now),
            departing: &now,
            now: &now,
            displays_now: &MONITORS,
        };
        assert_eq!(plan_window_restore(&input), vec![before[0]]);
    }

    #[test]
    fn what_the_decks_user_did_stays() {
        let before = [placed(1, 2100, 100, false)];
        let settled = [placed(1, 180, 100, false)];
        // Moved by the Deck's user while attached.
        let departing = [placed(1, 600, 300, false)];
        let input = WindowRestoreInput {
            before: &before,
            after_layout: Some(&settled),
            departing: &departing,
            now: &departing,
            displays_now: &MONITORS,
        };
        assert!(plan_window_restore(&input).is_empty());
        // Opened while attached: no recorded place.
        let opened = [placed(9, 180, 100, false)];
        let input = WindowRestoreInput {
            before: &before,
            after_layout: Some(&settled),
            departing: &opened,
            now: &opened,
            displays_now: &MONITORS,
        };
        assert!(plan_window_restore(&input).is_empty());
    }

    #[test]
    fn a_window_already_in_place_or_recorded_off_every_display_is_left() {
        let before = [placed(1, 100, 100, false), placed(2, 5000, 100, false)];
        let now = [placed(1, 100, 100, false), placed(2, 100, 400, false)];
        let input = WindowRestoreInput {
            before: &before,
            after_layout: Some(&now),
            departing: &now,
            now: &now,
            displays_now: &MONITORS,
        };
        assert!(plan_window_restore(&input).is_empty());
    }
}
