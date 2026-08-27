//! Production applied-topology adoption, absolute-pointer coordinate
//! mapping, and teardown detection for a real multi-monitor-v1 session.
//!
//! Everything here is pure and independently testable: it consumes an
//! already-parsed `AppliedMonitorTopologyMsg` (or plain values distilled from
//! one) and never touches AppKit/egui/the network itself.
//! [`validate_applied_topology_for_production`] is the one seam that turns a
//! host's applied topology into exactly the inputs
//! `crate::ui::multi_window_runtime::MultiWindowPlan::build` needs, so the
//! production session controller and its tests share one captured
//! snapshot/validation rather than re-deriving it piecemeal.
//!
//! The validation *rules* are not Deck's: they live in
//! [`arcen_media::validate_applied_topology_for_production`], shared with
//! every other client. What stays here is the macOS half -- resolving a wire
//! `client_display_id` to a live local `CGDirectDisplayID`, and naming the
//! resolved handle for what it is on this platform.
//!
//! # Scope
//!
//! - [`validate_applied_topology_for_production`]: injects this platform's
//!   `CGDirectDisplayID` resolver into the shared check that the negotiated
//!   carrier is one Deck actually implements
//!   (`crate::transport::multi_monitor::deck_supported_carriers`), the
//!   roster size is `1..=4`, and every monitor's local `CGDirectDisplayID`
//!   is resolvable -- producing a [`ValidatedAppliedTopology`] ordered with
//!   the negotiated primary first.
//! - [`map_local_fraction_to_wire_pointer`]: maps a pointer's fraction of one
//!   monitor's own local viewport into the exact `(x, y, server_x, server_y)`
//!   shape the existing wire `MouseMoveMsg` already carries -- normalized and
//!   absolute-pixel against the applied *desktop* rectangle spanning every
//!   monitor -- so no new wire field is needed. Handles negative desktop
//!   origins (a monitor placed left of/above the desktop's own origin).
//! - [`multi_window_teardown_required`]: pure comparison deciding whether a
//!   committed roster's displays are still all live, used alongside an
//!   explicit "a secondary window was closed" check to decide when a full
//!   disconnect/reconnect-required must fire (topology changes are never a
//!   live mutation -- see `crate::reconnect`).
//!
//! # Rotation
//!
//! `AppliedMonitorDescriptorMsg.rotation` is informational metadata, exactly
//! like the `crate::display::metrics::rotation_from_degrees` reading
//! `crate::display::topology::build_requested_topology` carries on each
//! display's shared `DisplayMetrics`: `width_px`/`height_px` there come from
//! `CGDisplayBounds`'s *current* logical size, which already reflects any
//! active display rotation (a portrait-rotated display reports swapped
//! bounds). The wire frame Deck receives and paints is encoded from that same
//! already-rotated compositor output. So the local-fraction-to-desktop-pixel
//! transform below is rotation-*independent* by construction: it is a plain
//! affine map against whatever `width_px`/`height_px` the applied descriptor
//! reports, and this module's own tests exercise it against asymmetric
//! (portrait-shaped) rectangles to prove that holds regardless of the
//! `rotation` value carried alongside.

use arcen_media::{ClientDisplayId, RegionMediaRoster, SessionMonitorId, TopologyGeneration};

use crate::protocol::messages::{AppliedMonitorTopologyMsg, MultiMonitorCarrierMsg};
use crate::ui::multi_window_runtime::cg_display_id_from_client_display_id;

/// The applied-topology contracts this module adopts wholesale from
/// `arcen_media`, re-exported so existing `crate::ui::multi_window_session::…`
/// call sites keep naming them here.
pub use arcen_media::{
    AppliedTopologyParts, AppliedTopologyValidationError, DesktopRect, MonitorDesktopRect,
    NativeDisplayResolver,
};

/// The macOS `NativeDisplayResolver`: the one platform-specific step in
/// applied-topology validation. Every other check lives in
/// [`arcen_media::validate_applied_topology_for_production`], shared with any
/// other client.
fn resolve_cg_display_id(client_display_id: &ClientDisplayId) -> Option<u32> {
    cg_display_id_from_client_display_id(client_display_id.as_str())
}

/// One applied monitor's resolved local identity: the host-negotiated
/// [`SessionMonitorId`] paired with the local `CGDirectDisplayID` its
/// `client_display_id` resolved to, plus its own applied desktop rectangle
/// (needed by [`map_local_fraction_to_wire_pointer`] to place this monitor's
/// local pointer fraction into the shared applied desktop space).
///
/// The macOS projection of [`arcen_media::ResolvedAppliedMonitor`], naming
/// the resolved handle for what it actually is on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAppliedMonitor {
    pub session_monitor_id: SessionMonitorId,
    pub cg_display_id: u32,
    pub rect: MonitorDesktopRect,
}

/// A production-validated applied multi-monitor-v1 topology: exactly the
/// facts `MultiWindowPlan::build` and the per-monitor decoder commit need,
/// with the negotiated carrier and roster size already checked. Ordered with
/// the negotiated primary monitor first, matching `MultiWindowPlan::build`'s
/// "index 0 is primary, stays on the root viewport" contract.
///
/// The macOS projection of [`arcen_media::ValidatedAppliedTopology`]; the
/// validation that produces it is entirely shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAppliedTopology {
    pub generation: TopologyGeneration,
    pub carrier: MultiMonitorCarrierMsg,
    pub monitors: Vec<ResolvedAppliedMonitor>,
    pub media_roster: Box<RegionMediaRoster>,
    pub desktop: DesktopRect,
}

impl From<arcen_media::ValidatedAppliedTopology<u32>> for ValidatedAppliedTopology {
    fn from(shared: arcen_media::ValidatedAppliedTopology<u32>) -> Self {
        Self {
            generation: shared.generation,
            carrier: shared.carrier,
            monitors: shared
                .monitors
                .iter()
                .map(|monitor| ResolvedAppliedMonitor {
                    session_monitor_id: monitor.session_monitor_id,
                    cg_display_id: monitor.native_display_id,
                    rect: monitor.rect,
                })
                .collect(),
            media_roster: shared.media_roster,
            desktop: shared.desktop,
        }
    }
}

impl ValidatedAppliedTopology {
    /// The negotiated session monitor id roster, primary first.
    #[must_use]
    pub fn monitor_ids(&self) -> Vec<SessionMonitorId> {
        self.monitors
            .iter()
            .map(|monitor| monitor.session_monitor_id)
            .collect()
    }

    /// Returns the complete host-authoritative per-monitor media roster.
    #[must_use]
    pub fn media_roster(&self) -> RegionMediaRoster {
        (*self.media_roster).clone()
    }

    /// The resolved local `CGDirectDisplayID` roster, primary first, parallel
    /// to [`Self::monitor_ids`] -- exactly `MultiWindowPlan::build`'s two
    /// input arrays.
    #[must_use]
    pub fn cg_display_ids(&self) -> Vec<u32> {
        self.monitors
            .iter()
            .map(|monitor| monitor.cg_display_id)
            .collect()
    }

    /// The negotiated primary monitor's session id, or `None` for an empty
    /// roster (never actually constructible by
    /// [`validate_applied_topology_for_production`], but avoids an
    /// unwrap/panic here).
    #[must_use]
    pub fn primary_monitor_id(&self) -> Option<SessionMonitorId> {
        self.monitors
            .first()
            .map(|monitor| monitor.session_monitor_id)
    }

    /// Looks up one validated monitor's own applied rectangle by its
    /// negotiated [`SessionMonitorId`], the input
    /// [`map_local_fraction_to_wire_pointer`] needs alongside [`Self::desktop`].
    /// `None` when `monitor_id` is not part of this validated roster.
    #[must_use]
    pub fn monitor_rect(&self, monitor_id: SessionMonitorId) -> Option<MonitorDesktopRect> {
        self.monitors
            .iter()
            .find(|monitor| monitor.session_monitor_id == monitor_id)
            .map(|monitor| monitor.rect)
    }
}

/// Validates `applied` for production multi-window use, resolving every
/// monitor's `client_display_id` against the live local `CGDirectDisplayID`
/// space.
///
/// Thin macOS adoption of
/// [`arcen_media::validate_applied_topology_for_production`]: the carrier,
/// generation, roster size, session monitor id, and media plan checks -- and
/// their exact rejection order -- are the shared crate's, and the only thing
/// injected here is [`resolve_cg_display_id`], this platform's own
/// [`NativeDisplayResolver`].
///
/// Returns monitors ordered with the negotiated primary first, matching
/// `MultiWindowPlan::build`'s contract -- callers must construct the plan
/// directly from this one validated snapshot rather than re-enumerating or
/// re-deriving order themselves.
///
/// # Errors
///
/// See [`AppliedTopologyValidationError`].
pub fn validate_applied_topology_for_production(
    applied: &AppliedMonitorTopologyMsg,
    supported_carriers: &[MultiMonitorCarrierMsg],
) -> Result<ValidatedAppliedTopology, AppliedTopologyValidationError> {
    arcen_media::validate_applied_topology_for_production(
        applied,
        supported_carriers,
        &resolve_cg_display_id,
    )
    .map(ValidatedAppliedTopology::from)
}

/// Maps `local_fraction` (a pointer position expressed as a `0.0..=1.0`
/// fraction of one monitor's own local viewport, exactly as
/// `crate::ui::app::normalized_pointer`'s first step already computes for
/// the legacy/root path) through `monitor_rect`'s applied host-desktop
/// position, then normalizes the result against `desktop`, the full applied
/// desktop rectangle spanning every monitor.
///
/// Returns the exact `(x, y, server_x, server_y)` shape the existing wire
/// `MouseMoveMsg` already carries: `x`/`y` normalized `0.0..=1.0` against the
/// whole applied desktop, `server_x`/`server_y` the absolute host pixel. No
/// new wire field is needed -- multi-monitor input reuses today's
/// whole-remote-frame normalized/absolute pair, just computed against the
/// applied desktop rectangle instead of the legacy single-screen size.
///
/// `local_fraction` is clamped to `0.0..=1.0` on each axis, mirroring
/// `normalized_pointer`'s own clamp (a pointer that is still technically
/// inside the egui rect due to rounding, or a synthetic/test value slightly
/// outside, never produces an out-of-range wire value). Returns `None` when
/// `monitor_rect` or `desktop` has zero width/height -- an invalid rectangle
/// can never produce a meaningful fraction.
#[must_use]
pub fn map_local_fraction_to_wire_pointer(
    local_fraction: (f64, f64),
    monitor_rect: MonitorDesktopRect,
    desktop: DesktopRect,
) -> Option<(f64, f64, i32, i32)> {
    if monitor_rect.width_px == 0
        || monitor_rect.height_px == 0
        || desktop.width_px == 0
        || desktop.height_px == 0
    {
        return None;
    }
    let (fx, fy) = (
        local_fraction.0.clamp(0.0, 1.0),
        local_fraction.1.clamp(0.0, 1.0),
    );

    // Absolute host pixel within the monitor's own rectangle, widened to
    // `i64` throughout so a negative `monitor_rect.x`/`.y` (a monitor placed
    // left of/above the desktop's own origin) can never wrap.
    let local_px_x = (fx * f64::from(monitor_rect.width_px.saturating_sub(1))).round() as i64;
    let local_px_y = (fy * f64::from(monitor_rect.height_px.saturating_sub(1))).round() as i64;
    let desktop_px_x = monitor_rect.x + local_px_x;
    let desktop_px_y = monitor_rect.y + local_px_y;

    // Normalize against the full applied desktop rectangle. Desktop-relative
    // fraction is clamped to `0.0..=1.0`: a monitor rectangle that (validly)
    // touches the desktop's outer edge can round to exactly the far edge,
    // and a defensively-clamped fraction is safer than ever emitting a
    // slightly-out-of-range wire value to a host.
    let dx =
        (desktop_px_x - desktop.x) as f64 / f64::from(desktop.width_px.saturating_sub(1).max(1));
    let dy =
        (desktop_px_y - desktop.y) as f64 / f64::from(desktop.height_px.saturating_sub(1).max(1));
    let (dx, dy) = (dx.clamp(0.0, 1.0), dy.clamp(0.0, 1.0));

    let server_x = i32::try_from(desktop_px_x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)))
        .unwrap_or(if desktop_px_x < 0 { i32::MIN } else { i32::MAX });
    let server_y = i32::try_from(desktop_px_y.clamp(i64::from(i32::MIN), i64::from(i32::MAX)))
        .unwrap_or(if desktop_px_y < 0 { i32::MIN } else { i32::MAX });

    Some((dx, dy, server_x, server_y))
}

/// Pure teardown decision: `true` when any of `committed_cg_display_ids` (the
/// display roster a production multi-window session transactionally
/// committed to) is no longer present in `live_cg_display_ids` (a fresh
/// enumeration, e.g. `crate::ui::multi_window_runtime::live_active_display_ids`),
/// or when `any_secondary_close_requested` is `true` (the user closed one of
/// the additional native windows -- displays can still be physically present,
/// so the roster-membership check alone cannot catch this).
///
/// Extra live displays beyond the committed roster are never a teardown
/// reason on their own -- only the committed roster disappearing, or a
/// window closing, forces the "topology changes require reconnect, never
/// live mutation" signal (see `crate::reconnect`).
#[must_use]
pub fn multi_window_teardown_required(
    committed_cg_display_ids: &[u32],
    live_cg_display_ids: &[u32],
    any_secondary_close_requested: bool,
) -> bool {
    if any_secondary_close_requested {
        return true;
    }
    committed_cg_display_ids
        .iter()
        .any(|id| !live_cg_display_ids.contains(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{BitrateBudgetKbps, VideoCodec};

    use crate::protocol::messages::{
        AppliedMonitorDescriptorMsg, AppliedMonitorMediaPlanMsg, CursorMode, RotationMsg,
    };

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

    fn monitor(
        cg_display_id: u32,
        session_monitor_id: u16,
        x: i32,
        width_px: u32,
        height_px: u32,
        is_primary: bool,
        rotation: RotationMsg,
    ) -> AppliedMonitorDescriptorMsg {
        AppliedMonitorDescriptorMsg {
            client_display_id: ClientDisplayId::new(cg_display_id.to_string()).expect("valid id"),
            session_monitor_id,
            x,
            y: 0,
            width_px,
            height_px,
            refresh_hz: 60,
            rotation,
            is_primary,
            media_plan: media_plan(),
        }
    }

    fn topology(
        carrier: MultiMonitorCarrierMsg,
        generation: u64,
        monitors: Vec<AppliedMonitorDescriptorMsg>,
    ) -> AppliedMonitorTopologyMsg {
        let total_width: u32 = monitors.iter().map(|m| m.width_px).sum();
        let max_height = monitors.iter().map(|m| m.height_px).max().unwrap_or(0);
        AppliedMonitorTopologyMsg::new(
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
        .expect("valid applied topology")
    }

    #[test]
    fn validates_a_two_monitor_muxed_topology_primary_first() {
        let mut secondary = monitor(200, 2, 1920, 1920, 1080, false, RotationMsg::Degrees0);
        secondary.media_plan.stream_epoch = 12;
        let mut primary = monitor(100, 1, 0, 1920, 1080, true, RotationMsg::Degrees0);
        primary.media_plan.stream_epoch = 11;
        primary.media_plan.encoder_backend = "native-nvenc".to_string();
        primary.media_plan.encoder_class = "hardware".to_string();
        primary.media_plan.codec = "h265".to_string();
        let applied = topology(
            MultiMonitorCarrierMsg::MuxedReliableStream,
            7,
            vec![secondary, primary],
        );
        let validated = validate_applied_topology_for_production(
            &applied,
            &[MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("valid");
        assert_eq!(validated.generation.get(), 7);
        assert_eq!(
            validated.carrier,
            MultiMonitorCarrierMsg::MuxedReliableStream
        );
        assert_eq!(validated.cg_display_ids(), vec![100, 200]);
        assert_eq!(
            validated.monitor_ids(),
            vec![
                SessionMonitorId::new(1).unwrap(),
                SessionMonitorId::new(2).unwrap(),
            ]
        );
        assert_eq!(
            validated.primary_monitor_id(),
            Some(SessionMonitorId::new(1).unwrap())
        );
        assert_eq!(
            validated.media_roster.plans()[0].video.codec,
            VideoCodec::H265
        );
        assert_eq!(validated.media_roster.plans()[0].stream_epoch.get(), 11);
        assert_eq!(
            validated.media_roster.plans()[1].video.codec,
            VideoCodec::H264
        );
        assert_eq!(validated.media_roster.plans()[1].stream_epoch.get(), 12);
        assert_eq!(
            validated.media_roster.plans()[0].bitrate_budget,
            BitrateBudgetKbps::new(8_000).expect("in-band wire budget")
        );
        assert_eq!(
            validated.media_roster.plans()[0].applied_bitrate_kbps(),
            8_000
        );
    }

    #[test]
    fn rejects_a_media_plan_bitrate_outside_the_budget_band() {
        // Zero is already rejected one layer earlier by the wire's own
        // `bitrate_kbps` nonzero invariant, so it can never reach this
        // reader. These two values are wire-legal but outside the media
        // domain's band, and are exactly what the value object adds.
        for rejected in [
            BitrateBudgetKbps::MIN_KBPS - 1,
            BitrateBudgetKbps::MAX_KBPS + 1,
        ] {
            let mut applied_monitor = monitor(100, 1, 0, 1920, 1080, true, RotationMsg::Degrees0);
            applied_monitor.media_plan.bitrate_kbps = rejected;
            let applied = topology(
                MultiMonitorCarrierMsg::MuxedReliableStream,
                1,
                vec![applied_monitor],
            );
            let error = validate_applied_topology_for_production(
                &applied,
                &[MultiMonitorCarrierMsg::MuxedReliableStream],
            )
            .expect_err("an out-of-band bitrate budget must fail closed");
            assert!(
                matches!(
                    error,
                    AppliedTopologyValidationError::InvalidMediaPlan(ref detail)
                        if detail.contains("bitrate_kbps")
                ),
                "unexpected error for {rejected} kbps: {error:?}"
            );
        }
    }

    #[test]
    fn accepts_every_bitrate_the_shared_host_policy_can_publish() {
        for (width, height, fps) in [(640, 480, 30), (1920, 1080, 60), (3840, 2160, 60)] {
            let published = BitrateBudgetKbps::nominal_for_geometry(width, height, fps);
            let mut applied_monitor =
                monitor(100, 1, 0, width, height, true, RotationMsg::Degrees0);
            applied_monitor.media_plan.width_px = width;
            applied_monitor.media_plan.height_px = height;
            applied_monitor.media_plan.fps = fps;
            applied_monitor.media_plan.bitrate_kbps = published.get();
            let applied = topology(
                MultiMonitorCarrierMsg::MuxedReliableStream,
                1,
                vec![applied_monitor],
            );
            let validated = validate_applied_topology_for_production(
                &applied,
                &[MultiMonitorCarrierMsg::MuxedReliableStream],
            )
            .expect("every host-publishable budget must be readable");
            assert_eq!(validated.media_roster.plans()[0].bitrate_budget, published);
        }
    }

    #[test]
    fn rejects_unsupported_carrier() {
        let applied = topology(
            MultiMonitorCarrierMsg::PerMonitorReliableStream,
            1,
            vec![monitor(100, 1, 0, 1920, 1080, true, RotationMsg::Degrees0)],
        );
        let error = validate_applied_topology_for_production(
            &applied,
            &[MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect_err("unsupported carrier must fail");
        assert_eq!(
            error,
            AppliedTopologyValidationError::UnsupportedCarrier(
                MultiMonitorCarrierMsg::PerMonitorReliableStream
            )
        );
    }

    #[test]
    fn rejects_unresolvable_client_display_id() {
        let mut applied_monitor = monitor(100, 1, 0, 1920, 1080, true, RotationMsg::Degrees0);
        applied_monitor.client_display_id = ClientDisplayId::new("not-a-number").expect("valid id");
        let applied = topology(
            MultiMonitorCarrierMsg::MuxedReliableStream,
            1,
            vec![applied_monitor],
        );
        let error = validate_applied_topology_for_production(
            &applied,
            &[MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect_err("unresolvable id must fail");
        assert!(matches!(
            error,
            AppliedTopologyValidationError::UnresolvedClientDisplayId(_)
        ));
    }

    #[test]
    fn accepts_four_monitor_roster() {
        let applied = topology(
            MultiMonitorCarrierMsg::MuxedReliableStream,
            1,
            vec![
                monitor(100, 1, 0, 1920, 1080, true, RotationMsg::Degrees0),
                monitor(101, 2, 1920, 1920, 1080, false, RotationMsg::Degrees0),
                monitor(102, 3, 3840, 1920, 1080, false, RotationMsg::Degrees0),
                monitor(103, 4, 5760, 1920, 1080, false, RotationMsg::Degrees0),
            ],
        );
        let validated = validate_applied_topology_for_production(
            &applied,
            &[MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("valid");
        assert_eq!(validated.monitors.len(), 4);
    }

    #[test]
    fn identity_mapping_at_the_desktop_origin() {
        let rect = MonitorDesktopRect {
            x: 0,
            y: 0,
            width_px: 1921,
            height_px: 1081,
        };
        let desktop = DesktopRect {
            x: 0,
            y: 0,
            width_px: 1921,
            height_px: 1081,
        };
        let (nx, ny, sx, sy) =
            map_local_fraction_to_wire_pointer((0.0, 0.0), rect, desktop).expect("valid");
        assert_eq!((nx, ny, sx, sy), (0.0, 0.0, 0, 0));

        let (nx, ny, sx, sy) =
            map_local_fraction_to_wire_pointer((1.0, 1.0), rect, desktop).expect("valid");
        assert_eq!((nx, ny, sx, sy), (1.0, 1.0, 1920, 1080));

        let (nx, ny, sx, sy) =
            map_local_fraction_to_wire_pointer((0.5, 0.5), rect, desktop).expect("valid");
        assert_eq!((nx, ny), (0.5, 0.5));
        assert_eq!((sx, sy), (960, 540));
    }

    #[test]
    fn maps_a_secondary_monitor_offset_within_the_desktop() {
        // Two side-by-side 1920x1080 monitors; the second starts at x=1920.
        let desktop = DesktopRect {
            x: 0,
            y: 0,
            width_px: 3840,
            height_px: 1080,
        };
        let secondary_rect = MonitorDesktopRect {
            x: 1920,
            y: 0,
            width_px: 1920,
            height_px: 1080,
        };
        let (nx, ny, sx, sy) =
            map_local_fraction_to_wire_pointer((0.0, 0.0), secondary_rect, desktop).expect("valid");
        assert_eq!(sx, 1920);
        assert_eq!(sy, 0);
        assert!((nx - 1920.0 / 3839.0).abs() < 1e-9);
        assert_eq!(ny, 0.0);

        let (_, _, sx, sy) =
            map_local_fraction_to_wire_pointer((1.0, 1.0), secondary_rect, desktop).expect("valid");
        assert_eq!(sx, 3839);
        assert_eq!(sy, 1079);
    }

    #[test]
    fn maps_correctly_with_a_negative_desktop_origin_monitor() {
        // A monitor placed to the left of/above the desktop's own origin,
        // e.g. a secondary display arranged up-and-to-the-left of primary.
        let desktop = DesktopRect {
            x: -1920,
            y: -200,
            width_px: 3840,
            height_px: 1280,
        };
        let left_rect = MonitorDesktopRect {
            x: -1920,
            y: -200,
            width_px: 1920,
            height_px: 1080,
        };
        let (nx, ny, sx, sy) =
            map_local_fraction_to_wire_pointer((0.0, 0.0), left_rect, desktop).expect("valid");
        assert_eq!((sx, sy), (-1920, -200));
        assert_eq!((nx, ny), (0.0, 0.0));

        let (_, _, sx, sy) =
            map_local_fraction_to_wire_pointer((1.0, 1.0), left_rect, desktop).expect("valid");
        assert_eq!((sx, sy), (-1, 879));
    }

    #[test]
    fn rotation_value_does_not_change_the_affine_mapping() {
        // A "portrait" (rotated) monitor's width_px/height_px already
        // reflects its current post-rotation logical shape (see module
        // docs); the mapping must be identical regardless of the `rotation`
        // metadata value carried alongside it. This test only exercises the
        // pure pixel-mapping function, which never even receives a rotation
        // parameter -- proving by construction that rotation cannot affect
        // it, matching the design rationale above.
        // Odd dimensions (mirroring `identity_mapping_at_the_desktop_origin`'s
        // own `1921`/`1081` choice), so `0.5`/`0.25` land exactly on a pixel
        // boundary rather than the exact midpoint between two pixels: at an
        // even dimension, a `0.5` fraction quantizes to a rounded pixel that
        // cannot re-normalize back to precisely `0.5`, which is a real
        // pixel-quantization property of the round-trip, not a rotation
        // effect -- this test's own point is only about rotation, so it
        // must not conflate the two.
        let desktop = DesktopRect {
            x: 0,
            y: 0,
            width_px: 1081,
            height_px: 1921,
        };
        let portrait_rect = MonitorDesktopRect {
            x: 0,
            y: 0,
            width_px: 1081,
            height_px: 1921,
        };
        let (nx, ny, sx, sy) =
            map_local_fraction_to_wire_pointer((0.5, 0.25), portrait_rect, desktop).expect("valid");
        assert_eq!(sx, 540);
        assert_eq!(sy, 480);
        assert!((nx - 0.5).abs() < 1e-9);
        assert!((ny - 0.25).abs() < 1e-9);
    }

    #[test]
    fn zero_sized_rect_returns_none() {
        let desktop = DesktopRect {
            x: 0,
            y: 0,
            width_px: 1920,
            height_px: 1080,
        };
        let rect = MonitorDesktopRect {
            x: 0,
            y: 0,
            width_px: 0,
            height_px: 1080,
        };
        assert_eq!(
            map_local_fraction_to_wire_pointer((0.5, 0.5), rect, desktop),
            None
        );
    }

    #[test]
    fn teardown_required_when_a_committed_display_disappears() {
        assert!(!multi_window_teardown_required(
            &[100, 200],
            &[100, 200, 300],
            false
        ));
        assert!(multi_window_teardown_required(&[100, 200], &[100], false));
    }

    #[test]
    fn teardown_required_when_a_secondary_window_closes() {
        assert!(multi_window_teardown_required(
            &[100, 200],
            &[100, 200],
            true
        ));
    }
}
