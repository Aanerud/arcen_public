//! Production live-session wiring for Carrier A (`MuxedReliableStream`)
//! multi-monitor-v1 presentation.
//!
//! This is the seam that closed the gap `crate::ui::multi_window` and
//! `crate::ui::multi_window_runtime`'s own module docs both named as the
//! "Deck blocker": turning a real `ServerHello`'s applied multi-monitor-v1
//! capability into a transactional
//! [`crate::ui::multi_window_runtime::MultiWindowEnterAttempt`], keeping it
//! alive frame over frame inside `ArcenApp`, and producing the pure
//! per-frame decisions (entry validation, teardown) the live session
//! controller acts on.
//!
//! # Scope of this module
//!
//! - [`begin_multi_window_entry`]: validates an applied `ServerHello`
//!   topology (delegating to
//!   `crate::ui::multi_window_session::validate_applied_topology_for_production`)
//!   and builds the additional-window plan
//!   (`crate::ui::multi_window_runtime::MultiWindowPlan::build`) in one seam,
//!   so `ArcenApp` never re-derives or re-orders either itself.
//! - [`MultiWindowSessionState`]: the small state machine `ArcenApp` holds
//!   for a live attempt -- `Inactive` (the default, and the only state for
//!   every legacy/primary-only session) or `Active` (a validated topology's
//!   transactional entry in progress or committed).
//! - [`teardown_required`]: whether the currently committed roster must
//!   trigger a full disconnect this frame, wrapping
//!   `crate::ui::multi_window_session::multi_window_teardown_required` with
//!   this session's own committed roster.
//!
//! Pointer coordinates are deliberately absent here. A committed Match My
//! Layout session maps every viewport's pointer, scroll, and pen sample
//! through one path only: `crate::ui::region_runtime::DeckRegionRuntime`'s
//! shared [`arcen_input::RegionInputEmitter`], keyed by region id and
//! logical fixed-point position. This module no longer offers a second,
//! desktop-space affine mapping for secondary viewports.
//!
//! # Live behind the runtime gate
//!
//! Every entry point here is reachable from `ArcenApp` behind
//! `crate::ui::multi_window::multi_window_runtime_available()`, which now
//! returns `true` (see that module's doc): any host that advertises
//! multi-monitor-v1 and whose negotiated topology passes local preflight
//! (Separate Spaces on, standard fullscreen, 1..=4 displays) is asked for a
//! real multi-monitor layout, and this module is what `ArcenApp` actually
//! drives that session through. `Inactive` (this module's default state)
//! still covers every legacy/single-monitor session and every old/
//! unsupported host that never negotiates the sidecar in the first place.
//!
//! # Relative pointer lock is per-viewport, not root-only
//!
//! `ArcenApp`'s relative (mouse-look style) pointer lock has an owner --
//! either root (`self.pointer_lock`) or a negotiated secondary
//! (`self.secondary_pointer_lock: Option<SessionMonitorId>`). A focused
//! secondary viewport can acquire its own relative lock
//! (`acquire_secondary_pointer_lock`) and dispatch relative motion/buttons/
//! scroll from that viewport (`dispatch_secondary_pointer`/
//! `dispatch_secondary_scroll`); root's own lock is released/parked whenever
//! focus moves to a secondary (`release_secondary_pointer_lock_if_not_focused`
//! and friends), so only one viewport ever owns the lock at a time.

use std::time::Duration;

use crate::protocol::messages::{MultiMonitorCarrierMsg, ServerHelloMsg};
use crate::ui::multi_window_runtime::{
    MultiWindowEnterAttempt, MultiWindowPlan, MultiWindowPlanError,
};
use crate::ui::multi_window_session::{
    multi_window_teardown_required, validate_applied_topology_for_production,
    AppliedTopologyValidationError, ValidatedAppliedTopology,
};

/// How long a transactional multi-window entry waits for every planned
/// window to confirm native fullscreen bind before giving up.
///
/// Mirrors `multi-monitor-window-diagnostic`'s own `--timeout-secs` default
/// (see `crate::main::run_multi_monitor_window_diagnostic_subcommand`), so
/// production and the physically-validated diagnostic share one proven
/// timeout value rather than an independently-chosen one.
pub const MULTI_WINDOW_ENTER_TIMEOUT: Duration = Duration::from_secs(10);

/// Failure turning an applied `ServerHello` multi-monitor-v1 capability into
/// a [`MultiWindowPlan`], once
/// [`validate_applied_topology_for_production`] has already confirmed the
/// carrier/roster/generation/`CGDirectDisplayID` facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiWindowEntryError {
    /// The applied topology itself failed validation.
    Validation(AppliedTopologyValidationError),
    /// The validated topology's roster could not become a window plan (only
    /// reachable defensively -- a topology that already passed validation's
    /// `1..=MAX_MULTI_MONITOR_COUNT`/no-duplicate invariants always builds a
    /// valid plan, but this is never assumed away with an `unwrap`).
    Plan(MultiWindowPlanError),
}

impl std::fmt::Display for MultiWindowEntryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(error) => write!(formatter, "{error}"),
            Self::Plan(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for MultiWindowEntryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Validation(error) => Some(error),
            Self::Plan(error) => Some(error),
        }
    }
}

/// Turns `hello`'s applied multi-monitor-v1 capability (if any) into a
/// validated topology plus the additional-window plan
/// [`MultiWindowEnterAttempt::new`] needs.
///
/// Returns `Ok(None)` for every legacy, no-capability, or
/// applied-topology-absent hello -- preserving today's single-monitor
/// behavior byte for byte in every one of those cases, exactly like
/// `crate::ui::multi_window::detect_multi_window_presentation_gap`'s own
/// contract. A single-monitor *applied* topology (the host echoed back
/// exactly the negotiated primary) also returns `Some` with a plan that has
/// zero additional-window assignments: `MultiWindowEnterAttempt::poll`
/// reports that trivially `Confirmed` on its very first poll (vacuously,
/// since there is nothing to wait for), so this degenerates harmlessly into
/// the exact same one-viewport presentation as today, just routed through
/// the committed-roster path instead of the legacy `monitor_id == 0` one.
///
/// # Errors
///
/// See [`MultiWindowEntryError`]. Callers must treat either variant as a
/// hard failure: disconnect rather than silently falling back, since a host
/// that claims to have applied multi-monitor-v1 but sent invalid facts
/// cannot be trusted to still be presenting the primary-only view Deck
/// would otherwise assume.
pub fn begin_multi_window_entry(
    hello: &ServerHelloMsg,
    supported_carriers: &[MultiMonitorCarrierMsg],
) -> Result<Option<(ValidatedAppliedTopology, MultiWindowPlan)>, MultiWindowEntryError> {
    let Ok(Some(capability)) = hello.multi_monitor_v1() else {
        return Ok(None);
    };
    let Some(applied) = capability.applied_topology() else {
        return Ok(None);
    };
    let validated = validate_applied_topology_for_production(applied, supported_carriers)
        .map_err(MultiWindowEntryError::Validation)?;
    let plan = MultiWindowPlan::build(&validated.monitor_ids(), &validated.cg_display_ids())
        .map_err(MultiWindowEntryError::Plan)?;
    Ok(Some((validated, plan)))
}

/// `ArcenApp`'s live per-session state for a production Carrier A
/// multi-window entry.
///
/// `Inactive` is the default: every legacy session, every primary-only
/// applied topology's degenerate zero-assignment plan once confirmed (see
/// [`begin_multi_window_entry`]'s doc), and every session with a
/// `displays_mode` other than Match My Layout or a single local display,
/// since none of those ever reach [`begin_multi_window_entry`] with a real
/// multi-monitor applied topology in the first place. A host that lacks or
/// declines multi-monitor-v1 while it *was* requested never reaches this
/// state either way: that fails the connection outright during auth, before
/// any session (and so this field) exists.
#[derive(Debug, Default)]
pub enum MultiWindowSessionState {
    /// No real multi-window entry is in progress or committed.
    #[default]
    Inactive,
    /// A validated applied topology's transactional entry is in progress or
    /// has committed.
    ///
    /// `attempt` is the single transactional enter/ongoing-teardown-detector
    /// object: `MultiWindowEnterAttempt::record_observation`/`poll` continue
    /// to detect hard failures (a secondary window closing, or its assigned
    /// display disappearing) every frame even after `committed` latches
    /// `true`, so the same object serves both roles without a second
    /// close-detection mechanism.
    Active {
        validated: ValidatedAppliedTopology,
        attempt: MultiWindowEnterAttempt,
        /// Latches `true` the first time `attempt.poll(..)` reports
        /// `Confirmed`. Before that, media/input activation must not
        /// happen: `SharedMediaState::committed_multi_monitor_roster` must
        /// stay `None` and no secondary pointer/keyboard event may be
        /// forwarded, per the task's "input paused until commit"
        /// requirement.
        committed: bool,
    },
}

impl MultiWindowSessionState {
    /// `true` for [`Self::Active`] regardless of `committed`.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    /// `true` only once this attempt has actually committed (see
    /// [`Self::Active`]'s own `committed` doc).
    #[must_use]
    pub const fn is_committed(&self) -> bool {
        matches!(
            self,
            Self::Active {
                committed: true,
                ..
            }
        )
    }
}

/// Whether the currently committed `validated` roster must force a full
/// disconnect this frame: any of its displays (primary included) is no
/// longer live, or `any_secondary_close_requested` is `true`.
///
/// Thin wrapper around
/// `crate::ui::multi_window_session::multi_window_teardown_required`
/// supplying this session's own full committed roster (primary and every
/// secondary) as `committed_cg_display_ids` -- `MultiWindowEnterAttempt`'s
/// own hard-failure detection already independently catches a *secondary*
/// window closing or its display disappearing (see [`MultiWindowSessionState::Active`]'s
/// doc), but only this roster-membership check also catches the *primary*
/// monitor's own display disappearing mid-session, since the primary never
/// gets its own [`crate::ui::multi_window_runtime::MonitorWindowAssignment`]
/// for `MultiWindowEnterAttempt` to track.
#[must_use]
pub fn teardown_required(
    validated: &ValidatedAppliedTopology,
    live_cg_display_ids: &[u32],
    any_secondary_close_requested: bool,
) -> bool {
    multi_window_teardown_required(
        &validated.cg_display_ids(),
        live_cg_display_ids,
        any_secondary_close_requested,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::messages::{
        AppliedMonitorDescriptorMsg, AppliedMonitorMediaPlanMsg, AppliedMonitorTopologyMsg,
        ClientDisplayId, CursorMode, RotationMsg, ServerMultiMonitorMsg, TopologyBackendKindMsg,
    };
    use arcen_media::SessionMonitorId;

    fn base_hello() -> ServerHelloMsg {
        serde_json::from_value(serde_json::json!({
            "type": "server_hello",
            "screen_width": 1920,
            "screen_height": 1080,
        }))
        .expect("minimal server_hello parses")
    }

    fn media_plan() -> AppliedMonitorMediaPlanMsg {
        AppliedMonitorMediaPlanMsg {
            stream_epoch: 1,
            encoder_backend: "openh264-sw-h264".to_string(),
            encoder_class: "software".to_string(),
            codec: "h264".to_string(),
            chroma: "yuv420".to_string(),
            width_px: 1920,
            height_px: 1080,
            fps: 60,
            bitrate_kbps: 8_000,
            cursor_mode: CursorMode::Local,
            degraded: false,
        }
    }

    fn applied_monitor(
        cg_display_id: u32,
        session_monitor_id: u16,
        x: i32,
        width_px: u32,
        height_px: u32,
        is_primary: bool,
    ) -> AppliedMonitorDescriptorMsg {
        AppliedMonitorDescriptorMsg {
            client_display_id: ClientDisplayId::new(cg_display_id.to_string()).expect("valid id"),
            session_monitor_id,
            x,
            y: 0,
            width_px,
            height_px,
            refresh_hz: 60,
            rotation: RotationMsg::Degrees0,
            is_primary,
            media_plan: media_plan(),
        }
    }

    fn server_multi_monitor(
        generation: u64,
        carrier: MultiMonitorCarrierMsg,
        monitors: Vec<AppliedMonitorDescriptorMsg>,
    ) -> ServerMultiMonitorMsg {
        let total_width: u32 = monitors.iter().map(|m| m.width_px).sum();
        let max_height = monitors.iter().map(|m| m.height_px).max().unwrap_or(0);
        let applied = AppliedMonitorTopologyMsg::new(
            generation,
            0,
            0,
            total_width,
            max_height,
            0,
            0,
            carrier,
            monitors,
        )
        .expect("valid applied topology");
        ServerMultiMonitorMsg::new(
            4,
            vec![RotationMsg::Degrees0],
            true,
            TopologyBackendKindMsg::PhysicalOutputs,
            vec![carrier],
            Some(applied),
        )
        .expect("server capability")
    }

    #[test]
    fn legacy_hello_without_the_capability_begins_nothing() {
        assert_eq!(begin_multi_window_entry(&base_hello(), &[]).unwrap(), None);
    }

    #[test]
    fn hello_with_no_applied_topology_begins_nothing() {
        let capability = ServerMultiMonitorMsg::new(
            4,
            vec![RotationMsg::Degrees0],
            true,
            TopologyBackendKindMsg::PhysicalOutputs,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
            None,
        )
        .expect("server capability without an applied topology");
        let hello = base_hello()
            .with_multi_monitor_v1(&capability)
            .expect("attach capability");
        assert_eq!(begin_multi_window_entry(&hello, &[]).unwrap(), None);
    }

    #[test]
    fn single_monitor_applied_topology_yields_a_zero_assignment_plan() {
        let hello = base_hello()
            .with_multi_monitor_v1(&server_multi_monitor(
                1,
                MultiMonitorCarrierMsg::MuxedReliableStream,
                vec![applied_monitor(100, 1, 0, 1920, 1080, true)],
            ))
            .expect("attach capability");
        let (validated, plan) =
            begin_multi_window_entry(&hello, &[MultiMonitorCarrierMsg::MuxedReliableStream])
                .unwrap()
                .expect("single-monitor applied topology still begins an entry");
        assert_eq!(validated.monitor_ids().len(), 1);
        assert!(plan.assignments().is_empty());
        assert_eq!(plan.primary_cg_display_id(), 100);

        // A zero-assignment plan has nothing *additional* to open, but root
        // itself must still independently confirm it is genuinely
        // fullscreen-bound on the negotiated primary display -- it must
        // never vacuously commit just because there are no other windows to
        // wait for.
        let mut attempt = MultiWindowEnterAttempt::new(plan, Duration::ZERO);
        assert_eq!(
            attempt.poll(Duration::ZERO, MULTI_WINDOW_ENTER_TIMEOUT),
            crate::ui::multi_window_runtime::MultiWindowEnterPoll::StillWaiting,
            "root has not confirmed yet, so even a zero-assignment plan must stay pending",
        );

        let active_displays = [crate::ui::multi_window_runtime::ActiveDisplayInfo {
            cg_display_id: 100,
            width_pts: 1920.0,
            height_pts: 1080.0,
        }];
        attempt.record_observation(
            egui::ViewportId::ROOT,
            crate::ui::multi_window_runtime::ViewportBindObservation {
                inner_rect_known: true,
                fullscreen: Some(true),
                close_requested: false,
                monitor_size_pts: Some((1920.0, 1080.0)),
                inner_rect_size_pts: Some((1920.0, 1080.0)),
                observed_display_id: Some(100),
            },
            &active_displays,
        );
        assert_eq!(
            attempt.poll(Duration::ZERO, MULTI_WINDOW_ENTER_TIMEOUT),
            crate::ui::multi_window_runtime::MultiWindowEnterPoll::Confirmed,
            "once root itself confirms, a zero-assignment plan degenerates to the same \
             single-viewport presentation as today",
        );
    }

    #[test]
    fn two_monitor_applied_topology_yields_one_assignment_primary_excluded() {
        let hello = base_hello()
            .with_multi_monitor_v1(&server_multi_monitor(
                7,
                MultiMonitorCarrierMsg::MuxedReliableStream,
                vec![
                    applied_monitor(200, 2, 1920, 1920, 1080, false),
                    applied_monitor(100, 1, 0, 1920, 1080, true),
                ],
            ))
            .expect("attach capability");
        let (validated, plan) =
            begin_multi_window_entry(&hello, &[MultiMonitorCarrierMsg::MuxedReliableStream])
                .unwrap()
                .expect("two-monitor applied topology begins an entry");
        assert_eq!(validated.generation.get(), 7);
        assert_eq!(plan.assignments().len(), 1);
        assert_eq!(
            plan.assignments()[0].session_monitor_id,
            SessionMonitorId::new(2).unwrap()
        );
        assert_eq!(plan.assignments()[0].cg_display_id, 200);
    }

    #[test]
    fn unsupported_carrier_is_a_validation_error_not_a_silent_fallback() {
        let hello = base_hello()
            .with_multi_monitor_v1(&server_multi_monitor(
                1,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                vec![applied_monitor(100, 1, 0, 1920, 1080, true)],
            ))
            .expect("attach capability");
        let error =
            begin_multi_window_entry(&hello, &[MultiMonitorCarrierMsg::MuxedReliableStream])
                .expect_err("unsupported carrier must fail, never silently fall back");
        assert!(matches!(error, MultiWindowEntryError::Validation(_)));
        assert!(error.to_string().contains("muxed_reliable_stream"));
    }

    #[test]
    fn session_state_defaults_to_inactive() {
        let state = MultiWindowSessionState::default();
        assert!(!state.is_active());
        assert!(!state.is_committed());
    }

    #[test]
    fn session_state_active_but_not_yet_committed() {
        let hello = base_hello()
            .with_multi_monitor_v1(&server_multi_monitor(
                1,
                MultiMonitorCarrierMsg::MuxedReliableStream,
                vec![
                    applied_monitor(200, 2, 1920, 1920, 1080, false),
                    applied_monitor(100, 1, 0, 1920, 1080, true),
                ],
            ))
            .expect("attach capability");
        let (validated, plan) =
            begin_multi_window_entry(&hello, &[MultiMonitorCarrierMsg::MuxedReliableStream])
                .unwrap()
                .expect("two-monitor applied topology begins an entry");
        let state = MultiWindowSessionState::Active {
            validated,
            attempt: MultiWindowEnterAttempt::new(plan, Duration::ZERO),
            committed: false,
        };
        assert!(state.is_active());
        assert!(!state.is_committed());
    }

    #[test]
    fn session_state_committed() {
        let hello = base_hello()
            .with_multi_monitor_v1(&server_multi_monitor(
                1,
                MultiMonitorCarrierMsg::MuxedReliableStream,
                vec![applied_monitor(100, 1, 0, 1920, 1080, true)],
            ))
            .expect("attach capability");
        let (validated, plan) =
            begin_multi_window_entry(&hello, &[MultiMonitorCarrierMsg::MuxedReliableStream])
                .unwrap()
                .expect("single-monitor applied topology begins an entry");
        let state = MultiWindowSessionState::Active {
            validated,
            attempt: MultiWindowEnterAttempt::new(plan, Duration::ZERO),
            committed: true,
        };
        assert!(state.is_active());
        assert!(state.is_committed());
    }

    #[test]
    fn teardown_required_when_the_primary_display_disappears() {
        let hello = base_hello()
            .with_multi_monitor_v1(&server_multi_monitor(
                1,
                MultiMonitorCarrierMsg::MuxedReliableStream,
                vec![
                    applied_monitor(200, 2, 1920, 1920, 1080, false),
                    applied_monitor(100, 1, 0, 1920, 1080, true),
                ],
            ))
            .expect("attach capability");
        let (validated, _plan) =
            begin_multi_window_entry(&hello, &[MultiMonitorCarrierMsg::MuxedReliableStream])
                .unwrap()
                .expect("two-monitor applied topology begins an entry");
        // Primary (100) vanished; only the secondary (200) is still live.
        assert!(teardown_required(&validated, &[200], false));
        // Both still live, no close requested: no teardown.
        assert!(!teardown_required(&validated, &[100, 200], false));
        // Both still live, but a secondary window's close was requested.
        assert!(teardown_required(&validated, &[100, 200], true));
    }
}
