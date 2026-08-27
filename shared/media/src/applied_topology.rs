//! Pure client-side applied-topology validation for multi-monitor-v1.
//!
//! Every client that opens a multi-monitor session has to turn the host's
//! applied topology sidecar
//! ([`AppliedMonitorTopologyMsg`]) into the exact facts its own window/decoder
//! plan needs: a checked carrier, a `1..=`[`MAX_MULTI_MONITOR_COUNT`] roster
//! ordered primary-first, a nonzero [`TopologyGeneration`], a
//! [`RegionMediaRoster`], the applied desktop rectangle, and -- the only
//! platform-specific part -- each monitor's *local* native display handle.
//!
//! Everything except that last step is pure, so it lives here rather than in
//! any one client. The platform step is injected as a
//! [`NativeDisplayResolver`]: macOS resolves a `client_display_id` to a
//! `CGDirectDisplayID`, and a Windows client resolves the same wire string to
//! whatever its own output identity is, without either re-implementing the
//! checks or their ordering.
//!
//! # Ordering
//!
//! [`validate_applied_topology_for_production`] rejects in exactly this
//! order, and callers depend on it:
//!
//! 1. an unsupported negotiated carrier,
//! 2. a zero topology generation,
//! 3. a roster outside `1..=`[`MAX_MULTI_MONITOR_COUNT`],
//! 4. then, per monitor in primary-first order: an unresolvable
//!    `client_display_id`, an invalid `session_monitor_id`, and each media
//!    plan field (epoch, encoder backend, codec, chroma, bitrate budget,
//!    geometry/fps),
//! 5. finally the roster-level media checks (duplicate monitor ids, roster
//!    size).
//!
//! # Coordinate conventions
//!
//! Applied rectangles are carried through verbatim in *host pixels* with
//! signed origins widened to `i64`, so a monitor placed left of or above the
//! desktop's own origin never wraps. No origin policy is applied here: the
//! host already published one coherent desktop rectangle (see
//! [`crate::OriginPolicy`] for where that choice is made), and re-translating
//! it client-side would desynchronise the client from the host's own applied
//! coordinates.
//!
//! Rotation is likewise informational only. Applied `width_px`/`height_px`
//! are the compositor-oriented on-screen footprint the encoded stream already
//! matches -- [`crate::TransformConvention::AlreadyCompositorOriented`] -- so
//! this module never swaps extents for a 90/270-degree monitor. Doing so
//! would double-apply the transform.

use std::fmt::{self, Debug, Display, Formatter};

use arcen_protocol::messages::{
    AppliedMonitorDescriptorMsg, AppliedMonitorTopologyMsg, MultiMonitorCarrierMsg,
};

use crate::video::EncoderBackend;
use crate::{
    BitrateBudgetKbps, ChromaSubsampling, ClientDisplayId, MAX_MULTI_MONITOR_COUNT,
    MediaContractError, MediaStreamEpoch, RegionMediaPlan, RegionMediaRoster, SessionMonitorId,
    TopologyGeneration, VideoCodec, VideoConfiguration,
};

/// Resolves a wire `client_display_id` to the local, native display handle
/// the client's own windowing system uses.
///
/// This is the single platform seam in applied-topology validation: macOS
/// parses the wire string into a `CGDirectDisplayID`, and another client
/// resolves it to its own native output identity. Returning `None` means the
/// display is not present locally right now, which fails validation with
/// [`AppliedTopologyValidationError::UnresolvedClientDisplayId`].
///
/// A plain `Fn(&ClientDisplayId) -> Option<T>` closure or function pointer
/// already implements this, so a client normally injects one directly.
pub trait NativeDisplayResolver {
    /// The client's own native display handle (a `CGDirectDisplayID`, an
    /// adapter/output pair, and so on).
    type NativeDisplayId: Copy + Eq + Debug;

    /// Resolves one applied monitor's wire display id, or `None` when it does
    /// not name a display that is present locally right now.
    fn resolve(&self, client_display_id: &ClientDisplayId) -> Option<Self::NativeDisplayId>;
}

impl<F, T> NativeDisplayResolver for F
where
    F: Fn(&ClientDisplayId) -> Option<T>,
    T: Copy + Eq + Debug,
{
    type NativeDisplayId = T;

    fn resolve(&self, client_display_id: &ClientDisplayId) -> Option<T> {
        self(client_display_id)
    }
}

/// One monitor's applied rectangle in the shared host desktop pixel space
/// (`AppliedMonitorDescriptorMsg.x/y/width_px/height_px`). `x`/`y` are widened
/// to `i64` so a monitor placed left of/above the desktop's own origin (a
/// negative applied coordinate) never wraps during pixel-mapping arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorDesktopRect {
    /// Applied host-desktop horizontal origin in pixels, signed.
    pub x: i64,
    /// Applied host-desktop vertical origin in pixels, signed.
    pub y: i64,
    /// Applied width in host pixels, already compositor-oriented.
    pub width_px: u32,
    /// Applied height in host pixels, already compositor-oriented.
    pub height_px: u32,
}

/// The full applied desktop rectangle spanning every monitor
/// (`AppliedMonitorTopologyMsg.desktop_*`), widened the same way as
/// [`MonitorDesktopRect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopRect {
    /// Applied host-desktop horizontal origin in pixels, signed.
    pub x: i64,
    /// Applied host-desktop vertical origin in pixels, signed.
    pub y: i64,
    /// Applied desktop width in host pixels.
    pub width_px: u32,
    /// Applied desktop height in host pixels.
    pub height_px: u32,
}

/// One applied monitor's resolved local identity: the host-negotiated
/// [`SessionMonitorId`] paired with the native display handle its
/// `client_display_id` resolved to, plus its own applied desktop rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAppliedMonitor<D> {
    /// The host-negotiated nonzero session monitor id.
    pub session_monitor_id: SessionMonitorId,
    /// The local native display handle this monitor resolved to.
    pub native_display_id: D,
    /// This monitor's applied host-pixel rectangle.
    pub rect: MonitorDesktopRect,
}

/// A production-validated applied multi-monitor-v1 topology: exactly the
/// facts a client's window plan and per-monitor decoder commit need, with the
/// negotiated carrier and roster size already checked. Ordered with the
/// negotiated primary monitor first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAppliedTopology<D> {
    /// The committed, nonzero topology generation.
    pub generation: TopologyGeneration,
    /// The negotiated multi-monitor carrier.
    pub carrier: MultiMonitorCarrierMsg,
    /// The resolved monitor roster, negotiated primary first.
    pub monitors: Vec<ResolvedAppliedMonitor<D>>,
    /// The complete host-authoritative per-monitor media roster.
    pub media_roster: Box<RegionMediaRoster>,
    /// The applied desktop rectangle spanning every monitor.
    pub desktop: DesktopRect,
}

impl<D: Copy> ValidatedAppliedTopology<D> {
    /// The negotiated session monitor id roster, primary first.
    #[must_use]
    pub fn monitor_ids(&self) -> Vec<SessionMonitorId> {
        self.monitors
            .iter()
            .map(|monitor| monitor.session_monitor_id)
            .collect()
    }

    /// The resolved native display handle roster, primary first, parallel to
    /// [`Self::monitor_ids`].
    #[must_use]
    pub fn native_display_ids(&self) -> Vec<D> {
        self.monitors
            .iter()
            .map(|monitor| monitor.native_display_id)
            .collect()
    }

    /// Returns the complete host-authoritative per-monitor media roster.
    #[must_use]
    pub fn media_roster(&self) -> RegionMediaRoster {
        (*self.media_roster).clone()
    }

    /// The negotiated primary monitor's session id, or `None` for an empty
    /// roster (never actually constructible by
    /// [`validate_applied_topology_for_production`], but avoids a panic here).
    #[must_use]
    pub fn primary_monitor_id(&self) -> Option<SessionMonitorId> {
        self.monitors
            .first()
            .map(|monitor| monitor.session_monitor_id)
    }

    /// Looks up one validated monitor's own applied rectangle by its
    /// negotiated [`SessionMonitorId`]. `None` when `monitor_id` is not part
    /// of this validated roster.
    #[must_use]
    pub fn monitor_rect(&self, monitor_id: SessionMonitorId) -> Option<MonitorDesktopRect> {
        self.monitors
            .iter()
            .find(|monitor| monitor.session_monitor_id == monitor_id)
            .map(|monitor| monitor.rect)
    }
}

/// Failure validating an applied topology for production multi-window use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedTopologyValidationError {
    /// The host negotiated a carrier this client does not implement
    /// production routing for.
    UnsupportedCarrier(MultiMonitorCarrierMsg),
    /// The applied roster was empty or exceeded [`MAX_MULTI_MONITOR_COUNT`].
    InvalidMonitorCount(usize),
    /// One monitor's `client_display_id` did not resolve to a local native
    /// display (see [`NativeDisplayResolver`]).
    UnresolvedClientDisplayId(String),
    /// One monitor's wire `session_monitor_id` was not a valid nonzero
    /// [`SessionMonitorId`].
    InvalidSessionMonitorId(MediaContractError),
    /// One monitor's advertised media plan was unknown or invalid.
    InvalidMediaPlan(String),
    /// The applied topology generation was zero -- impossible for a
    /// genuinely host-assigned generation, but never trusted blindly.
    InvalidGeneration,
}

impl Display for AppliedTopologyValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedCarrier(carrier) => write!(
                formatter,
                "host negotiated carrier {carrier} but this client only \
                 implements production multi-monitor routing for {}",
                MultiMonitorCarrierMsg::MuxedReliableStream,
            ),
            Self::InvalidMonitorCount(count) => write!(
                formatter,
                "applied topology has {count} monitors, outside the 1..={MAX_MULTI_MONITOR_COUNT} \
                 range"
            ),
            Self::UnresolvedClientDisplayId(id) => write!(
                formatter,
                "applied monitor client_display_id {id:?} did not resolve to a local display"
            ),
            Self::InvalidSessionMonitorId(error) => {
                write!(formatter, "applied monitor session id is invalid: {error}")
            }
            Self::InvalidMediaPlan(error) => write!(formatter, "invalid media roster: {error}"),
            Self::InvalidGeneration => formatter.write_str("applied topology generation is zero"),
        }
    }
}

impl std::error::Error for AppliedTopologyValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSessionMonitorId(error) => Some(error),
            _ => None,
        }
    }
}

/// The applied topology facts validation consumes, separated from the wire
/// message so the checks are exercisable against rosters a validated
/// [`AppliedMonitorTopologyMsg`] can never carry (an empty roster, five
/// monitors, a duplicated monitor, a primary that is missing from its own
/// roster).
///
/// Build one from a real message with [`Self::from_message`].
#[derive(Debug, Clone, Copy)]
pub struct AppliedTopologyParts<'a> {
    /// The host-assigned topology generation, still unchecked.
    pub generation: u64,
    /// The negotiated carrier, still unchecked.
    pub carrier: MultiMonitorCarrierMsg,
    /// The applied desktop rectangle spanning every monitor.
    pub desktop: DesktopRect,
    /// The applied monitor roster in wire order.
    pub monitors: &'a [AppliedMonitorDescriptorMsg],
    /// The negotiated primary monitor, which validation moves to index `0`.
    pub primary: &'a AppliedMonitorDescriptorMsg,
}

impl<'a> AppliedTopologyParts<'a> {
    /// Distils an already-parsed applied topology sidecar into its validation
    /// inputs, preserving every value verbatim.
    #[must_use]
    pub fn from_message(applied: &'a AppliedMonitorTopologyMsg) -> Self {
        Self {
            generation: applied.topology_generation(),
            carrier: applied.selected_carrier(),
            desktop: DesktopRect {
                x: i64::from(applied.desktop_x()),
                y: i64::from(applied.desktop_y()),
                width_px: applied.desktop_width_px(),
                height_px: applied.desktop_height_px(),
            },
            monitors: applied.monitors(),
            primary: applied.primary(),
        }
    }
}

/// Validates `applied` for production multi-window use: the negotiated
/// carrier must be one this client actually implements, the generation must
/// be nonzero, the roster must be `1..=`[`MAX_MULTI_MONITOR_COUNT`], every
/// monitor's `session_monitor_id` must be a valid nonzero
/// [`SessionMonitorId`], every monitor's media plan must be a readable
/// [`RegionMediaPlan`], and every monitor's `client_display_id` must resolve
/// through `resolver` to a local native display right now.
///
/// Returns monitors ordered with the negotiated primary first -- callers must
/// construct their window plan directly from this one validated snapshot
/// rather than re-enumerating or re-deriving order themselves.
///
/// # Errors
///
/// See [`AppliedTopologyValidationError`] and this module's ordering contract.
pub fn validate_applied_topology_for_production<R: NativeDisplayResolver>(
    applied: &AppliedMonitorTopologyMsg,
    supported_carriers: &[MultiMonitorCarrierMsg],
    resolver: &R,
) -> Result<ValidatedAppliedTopology<R::NativeDisplayId>, AppliedTopologyValidationError> {
    validate_applied_topology_parts(
        AppliedTopologyParts::from_message(applied),
        supported_carriers,
        resolver,
    )
}

/// [`validate_applied_topology_for_production`] against already-distilled
/// [`AppliedTopologyParts`], for callers (and tests) that do not hold a
/// wire-validated [`AppliedMonitorTopologyMsg`].
///
/// # Errors
///
/// See [`AppliedTopologyValidationError`] and this module's ordering contract.
pub fn validate_applied_topology_parts<R: NativeDisplayResolver>(
    parts: AppliedTopologyParts<'_>,
    supported_carriers: &[MultiMonitorCarrierMsg],
    resolver: &R,
) -> Result<ValidatedAppliedTopology<R::NativeDisplayId>, AppliedTopologyValidationError> {
    if !supported_carriers.contains(&parts.carrier) {
        return Err(AppliedTopologyValidationError::UnsupportedCarrier(
            parts.carrier,
        ));
    }
    let generation = TopologyGeneration::new(parts.generation)
        .map_err(|_| AppliedTopologyValidationError::InvalidGeneration)?;

    let count = parts.monitors.len();
    if count == 0 || count > MAX_MULTI_MONITOR_COUNT {
        return Err(AppliedTopologyValidationError::InvalidMonitorCount(count));
    }

    // Order with the negotiated primary first -- the "index 0 is primary,
    // stays on the root viewport" contract every client window plan builds on.
    let mut ordered = Vec::with_capacity(count);
    ordered.push(parts.primary);
    for monitor in parts.monitors {
        if monitor.session_monitor_id != parts.primary.session_monitor_id {
            ordered.push(monitor);
        }
    }

    let mut monitors = Vec::with_capacity(count);
    let mut media_plans = Vec::with_capacity(count);
    for monitor in ordered {
        let native_display_id = resolver
            .resolve(&monitor.client_display_id)
            .ok_or_else(|| {
                AppliedTopologyValidationError::UnresolvedClientDisplayId(
                    monitor.client_display_id.as_str().to_string(),
                )
            })?;
        let session_monitor_id = SessionMonitorId::new(monitor.session_monitor_id)
            .map_err(AppliedTopologyValidationError::InvalidSessionMonitorId)?;
        let media_plan = read_applied_media_plan(monitor, session_monitor_id)?;
        monitors.push(ResolvedAppliedMonitor {
            session_monitor_id,
            native_display_id,
            rect: MonitorDesktopRect {
                x: i64::from(monitor.x),
                y: i64::from(monitor.y),
                width_px: monitor.width_px,
                height_px: monitor.height_px,
            },
        });
        media_plans.push(media_plan);
    }
    let media_roster = RegionMediaRoster::new(media_plans)
        .map_err(|error| AppliedTopologyValidationError::InvalidMediaPlan(error.to_string()))?;

    Ok(ValidatedAppliedTopology {
        generation,
        carrier: parts.carrier,
        monitors,
        media_roster: Box::new(media_roster),
        desktop: parts.desktop,
    })
}

/// Reads one applied wire media plan into its validated
/// [`RegionMediaPlan`], preserving the exact per-field rejection order.
fn read_applied_media_plan(
    monitor: &AppliedMonitorDescriptorMsg,
    session_monitor_id: SessionMonitorId,
) -> Result<RegionMediaPlan, AppliedTopologyValidationError> {
    let stream_epoch = MediaStreamEpoch::new(monitor.media_plan.stream_epoch)
        .map_err(|error| AppliedTopologyValidationError::InvalidMediaPlan(error.to_string()))?;
    let backend =
        EncoderBackend::from_token(&monitor.media_plan.encoder_backend).ok_or_else(|| {
            AppliedTopologyValidationError::InvalidMediaPlan(format!(
                "monitor {} advertised unknown encoder backend {:?}",
                monitor.session_monitor_id, monitor.media_plan.encoder_backend
            ))
        })?;
    let codec = VideoCodec::from_token(&monitor.media_plan.codec).ok_or_else(|| {
        AppliedTopologyValidationError::InvalidMediaPlan(format!(
            "monitor {} advertised unknown codec {:?}",
            monitor.session_monitor_id, monitor.media_plan.codec
        ))
    })?;
    let chroma = ChromaSubsampling::from_token(&monitor.media_plan.chroma).ok_or_else(|| {
        AppliedTopologyValidationError::InvalidMediaPlan(format!(
            "monitor {} advertised unknown chroma {:?}",
            monitor.session_monitor_id, monitor.media_plan.chroma
        ))
    })?;
    let bitrate_budget =
        BitrateBudgetKbps::new(monitor.media_plan.bitrate_kbps).map_err(|error| {
            AppliedTopologyValidationError::InvalidMediaPlan(format!(
                "monitor {} advertised bitrate_kbps {}: {error}",
                monitor.session_monitor_id, monitor.media_plan.bitrate_kbps
            ))
        })?;
    RegionMediaPlan::new(
        session_monitor_id,
        stream_epoch,
        backend,
        VideoConfiguration {
            codec,
            chroma,
            // Region/multi-monitor plans stay on the legacy 8-bit limited
            // contract until the applied-topology wire schema carries depth
            // and range; the direct single-monitor path negotiates them.
            ..VideoConfiguration::legacy_h264()
        },
        monitor.media_plan.width_px,
        monitor.media_plan.height_px,
        monitor.media_plan.fps,
        bitrate_budget,
    )
    .map_err(|error| AppliedTopologyValidationError::InvalidMediaPlan(error.to_string()))
}

/// Rounds a signed fractional offset across `span` pixels to whole pixels,
/// saturating rather than wrapping.
///
/// The fraction is deliberately unclamped by callers -- a value outside
/// `0.0..=1.0` is the crossing signal itself -- so it is bounded here before
/// the conversion. Anything beyond a few desktops away is nonsense from a
/// pointer position and saturates instead of producing a wrapped pixel.
fn rounded_offset_pixels(fraction: f64, span: u32) -> i64 {
    const LIMIT: f64 = 1_000.0;
    let scaled = (fraction.clamp(-LIMIT, LIMIT) * f64::from(span)).round();
    // `scaled` is now bounded by 1000 * u32::MAX, which is far inside i64.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "explicitly clamped to +/-1000 * u32::MAX above, which i64 represents exactly"
    )]
    let pixels = scaled as i64;
    pixels
}

/// Resolves a pointer position that has left its origin monitor into the
/// monitor that actually contains it, in applied-desktop pixel space.
///
/// # Why this exists
///
/// A client that presents each monitor in its own native window derives the
/// pointer position from the window the event was delivered to. While a
/// button is held, the platform keeps delivering to the window the press
/// started in even after the cursor has physically left it, and the local
/// fraction then falls outside `0.0..=1.0`. Clamping it -- the obvious
/// reading of "keep the pointer inside this monitor" -- pins the remote
/// cursor to that monitor's edge, so a window being dragged towards a
/// neighbouring screen stops dead at the boundary and drops there.
///
/// Measured on 2026-08-11: a drag that pressed at applied-desktop
/// `(1089, 86)` released at `(0, 1119)` -- exactly the primary monitor's
/// left edge -- and never entered the secondary monitor occupying negative
/// x. This routine converts that same unclamped fraction into the secondary
/// monitor and its own local fraction instead.
///
/// `unclamped_fraction` is deliberately *not* clamped by the caller: values
/// outside `0.0..=1.0` are the entire signal that a crossing happened.
/// Returns the origin monitor unchanged for a fraction still inside it, and
/// `None` when the point lands in no admitted monitor at all (a desktop with
/// an L-shaped gap, or a fraction so far out it leaves the whole desktop) --
/// callers must then keep their existing clamped behaviour rather than
/// invent a position.
#[must_use]
pub fn resolve_pointer_crossing(
    monitors: &[(SessionMonitorId, MonitorDesktopRect)],
    origin: SessionMonitorId,
    unclamped_fraction: (f64, f64),
) -> Option<(SessionMonitorId, (f64, f64))> {
    if !unclamped_fraction.0.is_finite() || !unclamped_fraction.1.is_finite() {
        return None;
    }
    let origin_rect = monitors
        .iter()
        .find(|(monitor_id, _)| *monitor_id == origin)
        .map(|(_, rect)| *rect)?;
    if origin_rect.width_px == 0 || origin_rect.height_px == 0 {
        return None;
    }

    // Applied-desktop pixel the unclamped fraction points at, measured
    // against `width - 1` to match the inclusive-edge convention the
    // client's own local-fraction-to-wire mapping uses. The two must agree
    // or a crossing would shift the cursor by a pixel at every boundary.
    let desktop_x = origin_rect.x
        + rounded_offset_pixels(unclamped_fraction.0, origin_rect.width_px.saturating_sub(1));
    let desktop_y = origin_rect.y
        + rounded_offset_pixels(
            unclamped_fraction.1,
            origin_rect.height_px.saturating_sub(1),
        );

    let (containing_id, rect) = monitors.iter().find(|(_, rect)| {
        rect.width_px > 0
            && rect.height_px > 0
            && desktop_x >= rect.x
            && desktop_x < rect.x + i64::from(rect.width_px)
            && desktop_y >= rect.y
            && desktop_y < rect.y + i64::from(rect.height_px)
    })?;

    // Both differences are inside the containing monitor by construction, so
    // each fits `u32` exactly and converts to `f64` without precision loss.
    let local_x = u32::try_from(desktop_x - rect.x).ok()?;
    let local_y = u32::try_from(desktop_y - rect.y).ok()?;
    let span_x = f64::from(rect.width_px.saturating_sub(1)).max(1.0);
    let span_y = f64::from(rect.height_px.saturating_sub(1)).max(1.0);
    let fraction = (
        (f64::from(local_x) / span_x).clamp(0.0, 1.0),
        (f64::from(local_y) / span_y).clamp(0.0, 1.0),
    );
    Some((*containing_id, fraction))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Rotation, TransformConvention};
    use arcen_protocol::messages::{AppliedMonitorMediaPlanMsg, CursorMode, RotationMsg};

    /// A resolver that parses the wire id the way a macOS client does, which
    /// is also the simplest total resolver for tests.
    fn numeric_resolver(id: &ClientDisplayId) -> Option<u32> {
        id.as_str().parse::<u32>().ok()
    }

    /// A resolver that never resolves anything -- the "no local display"
    /// failure every client must fail closed on.
    fn failing_resolver(_id: &ClientDisplayId) -> Option<u32> {
        None
    }

    fn muxed() -> Vec<MultiMonitorCarrierMsg> {
        vec![MultiMonitorCarrierMsg::MuxedReliableStream]
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

    fn monitor(
        display_id: u32,
        session_monitor_id: u16,
        x: i32,
        y: i32,
        width_px: u32,
        height_px: u32,
        is_primary: bool,
    ) -> AppliedMonitorDescriptorMsg {
        AppliedMonitorDescriptorMsg {
            client_display_id: ClientDisplayId::new(display_id.to_string()).expect("valid id"),
            session_monitor_id,
            x,
            y,
            width_px,
            height_px,
            refresh_hz: 60,
            rotation: RotationMsg::Degrees0,
            is_primary,
            media_plan: media_plan(),
        }
    }

    fn parts<'a>(
        monitors: &'a [AppliedMonitorDescriptorMsg],
        primary: &'a AppliedMonitorDescriptorMsg,
        generation: u64,
    ) -> AppliedTopologyParts<'a> {
        AppliedTopologyParts {
            generation,
            carrier: MultiMonitorCarrierMsg::MuxedReliableStream,
            desktop: DesktopRect {
                x: 0,
                y: 0,
                width_px: 3840,
                height_px: 1080,
            },
            monitors,
            primary,
        }
    }

    #[test]
    fn orders_the_negotiated_primary_first_and_resolves_every_display() {
        let monitors = vec![
            monitor(200, 2, 1920, 0, 1920, 1080, false),
            monitor(100, 1, 0, 0, 1920, 1080, true),
        ];
        let primary = monitors[1].clone();
        let validated = validate_applied_topology_parts(
            parts(&monitors, &primary, 7),
            &muxed(),
            &numeric_resolver,
        )
        .expect("valid applied topology");
        assert_eq!(validated.generation.get(), 7);
        assert_eq!(validated.native_display_ids(), vec![100, 200]);
        assert_eq!(
            validated.monitor_ids(),
            vec![
                SessionMonitorId::new(1).expect("nonzero"),
                SessionMonitorId::new(2).expect("nonzero")
            ]
        );
        assert_eq!(
            validated.primary_monitor_id(),
            Some(SessionMonitorId::new(1).expect("nonzero"))
        );
        assert_eq!(validated.media_roster().plans().len(), 2);
    }

    #[test]
    fn an_unsupported_carrier_is_rejected_before_every_other_check() {
        // A zero generation, an empty roster, and an unresolvable display are
        // all present: the carrier must still be the reported failure.
        let monitors: Vec<AppliedMonitorDescriptorMsg> = Vec::new();
        let primary = monitor(0, 1, 0, 0, 1920, 1080, true);
        let mut input = parts(&monitors, &primary, 0);
        input.carrier = MultiMonitorCarrierMsg::PerMonitorReliableStream;
        assert_eq!(
            validate_applied_topology_parts(input, &muxed(), &failing_resolver)
                .expect_err("unsupported carrier"),
            AppliedTopologyValidationError::UnsupportedCarrier(
                MultiMonitorCarrierMsg::PerMonitorReliableStream
            )
        );
    }

    #[test]
    fn a_zero_generation_is_rejected_before_the_roster_size() {
        let monitors: Vec<AppliedMonitorDescriptorMsg> = Vec::new();
        let primary = monitor(100, 1, 0, 0, 1920, 1080, true);
        assert_eq!(
            validate_applied_topology_parts(
                parts(&monitors, &primary, 0),
                &muxed(),
                &numeric_resolver
            )
            .expect_err("zero generation"),
            AppliedTopologyValidationError::InvalidGeneration
        );
    }

    #[test]
    fn a_missing_or_extra_applied_display_roster_is_rejected_on_count() {
        let none: Vec<AppliedMonitorDescriptorMsg> = Vec::new();
        let primary = monitor(100, 1, 0, 0, 1920, 1080, true);
        assert_eq!(
            validate_applied_topology_parts(parts(&none, &primary, 1), &muxed(), &numeric_resolver)
                .expect_err("an empty applied roster"),
            AppliedTopologyValidationError::InvalidMonitorCount(0)
        );
        let five: Vec<AppliedMonitorDescriptorMsg> = (1..=5)
            .map(|id| {
                monitor(
                    100 + u32::from(id),
                    id,
                    i32::from(id) * 1920,
                    0,
                    1920,
                    1080,
                    id == 1,
                )
            })
            .collect();
        let five_primary = five[0].clone();
        assert_eq!(
            validate_applied_topology_parts(
                parts(&five, &five_primary, 1),
                &muxed(),
                &numeric_resolver
            )
            .expect_err("five applied monitors"),
            AppliedTopologyValidationError::InvalidMonitorCount(5)
        );
    }

    #[test]
    fn a_duplicated_applied_monitor_id_is_rejected_by_the_media_roster() {
        let monitors = vec![
            monitor(100, 1, 0, 0, 1920, 1080, true),
            monitor(200, 2, 1920, 0, 1920, 1080, false),
            monitor(300, 2, 3840, 0, 1920, 1080, false),
        ];
        let primary = monitors[0].clone();
        let error = validate_applied_topology_parts(
            parts(&monitors, &primary, 1),
            &muxed(),
            &numeric_resolver,
        )
        .expect_err("a duplicated session monitor id");
        assert!(
            matches!(
                error,
                AppliedTopologyValidationError::InvalidMediaPlan(ref detail)
                    if detail.contains("duplicate")
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn a_roster_entry_repeating_the_primarys_id_collapses_into_the_primary() {
        // Primary-first ordering skips every roster entry that repeats the
        // negotiated primary's own id, so such an entry is dropped rather
        // than rejected. The wire type already rejects duplicate session
        // monitor ids one layer earlier, so this is unreachable from a real
        // `AppliedMonitorTopologyMsg`; it is asserted here so the ordering
        // rule's exact consequence stays visible.
        let monitors = vec![
            monitor(100, 1, 0, 0, 1920, 1080, true),
            monitor(200, 1, 1920, 0, 1920, 1080, false),
        ];
        let primary = monitors[0].clone();
        let validated = validate_applied_topology_parts(
            parts(&monitors, &primary, 1),
            &muxed(),
            &numeric_resolver,
        )
        .expect("the repeated primary entry is skipped");
        assert_eq!(validated.native_display_ids(), vec![100]);
    }

    #[test]
    fn a_resolver_failure_fails_closed_and_names_the_wire_id() {
        let monitors = vec![monitor(100, 1, 0, 0, 1920, 1080, true)];
        let primary = monitors[0].clone();
        assert_eq!(
            validate_applied_topology_parts(
                parts(&monitors, &primary, 1),
                &muxed(),
                &failing_resolver
            )
            .expect_err("no local display"),
            AppliedTopologyValidationError::UnresolvedClientDisplayId("100".to_string())
        );
    }

    #[test]
    fn a_resolver_failure_is_reported_before_an_invalid_session_monitor_id() {
        let mut unresolvable = monitor(100, 0, 0, 0, 1920, 1080, true);
        unresolvable.client_display_id = ClientDisplayId::new("not-a-number").expect("valid id");
        let monitors = vec![unresolvable];
        let primary = monitors[0].clone();
        assert_eq!(
            validate_applied_topology_parts(
                parts(&monitors, &primary, 1),
                &muxed(),
                &numeric_resolver
            )
            .expect_err("unresolvable display id"),
            AppliedTopologyValidationError::UnresolvedClientDisplayId("not-a-number".to_string())
        );
    }

    #[test]
    fn a_zero_session_monitor_id_is_rejected_before_the_media_plan() {
        let mut invalid = monitor(100, 0, 0, 0, 1920, 1080, true);
        invalid.media_plan.codec = "not-a-codec".to_string();
        let monitors = vec![invalid];
        let primary = monitors[0].clone();
        let error = validate_applied_topology_parts(
            parts(&monitors, &primary, 1),
            &muxed(),
            &numeric_resolver,
        )
        .expect_err("zero session monitor id");
        assert!(
            matches!(
                error,
                AppliedTopologyValidationError::InvalidSessionMonitorId(_)
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn an_unknown_media_plan_token_is_rejected_per_field_in_order() {
        for (mutate, expected) in [
            (
                Box::new(|plan: &mut AppliedMonitorMediaPlanMsg| plan.stream_epoch = 0)
                    as Box<dyn Fn(&mut AppliedMonitorMediaPlanMsg)>,
                "epoch",
            ),
            (
                Box::new(|plan: &mut AppliedMonitorMediaPlanMsg| {
                    plan.encoder_backend = "not-a-backend".to_string();
                }),
                "encoder backend",
            ),
            (
                Box::new(|plan: &mut AppliedMonitorMediaPlanMsg| {
                    plan.codec = "not-a-codec".to_string();
                }),
                "codec",
            ),
            (
                Box::new(|plan: &mut AppliedMonitorMediaPlanMsg| {
                    plan.chroma = "not-a-chroma".to_string();
                }),
                "chroma",
            ),
            (
                Box::new(|plan: &mut AppliedMonitorMediaPlanMsg| {
                    plan.bitrate_kbps = BitrateBudgetKbps::MAX_KBPS + 1;
                }),
                "bitrate_kbps",
            ),
        ] {
            let mut invalid = monitor(100, 1, 0, 0, 1920, 1080, true);
            mutate(&mut invalid.media_plan);
            let monitors = vec![invalid];
            let primary = monitors[0].clone();
            let error = validate_applied_topology_parts(
                parts(&monitors, &primary, 1),
                &muxed(),
                &numeric_resolver,
            )
            .expect_err("an invalid media plan field");
            assert!(
                matches!(
                    error,
                    AppliedTopologyValidationError::InvalidMediaPlan(ref detail)
                        if detail.contains(expected)
                ),
                "unexpected error for {expected}: {error:?}"
            );
        }
    }

    #[test]
    fn applied_origins_are_carried_through_signed_and_untranslated() {
        // A monitor left of and above the desktop's own origin: the applied
        // coordinate convention is preserved verbatim (never re-translated to
        // a non-negative origin client-side), widened to `i64`.
        let monitors = vec![
            monitor(100, 1, 0, 0, 1920, 1080, true),
            monitor(200, 2, -1920, -120, 1920, 1080, false),
        ];
        let primary = monitors[0].clone();
        let mut input = parts(&monitors, &primary, 1);
        input.desktop = DesktopRect {
            x: -1920,
            y: -120,
            width_px: 3840,
            height_px: 1200,
        };
        let validated =
            validate_applied_topology_parts(input, &muxed(), &numeric_resolver).expect("valid");
        assert_eq!(
            validated.desktop,
            DesktopRect {
                x: -1920,
                y: -120,
                width_px: 3840,
                height_px: 1200
            }
        );
        assert_eq!(
            validated
                .monitor_rect(SessionMonitorId::new(2).expect("nonzero"))
                .expect("monitor 2 is in the roster"),
            MonitorDesktopRect {
                x: -1920,
                y: -120,
                width_px: 1920,
                height_px: 1080
            }
        );
    }

    #[test]
    fn a_rotated_monitors_applied_extent_is_never_transformed_again() {
        // The applied rectangle is already compositor-oriented, so a
        // portrait-shaped rectangle carrying 90-degree rotation metadata must
        // come through with its extents untouched.
        let mut rotated = monitor(200, 2, 1920, 0, 1080, 1920, false);
        rotated.rotation = RotationMsg::Degrees90;
        let monitors = vec![monitor(100, 1, 0, 0, 1920, 1080, true), rotated];
        let primary = monitors[0].clone();
        let validated = validate_applied_topology_parts(
            parts(&monitors, &primary, 1),
            &muxed(),
            &numeric_resolver,
        )
        .expect("valid");
        let rect = validated
            .monitor_rect(SessionMonitorId::new(2).expect("nonzero"))
            .expect("monitor 2 is in the roster");
        assert_eq!(rect.width_px, 1080);
        assert_eq!(rect.height_px, 1920);
        // This is exactly the `AlreadyCompositorOriented` convention: the
        // stream extent already absorbed the transform.
        assert_eq!(
            TransformConvention::AlreadyCompositorOriented.desktop_footprint(
                1080,
                1920,
                Rotation::Degrees90
            ),
            (1080, 1920)
        );
        assert_eq!(
            TransformConvention::NativeNeedsTransform.desktop_footprint(
                1080,
                1920,
                Rotation::Degrees90
            ),
            (1920, 1080)
        );
    }

    #[test]
    fn a_closure_resolver_may_return_any_native_display_handle() {
        let monitors = vec![monitor(100, 1, 0, 0, 1920, 1080, true)];
        let primary = monitors[0].clone();
        let resolver = |id: &ClientDisplayId| Some((7_u16, id.as_str().len()));
        let validated =
            validate_applied_topology_parts(parts(&monitors, &primary, 1), &muxed(), &resolver)
                .expect("valid");
        assert_eq!(validated.native_display_ids(), vec![(7, 3)]);
    }

    /// The exact pier-windows.example.internal geometry from the 2026-08-11 field test:
    /// a 3008x1692 primary at the origin and a 1800x1130 secondary occupying
    /// negative x, which is the direction the failed window drag went.
    fn crossing_roster() -> Vec<(SessionMonitorId, MonitorDesktopRect)> {
        vec![
            (
                SessionMonitorId::new(1).expect("nonzero"),
                MonitorDesktopRect {
                    x: 0,
                    y: 0,
                    width_px: 3008,
                    height_px: 1692,
                },
            ),
            (
                SessionMonitorId::new(2).expect("nonzero"),
                MonitorDesktopRect {
                    x: -1800,
                    y: 0,
                    width_px: 1800,
                    height_px: 1130,
                },
            ),
        ]
    }

    #[test]
    fn a_drag_leaving_the_primary_resolves_into_the_negative_x_secondary() {
        let monitors = crossing_roster();
        let primary = SessionMonitorId::new(1).expect("nonzero");
        let secondary = SessionMonitorId::new(2).expect("nonzero");

        // Half a primary-width to the left of the primary's own origin lands
        // squarely inside the secondary. Before this the same input clamped
        // to fraction 0.0 and pinned the cursor at desktop x = 0, which is
        // exactly where the measured drag released.
        let (monitor_id, fraction) =
            resolve_pointer_crossing(&monitors, primary, (-0.5, 0.5)).expect("crossing resolves");
        assert_eq!(monitor_id, secondary);
        assert!(
            fraction.0 > 0.0 && fraction.0 < 1.0,
            "the crossed position must land inside the secondary, got {fraction:?}",
        );
    }

    #[test]
    fn a_pointer_still_inside_its_own_monitor_stays_there() {
        let monitors = crossing_roster();
        let primary = SessionMonitorId::new(1).expect("nonzero");
        let (monitor_id, fraction) =
            resolve_pointer_crossing(&monitors, primary, (0.25, 0.75)).expect("resolves");
        assert_eq!(monitor_id, primary);
        assert!((fraction.0 - 0.25).abs() < 0.01);
        assert!((fraction.1 - 0.75).abs() < 0.01);
    }

    #[test]
    fn a_crossing_back_from_the_secondary_into_the_primary_resolves_too() {
        let monitors = crossing_roster();
        let secondary = SessionMonitorId::new(2).expect("nonzero");
        let primary = SessionMonitorId::new(1).expect("nonzero");
        // Just past the secondary's right edge is the primary's origin.
        let (monitor_id, _) =
            resolve_pointer_crossing(&monitors, secondary, (1.5, 0.25)).expect("resolves");
        assert_eq!(monitor_id, primary);
    }

    #[test]
    fn a_position_outside_every_monitor_resolves_to_none() {
        let monitors = crossing_roster();
        let primary = SessionMonitorId::new(1).expect("nonzero");
        // Far below both monitors: the secondary is only 1130 tall, so the
        // lower-left region belongs to no monitor at all. The caller must
        // keep its own clamped behaviour rather than invent a position.
        assert!(resolve_pointer_crossing(&monitors, primary, (-0.5, 0.95)).is_none());
        // And far outside the whole desktop.
        assert!(resolve_pointer_crossing(&monitors, primary, (-50.0, 0.5)).is_none());
    }

    #[test]
    fn a_non_finite_or_unknown_origin_never_resolves() {
        let monitors = crossing_roster();
        let primary = SessionMonitorId::new(1).expect("nonzero");
        assert!(resolve_pointer_crossing(&monitors, primary, (f64::NAN, 0.5)).is_none());
        assert!(resolve_pointer_crossing(&monitors, primary, (0.5, f64::INFINITY)).is_none());
        let unknown = SessionMonitorId::new(9).expect("nonzero");
        assert!(resolve_pointer_crossing(&monitors, unknown, (0.5, 0.5)).is_none());
    }

    /// A crossing must use the same inclusive-edge pixel convention the
    /// client's own local-fraction-to-wire mapping uses, so resolving a
    /// crossing lands on the applied-desktop pixel the unclamped position
    /// actually pointed at rather than one shifted by a pixel at every
    /// boundary.
    #[test]
    fn a_resolved_crossing_lands_on_the_expected_desktop_pixel() {
        let monitors = crossing_roster();
        let primary = SessionMonitorId::new(1).expect("nonzero");
        let (monitor_id, fraction) =
            resolve_pointer_crossing(&monitors, primary, (-0.25, 0.25)).expect("resolves");
        let rect = monitors
            .iter()
            .find(|(id, _)| *id == monitor_id)
            .expect("resolved monitor is in the roster")
            .1;
        // Reproduce the same inclusive-edge mapping the client applies.
        let desktop_x = rect.x + rounded_offset_pixels(fraction.0, rect.width_px.saturating_sub(1));
        // -0.25 of the primary's own 3007-pixel span from x = 0.
        let expected = -rounded_offset_pixels(0.25, 3007);
        assert!(
            (desktop_x - expected).abs() <= 1,
            "crossing mapped to {desktop_x}, expected about {expected}",
        );
    }
}
