//! Per-monitor bounded frame routing foundation for multi-monitor-v1.
//!
//! The live single-monitor hot path (`crate::ui::media_worker`,
//! `crate::pipeline::frame_queue`) is intentionally left untouched by this
//! tranche: it keeps exactly its current one-decoder/one-frame-slot behavior.
//! [`MonitorFrameRouter`] is an additive, independently testable foundation
//! that routes wire `VideoHeader.monitor_id` frames to one bounded decoder +
//! latest-frame slot per admitted [`MonitorRoute`], fenced by an explicit
//! [`TopologyGeneration`] the caller must supply on every call.
//!
//! The *admission* rules -- roster membership, monitor id classification,
//! topology generation, stream epoch, codec/chroma profile, and the
//! keyframe/recovery fencing decision -- are not Deck's: they live in
//! [`arcen_media::RegionFrameRoster`] and
//! [`arcen_media::region_frame_delivery`], shared with every other client.
//! What stays here is the native half this router actually owns: one
//! `VideoToolbox` decoder and one latest decoded frame per admitted route,
//! plus each route's own keyframe-recovery flag.
//!
//! `arcen_media::SessionMonitorId` is nonzero (`1..=65535`) by construction,
//! representing only a host-negotiated per-monitor route. Wire
//! `monitor_id == 0` is today's legacy single-monitor frame, which is *not* a
//! negotiated session monitor id and must never be represented by
//! constructing a (now statically impossible) zero `SessionMonitorId`.
//! [`MonitorRoute`] makes that distinction explicit at the type level:
//! [`MonitorRoute::LegacyPrimary`] for wire id `0`, or
//! [`MonitorRoute::Negotiated`] wrapping a real `SessionMonitorId` for wire
//! ids `1..=65535`.
//!
//! Region video v1 carries `monitor_id`, `topology_generation`, and
//! `stream_epoch` on every frame. This router validates all three against the
//! committed roster before touching a decoder. A new topology still means
//! building a new router after a fresh connection, never mutating this one's
//! roster or generation in place.

use std::collections::BTreeMap;
use std::fmt;

use arcen_media::{
    region_frame_delivery, RegionFrameDelivery, RegionFrameRoster, RegionMediaRoster,
    SessionMonitorId, TopologyGeneration,
};

use crate::pipeline::video_decoder::{DecodedVideoFrame, NativeVideoDecoder, VideoDecodeError};
use crate::protocol::VideoHeader;

/// The shared route classification (wire `monitor_id` `0` vs. a negotiated
/// [`SessionMonitorId`]), re-exported under the name Deck's call sites and
/// diagnostics already use.
pub use arcen_media::MonitorRoute;
/// Failure admitting a frame into a [`MonitorFrameRouter`] slot.
///
/// Distinct from a decode failure: admission is rejected before any decoder
/// is touched, which is what proves routing isolation (a frame for an
/// unrouted or stale-generation monitor can never reach another monitor's
/// decoder or overwrite its latest frame). Owned by
/// [`arcen_media::RegionFrameAdmissionError`], including its exact rejection
/// ordering.
pub use arcen_media::RegionFrameAdmissionError as RouterAdmissionError;
/// Failure building a [`MonitorFrameRouter`] roster, owned by
/// [`arcen_media::RegionFrameRosterError`].
pub use arcen_media::RegionFrameRosterError as RouterBuildError;

/// Failure routing one wire frame all the way through decode.
#[derive(Debug)]
pub enum RouteError {
    /// The frame was rejected before decode (see [`RouterAdmissionError`]).
    Admission(RouterAdmissionError),
    /// Admission succeeded but the per-monitor decoder rejected the payload.
    Decode(VideoDecodeError),
}

impl fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admission(error) => write!(formatter, "{error}"),
            Self::Decode(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RouteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Admission(error) => Some(error),
            Self::Decode(error) => Some(error),
        }
    }
}

impl From<RouterAdmissionError> for RouteError {
    fn from(error: RouterAdmissionError) -> Self {
        Self::Admission(error)
    }
}

/// The result of successfully routing and decoding one wire video frame into
/// a monitor's slot: distinguishes a genuinely fresh decode from every other
/// non-error outcome, so a caller's own "did new content arrive this batch"
/// bookkeeping (e.g. `crate::ui::media_worker::decode_batch`'s
/// `decoded_any`) can never be satisfied by a stale cached frame or a
/// packet that produced no new output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOutcome {
    /// A genuinely new frame decoded from this exact packet. The frame
    /// itself is not returned here -- read it via
    /// [`MonitorFrameRouter::latest_frame`] /
    /// [`MonitorFrameRouter::latest_frame_legacy_primary`], an explicit,
    /// separate *cached presentation lookup* that never itself implies this
    /// call produced fresh output.
    FreshFrame,
    /// The packet was admitted (right generation, routed monitor) but
    /// produced no new frame this call: either the underlying decoder
    /// consumed it without emitting a picture yet (e.g. buffered parameter
    /// sets), or this slot is still waiting for its own next keyframe and
    /// the packet was not one (skipped before ever reaching the decoder, so
    /// a non-keyframe payload is never even attempted while this monitor's
    /// own recovery is pending). The slot's previously cached frame (if any)
    /// is completely untouched either way -- a cached frame must never be
    /// returned here as if it were fresh, and it never clears this monitor's
    /// own IDR-recovery wait.
    NoOutputYet,
}

/// One monitor's bounded decode + presentation state: exactly one in-flight
/// decoder and exactly one latest decoded frame, mirroring the single-monitor
/// architecture's bound but per admitted monitor instead of globally.
struct MonitorSlot {
    decoder: NativeVideoDecoder,
    latest_frame: Option<DecodedVideoFrame>,
    frames_routed: u64,
    frames_rejected: u64,
    /// This monitor's own independent keyframe-recovery gate, mirroring the
    /// legacy single-decoder path's session-wide
    /// `SharedMediaState::waiting_for_keyframe` but scoped to exactly this
    /// monitor's own slot rather than the whole session. Starts `true`: a
    /// freshly admitted monitor slot requires its own keyframe before it
    /// ever produces a presentable frame. Set back to `true` independently
    /// by [`MonitorFrameRouter::notify_discontinuity`] (every slot) or by
    /// this slot's own decoder reporting `wants_keyframe()` -- one
    /// monitor's discontinuity/recovery never touches another's.
    waiting_for_keyframe: bool,
}

impl MonitorSlot {
    fn new() -> Self {
        Self {
            decoder: NativeVideoDecoder::new(),
            latest_frame: None,
            frames_routed: 0,
            frames_rejected: 0,
            waiting_for_keyframe: true,
        }
    }
}

/// Routes per-monitor wire video frames to independent bounded decode +
/// latest-frame slots, admitting only frames that match this router's
/// committed topology generation and roster.
pub struct MonitorFrameRouter {
    /// The shared, immutable admission fence this router owns its native
    /// slots for: the committed generation, the admitted routes, and each
    /// route's advertised media plan. Every rejection decision is delegated
    /// to it; `slots` below is keyed by exactly the routes it admits.
    admission: RegionFrameRoster,
    slots: BTreeMap<MonitorRoute, MonitorSlot>,
    /// The explicit negotiated primary monitor id this router was built
    /// for -- [`Self::new`]'s `monitor_ids[0]`, matching
    /// `ValidatedAppliedTopology::monitor_ids()`'s own "primary first"
    /// contract. Never derived from `slots`' `BTreeMap` iteration order:
    /// that map is keyed by [`MonitorRoute`]'s numeric `Ord` purely for
    /// deterministic iteration, and a negotiated primary can be *any* id in
    /// the roster (e.g. primary `7` alongside secondary `1`) -- treating
    /// the smallest admitted id as "the primary" was exactly the bug this
    /// field closes. `None` only for [`Self::single_monitor`]'s
    /// legacy-primary-only router, which has no negotiated
    /// `SessionMonitorId` at all.
    primary_monitor_id: Option<SessionMonitorId>,
}

impl fmt::Debug for MonitorFrameRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MonitorFrameRouter")
            .field("generation", &self.admission.generation())
            .field("routes", &self.slots.keys().collect::<Vec<_>>())
            .field("primary_monitor_id", &self.primary_monitor_id)
            .finish()
    }
}

impl MonitorFrameRouter {
    /// Shared slot-building constructor for [`Self::new`],
    /// [`Self::new_with_media_roster`], and [`Self::single_monitor`]: the
    /// empty/too-many/duplicate roster checks belong to
    /// [`arcen_media::RegionFrameRoster`], and this only allocates one native
    /// decode slot per route it admits. Leaves [`Self::primary_monitor_id`]
    /// `None`; the negotiated constructors fill it in afterward from their
    /// own explicit primary, which this routine has no access to.
    fn from_admission(admission: RegionFrameRoster) -> Self {
        let slots = admission
            .routes()
            .map(|route| (route, MonitorSlot::new()))
            .collect();
        Self {
            admission,
            slots,
            primary_monitor_id: None,
        }
    }

    /// Builds a router admitting exactly the negotiated `monitor_ids`,
    /// fenced to `generation`. Every admitted route is
    /// [`MonitorRoute::Negotiated`]; this constructor can never admit the
    /// legacy primary route (see [`Self::single_monitor`]).
    ///
    /// `monitor_ids[0]` is recorded verbatim as [`Self::primary_monitor_id`]
    /// -- the caller's explicit negotiated primary, matching
    /// `ValidatedAppliedTopology::monitor_ids()`'s "primary first" contract
    /// -- never re-derived from the admitted roster's sorted order.
    ///
    /// # Errors
    ///
    /// Returns [`RouterBuildError`] when the roster is empty, exceeds
    /// `arcen_media::MAX_MULTI_MONITOR_COUNT`, or contains a duplicate identifier.
    pub fn new(
        generation: TopologyGeneration,
        monitor_ids: &[SessionMonitorId],
    ) -> Result<Self, RouterBuildError> {
        let mut router =
            Self::from_admission(RegionFrameRoster::negotiated(generation, monitor_ids)?);
        router.primary_monitor_id = monitor_ids.first().copied();
        Ok(router)
    }

    /// Builds a router from the complete host-authoritative media roster.
    ///
    /// Every admitted route retains its own
    /// codec/chroma/backend/geometry/fps/epoch plan; frame admission
    /// validates the wire profile and stream epoch before touching that
    /// monitor's decoder.
    ///
    /// # Errors
    ///
    /// Returns [`RouterBuildError`] when the roster is empty, exceeds
    /// `arcen_media::MAX_MULTI_MONITOR_COUNT`, or contains a duplicate
    /// identifier.
    pub fn new_with_media_roster(
        generation: TopologyGeneration,
        roster: &RegionMediaRoster,
    ) -> Result<Self, RouterBuildError> {
        let mut router =
            Self::from_admission(RegionFrameRoster::from_media_roster(generation, roster)?);
        router.primary_monitor_id = roster.plans().first().map(|plan| plan.session_monitor_id);
        Ok(router)
    }

    /// Test-only mixed-roster constructor: builds a router from an explicit
    /// `(route, plan)` slice so the type-level legacy-vs-negotiated
    /// isolation can be exercised on one router. No production path mixes
    /// the two routes (see [`Self::new`] vs. [`Self::single_monitor`]).
    #[cfg(test)]
    fn from_routes(
        generation: TopologyGeneration,
        routes: &[(MonitorRoute, Option<arcen_media::RegionMediaPlan>)],
    ) -> Result<Self, RouterBuildError> {
        Ok(Self::from_admission(RegionFrameRoster::new(
            generation, routes,
        )?))
    }

    /// Convenience constructor preserving today's single-monitor behavior:
    /// one admitted [`MonitorRoute::LegacyPrimary`] route (wire id `0`) at
    /// generation `1`. Never admits a negotiated `SessionMonitorId`.
    #[must_use]
    pub fn single_monitor() -> Self {
        Self::from_admission(RegionFrameRoster::legacy_primary())
    }

    /// The topology generation this router is fenced to.
    #[must_use]
    pub const fn generation(&self) -> TopologyGeneration {
        self.admission.generation()
    }

    /// The admitted route roster, in ascending order (see [`MonitorRoute`]'s
    /// `Ord`).
    pub fn routes(&self) -> impl Iterator<Item = MonitorRoute> + '_ {
        self.slots.keys().copied()
    }

    /// The admitted negotiated session-monitor-id roster, in ascending
    /// order. Excludes [`MonitorRoute::LegacyPrimary`] when present -- use
    /// [`Self::routes`] to observe the full roster including the legacy
    /// route.
    pub fn monitor_ids(&self) -> impl Iterator<Item = SessionMonitorId> + '_ {
        self.slots.keys().filter_map(|route| match route {
            MonitorRoute::Negotiated(id) => Some(*id),
            MonitorRoute::LegacyPrimary => None,
        })
    }

    /// The explicit negotiated primary monitor id this router was built
    /// with ([`Self::new`]'s `monitor_ids[0]`), or `None` for
    /// [`Self::single_monitor`]'s legacy-primary-only router. Callers
    /// presenting the roster's primary (e.g. root's own
    /// `latest_frame`/texture) must use this, never
    /// `self.monitor_ids().next()` -- [`Self::monitor_ids`] iterates
    /// `slots`' `BTreeMap` in ascending numeric order purely for
    /// deterministic enumeration, which is *not* primary/secondary meaning:
    /// a negotiated primary can be any id in the roster (primary `7`
    /// alongside secondary `1` sorts secondary first).
    #[must_use]
    pub const fn primary_monitor_id(&self) -> Option<SessionMonitorId> {
        self.primary_monitor_id
    }

    /// The latest successfully decoded frame for negotiated `monitor_id`, or
    /// `None` when that route is unrouted or has not yet produced a frame.
    #[must_use]
    pub fn latest_frame(&self, monitor_id: SessionMonitorId) -> Option<&DecodedVideoFrame> {
        self.latest_frame_for_route(MonitorRoute::Negotiated(monitor_id))
    }

    /// The latest successfully decoded frame for the legacy primary route
    /// (wire `monitor_id == 0`), or `None` when that route is unrouted or has
    /// not yet produced a frame.
    #[must_use]
    pub fn latest_frame_legacy_primary(&self) -> Option<&DecodedVideoFrame> {
        self.latest_frame_for_route(MonitorRoute::LegacyPrimary)
    }

    #[must_use]
    pub fn decoder_backend_name(&self, monitor_id: SessionMonitorId) -> Option<&'static str> {
        self.slots
            .get(&MonitorRoute::Negotiated(monitor_id))
            .map(|slot| slot.decoder.backend_name())
    }

    #[must_use]
    pub fn decoder_hardware_accelerated(
        &self,
        monitor_id: SessionMonitorId,
    ) -> Option<Option<bool>> {
        self.slots
            .get(&MonitorRoute::Negotiated(monitor_id))
            .map(|slot| slot.decoder.is_hardware_accelerated())
    }

    fn latest_frame_for_route(&self, route: MonitorRoute) -> Option<&DecodedVideoFrame> {
        self.slots.get(&route)?.latest_frame.as_ref()
    }

    /// Frames successfully routed to `monitor_id`'s slot, or `None` when the
    /// monitor is unrouted.
    #[must_use]
    pub fn frames_routed(&self, monitor_id: SessionMonitorId) -> Option<u64> {
        self.slots
            .get(&MonitorRoute::Negotiated(monitor_id))
            .map(|slot| slot.frames_routed)
    }

    /// Frames rejected by `monitor_id`'s decoder after admission succeeded,
    /// or `None` when the monitor is unrouted.
    #[must_use]
    pub fn frames_rejected(&self, monitor_id: SessionMonitorId) -> Option<u64> {
        self.slots
            .get(&MonitorRoute::Negotiated(monitor_id))
            .map(|slot| slot.frames_rejected)
    }

    /// Whether one admitted monitor's own slot is waiting for its next
    /// keyframe, or `None` when the monitor is unrouted.
    ///
    /// Per-monitor complement of [`Self::any_waiting_for_keyframe`], for
    /// callers that must attribute a stalled viewport to the exact monitor
    /// rather than learn only that *some* monitor is recovering.
    #[must_use]
    pub fn waiting_for_keyframe(&self, monitor_id: SessionMonitorId) -> Option<bool> {
        self.slots
            .get(&MonitorRoute::Negotiated(monitor_id))
            .map(|slot| slot.waiting_for_keyframe)
    }

    /// Whether any admitted monitor slot is still waiting for its own next
    /// keyframe before it can present a fresh frame (see
    /// [`MonitorSlot::waiting_for_keyframe`]'s own doc for why this is
    /// per-slot, never session-wide).
    ///
    /// Callers must use this -- never a single slot's `RouteOutcome::
    /// FreshFrame`, which only proves *that one* monitor recovered -- to
    /// decide whether it is safe to stop asking the host for a full frame:
    /// multi-monitor-v1's wire has no per-monitor full-frame request, so one
    /// monitor recovering while a sibling is still mid-recovery must not
    /// cancel the shared ask, or the sibling would be stranded waiting for a
    /// keyframe the host was never asked to resend.
    #[must_use]
    pub fn any_waiting_for_keyframe(&self) -> bool {
        self.slots.values().any(|slot| slot.waiting_for_keyframe)
    }

    /// Whether every admitted monitor slot has recovered from its own
    /// keyframe wait. Exact complement of [`Self::any_waiting_for_keyframe`],
    /// kept as its own named predicate since callers reason about it
    /// directly as "safe to cancel the shared full-frame-request gate now."
    #[must_use]
    pub fn all_recovered(&self) -> bool {
        !self.any_waiting_for_keyframe()
    }

    fn admit(
        &mut self,
        generation: TopologyGeneration,
        route: MonitorRoute,
    ) -> Result<&mut MonitorSlot, RouterAdmissionError> {
        self.admission.admit_route(generation, route)?;
        self.slots
            .get_mut(&route)
            .ok_or(RouterAdmissionError::UnroutedMonitor(
                route.wire_monitor_id(),
            ))
    }

    /// Routes and decodes one wire video frame, admitting it only when the
    /// frame's wire-supplied topology generation, stream epoch, monitor id,
    /// and codec/chroma match the committed roster.
    ///
    /// Returns [`RouteOutcome::FreshFrame`] only when this exact call
    /// produced a genuinely new decoded frame; every other non-error outcome
    /// (buffered parameter sets, a non-keyframe packet skipped while this
    /// monitor's own recovery is pending) is [`RouteOutcome::NoOutputYet`].
    /// Read the resulting frame separately via [`Self::latest_frame`] /
    /// [`Self::latest_frame_legacy_primary`] -- a *cached* presentation
    /// lookup that is never itself evidence of freshness, and a slot's
    /// cached frame is never cleared by a `NoOutputYet` outcome.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::Admission`] when the frame is rejected before
    /// decode, and [`RouteError::Decode`] when the admitted monitor's decoder
    /// rejects the payload. Either error arms this monitor's own
    /// keyframe-recovery wait without touching any other monitor's slot.
    pub fn route_and_decode(
        &mut self,
        header: &VideoHeader,
        payload: &[u8],
    ) -> Result<RouteOutcome, RouteError> {
        // Every rejection -- generation, roster membership, stream epoch,
        // codec/chroma profile -- and its exact ordering belongs to the
        // shared admission fence; this router only owns what happens to the
        // native slot afterward.
        let route = self.admission.admit_frame(header)?;
        let slot = self
            .slots
            .get_mut(&route)
            .ok_or(RouterAdmissionError::UnroutedMonitor(
                route.wire_monitor_id(),
            ))?;
        if region_frame_delivery(slot.waiting_for_keyframe, header)
            == RegionFrameDelivery::SkipUntilKeyframe
        {
            // This monitor's own recovery is pending and this packet is not
            // a keyframe: skip it before ever reaching the decoder, exactly
            // mirroring `crate::ui::media_worker::first_decodable_index`'s
            // skip-until-keyframe philosophy but scoped to this one
            // monitor's slot instead of the whole legacy batch. The slot's
            // previously cached frame (if any, from before the
            // discontinuity) is left untouched but is stale and must not be
            // treated as fresh.
            slot.frames_routed += 1;
            return Ok(RouteOutcome::NoOutputYet);
        }
        match slot.decoder.decode(header, payload) {
            Ok(Some(frame)) => {
                slot.latest_frame = Some(frame);
                slot.frames_routed += 1;
                slot.waiting_for_keyframe = false;
                Ok(RouteOutcome::FreshFrame)
            }
            Ok(None) => {
                slot.frames_routed += 1;
                if slot.decoder.wants_keyframe() {
                    slot.waiting_for_keyframe = true;
                }
                Ok(RouteOutcome::NoOutputYet)
            }
            Err(error) => {
                slot.frames_rejected += 1;
                slot.waiting_for_keyframe = true;
                Err(RouteError::Decode(error))
            }
        }
    }

    /// Propagates a video discontinuity to every admitted monitor's own
    /// decoder and independent keyframe-recovery gate. Called from
    /// `crate::ui::media_worker::handle_media_batch`'s
    /// `batch.video_discontinuity` handling alongside (never instead of) the
    /// legacy single-decoder path's own discontinuity notification, so every
    /// monitor's own slot recovers on its own terms: no monitor's cached
    /// frame is cleared by this call, but none may present a fresh frame
    /// again until its own next keyframe arrives.
    pub fn notify_discontinuity(&mut self) {
        for slot in self.slots.values_mut() {
            slot.decoder.notify_discontinuity();
            slot.waiting_for_keyframe = true;
        }
    }

    /// Propagates a discontinuity to exactly one wire route. Returns `false`
    /// when the route is not part of this committed roster.
    pub fn notify_route_discontinuity(&mut self, wire_monitor_id: u16) -> bool {
        let route = MonitorRoute::from_wire_monitor_id(wire_monitor_id);
        let Some(slot) = self.slots.get_mut(&route) else {
            return false;
        };
        slot.decoder.notify_discontinuity();
        slot.waiting_for_keyframe = true;
        true
    }

    /// Injects an already-decoded frame directly into negotiated
    /// `monitor_id`'s slot, bypassing the real decoder. Used by the synthetic
    /// multi-monitor harness (`crate::pipeline::synthetic_multi_monitor`) and
    /// tests to prove routing isolation without a real host or codec
    /// bitstream.
    ///
    /// # Errors
    ///
    /// Returns [`RouterAdmissionError`] under the same fencing rules as
    /// [`Self::route_and_decode`].
    pub fn route_decoded_frame(
        &mut self,
        generation: TopologyGeneration,
        monitor_id: SessionMonitorId,
        frame: DecodedVideoFrame,
    ) -> Result<(), RouterAdmissionError> {
        self.route_decoded_frame_for_route(generation, MonitorRoute::Negotiated(monitor_id), frame)
    }

    /// Injects an already-decoded frame directly into the legacy primary
    /// route's slot (wire `monitor_id == 0`), bypassing the real decoder.
    /// Exists so tests and harnesses can exercise the legacy route in
    /// isolation without ever constructing a zero `SessionMonitorId`, which
    /// is statically impossible.
    ///
    /// # Errors
    ///
    /// Returns [`RouterAdmissionError`] under the same fencing rules as
    /// [`Self::route_and_decode`].
    pub fn route_decoded_frame_legacy_primary(
        &mut self,
        generation: TopologyGeneration,
        frame: DecodedVideoFrame,
    ) -> Result<(), RouterAdmissionError> {
        self.route_decoded_frame_for_route(generation, MonitorRoute::LegacyPrimary, frame)
    }

    fn route_decoded_frame_for_route(
        &mut self,
        generation: TopologyGeneration,
        route: MonitorRoute,
        frame: DecodedVideoFrame,
    ) -> Result<(), RouterAdmissionError> {
        let slot = self.admit(generation, route)?;
        slot.latest_frame = Some(frame);
        slot.frames_routed += 1;
        Ok(())
    }

    /// Test-only cross-module seam: directly marks `route`'s slot as no
    /// longer waiting for its own keyframe, without a real decode.
    /// [`Self::route_decoded_frame`]/[`Self::route_decoded_frame_legacy_primary`]
    /// deliberately do not touch this recovery gate (they only seed a
    /// presentation-routing test's cached frame), and constructing a real
    /// decodable bitstream from another module's own test (e.g.
    /// `crate::ui::media_worker`'s) to prove "this monitor recovered" is
    /// impractical -- this gives those tests the same white-box control
    /// this module's own tests already take via direct field access. A
    /// no-op if `route` is not part of this router's admitted roster.
    #[cfg(test)]
    pub(crate) fn force_recovered_for_test(&mut self, route: MonitorRoute) {
        if let Some(slot) = self.slots.get_mut(&route) {
            slot.waiting_for_keyframe = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{MediaStreamEpoch, RegionMediaPlan};

    use crate::protocol::{wire, ChromaSubsampling, FrameType, ProtocolError, VideoCodec};

    /// A syntactically valid but incomplete Annex-B access unit: a lone
    /// H.264 slice NAL (type 1, never a keyframe) with no cached SPS/PPS.
    /// Reaches the real platform decoder without a bitstream parse error
    /// (unlike fully garbage bytes), but can never itself yield a decoded
    /// picture (no parameter sets to build a session from), so
    /// `decoder.decode()` deterministically returns `Ok(None)` -- exactly
    /// the "admitted but no fresh output yet" case these tests need,
    /// without depending on a real VideoToolbox decode succeeding.
    const NON_KEYFRAME_SLICE_PAYLOAD: &[u8] = &[0, 0, 0, 1, 0x41, 7];

    /// Bytes with no Annex-B start code at all. Reaching the real decoder
    /// with this payload always fails to parse (`VideoDecodeError`),
    /// regardless of platform -- used to prove a packet was actually
    /// *attempted* against the decoder, as opposed to skipped beforehand.
    const NOT_ANNEX_B_PAYLOAD: &[u8] = &[9, 9, 9];

    fn video_header(monitor_id: u16, keyframe: bool) -> VideoHeader {
        VideoHeader {
            frame_type: if monitor_id == 0 {
                FrameType::VideoH264
            } else {
                FrameType::RegionVideoH264
            },
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            flags: u8::from(keyframe),
            timestamp_ms: 0,
            monitor_id,
            topology_generation: u64::from(monitor_id != 0),
            stream_epoch: u64::from(monitor_id != 0),
        }
    }

    /// Test-only white-box accessor: the whole point of these tests is to
    /// observe [`MonitorSlot::waiting_for_keyframe`] transitions that no
    /// public API exposes (by design -- it is purely an internal recovery
    /// gate), so reach into the same-module-private field directly rather
    /// than manufacture a public getter with no production caller.
    fn slot_waiting_for_keyframe(router: &MonitorFrameRouter, route: MonitorRoute) -> bool {
        router
            .slots
            .get(&route)
            .expect("route must be admitted for this test")
            .waiting_for_keyframe
    }

    fn frame(tag: u8) -> DecodedVideoFrame {
        DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: vec![tag; 16],
            timestamp_ms: u32::from(tag),
            pixel_format: "rgba".to_string(),
            backend: "synthetic",
            native: None,
        }
    }

    fn gen(value: u64) -> TopologyGeneration {
        TopologyGeneration::new(value).expect("nonzero generation")
    }

    /// Validated-constructor test helper centralizing the one place tests
    /// trust a literal is a nonzero session monitor id, instead of scattering
    /// `.expect(...)` calls across every test body.
    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("test session monitor id must be nonzero")
    }

    fn mixed_media_roster() -> RegionMediaRoster {
        let plan = |id, epoch, backend, codec| {
            RegionMediaPlan::new(
                sid(id),
                MediaStreamEpoch::new(epoch).expect("nonzero epoch"),
                backend,
                arcen_media::VideoConfiguration {
                    codec,
                    ..arcen_media::VideoConfiguration::legacy_h264()
                },
                1920,
                1080,
                60,
                arcen_media::BitrateBudgetKbps::nominal_for_geometry(1920, 1080, 60),
            )
            .expect("valid region plan")
        };
        RegionMediaRoster::new(vec![
            plan(
                1,
                11,
                arcen_media::video::EncoderBackend::NativeNvenc,
                arcen_media::VideoCodec::H265,
            ),
            plan(
                2,
                12,
                arcen_media::video::EncoderBackend::OpenH264,
                arcen_media::VideoCodec::H264,
            ),
        ])
        .expect("valid mixed roster")
    }

    #[test]
    fn wire_monitor_id_zero_is_always_legacy_primary_never_a_session_monitor_id() {
        assert_eq!(
            MonitorRoute::from_wire_monitor_id(0),
            MonitorRoute::LegacyPrimary
        );
        assert_eq!(MonitorRoute::LegacyPrimary.wire_monitor_id(), 0);
        // `SessionMonitorId` is nonzero by construction, so wire id 0 can
        // never round-trip through `MonitorRoute::Negotiated`.
        assert!(SessionMonitorId::new(0).is_err());
    }

    #[test]
    fn nonzero_wire_monitor_ids_round_trip_through_negotiated_routes() {
        for wire_id in [1_u16, 2, 4, 65535] {
            let route = MonitorRoute::from_wire_monitor_id(wire_id);
            assert_eq!(route, MonitorRoute::Negotiated(sid(wire_id)));
            assert_eq!(route.wire_monitor_id(), wire_id);
        }
    }

    #[test]
    fn single_monitor_router_preserves_current_behavior() {
        let router = MonitorFrameRouter::single_monitor();
        // The legacy router admits only the explicit legacy-primary route --
        // never a negotiated session monitor id, so `monitor_ids()` (the
        // negotiated-only view) is empty while `routes()` shows the one
        // legacy route.
        assert_eq!(router.monitor_ids().collect::<Vec<_>>(), Vec::new());
        assert_eq!(
            router.routes().collect::<Vec<_>>(),
            vec![MonitorRoute::LegacyPrimary]
        );
        assert_eq!(router.generation().get(), 1);
        // No negotiated `SessionMonitorId` exists for this legacy-only
        // router at all.
        assert!(router.primary_monitor_id().is_none());
    }

    #[test]
    fn new_records_the_explicit_negotiated_primary_never_the_numerically_smallest_id() {
        // Audit finding: `monitor_ids[0]` (`7`, the negotiated primary) must
        // be preserved verbatim even though `slots` is a `BTreeMap` that
        // sorts admitted routes by numeric id ascending, which would put
        // secondary `1` first in iteration order.
        let primary = sid(7);
        let secondary = sid(1);
        let router = MonitorFrameRouter::new(gen(1), &[primary, secondary])
            .expect("a two-monitor roster with primary first is valid");

        assert_eq!(router.primary_monitor_id(), Some(primary));
        // `monitor_ids()`'s own ascending iteration order is completely
        // unaffected by this fix -- it is a separate, purely enumerative
        // view that was never meant to carry primary/secondary meaning.
        assert_eq!(
            router.monitor_ids().collect::<Vec<_>>(),
            vec![secondary, primary]
        );
    }

    #[test]
    fn new_with_the_smallest_id_first_still_records_it_as_the_explicit_primary() {
        // The inverse ordering: when the negotiated primary genuinely *is*
        // the smallest id, it must still be recorded as the explicit
        // primary via `monitor_ids[0]`, not incidentally via sort order --
        // proving the fix is correct in both directions, not just when the
        // two happen to differ.
        let primary = sid(1);
        let secondary = sid(7);
        let router = MonitorFrameRouter::new(gen(1), &[primary, secondary])
            .expect("a two-monitor roster with primary first is valid");

        assert_eq!(router.primary_monitor_id(), Some(primary));
    }

    #[test]
    fn single_monitor_router_routes_the_legacy_primary_frame() {
        let mut router = MonitorFrameRouter::single_monitor();
        router
            .route_decoded_frame_legacy_primary(gen(1), frame(0x77))
            .expect("routes to the legacy primary slot");
        assert_eq!(
            router
                .latest_frame_legacy_primary()
                .expect("frame present")
                .rgba,
            vec![0x77; 16]
        );
    }

    #[test]
    fn empty_roster_is_rejected() {
        assert_eq!(
            MonitorFrameRouter::new(gen(1), &[]).unwrap_err(),
            RouterBuildError::EmptyRoster
        );
    }

    #[test]
    fn too_many_monitors_is_rejected() {
        let ids: Vec<_> = (1..=5).map(sid).collect();
        assert_eq!(
            MonitorFrameRouter::new(gen(1), &ids).unwrap_err(),
            RouterBuildError::TooManyMonitors(5)
        );
    }

    #[test]
    fn duplicate_monitor_id_is_rejected() {
        let ids = [sid(1), sid(1)];
        assert_eq!(
            MonitorFrameRouter::new(gen(1), &ids).unwrap_err(),
            RouterBuildError::DuplicateMonitor(1)
        );
    }

    #[test]
    fn two_monitor_routing_is_isolated_per_monitor() {
        let ids = [sid(1), sid(2)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        router
            .route_decoded_frame(gen(1), sid(1), frame(0xAA))
            .expect("routes to monitor 1");
        router
            .route_decoded_frame(gen(1), sid(2), frame(0xBB))
            .expect("routes to monitor 2");

        let first = router.latest_frame(sid(1)).expect("frame present");
        let second = router.latest_frame(sid(2)).expect("frame present");
        assert_eq!(first.rgba, vec![0xAA; 16]);
        assert_eq!(second.rgba, vec![0xBB; 16]);
        assert_ne!(first.rgba, second.rgba);
        assert_eq!(router.frames_routed(sid(1)), Some(1));
        assert_eq!(router.frames_routed(sid(2)), Some(1));
    }

    #[test]
    fn four_monitor_routing_is_isolated_per_monitor() {
        let ids: Vec<_> = (1..=4).map(sid).collect();
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        for id in &ids {
            router
                .route_decoded_frame(gen(1), *id, frame(id.get() as u8))
                .expect("routes");
        }
        for id in &ids {
            let routed = router.latest_frame(*id).expect("frame present");
            assert_eq!(routed.rgba, vec![id.get() as u8; 16]);
        }
    }

    #[test]
    fn unrouted_monitor_is_rejected_without_touching_other_slots() {
        let ids = [sid(1)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        let error = router
            .route_decoded_frame(gen(1), sid(9), frame(1))
            .unwrap_err();
        assert_eq!(error, RouterAdmissionError::UnroutedMonitor(9));
        assert!(router.latest_frame(sid(1)).is_none());
    }

    #[test]
    fn legacy_primary_route_is_unrouted_on_a_negotiated_only_router() {
        // A real negotiated multi-monitor router never admits wire id 0;
        // proving that rejection here (rather than only inside the harness)
        // demonstrates the legacy route in isolation, per review guidance.
        let ids = [sid(1), sid(2)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        let error = router
            .route_decoded_frame_legacy_primary(gen(1), frame(1))
            .unwrap_err();
        assert_eq!(error, RouterAdmissionError::UnroutedMonitor(0));
        assert!(router.latest_frame_legacy_primary().is_none());
        assert!(router.latest_frame(sid(1)).is_none());
        assert!(router.latest_frame(sid(2)).is_none());
    }

    #[test]
    fn stale_generation_is_rejected_even_for_a_routed_monitor() {
        let ids = [sid(1)];
        let mut router = MonitorFrameRouter::new(gen(5), &ids).expect("valid roster");
        let error = router
            .route_decoded_frame(gen(4), sid(1), frame(1))
            .unwrap_err();
        assert_eq!(
            error,
            RouterAdmissionError::StaleGeneration {
                expected: 5,
                actual: 4
            }
        );
        assert!(router.latest_frame(sid(1)).is_none());
    }

    #[test]
    fn mixed_roster_validates_codec_per_region_not_from_the_first_plan() {
        let roster = mixed_media_roster();
        let mut router =
            MonitorFrameRouter::new_with_media_roster(gen(1), &roster).expect("valid roster");
        let mut h265 = video_header(1, false);
        h265.frame_type = FrameType::RegionVideoH265;
        h265.codec = VideoCodec::H265;
        h265.stream_epoch = 11;
        router
            .route_and_decode(&h265, NON_KEYFRAME_SLICE_PAYLOAD)
            .expect("monitor 1 accepts its H265 profile before keyframe gating");

        let mut h264 = video_header(2, false);
        h264.stream_epoch = 12;
        router
            .route_and_decode(&h264, NON_KEYFRAME_SLICE_PAYLOAD)
            .expect("monitor 2 accepts its independent H264 profile");

        let error = router
            .route_and_decode(
                &VideoHeader {
                    monitor_id: 2,
                    stream_epoch: 12,
                    ..h265
                },
                NON_KEYFRAME_SLICE_PAYLOAD,
            )
            .expect_err("monitor 2 must not inherit monitor 1's H265 profile");
        assert!(matches!(
            error,
            RouteError::Admission(RouterAdmissionError::WireProfileMismatch { monitor_id: 2 })
        ));
    }

    #[test]
    fn stream_epoch_and_keyframe_recovery_are_scoped_per_region() {
        let roster = mixed_media_roster();
        let mut router =
            MonitorFrameRouter::new_with_media_roster(gen(1), &roster).expect("valid roster");
        let mut h264 = video_header(2, false);
        h264.stream_epoch = 11;
        let error = router
            .route_and_decode(&h264, NON_KEYFRAME_SLICE_PAYLOAD)
            .expect_err("stale region epoch must be rejected");
        assert!(matches!(
            error,
            RouteError::Admission(RouterAdmissionError::StaleStreamEpoch {
                expected: 12,
                actual: 11
            })
        ));
        assert!(slot_waiting_for_keyframe(
            &router,
            MonitorRoute::Negotiated(sid(2))
        ));
        assert!(slot_waiting_for_keyframe(
            &router,
            MonitorRoute::Negotiated(sid(1))
        ));

        router.force_recovered_for_test(MonitorRoute::Negotiated(sid(1)));
        assert!(!slot_waiting_for_keyframe(
            &router,
            MonitorRoute::Negotiated(sid(1))
        ));
        assert!(slot_waiting_for_keyframe(
            &router,
            MonitorRoute::Negotiated(sid(2))
        ));
    }

    #[test]
    fn wire_supplied_stale_topology_generation_is_rejected() {
        let roster = mixed_media_roster();
        let mut router =
            MonitorFrameRouter::new_with_media_roster(gen(2), &roster).expect("valid roster");
        let mut header = video_header(1, false);
        header.topology_generation = 1;
        header.stream_epoch = 11;
        let error = router
            .route_and_decode(&header, NON_KEYFRAME_SLICE_PAYLOAD)
            .expect_err("old wire topology generation must be rejected");
        assert!(matches!(
            error,
            RouteError::Admission(RouterAdmissionError::StaleGeneration {
                expected: 2,
                actual: 1
            })
        ));
    }

    #[test]
    fn misrouted_frame_never_overwrites_a_different_monitors_slot() {
        let ids = [sid(1), sid(2)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        router
            .route_decoded_frame(gen(1), sid(1), frame(0x11))
            .expect("routes");
        // A frame nominally destined for monitor 2 with a stale generation
        // must not land anywhere, and monitor 1's slot must stay untouched.
        let _ = router.route_decoded_frame(gen(2), sid(2), frame(0x22));
        assert!(router.latest_frame(sid(2)).is_none());
        assert_eq!(
            router.latest_frame(sid(1)).expect("still present").rgba,
            vec![0x11; 16]
        );
    }

    #[test]
    fn legacy_primary_and_negotiated_routes_never_cross_contaminate() {
        // No production router mixes the legacy primary route with
        // negotiated ids today, but `MonitorRoute`'s admission logic must
        // still keep them fully isolated if it is ever asked to (proving the
        // type-level distinction, not just today's two separate
        // constructors, is what prevents cross-routing).
        let mut router = MonitorFrameRouter::from_routes(
            gen(1),
            &[
                (MonitorRoute::LegacyPrimary, None),
                (MonitorRoute::Negotiated(sid(1)), None),
            ],
        )
        .expect("mixed roster is valid");
        router
            .route_decoded_frame_legacy_primary(gen(1), frame(0xEE))
            .expect("routes to legacy primary");
        router
            .route_decoded_frame(gen(1), sid(1), frame(0x11))
            .expect("routes to negotiated monitor 1");

        assert_eq!(
            router
                .latest_frame_legacy_primary()
                .expect("legacy frame present")
                .rgba,
            vec![0xEE; 16]
        );
        assert_eq!(
            router
                .latest_frame(sid(1))
                .expect("negotiated frame present")
                .rgba,
            vec![0x11; 16]
        );
    }

    #[test]
    fn non_keyframe_packet_is_skipped_before_reaching_the_decoder_while_a_slot_awaits_its_own_keyframe(
    ) {
        let ids = [sid(1)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        let route = MonitorRoute::Negotiated(sid(1));
        // A freshly built slot starts waiting for its own keyframe.
        assert!(slot_waiting_for_keyframe(&router, route));

        // A non-keyframe packet, with a payload that would fail to parse
        // as Annex-B if it ever reached the decoder, must be skipped
        // *before* decode is attempted: if the skip-gate did not exist,
        // this call would instead return `Err(RouteError::Decode(_))`.
        let outcome = router
            .route_and_decode(&video_header(1, false), NOT_ANNEX_B_PAYLOAD)
            .expect("a skipped non-keyframe packet is never a routing error");
        assert_eq!(outcome, RouteOutcome::NoOutputYet);

        // Proof the decoder was never touched: a real decode attempt on
        // `NOT_ANNEX_B_PAYLOAD` always fails, which would have shown up as
        // a rejected frame. Only the routed counter advances.
        assert_eq!(router.frames_routed(sid(1)), Some(1));
        assert_eq!(router.frames_rejected(sid(1)), Some(0));
        assert!(router.latest_frame(sid(1)).is_none());
        // Still waiting: a skipped packet never satisfies this slot's own
        // keyframe recovery.
        assert!(slot_waiting_for_keyframe(&router, route));
    }

    #[test]
    fn a_keyframe_flagged_packet_is_attempted_and_a_malformed_one_arms_recovery_via_decode_error() {
        let ids = [sid(1), sid(2)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        let route1 = MonitorRoute::Negotiated(sid(1));
        let route2 = MonitorRoute::Negotiated(sid(2));

        // Unlike the non-keyframe case, a keyframe-flagged header must
        // always be attempted against the real decoder, even while this
        // slot is already waiting for a keyframe.
        let error = router
            .route_and_decode(&video_header(1, true), NOT_ANNEX_B_PAYLOAD)
            .expect_err("a malformed keyframe payload must fail decode, not be silently skipped");
        assert!(matches!(error, RouteError::Decode(_)));

        assert_eq!(router.frames_rejected(sid(1)), Some(1));
        assert_eq!(router.frames_routed(sid(1)), Some(0));
        assert!(slot_waiting_for_keyframe(&router, route1));

        // Monitor 2's slot is completely untouched by monitor 1's decode
        // failure: no cross-route contamination of counters or recovery
        // state between independent monitor slots.
        assert_eq!(router.frames_rejected(sid(2)), Some(0));
        assert_eq!(router.frames_routed(sid(2)), Some(0));
        assert!(slot_waiting_for_keyframe(&router, route2));
    }

    #[test]
    fn reserved_colour_flags_are_hard_decode_errors_that_arm_recovery() {
        let ids = [sid(1)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        let route = MonitorRoute::Negotiated(sid(1));

        let mut invalid_depth = video_header(1, true);
        invalid_depth.flags |= wire::VIDEO_BIT_DEPTH_MASK;
        let error = router
            .route_and_decode(&invalid_depth, NON_KEYFRAME_SLICE_PAYLOAD)
            .expect_err("reserved bit depth must never fall back to eight-bit");
        assert!(matches!(
            error,
            RouteError::Decode(VideoDecodeError::InvalidWireColor(
                ProtocolError::UnknownBitDepth(3)
            ))
        ));

        let mut invalid_matrix = video_header(1, true);
        invalid_matrix.flags |= 0x40;
        let error = router
            .route_and_decode(&invalid_matrix, NON_KEYFRAME_SLICE_PAYLOAD)
            .expect_err("reserved matrix must never fall back to BT.709");
        assert!(matches!(
            error,
            RouteError::Decode(VideoDecodeError::InvalidWireColor(
                ProtocolError::UnknownColorMatrix(4)
            ))
        ));

        assert_eq!(router.frames_rejected(sid(1)), Some(2));
        assert!(slot_waiting_for_keyframe(&router, route));
    }

    #[test]
    fn no_output_yet_never_clears_a_slots_cached_frame_and_rearms_from_the_decoders_own_signal() {
        let ids = [sid(1)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        let route = MonitorRoute::Negotiated(sid(1));

        // Simulate an already-synced, presenting slot: a cached frame is
        // in place and this monitor's own recovery gate is clear.
        router
            .route_decoded_frame(gen(1), sid(1), frame(0xCD))
            .expect("seeds a presenting frame");
        router
            .slots
            .get_mut(&route)
            .expect("route is admitted")
            .waiting_for_keyframe = false;

        // A syntactically valid but parameter-set-less, non-keyframe
        // slice reaches the real decoder (the route-level gate is clear)
        // and deterministically yields `Ok(None)`: admitted, no fresh
        // output.
        let outcome = router
            .route_and_decode(&video_header(1, false), NON_KEYFRAME_SLICE_PAYLOAD)
            .expect("an admitted packet that yields no picture is not a routing error");
        assert_eq!(outcome, RouteOutcome::NoOutputYet);

        // The stale cached frame from before must never be cleared by a
        // `NoOutputYet` outcome.
        assert_eq!(
            router
                .latest_frame(sid(1))
                .expect("cached frame is preserved")
                .rgba,
            vec![0xCD; 16]
        );
        // The decoder's own signal that it now wants a keyframe re-arms
        // this slot's recovery gate.
        assert!(slot_waiting_for_keyframe(&router, route));
    }

    #[test]
    fn notify_discontinuity_rearms_every_slot_independently_and_never_clears_any_cached_frame() {
        let ids = [sid(1), sid(2)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        let route1 = MonitorRoute::Negotiated(sid(1));
        let route2 = MonitorRoute::Negotiated(sid(2));

        router
            .route_decoded_frame(gen(1), sid(1), frame(0x11))
            .expect("seeds monitor 1's presenting frame");
        router
            .route_decoded_frame(gen(1), sid(2), frame(0x22))
            .expect("seeds monitor 2's presenting frame");
        for route in [route1, route2] {
            router
                .slots
                .get_mut(&route)
                .expect("route is admitted")
                .waiting_for_keyframe = false;
        }
        assert!(!slot_waiting_for_keyframe(&router, route1));
        assert!(!slot_waiting_for_keyframe(&router, route2));

        router.notify_discontinuity();

        // Every admitted slot is independently re-armed...
        assert!(slot_waiting_for_keyframe(&router, route1));
        assert!(slot_waiting_for_keyframe(&router, route2));
        // ...but neither slot's cached (now stale, pending recovery) frame
        // is cleared -- presentation keeps showing the last good frame
        // while each monitor recovers on its own.
        assert_eq!(
            router.latest_frame(sid(1)).expect("still cached").rgba,
            vec![0x11; 16]
        );
        assert_eq!(
            router.latest_frame(sid(2)).expect("still cached").rgba,
            vec![0x22; 16]
        );

        // A subsequent non-keyframe packet to monitor 1 only is skipped by
        // its own gate and never touches monitor 2's independent counters.
        let outcome = router
            .route_and_decode(&video_header(1, false), NOT_ANNEX_B_PAYLOAD)
            .expect("skipped, not a routing error");
        assert_eq!(outcome, RouteOutcome::NoOutputYet);
        assert_eq!(router.frames_rejected(sid(1)), Some(0));
        assert_eq!(router.frames_routed(sid(1)), Some(2));
        assert_eq!(router.frames_routed(sid(2)), Some(1));
        assert_eq!(router.frames_rejected(sid(2)), Some(0));
    }

    #[test]
    fn any_waiting_for_keyframe_stays_true_until_every_admitted_monitor_recovers() {
        // Release-candidate media finding #1: a fresh two-monitor router
        // starts with every slot waiting for its own keyframe.
        let ids = [sid(1), sid(2)];
        let mut router = MonitorFrameRouter::new(gen(1), &ids).expect("valid roster");
        let route1 = MonitorRoute::Negotiated(sid(1));
        let route2 = MonitorRoute::Negotiated(sid(2));
        assert!(router.any_waiting_for_keyframe());
        assert!(!router.all_recovered());

        // Monitor 1 receives its keyframe and recovers; monitor 2 has only
        // seen a delta (skipped, still waiting). The router-wide signal
        // must still report "not all recovered" -- one recovered monitor
        // must never let a caller cancel the shared full-frame-request
        // gate while a sibling is still stuck waiting.
        router.force_recovered_for_test(route1);
        let outcome = router
            .route_and_decode(&video_header(2, false), NOT_ANNEX_B_PAYLOAD)
            .expect("a skipped delta packet is never a routing error");
        assert_eq!(outcome, RouteOutcome::NoOutputYet);
        assert!(slot_waiting_for_keyframe(&router, route2));
        assert!(
            router.any_waiting_for_keyframe(),
            "monitor 2 is still waiting for its own keyframe",
        );
        assert!(!router.all_recovered());

        // Monitor 2 now recovers too: only once *every* admitted monitor
        // has recovered does the router-wide signal flip.
        router.force_recovered_for_test(route2);
        assert!(!router.any_waiting_for_keyframe());
        assert!(router.all_recovered());

        // A discontinuity resets every admitted monitor's own recovery gate
        // independently, regardless of how recovered they were a moment
        // ago.
        router.notify_discontinuity();
        assert!(router.any_waiting_for_keyframe());
        assert!(!router.all_recovered());
        assert!(slot_waiting_for_keyframe(&router, route1));
        assert!(slot_waiting_for_keyframe(&router, route2));
    }
}
