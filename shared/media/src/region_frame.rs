//! Pure region-video frame admission for multi-monitor-v1 clients.
//!
//! Every client that presents region video (Deck today, the Windows client
//! next) has to answer the same question before a wire frame may touch a
//! decoder: *is this frame allowed into this monitor's slot right now?* That
//! decision is entirely pure -- it depends only on the committed roster, the
//! committed [`TopologyGeneration`], each region's [`RegionMediaPlan`], and
//! the frame's own `VideoHeader` -- so it lives here rather than in any one
//! client's routing code.
//!
//! This module deliberately owns **no** queues, decoders, textures, or UI:
//! [`RegionFrameRoster`] is an immutable fence, and
//! [`region_frame_delivery`] is a pure function of a caller-owned recovery
//! flag. A client keeps its own per-monitor decoder/latest-frame slots and
//! its own keyframe-recovery state, and asks this module whether a frame is
//! admissible and whether it should reach the decoder at all.
//!
//! # Ordering
//!
//! [`RegionFrameRoster::admit_frame`] rejects in exactly this order, and
//! callers depend on it:
//!
//! 1. a zero or mismatched wire `topology_generation`
//!    ([`RegionFrameAdmissionError::StaleGeneration`]),
//! 2. a monitor id outside the committed roster
//!    ([`RegionFrameAdmissionError::UnroutedMonitor`]),
//! 3. a zero or mismatched wire `stream_epoch`
//!    ([`RegionFrameAdmissionError::StaleStreamEpoch`]),
//! 4. a codec/chroma that does not match the region's advertised plan
//!    ([`RegionFrameAdmissionError::WireProfileMismatch`]).
//!
//! Keyframe/recovery fencing is deliberately *not* an admission error: a
//! non-keyframe arriving while a region is still recovering is a legitimate
//! frame that is simply skipped before the decoder, which is what
//! [`region_frame_delivery`] expresses.

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};

use arcen_protocol::{
    ChromaSubsampling as WireChromaSubsampling, VideoCodec as WireVideoCodec, VideoHeader,
};

use crate::{
    ChromaSubsampling, MAX_MULTI_MONITOR_COUNT, MediaStreamEpoch, RegionMediaPlan,
    RegionMediaRoster, SessionMonitorId, TopologyGeneration, VideoCodec,
};

/// A frame's routed destination: either a legacy single-monitor wire frame
/// (`VideoHeader.monitor_id == 0`), or a negotiated per-monitor route
/// identified by a nonzero [`SessionMonitorId`] (wire `monitor_id`
/// `1..=65535`).
///
/// [`SessionMonitorId`] is nonzero by construction and represents only a
/// host-negotiated per-monitor route, so wire id `0` -- which is *not* a
/// negotiated session monitor id -- must never be modelled by constructing a
/// zero [`SessionMonitorId`]. This enum makes that distinction explicit at
/// the type level.
///
/// Ordered so [`Self::LegacyPrimary`] sorts before every [`Self::Negotiated`]
/// id, which is only observable in [`RegionFrameRoster::routes`]'s iteration
/// order; no production roster mixes the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MonitorRoute {
    /// Wire `monitor_id == 0`, the legacy single-monitor frame.
    LegacyPrimary,
    /// Wire `monitor_id` `1..=65535`, a host-negotiated per-monitor route.
    Negotiated(SessionMonitorId),
}

impl MonitorRoute {
    /// Classifies a raw wire `VideoHeader.monitor_id` into its route: `0` is
    /// always [`Self::LegacyPrimary`]; any nonzero value is
    /// [`Self::Negotiated`] for that session monitor id. Total and
    /// infallible -- every `u16` has an unambiguous route.
    #[must_use]
    pub fn from_wire_monitor_id(wire_monitor_id: u16) -> Self {
        match SessionMonitorId::new(wire_monitor_id) {
            Ok(id) => Self::Negotiated(id),
            Err(_) => Self::LegacyPrimary,
        }
    }

    /// The raw wire `monitor_id` this route corresponds to: `0` for
    /// [`Self::LegacyPrimary`], the session monitor id's value for
    /// [`Self::Negotiated`]. Exact inverse of [`Self::from_wire_monitor_id`].
    #[must_use]
    pub const fn wire_monitor_id(self) -> u16 {
        match self {
            Self::LegacyPrimary => 0,
            Self::Negotiated(id) => id.get(),
        }
    }
}

impl Display for MonitorRoute {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::LegacyPrimary => formatter.write_str("legacy-primary (wire monitor id 0)"),
            Self::Negotiated(id) => {
                write!(formatter, "negotiated session monitor id {}", id.get())
            }
        }
    }
}

/// Failure building a [`RegionFrameRoster`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionFrameRosterError {
    /// No monitor identifiers were supplied.
    EmptyRoster,
    /// More monitor identifiers were supplied than multi-monitor-v1 supports.
    TooManyMonitors(usize),
    /// The same route (wire monitor id) was supplied more than once.
    DuplicateMonitor(u16),
}

impl Display for RegionFrameRosterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRoster => formatter.write_str("monitor router roster is empty"),
            Self::TooManyMonitors(count) => write!(
                formatter,
                "{count} monitors exceed the {MAX_MULTI_MONITOR_COUNT}-monitor router limit"
            ),
            Self::DuplicateMonitor(wire_monitor_id) => {
                write!(
                    formatter,
                    "duplicate route for wire monitor id {wire_monitor_id} in router roster"
                )
            }
        }
    }
}

impl std::error::Error for RegionFrameRosterError {}

/// Failure admitting one wire region frame against a [`RegionFrameRoster`].
///
/// Distinct from a decode failure: admission is rejected before any decoder
/// is touched, which is what proves routing isolation (a frame for an
/// unrouted, stale-generation, stale-epoch, or wrong-profile monitor can
/// never reach another monitor's decoder or overwrite its latest frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionFrameAdmissionError {
    /// The wire `monitor_id` is not part of this roster.
    UnroutedMonitor(u16),
    /// The frame's topology generation does not match the generation this
    /// roster was committed for.
    StaleGeneration {
        /// The committed generation.
        expected: u64,
        /// The generation the frame carried (`0` when the wire value was not
        /// a valid nonzero generation at all).
        actual: u64,
    },
    /// The frame belongs to a replaced region stream.
    StaleStreamEpoch {
        /// The committed epoch for this region.
        expected: u64,
        /// The epoch the frame carried (`0` when the wire value was not a
        /// valid nonzero epoch at all).
        actual: u64,
    },
    /// The frame header does not match this region's advertised codec/chroma.
    WireProfileMismatch {
        /// The wire monitor id whose advertised plan was violated.
        monitor_id: u16,
    },
}

impl Display for RegionFrameAdmissionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnroutedMonitor(id) => {
                write!(
                    formatter,
                    "monitor id {id} is not part of the admitted roster"
                )
            }
            Self::StaleGeneration { expected, actual } => write!(
                formatter,
                "topology generation {actual} is stale; router is fenced to generation {expected}"
            ),
            Self::StaleStreamEpoch { expected, actual } => write!(
                formatter,
                "stream epoch {actual} is stale; route is fenced to epoch {expected}"
            ),
            Self::WireProfileMismatch { monitor_id } => write!(
                formatter,
                "monitor id {monitor_id} frame does not match its advertised codec/chroma plan"
            ),
        }
    }
}

impl std::error::Error for RegionFrameAdmissionError {}

/// Whether an already-admitted frame may reach this region's decoder now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionFrameDelivery {
    /// Hand the payload to this region's decoder.
    Decode,
    /// Skip the payload *before* the decoder: this region's own recovery is
    /// pending and the frame is not a keyframe. The caller's previously
    /// cached frame (if any) must be left untouched -- it is stale and must
    /// never be presented as fresh -- and this region's keyframe wait must
    /// not be cleared.
    SkipUntilKeyframe,
}

/// Whether the frame's wire codec/chroma match the region's advertised plan.
///
/// The wire vocabulary ([`arcen_protocol`]) and the media domain vocabulary
/// ([`crate::VideoCodec`] / [`crate::ChromaSubsampling`]) are deliberately
/// separate types, so this is the one place the two are related.
#[must_use]
pub fn wire_profile_matches(header: &VideoHeader, plan: RegionMediaPlan) -> bool {
    let codec_matches = matches!(
        (header.codec, plan.video.codec),
        (WireVideoCodec::Jpeg, VideoCodec::Jpeg)
            | (WireVideoCodec::H264, VideoCodec::H264)
            | (WireVideoCodec::H265, VideoCodec::H265)
            | (WireVideoCodec::Vp9, VideoCodec::Vp9)
            | (WireVideoCodec::Av1, VideoCodec::Av1)
    );
    let chroma_matches = matches!(
        (header.chroma, plan.video.chroma),
        (WireChromaSubsampling::Yuv420, ChromaSubsampling::Yuv420)
            | (WireChromaSubsampling::Yuv422, ChromaSubsampling::Yuv422)
            | (WireChromaSubsampling::Yuv444, ChromaSubsampling::Yuv444)
    );
    codec_matches && chroma_matches
}

/// Whether an admitted frame may reach the decoder, given this region's own
/// keyframe-recovery state.
///
/// `waiting_for_keyframe` is the caller's per-region recovery flag -- never a
/// session-wide one: one region's discontinuity must never gate another's.
#[must_use]
pub fn region_frame_delivery(
    waiting_for_keyframe: bool,
    header: &VideoHeader,
) -> RegionFrameDelivery {
    if waiting_for_keyframe && !header.is_keyframe() {
        RegionFrameDelivery::SkipUntilKeyframe
    } else {
        RegionFrameDelivery::Decode
    }
}

/// The committed, immutable fence one client session admits region frames
/// against: the routes it accepts, each route's advertised media plan, and
/// the topology generation the whole roster is pinned to.
///
/// A new topology means building a new roster after a fresh connection,
/// never mutating this one's routes or generation in place -- which is why
/// every method here takes `&self`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionFrameRoster {
    generation: TopologyGeneration,
    routes: BTreeMap<MonitorRoute, Option<RegionMediaPlan>>,
}

impl RegionFrameRoster {
    /// Builds a roster admitting exactly `routes`, fenced to `generation`.
    ///
    /// A `None` plan means the route is admitted without a codec/chroma or
    /// stream-epoch contract, so [`Self::admit_frame`] checks only the
    /// generation and roster membership for it.
    ///
    /// # Errors
    ///
    /// Returns [`RegionFrameRosterError`] when the roster is empty, exceeds
    /// [`MAX_MULTI_MONITOR_COUNT`], or repeats a route.
    pub fn new(
        generation: TopologyGeneration,
        routes: &[(MonitorRoute, Option<RegionMediaPlan>)],
    ) -> Result<Self, RegionFrameRosterError> {
        if routes.is_empty() {
            return Err(RegionFrameRosterError::EmptyRoster);
        }
        if routes.len() > MAX_MULTI_MONITOR_COUNT {
            return Err(RegionFrameRosterError::TooManyMonitors(routes.len()));
        }
        let mut map = BTreeMap::new();
        for (route, plan) in routes {
            if map.insert(*route, *plan).is_some() {
                return Err(RegionFrameRosterError::DuplicateMonitor(
                    route.wire_monitor_id(),
                ));
            }
        }
        Ok(Self {
            generation,
            routes: map,
        })
    }

    /// Builds a roster admitting exactly the negotiated `monitor_ids`, with
    /// no per-region media plan. Every admitted route is
    /// [`MonitorRoute::Negotiated`]; this constructor can never admit the
    /// legacy primary route (see [`Self::legacy_primary`]).
    ///
    /// # Errors
    ///
    /// See [`Self::new`].
    pub fn negotiated(
        generation: TopologyGeneration,
        monitor_ids: &[SessionMonitorId],
    ) -> Result<Self, RegionFrameRosterError> {
        let routes: Vec<(MonitorRoute, Option<RegionMediaPlan>)> = monitor_ids
            .iter()
            .copied()
            .map(|id| (MonitorRoute::Negotiated(id), None))
            .collect();
        Self::new(generation, &routes)
    }

    /// Builds a roster from the complete host-authoritative media roster, so
    /// every route keeps its own codec/chroma/epoch contract.
    ///
    /// # Errors
    ///
    /// See [`Self::new`].
    pub fn from_media_roster(
        generation: TopologyGeneration,
        roster: &RegionMediaRoster,
    ) -> Result<Self, RegionFrameRosterError> {
        let routes: Vec<(MonitorRoute, Option<RegionMediaPlan>)> = roster
            .plans()
            .iter()
            .copied()
            .map(|plan| {
                (
                    MonitorRoute::Negotiated(plan.session_monitor_id),
                    Some(plan),
                )
            })
            .collect();
        Self::new(generation, &routes)
    }

    /// A roster admitting only [`MonitorRoute::LegacyPrimary`] (wire id `0`)
    /// at generation `1`, preserving legacy single-monitor behavior. Never
    /// admits a negotiated [`SessionMonitorId`].
    #[must_use]
    pub fn legacy_primary() -> Self {
        let generation = TopologyGeneration::new(1).unwrap_or_else(|_| unreachable!("1 != 0"));
        Self::new(generation, &[(MonitorRoute::LegacyPrimary, None)])
            .unwrap_or_else(|_| unreachable!("a single legacy-primary route is always valid"))
    }

    /// The topology generation this roster is fenced to.
    #[must_use]
    pub const fn generation(&self) -> TopologyGeneration {
        self.generation
    }

    /// The admitted route roster, in ascending order (see [`MonitorRoute`]'s
    /// `Ord`).
    pub fn routes(&self) -> impl Iterator<Item = MonitorRoute> + '_ {
        self.routes.keys().copied()
    }

    /// The admitted negotiated session-monitor-id roster, in ascending
    /// order, excluding [`MonitorRoute::LegacyPrimary`] when present.
    pub fn monitor_ids(&self) -> impl Iterator<Item = SessionMonitorId> + '_ {
        self.routes.keys().filter_map(|route| match route {
            MonitorRoute::Negotiated(id) => Some(*id),
            MonitorRoute::LegacyPrimary => None,
        })
    }

    /// Whether `route` is part of this roster.
    #[must_use]
    pub fn contains(&self, route: MonitorRoute) -> bool {
        self.routes.contains_key(&route)
    }

    /// This route's advertised media plan, or `None` when the route is
    /// unrouted or was admitted without a plan.
    #[must_use]
    pub fn plan(&self, route: MonitorRoute) -> Option<RegionMediaPlan> {
        self.routes.get(&route).copied().flatten()
    }

    /// Admits `route` at `generation`, checking only the fence and roster
    /// membership -- the two checks that apply even to an already-decoded
    /// frame injected without a wire header.
    ///
    /// # Errors
    ///
    /// Returns [`RegionFrameAdmissionError::StaleGeneration`] before
    /// [`RegionFrameAdmissionError::UnroutedMonitor`].
    pub fn admit_route(
        &self,
        generation: TopologyGeneration,
        route: MonitorRoute,
    ) -> Result<(), RegionFrameAdmissionError> {
        if generation != self.generation {
            return Err(RegionFrameAdmissionError::StaleGeneration {
                expected: self.generation.get(),
                actual: generation.get(),
            });
        }
        if !self.routes.contains_key(&route) {
            return Err(RegionFrameAdmissionError::UnroutedMonitor(
                route.wire_monitor_id(),
            ));
        }
        Ok(())
    }

    /// Admits one wire frame, returning the [`MonitorRoute`] it belongs to.
    ///
    /// Checks, in order: the frame's topology generation, roster membership,
    /// then -- only for a route carrying a [`RegionMediaPlan`] -- the stream
    /// epoch and the codec/chroma profile. Keyframe/recovery fencing is a
    /// separate, non-error decision (see [`region_frame_delivery`]).
    ///
    /// # Errors
    ///
    /// See [`RegionFrameAdmissionError`] and this module's ordering contract.
    pub fn admit_frame(
        &self,
        header: &VideoHeader,
    ) -> Result<MonitorRoute, RegionFrameAdmissionError> {
        let route = MonitorRoute::from_wire_monitor_id(header.monitor_id);
        let generation = TopologyGeneration::new(header.topology_generation).map_err(|_| {
            RegionFrameAdmissionError::StaleGeneration {
                expected: self.generation.get(),
                actual: header.topology_generation,
            }
        })?;
        self.admit_route(generation, route)?;
        if let Some(plan) = self.plan(route) {
            let actual = MediaStreamEpoch::new(header.stream_epoch).map_err(|_| {
                RegionFrameAdmissionError::StaleStreamEpoch {
                    expected: plan.stream_epoch.get(),
                    actual: header.stream_epoch,
                }
            })?;
            if actual != plan.stream_epoch {
                return Err(RegionFrameAdmissionError::StaleStreamEpoch {
                    expected: plan.stream_epoch.get(),
                    actual: actual.get(),
                });
            }
            if !wire_profile_matches(header, plan) {
                return Err(RegionFrameAdmissionError::WireProfileMismatch {
                    monitor_id: header.monitor_id,
                });
            }
        }
        Ok(route)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::EncoderBackend;
    use crate::{BitrateBudgetKbps, VideoConfiguration};
    use arcen_protocol::FrameType;

    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("nonzero session monitor id")
    }

    fn generation(value: u64) -> TopologyGeneration {
        TopologyGeneration::new(value).expect("nonzero generation")
    }

    fn plan(
        monitor_id: u16,
        epoch: u64,
        codec: VideoCodec,
        chroma: ChromaSubsampling,
    ) -> RegionMediaPlan {
        RegionMediaPlan::new(
            sid(monitor_id),
            MediaStreamEpoch::new(epoch).expect("nonzero epoch"),
            EncoderBackend::OpenH264,
            VideoConfiguration {
                codec,
                chroma,
                ..VideoConfiguration::legacy_h264()
            },
            1920,
            1080,
            60,
            BitrateBudgetKbps::new(8_000).expect("in-band budget"),
        )
        .expect("valid region media plan")
    }

    fn header(
        monitor_id: u16,
        topology_generation: u64,
        stream_epoch: u64,
        keyframe: bool,
    ) -> VideoHeader {
        VideoHeader {
            frame_type: if monitor_id == 0 {
                FrameType::VideoH264
            } else {
                FrameType::RegionVideoH264
            },
            codec: WireVideoCodec::H264,
            chroma: WireChromaSubsampling::Yuv420,
            flags: u8::from(keyframe),
            timestamp_ms: 0,
            monitor_id,
            topology_generation,
            stream_epoch,
        }
    }

    fn two_region_roster() -> RegionFrameRoster {
        let roster = RegionMediaRoster::new(vec![
            plan(1, 11, VideoCodec::H264, ChromaSubsampling::Yuv420),
            plan(2, 12, VideoCodec::H265, ChromaSubsampling::Yuv420),
        ])
        .expect("valid media roster");
        RegionFrameRoster::from_media_roster(generation(7), &roster).expect("valid frame roster")
    }

    #[test]
    fn wire_monitor_id_zero_is_always_the_legacy_primary_route() {
        assert_eq!(
            MonitorRoute::from_wire_monitor_id(0),
            MonitorRoute::LegacyPrimary
        );
        assert_eq!(MonitorRoute::LegacyPrimary.wire_monitor_id(), 0);
        for id in [1_u16, 2, 4, 65_535] {
            assert_eq!(
                MonitorRoute::from_wire_monitor_id(id),
                MonitorRoute::Negotiated(sid(id))
            );
            assert_eq!(MonitorRoute::from_wire_monitor_id(id).wire_monitor_id(), id);
        }
    }

    #[test]
    fn empty_too_many_and_duplicate_rosters_are_rejected() {
        assert_eq!(
            RegionFrameRoster::negotiated(generation(1), &[]).expect_err("empty roster"),
            RegionFrameRosterError::EmptyRoster
        );
        let five: Vec<SessionMonitorId> = (1..=5).map(sid).collect();
        assert_eq!(
            RegionFrameRoster::negotiated(generation(1), &five).expect_err("five monitors"),
            RegionFrameRosterError::TooManyMonitors(5)
        );
        assert_eq!(
            RegionFrameRoster::negotiated(generation(1), &[sid(1), sid(1)])
                .expect_err("duplicate route"),
            RegionFrameRosterError::DuplicateMonitor(1)
        );
    }

    #[test]
    fn stale_topology_generation_is_rejected_before_roster_membership() {
        let roster = two_region_roster();
        // Wire generation 6 against a roster fenced to 7, for a monitor id
        // that is *also* unrouted: generation must be reported first.
        assert_eq!(
            roster
                .admit_frame(&header(9, 6, 11, true))
                .expect_err("stale generation"),
            RegionFrameAdmissionError::StaleGeneration {
                expected: 7,
                actual: 6
            }
        );
        // A zero wire generation is never a valid generation at all and is
        // reported with its raw zero value.
        assert_eq!(
            roster
                .admit_frame(&header(1, 0, 11, true))
                .expect_err("zero generation"),
            RegionFrameAdmissionError::StaleGeneration {
                expected: 7,
                actual: 0
            }
        );
    }

    #[test]
    fn unknown_monitor_id_is_rejected_before_epoch_and_profile() {
        let roster = two_region_roster();
        assert_eq!(
            roster
                .admit_frame(&header(9, 7, 999, true))
                .expect_err("unrouted monitor"),
            RegionFrameAdmissionError::UnroutedMonitor(9)
        );
        // The legacy primary route is unrouted on a negotiated-only roster.
        assert_eq!(
            roster
                .admit_frame(&header(0, 7, 11, true))
                .expect_err("legacy primary is unrouted here"),
            RegionFrameAdmissionError::UnroutedMonitor(0)
        );
    }

    #[test]
    fn stale_stream_epoch_is_rejected_per_region() {
        let roster = two_region_roster();
        assert_eq!(
            roster
                .admit_frame(&header(1, 7, 12, true))
                .expect_err("region 1 is fenced to epoch 11"),
            RegionFrameAdmissionError::StaleStreamEpoch {
                expected: 11,
                actual: 12
            }
        );
        assert_eq!(
            roster
                .admit_frame(&header(2, 7, 0, true))
                .expect_err("zero epoch is never valid"),
            RegionFrameAdmissionError::StaleStreamEpoch {
                expected: 12,
                actual: 0
            }
        );
        assert_eq!(
            roster
                .admit_frame(&header(1, 7, 11, true))
                .expect("matching epoch admits"),
            MonitorRoute::Negotiated(sid(1))
        );
    }

    #[test]
    fn profile_mismatch_is_checked_per_region_never_from_the_first_plan() {
        let roster = two_region_roster();
        // Region 2 advertised H.265; an H.264 frame for it is rejected even
        // though region 1 legitimately carries H.264.
        assert_eq!(
            roster
                .admit_frame(&header(2, 7, 12, true))
                .expect_err("region 2 advertised h265"),
            RegionFrameAdmissionError::WireProfileMismatch { monitor_id: 2 }
        );
        let mut chroma_mismatch = header(1, 7, 11, true);
        chroma_mismatch.chroma = WireChromaSubsampling::Yuv444;
        assert_eq!(
            roster
                .admit_frame(&chroma_mismatch)
                .expect_err("region 1 advertised yuv420"),
            RegionFrameAdmissionError::WireProfileMismatch { monitor_id: 1 }
        );
    }

    #[test]
    fn a_planless_route_checks_only_the_generation_and_membership() {
        let roster =
            RegionFrameRoster::negotiated(generation(3), &[sid(1)]).expect("valid frame roster");
        // Any epoch, including a wire zero, and any profile is admitted for
        // a route with no advertised plan.
        assert_eq!(
            roster
                .admit_frame(&header(1, 3, 0, false))
                .expect("planless route admits"),
            MonitorRoute::Negotiated(sid(1))
        );
    }

    #[test]
    fn legacy_primary_roster_admits_only_wire_monitor_id_zero() {
        let roster = RegionFrameRoster::legacy_primary();
        assert_eq!(roster.generation().get(), 1);
        assert_eq!(
            roster.admit_frame(&header(0, 1, 0, true)).expect("legacy"),
            MonitorRoute::LegacyPrimary
        );
        assert_eq!(
            roster
                .admit_frame(&header(1, 1, 1, true))
                .expect_err("negotiated id is unrouted here"),
            RegionFrameAdmissionError::UnroutedMonitor(1)
        );
        assert!(roster.monitor_ids().next().is_none());
    }

    #[test]
    fn admit_route_fences_injected_frames_on_generation_then_membership() {
        let roster = two_region_roster();
        assert_eq!(
            roster
                .admit_route(generation(6), MonitorRoute::Negotiated(sid(1)))
                .expect_err("stale generation"),
            RegionFrameAdmissionError::StaleGeneration {
                expected: 7,
                actual: 6
            }
        );
        assert_eq!(
            roster
                .admit_route(generation(7), MonitorRoute::LegacyPrimary)
                .expect_err("legacy primary is unrouted here"),
            RegionFrameAdmissionError::UnroutedMonitor(0)
        );
        roster
            .admit_route(generation(7), MonitorRoute::Negotiated(sid(2)))
            .expect("routed monitor at the committed generation");
    }

    #[test]
    fn a_non_keyframe_after_a_discontinuity_is_skipped_before_the_decoder() {
        let non_keyframe = header(1, 7, 11, false);
        let keyframe = header(1, 7, 11, true);
        assert_eq!(
            region_frame_delivery(true, &non_keyframe),
            RegionFrameDelivery::SkipUntilKeyframe
        );
        assert_eq!(
            region_frame_delivery(true, &keyframe),
            RegionFrameDelivery::Decode
        );
        // Once a region has recovered, every packet reaches the decoder.
        assert_eq!(
            region_frame_delivery(false, &non_keyframe),
            RegionFrameDelivery::Decode
        );
        assert_eq!(
            region_frame_delivery(false, &keyframe),
            RegionFrameDelivery::Decode
        );
    }

    #[test]
    fn a_full_frame_type_counts_as_a_keyframe_without_the_flag() {
        let mut full = header(1, 7, 11, false);
        full.frame_type = FrameType::RegionVideoFull;
        assert!(full.is_keyframe());
        assert_eq!(
            region_frame_delivery(true, &full),
            RegionFrameDelivery::Decode
        );
    }

    #[test]
    fn keyframe_fencing_is_never_an_admission_error() {
        let roster = two_region_roster();
        // A non-keyframe frame with an otherwise valid header is admitted;
        // only the separate delivery decision skips it.
        let admitted = roster
            .admit_frame(&header(1, 7, 11, false))
            .expect("a non-keyframe is still admissible");
        assert_eq!(admitted, MonitorRoute::Negotiated(sid(1)));
    }

    #[test]
    fn roster_exposes_its_routes_plans_and_membership() {
        let roster = two_region_roster();
        assert_eq!(
            roster.routes().collect::<Vec<_>>(),
            vec![
                MonitorRoute::Negotiated(sid(1)),
                MonitorRoute::Negotiated(sid(2))
            ]
        );
        assert_eq!(
            roster.monitor_ids().collect::<Vec<_>>(),
            vec![sid(1), sid(2)]
        );
        assert!(roster.contains(MonitorRoute::Negotiated(sid(1))));
        assert!(!roster.contains(MonitorRoute::LegacyPrimary));
        assert_eq!(
            roster
                .plan(MonitorRoute::Negotiated(sid(2)))
                .expect("region 2 has a plan")
                .video
                .codec,
            VideoCodec::H265
        );
        assert!(roster.plan(MonitorRoute::LegacyPrimary).is_none());
    }
}
