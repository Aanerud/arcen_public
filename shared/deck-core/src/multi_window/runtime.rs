// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The multi-window plan and its transactional enter engine (moved from the
//! macOS Deck's `ui::multi_window_runtime`, generic over the Deck's window
//! key: an `egui::ViewportId` on the Mac, a window slot on Windows).
//!
//! A validated 1-4 monitor plan names one additional fullscreen window per
//! negotiated monitor beyond the primary, which the Deck's root window keeps
//! presenting. Windows are opened by the platform; this engine only judges
//! what the platform observes of each one ([`ViewportBindObservation`]) and
//! decides, frame by frame, whether the whole layout is confirmed on its
//! exact displays, still pending, timed out, or must be aborted
//! ([`MultiWindowEnterAttempt`]). Display ids are the Deck's own local
//! display numbers (`CGDirectDisplayID` on the Mac, the `\\.\DISPLAYn`
//! number on Windows), carried as the decimal client display id.

#![allow(
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    reason = "moved verbatim from the macOS Deck (AppKit/egui names in prose); \
              tightened in follow-ups, never by changing behaviour in the move"
)]

use std::collections::BTreeSet;
use std::time::Duration;

use arcen_media::{MAX_MULTI_MONITOR_COUNT, MediaContractError, SessionMonitorId};

/// One additional native fullscreen window assigned to one negotiated,
/// non-primary monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorWindowAssignment<K> {
    pub session_monitor_id: SessionMonitorId,
    /// The Deck's key for the window that presents this monitor.
    pub window: K,
    /// This monitor's local `CGDirectDisplayID`, used to resolve which
    /// winit monitor index to target (see [`monitor_index_in`]).
    pub display_id: u32,
}

/// A validated plan for the *additional* native fullscreen windows a
/// multi-monitor session needs: one entry per negotiated monitor beyond the
/// primary (which continues to use the existing root viewport).
///
/// Always 0..=3 entries: a 1-monitor session plans zero additional windows
/// (root-only, today's behavior); a 4-monitor session plans 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiWindowPlan<K> {
    assignments: Vec<MonitorWindowAssignment<K>>,
    /// `display_ids[0]`: the negotiated primary monitor's local
    /// `CGDirectDisplayID`, i.e. the exact display the *root* viewport must
    /// be validated against. Unlike every other monitor in the roster, the
    /// primary has no [`MonitorWindowAssignment`] (the root viewport
    /// presents it, not an additional window) -- this field is the one place
    /// the plan still records the primary's own expected identity, so a
    /// caller (`MultiWindowEnterAttempt`) can hold root to the exact same
    /// "confirm on its precise expected display" contract every secondary
    /// window already has, closing the gap where root previously had no
    /// tracked expectation at all.
    primary_display_id: u32,
    /// The key of the Deck's root (primary) window.
    root: K,
}

/// Failure validating a [`MultiWindowPlan`]'s inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiWindowPlanError {
    /// `monitor_ids` and `display_ids` had different lengths.
    MismatchedLengths {
        monitor_ids: usize,
        display_ids: usize,
    },
    /// The full negotiated roster (primary included) was empty or exceeded
    /// [`MAX_MULTI_MONITOR_COUNT`].
    InvalidMonitorCount(usize),
    /// The same session monitor id appeared more than once.
    DuplicateSessionMonitorId(SessionMonitorId),
    /// The same local display was assigned to more than one monitor id.
    DuplicateDisplayId(u32),
}

impl std::fmt::Display for MultiWindowPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MismatchedLengths {
                monitor_ids,
                display_ids,
            } => write!(
                formatter,
                "{monitor_ids} monitor ids but {display_ids} display ids: plan inputs must be \
                 parallel arrays over the same negotiated roster"
            ),
            Self::InvalidMonitorCount(count) => write!(
                formatter,
                "{count} monitors is not a valid 1..={MAX_MULTI_MONITOR_COUNT} multi-monitor-v1 roster"
            ),
            Self::DuplicateSessionMonitorId(id) => {
                write!(
                    formatter,
                    "session monitor id {} appears more than once in the roster",
                    id.get()
                )
            }
            Self::DuplicateDisplayId(id) => {
                write!(
                    formatter,
                    "display {id} is assigned to more than one monitor id"
                )
            }
        }
    }
}

impl std::error::Error for MultiWindowPlanError {}

impl<K: Copy + Ord> MultiWindowPlan<K> {
    /// Builds the additional-window plan for a full negotiated roster.
    ///
    /// `monitor_ids[0]`/`display_ids[0]` is the primary monitor and is
    /// deliberately *not* included in the returned plan's assignments — the
    /// existing root viewport keeps presenting it unchanged. Every other
    /// entry becomes one [`MonitorWindowAssignment`].
    ///
    /// # Errors
    ///
    /// See [`MultiWindowPlanError`].
    pub fn build(
        monitor_ids: &[SessionMonitorId],
        display_ids: &[u32],
        root: K,
        window_for: impl Fn(SessionMonitorId) -> K,
    ) -> Result<Self, MultiWindowPlanError> {
        if monitor_ids.len() != display_ids.len() {
            return Err(MultiWindowPlanError::MismatchedLengths {
                monitor_ids: monitor_ids.len(),
                display_ids: display_ids.len(),
            });
        }
        let total = monitor_ids.len();
        if total == 0 || total > MAX_MULTI_MONITOR_COUNT {
            return Err(MultiWindowPlanError::InvalidMonitorCount(total));
        }

        let mut seen_ids = BTreeSet::new();
        let mut seen_displays = BTreeSet::new();
        for (&id, &display_id) in monitor_ids.iter().zip(display_ids.iter()) {
            if !seen_ids.insert(id) {
                return Err(MultiWindowPlanError::DuplicateSessionMonitorId(id));
            }
            if !seen_displays.insert(display_id) {
                return Err(MultiWindowPlanError::DuplicateDisplayId(display_id));
            }
        }

        let assignments = monitor_ids
            .iter()
            .zip(display_ids.iter())
            .skip(1) // index 0 (primary) stays on the root viewport.
            .map(
                |(&session_monitor_id, &display_id)| MonitorWindowAssignment {
                    session_monitor_id,
                    window: window_for(session_monitor_id),
                    display_id,
                },
            )
            .collect();

        Ok(Self {
            assignments,
            primary_display_id: display_ids[0],
            root,
        })
    }

    #[must_use]
    pub fn assignments(&self) -> &[MonitorWindowAssignment<K>] {
        &self.assignments
    }

    /// The negotiated primary monitor's expected local `CGDirectDisplayID`
    /// -- the root viewport's own expected identity, exactly parallel to
    /// each [`MonitorWindowAssignment::display_id`].
    #[must_use]
    pub const fn primary_display_id(&self) -> u32 {
        self.primary_display_id
    }

    /// The keys of every additional window this plan opens (not
    /// including the root viewport), used to close everything on rollback.
    #[must_use]
    pub fn windows(&self) -> Vec<K> {
        self.assignments
            .iter()
            .map(|assignment| assignment.window)
            .collect()
    }

    /// The key of the root window, which presents the primary monitor.
    #[must_use]
    pub const fn root(&self) -> K {
        self.root
    }
}

/// Recovers the local `CGDirectDisplayID` encoded in an
/// `arcen_protocol::messages::ClientDisplayId`, which
/// `crate::display::topology::build_requested_topology` always sets to the
/// display's decimal `CGDirectDisplayID` (see `MonitorIdentity.id` there).
///
/// Returns `None` for any identifier that is not a plain `u32` (never
/// expected for a Deck-originated request, but a host echoing something else
/// back must not panic).
#[must_use]
pub fn display_id_from_client_display_id(id: &str) -> Option<u32> {
    id.parse::<u32>().ok()
}

/// Safe wrapper resolving which winit `available_monitors()` index
/// corresponds to a local `CGDirectDisplayID`.
///
/// Pure and independent of any live AppKit/CoreGraphics call so it is
/// directly unit-testable; the live call site is [`resolve_monitor_index_live`].
#[must_use]
pub fn monitor_index_in(active_display_ids: &[u32], target: u32) -> Option<usize> {
    active_display_ids.iter().position(|&id| id == target)
}

/// One live display's identity and logical (point-space) size, as reported
/// by `CGDisplayBounds`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveDisplayInfo {
    pub display_id: u32,
    pub width_pts: f32,
    pub height_pts: f32,
}

/// Failure resolving the winit monitor index for a planned window's target
/// display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorResolutionError {
    /// The assignment's `display_id` is not in the live active display
    /// list (disconnected between negotiation and window creation, or
    /// between frames of an already-open transactional attempt).
    DisplayNotActive(u32),
}

impl std::fmt::Display for MonitorResolutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DisplayNotActive(id) => write!(
                formatter,
                "display {id} is not in the live active display list"
            ),
        }
    }
}

impl std::error::Error for MonitorResolutionError {}

/// The deterministic, per-session-monitor-id window title used both when
/// opening a window (`viewport_builder_for`, and the root viewport's own
/// `NativeOptions`) and when later resolving that exact window's current
/// `CGDirectDisplayID` back via [`window_display_id`] -- a single shared
/// source of the title format so the two can never drift apart.
#[must_use]
pub fn window_title_for(session_monitor_id: SessionMonitorId) -> String {
    format!("Arcen Deck — Display {}", session_monitor_id.get())
}

/// One frame's worth of observed viewport state, mirroring exactly the
/// fields of `egui::ViewportInfo` that the transactional enter engine and
/// focus tracking need. Kept as a small local copy (rather than depending on
/// `egui::ViewportInfo` directly in the pure engine below) so the engine's
/// tests do not need to construct a full egui `InputState`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ViewportBindObservation {
    pub inner_rect_known: bool,
    pub fullscreen: Option<bool>,
    pub close_requested: bool,
    /// `egui::ViewportInfo::monitor_size`: the reported current monitor's
    /// logical (point-space) size, when known. Informational only -- kept
    /// for logging/telemetry context, never used to confirm a bind (two
    /// distinct physical displays can share an identical logical size, so a
    /// size match alone can never distinguish them; see
    /// [`observed_display_id`] and [`viewport_confirmed_on_exact_display`]).
    pub monitor_size_pts: Option<(f32, f32)>,
    /// `egui::ViewportInfo::inner_rect`'s size, when known. Same
    /// informational-only status as `monitor_size_pts`.
    pub inner_rect_size_pts: Option<(f32, f32)>,
    /// The exact `CGDirectDisplayID` this viewport's `NSWindow` is genuinely
    /// on right now, resolved via the authoritative
    /// `NSWindow.screen -> NSScreen.deviceDescription[NSScreenNumber]`
    /// mapping (see [`window_display_id`]) -- never inferred from
    /// coordinates or size. `None` means the identity could not be observed
    /// at all (off the main thread, the window is not open yet, it has no
    /// screen, or the device description is missing/mistyped); confirmation
    /// must fail closed in that case, exactly like any other unconfirmed
    /// observation.
    pub observed_display_id: Option<u32>,
}

/// True once a window has genuinely bound to native fullscreen: it has
/// reported a real inner rect (the OS window exists and is sized) and
/// `fullscreen == Some(true)` (egui-winit only reports this once
/// `winit::window::Window::fullscreen()` is actually `Some`, i.e. macOS has
/// completed the `toggleFullScreen:` transition), and the user/OS has not
/// asked to close it mid-transition.
///
/// This alone does *not* prove the window bound to the *expected* display --
/// see [`viewport_confirmed_on_exact_display`] for the check that also
/// requires the exact observed `CGDirectDisplayID` to match.
#[must_use]
pub const fn viewport_bind_confirmed(observation: ViewportBindObservation) -> bool {
    observation.inner_rect_known
        && matches!(observation.fullscreen, Some(true))
        && !observation.close_requested
}

/// True when `observation` is a genuine native fullscreen bind (see
/// [`viewport_bind_confirmed`]) *and* its exact observed
/// `CGDirectDisplayID` (`observation.observed_display_id`) is exactly
/// `expected_display_id`.
///
/// This is the one confirmation rule this module uses for every viewport it
/// opens -- the reused root/primary viewport and every additional
/// per-monitor window alike -- so there is exactly one place a
/// same-sized-but-different-display mismatch (e.g. two identical external
/// monitors swapped) could be missed. It is deliberately an exact identity
/// comparison, never a coordinate/size heuristic: a `None` observed id
/// (identity could not be read at all) never confirms.
#[must_use]
pub fn viewport_confirmed_on_exact_display(
    observation: ViewportBindObservation,
    expected_display_id: u32,
) -> bool {
    viewport_bind_confirmed(observation)
        && observation.observed_display_id == Some(expected_display_id)
}

/// A single viewport's hard-failure reason (no `viewport_id`; the caller --
/// [`MultiWindowEnterAttempt::record_observation`] or a diagnostic/runtime's
/// own root check -- attaches which viewport this happened to, since root is
/// not itself a [`MultiWindowPlan`] assignment).
///
/// Every reason here is a *definite*, immediately-actionable failure, never
/// a "not observable/ready yet" state -- see [`evaluate_viewport_bind`] for
/// the pending/confirmed/hard-failure classification that produces these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardFailureKind {
    /// The OS/user asked to close this viewport before the whole
    /// transactional enter committed.
    CloseRequested,
    /// This viewport's genuine, bind-ready native fullscreen window resolved
    /// to a *different*, definite `CGDirectDisplayID` than the one it was
    /// assigned -- e.g. two identically-sized displays landed swapped.
    WrongDisplay {
        expected_display_id: u32,
        observed_display_id: u32,
    },
    /// The display this viewport was assigned to is no longer in the live
    /// active display list (disconnected mid-attempt).
    DisplayDisappeared { display_id: u32 },
}

impl std::fmt::Display for HardFailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CloseRequested => formatter.write_str("close was requested before commit"),
            Self::WrongDisplay {
                expected_display_id,
                observed_display_id,
            } => write!(
                formatter,
                "expected display {expected_display_id} but bound to display {observed_display_id}"
            ),
            Self::DisplayDisappeared { display_id } => {
                write!(formatter, "display {display_id} is no longer active")
            }
        }
    }
}

/// A hard failure attached to the specific viewport it happened to -- the
/// permanent, latched payload [`MultiWindowEnterAttempt`] stores once
/// tripped and reports via [`MultiWindowEnterPoll::Aborted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiWindowAbortReason<K> {
    pub window: K,
    pub kind: HardFailureKind,
}

impl<K: std::fmt::Debug> std::fmt::Display for MultiWindowAbortReason<K> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "window {:?} aborted: {}", self.window, self.kind)
    }
}

impl<K: std::fmt::Debug> std::error::Error for MultiWindowAbortReason<K> {}

/// The three-way outcome of evaluating one viewport's latest observation
/// against its expected display and current display-list membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportBindEvaluation {
    /// Not confirmed yet, but not a definite failure either -- ordinary
    /// still-waiting state: fullscreen not yet reported, inner rect not yet
    /// known, or (notably) the exact identity simply not observable *yet*
    /// (`observed_display_id: None`) even though the window otherwise
    /// claims to be bind-ready. A transient/benign lookup miss must not be
    /// treated the same as a *definite* wrong identity.
    Pending,
    /// Genuinely bind-ready on the exact expected display.
    Confirmed,
    /// A definite, immediate hard failure -- never wait for a timeout to
    /// react to this; see [`HardFailureKind`].
    HardFailure(HardFailureKind),
}

/// Classifies one viewport's latest [`ViewportBindObservation`] into
/// [`ViewportBindEvaluation::Pending`], [`ViewportBindEvaluation::Confirmed`]
/// (delegating to [`viewport_confirmed_on_exact_display`], the single shared
/// confirmation rule), or a definite [`ViewportBindEvaluation::HardFailure`]:
/// `close_requested`, the assigned display disappearing from
/// `target_still_active`, or a bind-ready window resolving to a *different*,
/// definite (`Some`, not `None`) observed display id. Checked in that exact
/// priority order, since any one of them is independently sufficient to
/// abort.
#[must_use]
pub fn evaluate_viewport_bind(
    observation: ViewportBindObservation,
    expected_display_id: u32,
    target_still_active: bool,
) -> ViewportBindEvaluation {
    if observation.close_requested {
        return ViewportBindEvaluation::HardFailure(HardFailureKind::CloseRequested);
    }
    if !target_still_active {
        return ViewportBindEvaluation::HardFailure(HardFailureKind::DisplayDisappeared {
            display_id: expected_display_id,
        });
    }
    if viewport_confirmed_on_exact_display(observation, expected_display_id) {
        return ViewportBindEvaluation::Confirmed;
    }
    // Bind-ready (genuine native fullscreen, real inner rect) but a
    // *different*, definite observed identity is a hard failure. Identity
    // simply not being observable yet (`None`) stays ordinary pending state
    // -- see this function's own doc and `ViewportBindEvaluation::Pending`.
    let bind_ready = observation.inner_rect_known && matches!(observation.fullscreen, Some(true));
    if bind_ready
        && let Some(observed) = observation.observed_display_id
        && observed != expected_display_id
    {
        return ViewportBindEvaluation::HardFailure(HardFailureKind::WrongDisplay {
            expected_display_id,
            observed_display_id: observed,
        });
    }
    ViewportBindEvaluation::Pending
}

/// Root-only refinement of [`evaluate_viewport_bind`] for the
/// `egui::ViewportCommand::SetMonitor`-driven move that puts root's
/// pre-existing app window onto the negotiated primary display
/// (`drive_root_multi_window_target`).
///
/// Unlike every other planned viewport (opened already targeting its final
/// display via [`viewport_builder_for`], so a wrong-display bind-ready
/// observation is always an immediate, genuine hard failure for it), root
/// is the app's pre-existing window: at the moment a multi-monitor session
/// is first negotiated it may already be genuinely bind-ready -- fullscreen
/// or not -- on whatever display it happened to occupy before the session
/// started, and moving it is not instantaneous. Several polls typically
/// pass while AppKit tears the window out of its old fullscreen Space and
/// re-enters a new one on the target display, during which root's own
/// latest observation may still legitimately report bind-ready on the old
/// display, or report not bind-ready at all -- neither of which
/// [`evaluate_viewport_bind`] alone can tell apart from a genuine, settled
/// wrong-display bind (the same-sized-swap failure mode every other
/// viewport's hard failure legitimately guards against).
///
/// - `move_in_flight` is `false` (root is not mid-move, e.g. it already
///   started on the correct display, or the move already settled and this
///   attempt confirmed it once): identical to [`evaluate_viewport_bind`] --
///   a wrong-display bind-ready observation is exactly as real a hard
///   failure as any other viewport's.
/// - `move_in_flight` and `!has_seen_unbound_since_move_started`: root has
///   not yet been observed leaving its pre-move state at all. A would-be
///   [`HardFailureKind::WrongDisplay`] downgrades to
///   [`ViewportBindEvaluation::Pending`] -- root simply has not started
///   transitioning yet, and the caller's own bounded entry deadline (never
///   this function) is what ends a move that never converges
///   ([`MultiWindowEnterPoll::TimedOut`], never `Aborted`, in that case).
/// - `move_in_flight` and `has_seen_unbound_since_move_started` and this
///   observation is bind-ready again ("settled"): identical to
///   [`evaluate_viewport_bind`] again -- the move has visibly completed
///   into some final state, so a persisting wrong display is now a
///   genuine, immediate hard failure, exactly like any other viewport's.
/// - `move_in_flight` and `has_seen_unbound_since_move_started` but this
///   observation is *not* bind-ready (still mid-transition): still
///   downgrades a would-be `WrongDisplay` to `Pending`, same as the
///   not-yet-started case -- an in-between frame's stale identity read must
///   not itself count as "settled".
///
/// `HardFailureKind::CloseRequested` and `HardFailureKind::DisplayDisappeared`
/// are never downgraded by any of this, regardless of `move_in_flight` or
/// settlement -- both are unconditional hard failures
/// ([`evaluate_viewport_bind`] checks them before ever comparing display
/// identity), exactly matching the task's "hard fail only if ... the
/// assigned display disappears/close" requirement.
#[must_use]
pub fn evaluate_root_viewport_bind_during_move(
    observation: ViewportBindObservation,
    expected_display_id: u32,
    target_still_active: bool,
    move_in_flight: bool,
    has_seen_unbound_since_move_started: bool,
) -> ViewportBindEvaluation {
    let evaluation = evaluate_viewport_bind(observation, expected_display_id, target_still_active);
    if !move_in_flight {
        return evaluation;
    }
    let settled = has_seen_unbound_since_move_started && viewport_bind_confirmed(observation);
    if settled {
        return evaluation;
    }
    match evaluation {
        ViewportBindEvaluation::HardFailure(HardFailureKind::WrongDisplay { .. }) => {
            ViewportBindEvaluation::Pending
        }
        other => other,
    }
}

/// Outcome of polling an in-progress [`MultiWindowEnterAttempt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiWindowEnterPoll<K> {
    /// Not every window has confirmed yet, and the deadline has not passed.
    StillWaiting,
    /// Every planned window confirmed native fullscreen bind.
    Confirmed,
    /// The deadline passed before every window confirmed.
    TimedOut { unconfirmed: Vec<K> },
    /// A definite hard failure was recorded on some earlier
    /// [`MultiWindowEnterAttempt::record_observation`] call -- never merely
    /// a timeout. This is latched: once tripped, this same attempt reports
    /// [`Self::Aborted`] forever afterward (see
    /// [`MultiWindowEnterAttempt::record_observation`]'s doc), so a caller
    /// must close every window immediately and construct a brand new
    /// attempt (and therefore a brand new session) rather than ever reopen
    /// a window on this one.
    Aborted(MultiWindowAbortReason<K>),
}

/// Transactional "enter multi-window presentation" state.
///
/// Per the task's transactional-enter requirement: every planned window is
/// opened at once, but input/media activation must wait until *all* of them
/// confirm a genuine native fullscreen bind; if any fails to confirm before
/// the deadline, every window (including ones that did confirm) must be
/// closed and the session must fail before activation, never partially
/// present a subset of monitors.
///
/// egui's viewport API has no synchronous "window creation failed" callback
/// back to app code (window creation happens inside the platform event
/// loop), so this is deliberately poll-based: the caller opens every window
/// via `show_viewport_immediate` each frame regardless of confirmation
/// state, records each viewport's latest [`ViewportBindObservation`] via
/// [`Self::record_observation`], and calls [`Self::poll`] once per frame
/// against a monotonic clock until it returns [`MultiWindowEnterPoll::Confirmed`]
/// or [`MultiWindowEnterPoll::TimedOut`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiWindowEnterAttempt<K> {
    plan: MultiWindowPlan<K>,
    confirmed: BTreeSet<K>,
    started_at: Duration,
    /// Once `Some`, latched forever: [`Self::record_observation`] becomes a
    /// no-op and [`Self::poll`] reports [`MultiWindowEnterPoll::Aborted`]
    /// unconditionally, regardless of `now`/`timeout` or any later
    /// observation. There is deliberately no API to clear this -- per the
    /// task's "never recreate them until app exit" requirement, a caller
    /// must construct a brand new [`MultiWindowEnterAttempt`] (and
    /// therefore a brand new session) rather than ever resume this one.
    aborted: Option<MultiWindowAbortReason<K>>,
    /// The exact `CGDirectDisplayID` a caller (`drive_root_multi_window_target`)
    /// has most recently sent root's `egui::ViewportCommand::SetMonitor` move
    /// command to target, or `None` before the first command this attempt
    /// has ever needed. [`Self::root_move_command_needed`]/
    /// [`Self::record_root_move_command_sent`] use this to send that command
    /// exactly once per distinct target rather than resending it every
    /// frame while the move is still in flight, and
    /// [`Self::record_observation`]'s root branch uses it to know root is
    /// currently mid-move (see [`evaluate_root_viewport_bind_during_move`]).
    root_move_requested_target: Option<u32>,
    /// `true` once, since `root_move_requested_target` was last set (or
    /// changed to a new target), some observation reported root genuinely
    /// *not* bind-ready -- proving the `SetMonitor` move visibly began
    /// tearing down root's previous native fullscreen bind. Until this
    /// happens, a bind-ready-but-wrong-display observation is still-in-
    /// flight/expected, ordinary pending state (root simply has not started
    /// transitioning away from wherever it started yet); once this flips,
    /// the *next* bind-ready observation -- right or wrong display -- is
    /// the move's settled outcome, not a still-transitioning one, so a
    /// wrong-display observation at that point is a genuine, immediate hard
    /// failure exactly like any other viewport's.
    root_move_saw_unbound_since_request: bool,
}

impl<K: Copy + Ord> MultiWindowEnterAttempt<K> {
    #[must_use]
    pub fn new(plan: MultiWindowPlan<K>, started_at: Duration) -> Self {
        Self {
            plan,
            confirmed: BTreeSet::new(),
            started_at,
            aborted: None,
            root_move_requested_target: None,
            root_move_saw_unbound_since_request: false,
        }
    }

    #[must_use]
    pub fn plan(&self) -> &MultiWindowPlan<K> {
        &self.plan
    }

    /// The latched hard-failure reason, if [`Self::record_observation`] has
    /// ever recorded one. Equivalent to (but does not require a clock/
    /// timeout to inspect, unlike) matching [`MultiWindowEnterPoll::Aborted`]
    /// out of [`Self::poll`] -- useful for a caller that wants to react to
    /// an abort the instant it is recorded, in the same frame, without
    /// waiting for its own next scheduled poll.
    #[must_use]
    pub const fn abort_reason(&self) -> Option<MultiWindowAbortReason<K>> {
        self.aborted
    }

    /// Whether a caller (`drive_root_multi_window_target`) still needs to
    /// send `egui::ViewportCommand::SetMonitor` to move root onto
    /// `expected_display_id` -- `true` only the first time this exact
    /// target is asked about (or again if `expected_display_id` differs
    /// from whatever target this attempt last recorded, e.g. after a
    /// renegotiated primary), never every frame while the same move is
    /// still settling. The caller must call
    /// [`Self::record_root_move_command_sent`] immediately after actually
    /// sending the command so this flips back to `false` for the same
    /// target.
    #[must_use]
    pub fn root_move_command_needed(&self, expected_display_id: u32) -> bool {
        self.root_move_requested_target != Some(expected_display_id)
    }

    /// Records that a caller has just sent
    /// `egui::ViewportCommand::SetMonitor` targeting
    /// `expected_display_id` for root -- starts (or restarts, if
    /// `expected_display_id` differs from the last request) this
    /// attempt's root move-in-flight tracking that
    /// [`evaluate_root_viewport_bind_during_move`] uses via
    /// [`Self::record_observation`]'s root branch.
    pub fn record_root_move_command_sent(&mut self, expected_display_id: u32) {
        if self.root_move_requested_target != Some(expected_display_id) {
            self.root_move_requested_target = Some(expected_display_id);
            self.root_move_saw_unbound_since_request = false;
        }
    }

    /// Records this frame's observation for one planned viewport, *or* the
    /// root viewport (`egui::ViewportId::ROOT`) against the plan's own
    /// [`MultiWindowPlan::primary_display_id`] -- root is tracked by
    /// exactly this same attempt using exactly the same confirmation rule as
    /// every additional window, closing the gap where root previously had no
    /// tracked expected display at all. Any other id outside the plan is
    /// ignored (defensive: a stray/late observation for a viewport from a
    /// previous attempt must never confirm this one).
    ///
    /// Once this attempt has latched a hard failure (see
    /// [`Self::abort_reason`]), every further call is a no-op: the attempt
    /// must never un-abort or accept further mutation, even if a later
    /// observation would look fine in isolation.
    ///
    /// Every non-root viewport delegates to [`evaluate_viewport_bind`]
    /// against its own assigned `CGDirectDisplayID`: it is opened already
    /// targeting its final display via
    /// the Deck's window builder, so
    /// a wrong-display bind-ready observation is always an immediate,
    /// genuine hard failure for it, exactly as before.
    ///
    /// The root viewport is different: it is the pre-existing app window,
    /// which a caller must actively move onto the negotiated primary
    /// display via `egui::ViewportCommand::SetMonitor`
    /// (`drive_root_multi_window_target`), a transition that is not
    /// instantaneous. This delegates root's evaluation to
    /// [`evaluate_root_viewport_bind_during_move`] instead, passing whether
    /// root's move is currently in flight
    /// ([`Self::root_move_command_needed`] being `false`, i.e. a command
    /// was sent for exactly this expected display and root has not yet
    /// confirmed) and whether it has already been seen tearing down its
    /// previous bind ([`Self::root_move_saw_unbound_since_request`]) -- so a
    /// wrong/old observed display only ever becomes a hard failure once the
    /// move has visibly settled, not while it is still merely in progress
    /// (see that function's own doc for the exact rule). This flag is
    /// updated immediately after evaluating each observation, from that same
    /// observation, so the *next* call sees whether root dipped to
    /// not-bind-ready this frame.
    ///
    /// [`ViewportBindEvaluation::Confirmed`] marks this viewport confirmed;
    /// [`ViewportBindEvaluation::Pending`] un-confirms it (a previously
    /// confirmed viewport that merely regresses to not-yet-ready, e.g. the
    /// user briefly exits fullscreen, must un-confirm rather than stick, but
    /// is not itself a hard failure); and
    /// [`ViewportBindEvaluation::HardFailure`] immediately latches
    /// [`Self::abort_reason`] and clears every confirmation -- a
    /// `close_requested` observation, an exact observed-display-id mismatch
    /// while the window is genuinely bind-ready (once settled, for root), or
    /// the assigned display no longer being active are never merely waited
    /// out to the timeout.
    pub fn record_observation(
        &mut self,
        window: K,
        observation: ViewportBindObservation,
        active_displays: &[ActiveDisplayInfo],
    ) {
        if self.aborted.is_some() {
            return;
        }
        let is_root = window == self.plan.root;
        let expected_display_id = if is_root {
            self.plan.primary_display_id
        } else {
            let Some(assignment) = self
                .plan
                .assignments
                .iter()
                .find(|assignment| assignment.window == window)
            else {
                return;
            };
            assignment.display_id
        };
        let target_still_active = active_displays
            .iter()
            .any(|display| display.display_id == expected_display_id);
        let evaluation = if is_root {
            let move_in_flight = self.root_move_requested_target == Some(expected_display_id)
                && !self.confirmed.contains(&self.plan.root);
            let evaluation = evaluate_root_viewport_bind_during_move(
                observation,
                expected_display_id,
                target_still_active,
                move_in_flight,
                self.root_move_saw_unbound_since_request,
            );
            if move_in_flight && !viewport_bind_confirmed(observation) {
                self.root_move_saw_unbound_since_request = true;
            }
            evaluation
        } else {
            evaluate_viewport_bind(observation, expected_display_id, target_still_active)
        };
        match evaluation {
            ViewportBindEvaluation::Confirmed => {
                self.confirmed.insert(window);
            }
            ViewportBindEvaluation::Pending => {
                self.confirmed.remove(&window);
            }
            ViewportBindEvaluation::HardFailure(kind) => {
                self.confirmed.clear();
                self.aborted = Some(MultiWindowAbortReason { window, kind });
            }
        }
    }

    /// Polls this attempt's outcome. Pure given `now`, so unit tests can
    /// drive it deterministically without a real clock/event loop.
    ///
    /// Checks [`Self::abort_reason`] first and unconditionally: a latched
    /// hard failure is reported as [`MultiWindowEnterPoll::Aborted`]
    /// regardless of `now`/`timeout` or confirmation state, since it must
    /// never be waited out.
    ///
    /// `Confirmed` requires *both* `egui::ViewportId::ROOT` and every
    /// planned secondary assignment to have confirmed -- root is never
    /// vacuously assumed bound just because it needed no additional window;
    /// even a single-monitor (zero-assignment) plan must still see root
    /// itself confirm on the negotiated primary display before this attempt
    /// commits.
    #[must_use]
    pub fn poll(&self, now: Duration, timeout: Duration) -> MultiWindowEnterPoll<K> {
        if let Some(reason) = self.aborted {
            return MultiWindowEnterPoll::Aborted(reason);
        }
        let all_confirmed = self.confirmed.contains(&self.plan.root)
            && self
                .plan
                .assignments
                .iter()
                .all(|assignment| self.confirmed.contains(&assignment.window));
        if all_confirmed {
            return MultiWindowEnterPoll::Confirmed;
        }
        if now.saturating_sub(self.started_at) >= timeout {
            let mut unconfirmed = Vec::with_capacity(self.plan.assignments.len() + 1);
            if !self.confirmed.contains(&self.plan.root) {
                unconfirmed.push(self.plan.root);
            }
            unconfirmed.extend(
                self.plan
                    .assignments
                    .iter()
                    .map(|assignment| assignment.window)
                    .filter(|id| !self.confirmed.contains(id)),
            );
            return MultiWindowEnterPoll::TimedOut { unconfirmed };
        }
        MultiWindowEnterPoll::StillWaiting
    }
}

/// Which monitor currently has native input focus, for the
/// "focus/relative-pointer-lock follows the active monitor" requirement:
/// relative (mouse-look style) pointer capture and keyboard focus should
/// track whichever window the OS says is focused, while absolute/pen input
/// stays monitor-scoped to whichever window it physically arrived on
/// (unaffected by this decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveMonitor {
    /// The root viewport (always the primary monitor) is focused, or no
    /// window reports focus this frame (fail-safe default).
    Primary,
    /// One of the additional per-monitor windows is focused.
    Secondary(SessionMonitorId),
}

/// Pure decision for which monitor is "active" this frame, given the root
/// viewport's own focus state and each additional window's focus state.
///
/// If more than one window simultaneously reports focus (should not happen,
/// but the OS is not required to guarantee it), the root wins first, then
/// the lowest session monitor id -- a deterministic, arbitrary-but-stable
/// tiebreak rather than an unspecified iteration order.
#[must_use]
pub fn active_monitor(
    root_focused: bool,
    secondary_focus: &[(SessionMonitorId, bool)],
) -> ActiveMonitor {
    if root_focused {
        return ActiveMonitor::Primary;
    }
    secondary_focus
        .iter()
        .filter(|(_, focused)| *focused)
        .map(|(id, _)| *id)
        .min()
        .map_or(ActiveMonitor::Primary, ActiveMonitor::Secondary)
}

/// Reduces a `MediaContractError` from building a
/// [`arcen_media::RequestedMonitorTopology`]-adjacent per-window plan into a
/// single display's worth of context, mirroring
/// `crate::display::topology::TopologyPreflightError`'s style. Currently
/// unused outside tests; kept as the typed seam a future production caller
/// (mapping an applied topology's per-monitor facts into a plan) will need
/// once wired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorWindowMediaError(pub MediaContractError);

impl std::fmt::Display for MonitorWindowMediaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "monitor window plan rejected: {}", self.0)
    }
}

impl std::error::Error for MonitorWindowMediaError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests' window key: the root window is 0, each monitor's window
    /// its session monitor id.
    type Key = u16;
    const ROOT: Key = 0;

    fn key_for(id: SessionMonitorId) -> Key {
        id.get()
    }

    fn build_plan(
        monitor_ids: &[SessionMonitorId],
        display_ids: &[u32],
    ) -> Result<MultiWindowPlan<Key>, MultiWindowPlanError> {
        MultiWindowPlan::build(monitor_ids, display_ids, ROOT, key_for)
    }

    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("test session monitor id must be nonzero")
    }

    /// A synthetic active display, defaulting to a common 1920x1080 size --
    /// most tests only care about identity (`display_id`), not size,
    /// since confirmation is now an exact-id check.
    fn active_display(display_id: u32) -> ActiveDisplayInfo {
        ActiveDisplayInfo {
            display_id,
            width_pts: 1920.0,
            height_pts: 1080.0,
        }
    }

    /// A genuine fullscreen-bound observation whose exact observed
    /// `CGDirectDisplayID` is `observed_display_id` -- pass the
    /// assignment's own target id to build a confirming observation, or any
    /// other id to build a same-size-but-wrong-display mismatch.
    fn confirmed_observation_for(observed_display_id: u32) -> ViewportBindObservation {
        ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: false,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: Some(observed_display_id),
        }
    }

    /// A "mid-transition" observation: not genuinely bind-ready (root has
    /// dipped out of fullscreen while AppKit tears down its previous
    /// Space/bind, exactly as a real `SetMonitor` move visibly does for a
    /// frame or more), and not closed. `observed_display_id` is left `None`
    /// (identity typically cannot even be read mid-transition), matching
    /// [`viewport_bind_confirmed`]'s definition of "not bind-ready".
    fn mid_move_unbound_observation() -> ViewportBindObservation {
        ViewportBindObservation {
            inner_rect_known: false,
            fullscreen: Some(false),
            close_requested: false,
            monitor_size_pts: None,
            inner_rect_size_pts: None,
            observed_display_id: None,
        }
    }

    #[test]
    fn single_monitor_plan_has_no_additional_windows() {
        let plan = build_plan(&[sid(1)], &[100]).expect("valid plan");
        assert!(plan.assignments().is_empty());
        assert!(plan.windows().is_empty());
    }

    #[test]
    fn two_monitor_plan_has_exactly_one_additional_window() {
        let plan = build_plan(&[sid(1), sid(2)], &[100, 200]).expect("valid plan");
        assert_eq!(plan.assignments().len(), 1);
        assert_eq!(plan.assignments()[0].session_monitor_id, sid(2));
        assert_eq!(plan.assignments()[0].display_id, 200);
    }

    #[test]
    fn four_monitor_plan_has_exactly_three_additional_windows() {
        let plan =
            build_plan(&[sid(1), sid(2), sid(3), sid(4)], &[10, 20, 30, 40]).expect("valid plan");
        assert_eq!(plan.assignments().len(), 3);
        let ids: Vec<_> = plan
            .assignments()
            .iter()
            .map(|assignment| assignment.session_monitor_id)
            .collect();
        assert_eq!(ids, vec![sid(2), sid(3), sid(4)]);
    }

    #[test]
    fn viewport_ids_are_deterministic_and_distinct() {
        let a = key_for(sid(2));
        let b = key_for(sid(2));
        let c = key_for(sid(3));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn mismatched_lengths_are_rejected() {
        let error = build_plan(&[sid(1), sid(2)], &[100]).unwrap_err();
        assert_eq!(
            error,
            MultiWindowPlanError::MismatchedLengths {
                monitor_ids: 2,
                display_ids: 1
            }
        );
    }

    #[test]
    fn zero_monitors_is_rejected() {
        assert_eq!(
            build_plan(&[], &[]).unwrap_err(),
            MultiWindowPlanError::InvalidMonitorCount(0)
        );
    }

    #[test]
    fn more_than_four_monitors_is_rejected() {
        let ids = [sid(1), sid(2), sid(3), sid(4), sid(5)];
        let displays = [1, 2, 3, 4, 5];
        assert_eq!(
            build_plan(&ids, &displays).unwrap_err(),
            MultiWindowPlanError::InvalidMonitorCount(5)
        );
    }

    #[test]
    fn duplicate_session_monitor_id_is_rejected() {
        let error = build_plan(&[sid(1), sid(1)], &[100, 200]).unwrap_err();
        assert_eq!(
            error,
            MultiWindowPlanError::DuplicateSessionMonitorId(sid(1))
        );
    }

    #[test]
    fn duplicate_display_id_is_rejected() {
        let error = build_plan(&[sid(1), sid(2)], &[100, 100]).unwrap_err();
        assert_eq!(error, MultiWindowPlanError::DuplicateDisplayId(100));
    }

    #[test]
    fn cg_display_id_parses_the_decimal_client_display_id() {
        assert_eq!(display_id_from_client_display_id("1234"), Some(1234));
        assert_eq!(display_id_from_client_display_id("not-a-number"), None);
        assert_eq!(display_id_from_client_display_id(""), None);
    }

    #[test]
    fn monitor_index_in_finds_the_exact_position() {
        let active = [500_u32, 100, 300];
        assert_eq!(monitor_index_in(&active, 500), Some(0));
        assert_eq!(monitor_index_in(&active, 100), Some(1));
        assert_eq!(monitor_index_in(&active, 300), Some(2));
        assert_eq!(monitor_index_in(&active, 999), None);
    }

    #[test]
    fn window_title_for_is_deterministic_and_distinct_per_session_monitor_id() {
        assert_eq!(window_title_for(sid(2)), window_title_for(sid(2)));
        assert_ne!(window_title_for(sid(2)), window_title_for(sid(3)));
        assert!(window_title_for(sid(2)).contains('2'));
    }

    #[test]
    fn viewport_bind_confirmed_requires_rect_and_fullscreen_and_no_close_request() {
        assert!(viewport_bind_confirmed(ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: false,
            ..Default::default()
        }));
        assert!(!viewport_bind_confirmed(ViewportBindObservation {
            inner_rect_known: false,
            fullscreen: Some(true),
            close_requested: false,
            ..Default::default()
        }));
        assert!(!viewport_bind_confirmed(ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(false),
            close_requested: false,
            ..Default::default()
        }));
        assert!(!viewport_bind_confirmed(ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: None,
            close_requested: false,
            ..Default::default()
        }));
        assert!(!viewport_bind_confirmed(ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: true,
            ..Default::default()
        }));
    }

    #[test]
    fn viewport_confirmed_on_exact_display_requires_the_exact_id() {
        let confirmed = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: false,
            observed_display_id: Some(1),
            ..Default::default()
        };
        assert!(viewport_confirmed_on_exact_display(confirmed, 1));
        assert!(!viewport_confirmed_on_exact_display(confirmed, 2));
    }

    #[test]
    fn viewport_confirmed_on_exact_display_never_confirms_a_same_size_different_display() {
        // Core regression: size-only matching could not distinguish two
        // identically-sized displays. A window that is genuinely fullscreen
        // and reports a matching *size* but a *different* exact display id
        // must never confirm.
        let same_size_wrong_display = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: false,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: Some(2),
        };
        assert!(!viewport_confirmed_on_exact_display(
            same_size_wrong_display,
            1
        ));
    }

    #[test]
    fn viewport_confirmed_on_exact_display_never_confirms_a_missing_identity() {
        // Identity could not be observed at all -- must fail closed, never
        // fall back to size/coordinates as a substitute success signal.
        let missing_identity = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: false,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: None,
        };
        assert!(!viewport_confirmed_on_exact_display(missing_identity, 1));
    }

    #[test]
    fn viewport_confirmed_on_exact_display_rejects_an_unconfirmed_bind_even_with_the_right_id() {
        let not_yet_fullscreen = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(false),
            close_requested: false,
            observed_display_id: Some(1),
            ..Default::default()
        };
        assert!(!viewport_confirmed_on_exact_display(not_yet_fullscreen, 1));
    }

    #[test]
    fn swapped_root_and_secondary_identical_size_displays_never_cross_confirm() {
        // "Root confirmation uses the same exact-id check" -- simulate a
        // root assigned to display 1 and a secondary assigned to display 2
        // (identical logical size), but their windows land swapped. Both
        // checks -- using the exact same shared function root confirmation
        // and `record_observation` both call -- must independently reject.
        let root_expected = 1_u32;
        let secondary_expected = 2_u32;
        let root_landed_on_secondarys_display = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: false,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: Some(secondary_expected),
        };
        let secondary_landed_on_roots_display = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: false,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: Some(root_expected),
        };
        assert!(!viewport_confirmed_on_exact_display(
            root_landed_on_secondarys_display,
            root_expected
        ));
        assert!(!viewport_confirmed_on_exact_display(
            secondary_landed_on_roots_display,
            secondary_expected
        ));
    }

    #[test]
    fn plan_records_the_primary_cg_display_id_root_must_confirm_against() {
        let plan = build_plan(&[sid(1), sid(2), sid(3)], &[10, 20, 30]).expect("valid plan");
        assert_eq!(plan.primary_display_id(), 10);
    }

    #[test]
    fn a_degenerate_single_monitor_plan_still_requires_root_to_independently_confirm() {
        // Finding 1: even a zero-additional-window (single-monitor) plan
        // must not vacuously commit -- root itself must still confirm on
        // the negotiated primary display before the whole attempt reports
        // `Confirmed`.
        let plan = build_plan(&[sid(1)], &[100]).expect("valid plan");
        assert!(plan.assignments().is_empty());
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let active_displays = [active_display(100)];
        assert_eq!(
            attempt.poll(Duration::from_millis(1), Duration::from_secs(5)),
            MultiWindowEnterPoll::StillWaiting,
            "a zero-assignment plan must not commit before root itself confirms",
        );
        attempt.record_observation(ROOT, confirmed_observation_for(100), &active_displays);
        assert_eq!(
            attempt.poll(Duration::from_millis(2), Duration::from_secs(5)),
            MultiWindowEnterPoll::Confirmed,
        );
    }

    #[test]
    fn root_and_secondary_same_sized_displays_swapped_aborts_the_whole_attempt() {
        // Same-sized-display regression test at the `MultiWindowEnterAttempt`
        // level (not just the pure `viewport_confirmed_on_exact_display`
        // function): root assigned to display 1, secondary assigned to an
        // identically-sized display 2, but root's own observation reports it
        // landed on display 2 instead (a real swap). `record_observation`
        // must reject this for root exactly like it already does for any
        // secondary, aborting the whole transactional attempt.
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let active_displays = [active_display(1), active_display(2)];
        attempt.record_observation(ROOT, confirmed_observation_for(2), &active_displays);
        match attempt.poll(Duration::ZERO, Duration::from_secs(999)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ROOT);
                assert_eq!(
                    reason.kind,
                    HardFailureKind::WrongDisplay {
                        expected_display_id: 1,
                        observed_display_id: 2,
                    }
                );
            }
            other => panic!("expected root's wrong-display bind to abort, got {other:?}"),
        }
    }

    #[test]
    fn root_hard_fails_immediately_when_the_negotiated_primary_display_disappears() {
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        // Display 1 (root's negotiated primary) is no longer active; only
        // the secondary's display remains.
        let active_displays = [active_display(2)];
        attempt.record_observation(ROOT, confirmed_observation_for(1), &active_displays);
        match attempt.poll(Duration::ZERO, Duration::from_secs(999)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ROOT);
                assert_eq!(
                    reason.kind,
                    HardFailureKind::DisplayDisappeared { display_id: 1 }
                );
            }
            other => panic!("expected root's disappeared primary display to abort, got {other:?}"),
        }
    }

    #[test]
    fn root_move_command_needed_only_until_recorded_for_the_exact_target() {
        // "Avoid sending SetMonitor every frame; track requested target."
        let plan = build_plan(&[sid(1)], &[100]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        assert!(
            attempt.root_move_command_needed(100),
            "no command has ever been sent yet"
        );
        attempt.record_root_move_command_sent(100);
        assert!(
            !attempt.root_move_command_needed(100),
            "the exact same target must not be re-requested every frame"
        );
        // A renegotiated primary (a different target) must be requested
        // again even though *a* command was already sent for the old one.
        assert!(attempt.root_move_command_needed(200));
        attempt.record_root_move_command_sent(200);
        assert!(!attempt.root_move_command_needed(200));
    }

    #[test]
    fn root_move_in_flight_treats_a_still_wrong_display_as_pending_not_a_hard_failure() {
        // Root starts the move already bind-ready on some *other* display
        // (its pre-session state) before AppKit has had a chance to even
        // begin tearing that bind down. A caller must not have this
        // treated as an immediate hard failure -- the move has not even
        // visibly started yet.
        let plan = build_plan(&[sid(1), sid(2)], &[100, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let active_displays = [active_display(100), active_display(2)];
        attempt.record_root_move_command_sent(100);
        // Still bind-ready on the old display 999 (not 100): would be an
        // immediate `HardFailure(WrongDisplay)` via `evaluate_viewport_bind`
        // alone, but must downgrade to `Pending` while the move is in
        // flight and has not yet dipped through an unbound observation.
        attempt.record_observation(ROOT, confirmed_observation_for(999), &active_displays);
        assert!(
            attempt.abort_reason().is_none(),
            "an in-flight root move must not hard-fail on the pre-move display"
        );
        assert_eq!(
            attempt.poll(Duration::from_millis(1), Duration::from_secs(999)),
            MultiWindowEnterPoll::StillWaiting,
        );
    }

    #[test]
    fn root_move_eventually_confirms_the_expected_display_after_dipping_through_unbound() {
        let plan = build_plan(&[sid(1), sid(2)], &[100, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan.clone(), Duration::ZERO);
        let secondary_viewport_id = plan.windows()[0];
        let active_displays = [active_display(100), active_display(2)];
        attempt.record_root_move_command_sent(100);
        // Still on the old display -- ordinary in-flight pending.
        attempt.record_observation(ROOT, confirmed_observation_for(999), &active_displays);
        // AppKit visibly tears the old bind down for a frame.
        attempt.record_observation(ROOT, mid_move_unbound_observation(), &active_displays);
        // Now settles onto the correct, expected display.
        attempt.record_observation(ROOT, confirmed_observation_for(100), &active_displays);
        attempt.record_observation(
            secondary_viewport_id,
            confirmed_observation_for(2),
            &active_displays,
        );
        assert!(attempt.abort_reason().is_none());
        assert_eq!(
            attempt.poll(Duration::from_millis(5), Duration::from_secs(999)),
            MultiWindowEnterPoll::Confirmed,
        );
    }

    #[test]
    fn root_move_that_never_settles_times_out_rather_than_aborting() {
        // A move that is requested but never resolves (root stays wherever
        // it started, forever) must be waited out by the caller's own
        // bounded entry deadline -- never a hard failure from this
        // function itself.
        let plan = build_plan(&[sid(1)], &[100]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let active_displays = [active_display(100)];
        attempt.record_root_move_command_sent(100);
        for millis in [10, 50, 200, 999] {
            attempt.record_observation(ROOT, confirmed_observation_for(999), &active_displays);
            assert!(
                attempt.abort_reason().is_none(),
                "a still-in-flight move must never hard-fail on its own, at t={millis}ms",
            );
        }
        match attempt.poll(Duration::from_secs(10), Duration::from_secs(5)) {
            MultiWindowEnterPoll::TimedOut { unconfirmed } => {
                assert_eq!(unconfirmed, vec![ROOT]);
            }
            other => panic!("expected TimedOut (never Aborted), got {other:?}"),
        }
    }

    #[test]
    fn root_move_that_settles_on_a_genuinely_different_display_hard_fails() {
        // Once the move has visibly dipped through an unbound observation
        // and then re-bound -- anywhere -- that is the settled outcome: a
        // persisting wrong display at that point is a real, immediate hard
        // failure, exactly like any other viewport's, not more pending
        // state.
        let plan = build_plan(&[sid(1)], &[100]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let active_displays = [active_display(100), active_display(777)];
        attempt.record_root_move_command_sent(100);
        attempt.record_observation(ROOT, mid_move_unbound_observation(), &active_displays);
        attempt.record_observation(ROOT, confirmed_observation_for(777), &active_displays);
        match attempt.poll(Duration::ZERO, Duration::from_secs(999)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ROOT);
                assert_eq!(
                    reason.kind,
                    HardFailureKind::WrongDisplay {
                        expected_display_id: 100,
                        observed_display_id: 777,
                    }
                );
            }
            other => {
                panic!("expected an immediate Aborted once the move has settled, got {other:?}")
            }
        }
    }

    #[test]
    fn root_move_close_requested_hard_fails_immediately_even_while_in_flight() {
        let plan = build_plan(&[sid(1)], &[100]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let active_displays = [active_display(100)];
        attempt.record_root_move_command_sent(100);
        let closed = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: true,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: Some(999),
        };
        attempt.record_observation(ROOT, closed, &active_displays);
        match attempt.poll(Duration::ZERO, Duration::from_secs(999)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ROOT);
                assert_eq!(reason.kind, HardFailureKind::CloseRequested);
            }
            other => panic!(
                "close_requested must never be downgraded by an in-flight move, got {other:?}"
            ),
        }
    }

    #[test]
    fn root_move_display_disappeared_hard_fails_immediately_even_while_in_flight() {
        let plan = build_plan(&[sid(1)], &[100]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        // The negotiated primary (100) is not in the active list at all.
        let active_displays: [ActiveDisplayInfo; 0] = [];
        attempt.record_root_move_command_sent(100);
        attempt.record_observation(ROOT, confirmed_observation_for(999), &active_displays);
        match attempt.poll(Duration::ZERO, Duration::from_secs(999)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ROOT);
                assert_eq!(
                    reason.kind,
                    HardFailureKind::DisplayDisappeared { display_id: 100 }
                );
            }
            other => panic!(
                "display disappearance must never be downgraded by an in-flight move, got {other:?}"
            ),
        }
    }

    #[test]
    fn evaluate_root_viewport_bind_during_move_matches_the_plain_evaluation_once_not_in_flight() {
        // Once `move_in_flight` is false (root never needed a move, or the
        // move already confirmed once), this must be identical to
        // `evaluate_viewport_bind` -- a regression to wrong-display is a
        // real, immediate hard failure again.
        let observation = confirmed_observation_for(999);
        assert_eq!(
            evaluate_root_viewport_bind_during_move(observation, 100, true, false, true),
            evaluate_viewport_bind(observation, 100, true),
        );
        assert_eq!(
            evaluate_root_viewport_bind_during_move(observation, 100, true, false, true),
            ViewportBindEvaluation::HardFailure(HardFailureKind::WrongDisplay {
                expected_display_id: 100,
                observed_display_id: 999,
            }),
        );
    }

    #[test]
    fn full_roster_including_root_confirms_together() {
        // The complete success path: root plus every secondary must all
        // independently confirm before the transaction reports `Confirmed`.
        let plan = build_plan(&[sid(1), sid(2), sid(3)], &[1, 2, 3]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan.clone(), Duration::ZERO);
        let active_displays = [active_display(1), active_display(2), active_display(3)];
        let ids = plan.windows();
        attempt.record_observation(ROOT, confirmed_observation_for(1), &active_displays);
        attempt.record_observation(ids[0], confirmed_observation_for(2), &active_displays);
        attempt.record_observation(ids[1], confirmed_observation_for(3), &active_displays);
        assert_eq!(
            attempt.poll(Duration::from_millis(5), Duration::from_secs(5)),
            MultiWindowEnterPoll::Confirmed,
        );
    }

    #[test]
    fn enter_attempt_waits_until_every_window_confirms() {
        let plan = build_plan(&[sid(1), sid(2), sid(3)], &[1, 2, 3]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan.clone(), Duration::ZERO);
        let active_displays = [active_display(1), active_display(2), active_display(3)];
        let ids = plan.windows();
        assert_eq!(
            attempt.poll(Duration::from_millis(10), Duration::from_secs(5)),
            MultiWindowEnterPoll::StillWaiting
        );
        attempt.record_observation(ids[0], confirmed_observation_for(2), &active_displays);
        assert_eq!(
            attempt.poll(Duration::from_millis(20), Duration::from_secs(5)),
            MultiWindowEnterPoll::StillWaiting
        );
        attempt.record_observation(ids[1], confirmed_observation_for(3), &active_displays);
        // Every secondary window has confirmed, but root itself never has --
        // the whole transaction must still not commit.
        assert_eq!(
            attempt.poll(Duration::from_millis(25), Duration::from_secs(5)),
            MultiWindowEnterPoll::StillWaiting
        );
        attempt.record_observation(ROOT, confirmed_observation_for(1), &active_displays);
        assert_eq!(
            attempt.poll(Duration::from_millis(30), Duration::from_secs(5)),
            MultiWindowEnterPoll::Confirmed
        );
    }

    #[test]
    fn enter_attempt_times_out_and_names_every_unconfirmed_window() {
        let plan = build_plan(&[sid(1), sid(2), sid(3)], &[1, 2, 3]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan.clone(), Duration::ZERO);
        let active_displays = [active_display(1), active_display(2), active_display(3)];
        let ids = plan.windows();
        attempt.record_observation(ROOT, confirmed_observation_for(1), &active_displays);
        attempt.record_observation(ids[0], confirmed_observation_for(2), &active_displays);
        match attempt.poll(Duration::from_secs(5), Duration::from_secs(5)) {
            MultiWindowEnterPoll::TimedOut { unconfirmed } => {
                assert_eq!(unconfirmed, vec![ids[1]]);
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[test]
    fn enter_attempt_times_out_naming_root_when_only_root_is_unconfirmed() {
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan.clone(), Duration::ZERO);
        let active_displays = [active_display(1), active_display(2)];
        let ids = plan.windows();
        attempt.record_observation(ids[0], confirmed_observation_for(2), &active_displays);
        match attempt.poll(Duration::from_secs(5), Duration::from_secs(5)) {
            MultiWindowEnterPoll::TimedOut { unconfirmed } => {
                assert_eq!(unconfirmed, vec![ROOT]);
            }
            other => panic!("expected TimedOut naming root, got {other:?}"),
        }
    }

    #[test]
    fn enter_attempt_never_confirms_from_an_observation_outside_the_plan() {
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let stray_id = key_for(sid(99));
        attempt.record_observation(stray_id, confirmed_observation_for(2), &[active_display(2)]);
        let mut expected_unconfirmed = vec![ROOT];
        expected_unconfirmed.extend(attempt.plan().windows());
        assert_eq!(
            attempt.poll(Duration::from_secs(999), Duration::from_secs(5)),
            MultiWindowEnterPoll::TimedOut {
                unconfirmed: expected_unconfirmed,
            }
        );
    }

    #[test]
    fn a_regressed_confirmation_uncommits_rather_than_sticking() {
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let ids = attempt.plan().windows();
        let active_displays = [active_display(2)];
        attempt.record_observation(ids[0], confirmed_observation_for(2), &active_displays);
        // Regress: the user exits fullscreen before the second window binds.
        attempt.record_observation(
            ids[0],
            ViewportBindObservation {
                inner_rect_known: true,
                fullscreen: Some(false),
                close_requested: false,
                monitor_size_pts: Some((1920.0, 1080.0)),
                inner_rect_size_pts: None,
                observed_display_id: Some(2),
            },
            &active_displays,
        );
        match attempt.poll(Duration::from_secs(999), Duration::from_secs(5)) {
            MultiWindowEnterPoll::TimedOut { unconfirmed } => {
                assert!(unconfirmed.contains(&ids[0]));
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[test]
    fn record_observation_never_confirms_when_the_target_display_is_no_longer_active() {
        // Finding 1 (now hardened by the hard-failure abort): a display
        // that disconnects between negotiation and the window binding must
        // never confirm, even if the viewport itself otherwise reports a
        // fullscreen bind (e.g. it landed on some other remaining display).
        // A disappearing assigned display is a *definite* failure, not a
        // "maybe it'll show up later" pending state, so the whole attempt
        // must abort immediately rather than wait out the timeout.
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let ids = attempt.plan().windows();
        // Display 2 (the assignment's target) is no longer active; only an
        // unrelated display 3 remains.
        let active_displays = [active_display(3)];
        attempt.record_observation(ids[0], confirmed_observation_for(2), &active_displays);
        match attempt.poll(Duration::from_secs(999), Duration::from_secs(5)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ids[0]);
                assert_eq!(
                    reason.kind,
                    HardFailureKind::DisplayDisappeared { display_id: 2 }
                );
            }
            other => panic!("expected Aborted, got {other:?}"),
        }
    }

    #[test]
    fn record_observation_never_confirms_a_same_size_wrong_display_bind() {
        // Core regression: `fullscreen == true` and a matching *size* are
        // not enough -- a window that reports fullscreen on a *different*
        // display than the one it was assigned (even an identically-sized
        // one) must not confirm. Size-only matching could not have caught
        // this. A definite wrong id while bind-ready is a hard failure, so
        // the whole attempt aborts immediately rather than waiting out the
        // timeout.
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let ids = attempt.plan().windows();
        // Display 2 (the assignment's target) is genuinely active and the
        // same logical size the observation reports -- only the exact
        // observed display id is wrong (it landed on display 99 instead).
        let active_displays = [active_display(2)];
        attempt.record_observation(ids[0], confirmed_observation_for(99), &active_displays);
        match attempt.poll(Duration::from_secs(999), Duration::from_secs(5)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ids[0]);
                assert_eq!(
                    reason.kind,
                    HardFailureKind::WrongDisplay {
                        expected_display_id: 2,
                        observed_display_id: 99
                    }
                );
            }
            other => panic!("expected Aborted, got {other:?}"),
        }
    }

    #[test]
    fn record_observation_never_confirms_a_missing_identity_bind() {
        // Identity could not be observed at all -- must fail closed rather
        // than falling back to size/coordinates as a substitute.
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let ids = attempt.plan().windows();
        let active_displays = [active_display(2)];
        let missing_identity = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: false,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: None,
        };
        attempt.record_observation(ids[0], missing_identity, &active_displays);
        match attempt.poll(Duration::from_secs(999), Duration::from_secs(5)) {
            MultiWindowEnterPoll::TimedOut { unconfirmed } => {
                assert!(unconfirmed.contains(&ids[0]));
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[test]
    fn record_observation_maps_each_assignment_to_its_own_expected_screen() {
        // A real 4-monitor mapping: each secondary assignment must only
        // confirm against *its own* exact display id, never a different
        // assignment's.
        let plan = build_plan(&[sid(1), sid(2), sid(3), sid(4)], &[10, 20, 30, 40]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan.clone(), Duration::ZERO);
        let ids = plan.windows();
        let active_displays = [
            ActiveDisplayInfo {
                display_id: 20,
                width_pts: 1920.0,
                height_pts: 1080.0,
            },
            ActiveDisplayInfo {
                display_id: 30,
                width_pts: 2560.0,
                height_pts: 1440.0,
            },
            ActiveDisplayInfo {
                display_id: 40,
                width_pts: 1280.0,
                height_pts: 1024.0,
            },
        ];
        attempt.record_observation(
            ids[0],
            ViewportBindObservation {
                inner_rect_known: true,
                fullscreen: Some(true),
                close_requested: false,
                monitor_size_pts: Some((1920.0, 1080.0)),
                inner_rect_size_pts: None,
                observed_display_id: Some(20),
            },
            &active_displays,
        );
        attempt.record_observation(
            ids[1],
            ViewportBindObservation {
                inner_rect_known: true,
                fullscreen: Some(true),
                close_requested: false,
                monitor_size_pts: Some((2560.0, 1440.0)),
                inner_rect_size_pts: None,
                observed_display_id: Some(30),
            },
            &active_displays,
        );
        // Wrong display for assignment 40 (reports display 30's exact id
        // instead -- and, notably, display 30's identical size too, so a
        // size-only check could never have caught this).
        attempt.record_observation(
            ids[2],
            ViewportBindObservation {
                inner_rect_known: true,
                fullscreen: Some(true),
                close_requested: false,
                monitor_size_pts: Some((2560.0, 1440.0)),
                inner_rect_size_pts: None,
                observed_display_id: Some(30),
            },
            &active_displays,
        );
        match attempt.poll(Duration::from_secs(999), Duration::from_secs(5)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                // A definite wrong-display bind on assignment `ids[2]` is a
                // hard failure for the *whole* transactional attempt, not
                // just that one viewport -- per the "close/leave all and
                // fail before input/media activation" requirement, even
                // though `ids[0]`/`ids[1]` had already genuinely confirmed.
                assert_eq!(reason.window, ids[2]);
                assert_eq!(
                    reason.kind,
                    HardFailureKind::WrongDisplay {
                        expected_display_id: 40,
                        observed_display_id: 30
                    }
                );
            }
            other => panic!("expected Aborted, got {other:?}"),
        }
    }

    #[test]
    fn close_requested_aborts_immediately_without_waiting_for_the_timeout() {
        // Finding 2: `close_requested` before commit must abort the whole
        // attempt the instant it is observed, never waiting out whatever
        // timeout the caller configured.
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let ids = attempt.plan().windows();
        let active_displays = [active_display(2)];
        let closed = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: true,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: Some(2),
        };
        attempt.record_observation(ids[0], closed, &active_displays);
        // A huge timeout and an `now` of zero: if the abort waited on the
        // timeout this would still report `TimedOut`/still be pending.
        match attempt.poll(Duration::ZERO, Duration::from_secs(999)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ids[0]);
                assert_eq!(reason.kind, HardFailureKind::CloseRequested);
            }
            other => panic!("expected immediate Aborted, got {other:?}"),
        }
    }

    #[test]
    fn wrong_display_while_bind_ready_aborts_immediately_without_waiting_for_the_timeout() {
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let ids = attempt.plan().windows();
        let active_displays = [active_display(2)];
        attempt.record_observation(ids[0], confirmed_observation_for(99), &active_displays);
        match attempt.poll(Duration::ZERO, Duration::from_secs(999)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ids[0]);
                assert_eq!(
                    reason.kind,
                    HardFailureKind::WrongDisplay {
                        expected_display_id: 2,
                        observed_display_id: 99
                    }
                );
            }
            other => panic!("expected immediate Aborted, got {other:?}"),
        }
    }

    #[test]
    fn disappearing_display_aborts_immediately_without_waiting_for_the_timeout() {
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let ids = attempt.plan().windows();
        let active_displays = [active_display(3)];
        attempt.record_observation(ids[0], confirmed_observation_for(2), &active_displays);
        match attempt.poll(Duration::ZERO, Duration::from_secs(999)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.window, ids[0]);
                assert_eq!(
                    reason.kind,
                    HardFailureKind::DisplayDisappeared { display_id: 2 }
                );
            }
            other => panic!("expected immediate Aborted, got {other:?}"),
        }
    }

    #[test]
    fn a_pending_bind_ready_observation_with_a_missing_identity_is_never_a_hard_failure() {
        // Distinguishing "not observable yet" from "definite wrong
        // identity": a missing `observed_display_id` while otherwise
        // bind-ready must stay `Pending` (and thus eventually `TimedOut`),
        // never abort the whole attempt the way a *definite* wrong id does.
        let plan = build_plan(&[sid(1), sid(2)], &[1, 2]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let ids = attempt.plan().windows();
        let active_displays = [active_display(2)];
        let bind_ready_but_unresolved = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: false,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: None,
        };
        attempt.record_observation(ids[0], bind_ready_but_unresolved, &active_displays);
        match attempt.poll(Duration::from_secs(999), Duration::from_secs(5)) {
            MultiWindowEnterPoll::TimedOut { unconfirmed } => {
                assert!(unconfirmed.contains(&ids[0]));
            }
            other => panic!("expected TimedOut (pending, not aborted), got {other:?}"),
        }
    }

    #[test]
    fn once_aborted_a_later_fully_correct_observation_never_un_aborts_the_attempt() {
        // "Never recreate them until app exit": once an attempt has hard
        // failed it must latch -- a subsequent, fully genuine confirmation
        // for every window must not resurrect it into `Confirmed`.
        let plan = build_plan(&[sid(1), sid(2), sid(3)], &[1, 2, 3]).expect("plan");
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        let ids = attempt.plan().windows();
        let active_displays = [active_display(2), active_display(3)];
        // First, hard-fail it via a close request on the first secondary
        // window.
        let closed = ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(true),
            close_requested: true,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: None,
            observed_display_id: Some(2),
        };
        attempt.record_observation(ids[0], closed, &active_displays);
        assert!(matches!(
            attempt.poll(Duration::ZERO, Duration::from_secs(999)),
            MultiWindowEnterPoll::Aborted(_)
        ));
        assert!(attempt.abort_reason().is_some());
        // Now feed a genuinely correct, fully confirmed observation for
        // every window in the plan.
        attempt.record_observation(ids[0], confirmed_observation_for(2), &active_displays);
        attempt.record_observation(ids[1], confirmed_observation_for(3), &active_displays);
        match attempt.poll(Duration::from_secs(999), Duration::from_secs(999)) {
            MultiWindowEnterPoll::Aborted(reason) => {
                assert_eq!(reason.kind, HardFailureKind::CloseRequested);
            }
            other => panic!("expected the attempt to stay latched as Aborted, got {other:?}"),
        }
    }

    #[test]
    fn active_monitor_prefers_root_focus() {
        assert_eq!(
            active_monitor(true, &[(sid(2), true), (sid(3), true)]),
            ActiveMonitor::Primary
        );
    }

    #[test]
    fn active_monitor_follows_the_focused_secondary_window() {
        assert_eq!(
            active_monitor(false, &[(sid(2), false), (sid(3), true)]),
            ActiveMonitor::Secondary(sid(3))
        );
    }

    #[test]
    fn active_monitor_defaults_to_primary_when_nothing_reports_focus() {
        assert_eq!(
            active_monitor(false, &[(sid(2), false), (sid(3), false)]),
            ActiveMonitor::Primary
        );
    }

    #[test]
    fn active_monitor_breaks_ties_on_the_lowest_session_monitor_id() {
        assert_eq!(
            active_monitor(false, &[(sid(3), true), (sid(2), true)]),
            ActiveMonitor::Secondary(sid(2))
        );
    }
}
