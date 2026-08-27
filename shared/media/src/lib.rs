//! Transport-independent media and monitor capability contracts.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use arcen_protocol::messages::ClientDisplayIdError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};

pub use arcen_keel::{
    ActivityClass, ActivityDiagnostics, ActivityHint, CadenceRecommendation, DirtyRatio,
};
mod applied_topology;
pub mod audio;
pub mod clipboard;
mod encoder_admission;
mod multi_monitor;
mod region;
mod region_activity;
mod region_frame;
mod region_schedule;
pub mod test_pattern;
mod topology_placement;
pub mod video;

pub use applied_topology::{
    AppliedTopologyParts, AppliedTopologyValidationError, DesktopRect, MonitorDesktopRect,
    NativeDisplayResolver, ResolvedAppliedMonitor, ValidatedAppliedTopology,
    resolve_pointer_crossing, validate_applied_topology_for_production,
    validate_applied_topology_parts,
};
pub use arcen_protocol::messages::ClientDisplayId;
pub use encoder_admission::{
    EncoderAdmissionError, EncoderAdmissionThresholds, EncoderBindingId, EncoderMeasurementAdapter,
    EncoderProbeFailure, EncoderProbeFailureKind, EncoderProbeRequest, EncoderProbeSample,
    EncoderProbeTrace, EncoderSetAttempt, EncoderSetAttemptOutcome, EncoderSetCandidate,
    EncoderSetDecision, EncoderSetMeasurements, EncoderThresholdViolation,
    MAX_ENCODER_BINDING_ID_BYTES, MAX_ENCODER_PROBE_DURATION, MAX_ENCODER_PROBE_FRAMES_PER_REGION,
    MAX_ENCODER_PROBE_WARMUP_FRAMES, MAX_ENCODER_PROBE_WINDOW, MAX_ENCODER_SET_CANDIDATES,
    RegionActivityProfile, RegionActivityProfiles, RegionAdmissionPriority, RegionEncoderBinding,
    RegionEncoderMeasurements, RegionProbeFailure, RepresentativeFrame, RepresentativeFrameKind,
    admit_encoder_sets,
};
pub use multi_monitor::{
    AggregateMediaBudget, AggregateMediaPlan, AppliedMonitor, AppliedMonitorTopology,
    BitrateBudgetKbps, LayoutBounds, LayoutRect, LayoutTranslation, MAX_MULTI_MONITOR_COUNT,
    MediaStreamEpoch, PerMonitorMediaPlan, RegionMediaPlan, RegionMediaRoster, RequestedMonitor,
    RequestedMonitorTopology, SessionMonitorId, TopologyGeneration,
};
pub use region::{
    AppliedPoint, AppliedRect, AppliedRegionDescriptor, AppliedRegionSet, AppliedSize,
    LOGICAL_UNITS_PER_PIXEL, LogicalPoint, LogicalRect, LogicalSize, MAX_OUTPUT_IDENTITY_BYTES,
    MAX_REGION_COUNT, OutputIdentity, OutputTransform, PhysicalSize, RegionContractError,
    RegionDescriptor, RegionGeneration, RegionId, RegionSet, Scale120,
};
pub use region_activity::{
    RegionActivityDiagnostics, RegionActivityError, RegionActivityGrid, RegionActivityOwner,
};
pub use region_frame::{
    MonitorRoute, RegionFrameAdmissionError, RegionFrameDelivery, RegionFrameRoster,
    RegionFrameRosterError, region_frame_delivery, wire_profile_matches,
};
pub use region_schedule::{
    ForcedKeyframe, MAX_REGION_FRAME_INTERVAL, MAX_REGION_IDLE_REFRESH,
    MAX_REGION_KEYFRAME_INTERVAL, MIN_REGION_FRAME_INTERVAL, REGION_IDLE_BACKOFF_FACTOR,
    REGION_IDLE_BACKOFF_STREAK, REGION_SCHEDULE_TELEMETRY_FIELDS, RegionActivityScheduler,
    RegionSchedulePolicy, RegionSchedulePolicyError, RegionScheduleSignals, RegionScheduleSnapshot,
    RegionScheduleTelemetry, RegionServiceAction, RegionServiceDecision, RegionServiceReason,
};
pub use topology_placement::{
    LayoutSpace, OriginPolicy, PlacedLayout, RegionPlacement, SpacedLayoutRect,
    TopologyPlacementError, TransformConvention, applied_desktop_rect, apply_origin_policy,
    build_region_sets, checked_layout_bounds, logical_arrangement_rect,
    logical_origin_with_stream_extent_rect, logical_rect_from_layout, place_monitors,
    plan_edge_aware_offsets, scale120_from_scale,
};

/// Video codecs that Arcen peers may negotiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    /// JPEG intra frames retained for protocol-v3 compatibility.
    Jpeg,
    /// H.264/AVC.
    H264,
    /// H.265/HEVC.
    H265,
    /// VP9.
    Vp9,
    /// AV1.
    Av1,
}

/// Chroma formats that Arcen peers may negotiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ChromaSubsampling {
    /// 4:2:0.
    #[serde(rename = "yuv420")]
    Yuv420,
    /// 4:2:2.
    #[serde(rename = "yuv422")]
    Yuv422,
    /// 4:4:4.
    #[serde(rename = "yuv444")]
    Yuv444,
}

/// Coded sample range.
///
/// This is the difference between studio/video range, where 8-bit luma spans
/// 16..=235, and full range, where it spans 0..=255. Desktop content is
/// natively full-range RGB, so limited range discards roughly 14% of the
/// available code values *before* any coding loss and cannot represent
/// superblacks or superwhites distinctly. That is a correctness problem for
/// colour work, not an aesthetic preference, which is why range is a
/// negotiated part of the plan rather than an encoder implementation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorRange {
    /// Studio/video range. 8-bit luma 16..=235, chroma 16..=240.
    Limited,
    /// Full range. All code values carry picture data.
    Full,
}

/// Matrix coefficients used to derive luma and chroma from RGB.
///
/// [`ColorMatrix::Identity`] is the GBR passthrough of ITU-T H.273
/// `matrix_coefficients = 0`: no conversion happens at all and the coded
/// "luma" and "chroma" planes carry G, B and R directly. It is the only
/// mathematically exact option for screen content, but consuming it requires
/// a private render path — see `docs/architecture/color-fidelity.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorMatrix {
    /// H.273 value 0: identity/GBR. No RGB-to-YCbCr conversion.
    Identity,
    /// H.273 value 1: ITU-R BT.709.
    Bt709,
    /// H.273 value 6: ITU-R BT.601 625/525 line.
    Bt601,
    /// H.273 value 9: ITU-R BT.2020 non-constant luminance.
    Bt2020Ncl,
}

/// Colour primaries, as ITU-T H.273 `colour_primaries`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorPrimaries {
    /// H.273 value 1: ITU-R BT.709 / sRGB.
    Bt709,
    /// H.273 value 9: ITU-R BT.2020.
    Bt2020,
    /// H.273 value 12: SMPTE EG 432-1 (Display P3).
    DisplayP3,
}

/// Transfer characteristics, as ITU-T H.273 `transfer_characteristics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferCharacteristics {
    /// H.273 value 1: ITU-R BT.709.
    Bt709,
    /// H.273 value 13: IEC 61966-2-1 sRGB.
    Srgb,
    /// H.273 value 16: SMPTE ST 2084 (PQ).
    Pq,
    /// H.273 value 18: ARIB STD-B67 (HLG).
    Hlg,
}

impl VideoCodec {
    /// Every codec in the vocabulary, in declaration order.
    ///
    /// A new codec is added here and to [`VideoCodec::token`]. Everything that
    /// enumerates codecs iterates this, so nothing else needs a new match arm
    /// just to know the codec exists.
    pub const ALL: &'static [Self] = &[Self::Jpeg, Self::H264, Self::H265, Self::Vp9, Self::Av1];

    /// Stable wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
        }
    }

    /// Parse a wire token. Unknown codecs are `None` rather than a default, so
    /// a peer announcing something newer is never silently misread.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        let mut index = 0;
        while index < Self::ALL.len() {
            let codec = Self::ALL[index];
            if codec.token().as_bytes() == value.as_bytes() {
                return Some(codec);
            }
            index += 1;
        }
        None
    }

    /// Chroma formats Arcen offers for this codec.
    ///
    /// This is a product decision, not a codec capability. It is the widest
    /// thing Arcen will *ask* for; a backend contract and a runtime probe both
    /// narrow it. Keeping it as a table means the rule is stated once instead
    /// of being re-derived as a conditional wherever a plan is validated.
    #[must_use]
    pub const fn offered_chroma(self) -> ChromaSet {
        match self {
            // HEVC Rext covers 4:2:2 and 4:4:4 at every depth Arcen offers.
            Self::H265 => ChromaSet::from_slice(&[
                ChromaSubsampling::Yuv420,
                ChromaSubsampling::Yuv422,
                ChromaSubsampling::Yuv444,
            ]),
            // H.264 High 4:4:4 Predictive is offered so the encoder side can
            // be exercised, and AV1 Professional carries 4:4:4 in the software
            // tier. Whether a given client can decode either is a client
            // capability question, answered by the probe matrix rather than
            // assumed here.
            Self::H264 | Self::Av1 => {
                ChromaSet::from_slice(&[ChromaSubsampling::Yuv420, ChromaSubsampling::Yuv444])
            }
            Self::Jpeg | Self::Vp9 => ChromaSet::from_slice(&[ChromaSubsampling::Yuv420]),
        }
    }

    /// Bit depths Arcen offers for this codec.
    ///
    /// Companion policy table to [`VideoCodec::offered_chroma`]. Twelve-bit is
    /// offered only for AV1 because no other codec Arcen targets has a
    /// twelve-bit path on any encoder it uses — notably NVENC has none at all.
    #[must_use]
    pub const fn offered_bit_depths(self) -> BitDepthSet {
        match self {
            Self::Av1 => {
                BitDepthSet::from_slice(&[BitDepth::Eight, BitDepth::Ten, BitDepth::Twelve])
            }
            Self::H264 | Self::H265 | Self::Vp9 => {
                BitDepthSet::from_slice(&[BitDepth::Eight, BitDepth::Ten])
            }
            Self::Jpeg => BitDepthSet::from_slice(&[BitDepth::Eight]),
        }
    }

    /// Position of this codec in [`CodecSet`]. Derived from declaration order
    /// so a new codec cannot forget to claim a bit.
    const fn bit_index(self) -> u32 {
        match self {
            Self::Jpeg => 0,
            Self::H264 => 1,
            Self::H265 => 2,
            Self::Vp9 => 3,
            Self::Av1 => 4,
        }
    }
}

impl ChromaSubsampling {
    /// Every chroma format in the vocabulary.
    pub const ALL: &'static [Self] = &[Self::Yuv420, Self::Yuv422, Self::Yuv444];

    /// Stable wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Yuv420 => "yuv420",
            Self::Yuv422 => "yuv422",
            Self::Yuv444 => "yuv444",
        }
    }

    /// Parse a wire token; unknown values are `None`.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        let mut index = 0;
        while index < Self::ALL.len() {
            let chroma = Self::ALL[index];
            if chroma.token().as_bytes() == value.as_bytes() {
                return Some(chroma);
            }
            index += 1;
        }
        None
    }

    const fn bit_index(self) -> u32 {
        match self {
            Self::Yuv420 => 0,
            Self::Yuv422 => 1,
            Self::Yuv444 => 2,
        }
    }
}

impl ColorRange {
    /// Every range in the vocabulary.
    pub const ALL: &'static [Self] = &[Self::Limited, Self::Full];

    /// Stable wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Limited => "limited",
            Self::Full => "full",
        }
    }

    /// Parse a wire token; unknown values are `None`.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        let mut index = 0;
        while index < Self::ALL.len() {
            let range = Self::ALL[index];
            if range.token().as_bytes() == value.as_bytes() {
                return Some(range);
            }
            index += 1;
        }
        None
    }

    /// The H.264/HEVC VUI `video_full_range_flag` value for this range.
    #[must_use]
    pub const fn full_range_flag(self) -> u8 {
        match self {
            Self::Limited => 0,
            Self::Full => 1,
        }
    }

    /// Inclusive luma code bounds at `depth`.
    ///
    /// Limited range scales the classic 8-bit 16..=235 bounds by the depth, as
    /// ITU-T H.273 specifies; full range always spans the whole code space.
    #[must_use]
    pub const fn luma_bounds(self, depth: BitDepth) -> (u16, u16) {
        let shift = depth.bits() - 8;
        match self {
            Self::Limited => (16 << shift, 235 << shift),
            Self::Full => (0, (1 << depth.bits()) - 1),
        }
    }

    /// Inclusive chroma code bounds at `depth`.
    #[must_use]
    pub const fn chroma_bounds(self, depth: BitDepth) -> (u16, u16) {
        let shift = depth.bits() - 8;
        match self {
            Self::Limited => (16 << shift, 240 << shift),
            Self::Full => (0, (1 << depth.bits()) - 1),
        }
    }

    const fn bit_index(self) -> u32 {
        match self {
            Self::Limited => 0,
            Self::Full => 1,
        }
    }
}

impl ColorMatrix {
    /// Every matrix in the vocabulary.
    pub const ALL: &'static [Self] = &[Self::Identity, Self::Bt709, Self::Bt601, Self::Bt2020Ncl];

    /// Stable wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Bt709 => "bt709",
            Self::Bt601 => "bt601",
            Self::Bt2020Ncl => "bt2020ncl",
        }
    }

    /// Parse a wire token; unknown values are `None`.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        let mut index = 0;
        while index < Self::ALL.len() {
            let matrix = Self::ALL[index];
            if matrix.token().as_bytes() == value.as_bytes() {
                return Some(matrix);
            }
            index += 1;
        }
        None
    }

    /// ITU-T H.273 `matrix_coefficients` value written to the bitstream VUI.
    #[must_use]
    pub const fn h273_value(self) -> u8 {
        match self {
            Self::Identity => 0,
            Self::Bt709 => 1,
            Self::Bt601 => 6,
            Self::Bt2020Ncl => 9,
        }
    }

    /// Whether this matrix performs no conversion, so the coded planes carry
    /// G, B and R directly.
    #[must_use]
    pub const fn is_identity(self) -> bool {
        matches!(self, Self::Identity)
    }
}

impl ColorPrimaries {
    /// Every primaries value in the vocabulary.
    pub const ALL: &'static [Self] = &[Self::Bt709, Self::Bt2020, Self::DisplayP3];

    /// Stable wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Bt709 => "bt709",
            Self::Bt2020 => "bt2020",
            Self::DisplayP3 => "display_p3",
        }
    }

    /// Parse a wire token; unknown values are `None`.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        let mut index = 0;
        while index < Self::ALL.len() {
            let primaries = Self::ALL[index];
            if primaries.token().as_bytes() == value.as_bytes() {
                return Some(primaries);
            }
            index += 1;
        }
        None
    }

    /// ITU-T H.273 `colour_primaries` value written to the bitstream VUI.
    #[must_use]
    pub const fn h273_value(self) -> u8 {
        match self {
            Self::Bt709 => 1,
            Self::Bt2020 => 9,
            Self::DisplayP3 => 12,
        }
    }
}

impl TransferCharacteristics {
    /// Every transfer characteristic in the vocabulary.
    pub const ALL: &'static [Self] = &[Self::Bt709, Self::Srgb, Self::Pq, Self::Hlg];

    /// Stable wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Bt709 => "bt709",
            Self::Srgb => "srgb",
            Self::Pq => "pq",
            Self::Hlg => "hlg",
        }
    }

    /// Parse a wire token; unknown values are `None`.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        let mut index = 0;
        while index < Self::ALL.len() {
            let transfer = Self::ALL[index];
            if transfer.token().as_bytes() == value.as_bytes() {
                return Some(transfer);
            }
            index += 1;
        }
        None
    }

    /// ITU-T H.273 `transfer_characteristics` value written to the VUI.
    #[must_use]
    pub const fn h273_value(self) -> u8 {
        match self {
            Self::Bt709 => 1,
            Self::Srgb => 13,
            Self::Pq => 16,
            Self::Hlg => 18,
        }
    }
}

/// A set of [`VideoCodec`]s.
///
/// This replaces the previous one-boolean-per-codec capability model. That
/// model required a new struct field, threaded by hand through every layer
/// including the wire, each time a codec was added. A set means a new codec
/// costs an enum variant and a table entry, and nothing that merely *carries*
/// capability has to change at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CodecSet(u32);

impl CodecSet {
    /// The empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build a set in a `const` context, for capability tables.
    #[must_use]
    pub const fn from_slice(codecs: &[VideoCodec]) -> Self {
        let mut bits = 0u32;
        let mut index = 0;
        while index < codecs.len() {
            bits |= 1 << codecs[index].bit_index();
            index += 1;
        }
        Self(bits)
    }

    /// This set plus `codec`.
    #[must_use]
    pub const fn with(self, codec: VideoCodec) -> Self {
        Self(self.0 | (1 << codec.bit_index()))
    }

    /// Whether `codec` is present.
    #[must_use]
    pub const fn contains(self, codec: VideoCodec) -> bool {
        self.0 & (1 << codec.bit_index()) != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Members, in [`VideoCodec::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = VideoCodec> {
        VideoCodec::ALL
            .iter()
            .copied()
            .filter(move |codec| self.contains(*codec))
    }

    /// Members present in both sets.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Whether every member of `other` is also in this set.
    #[must_use]
    pub const fn contains_all(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// A set of [`ChromaSubsampling`] formats. See [`CodecSet`] for the rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChromaSet(u32);

impl ChromaSet {
    /// The empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build a set in a `const` context, for capability tables.
    #[must_use]
    pub const fn from_slice(formats: &[ChromaSubsampling]) -> Self {
        let mut bits = 0u32;
        let mut index = 0;
        while index < formats.len() {
            bits |= 1 << formats[index].bit_index();
            index += 1;
        }
        Self(bits)
    }

    /// This set plus `chroma`.
    #[must_use]
    pub const fn with(self, chroma: ChromaSubsampling) -> Self {
        Self(self.0 | (1 << chroma.bit_index()))
    }

    /// Whether `chroma` is present.
    #[must_use]
    pub const fn contains(self, chroma: ChromaSubsampling) -> bool {
        self.0 & (1 << chroma.bit_index()) != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Members, in [`ChromaSubsampling::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = ChromaSubsampling> {
        ChromaSubsampling::ALL
            .iter()
            .copied()
            .filter(move |chroma| self.contains(*chroma))
    }
}

/// A set of [`BitDepth`] values. See [`CodecSet`] for the rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BitDepthSet(u32);

impl BitDepthSet {
    /// The empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build a set in a `const` context, for capability tables.
    #[must_use]
    pub const fn from_slice(depths: &[BitDepth]) -> Self {
        let mut bits = 0u32;
        let mut index = 0;
        while index < depths.len() {
            bits |= 1 << depths[index].bit_index();
            index += 1;
        }
        Self(bits)
    }

    /// This set plus `depth`.
    #[must_use]
    pub const fn with(self, depth: BitDepth) -> Self {
        Self(self.0 | (1 << depth.bit_index()))
    }

    /// Whether `depth` is present.
    #[must_use]
    pub const fn contains(self, depth: BitDepth) -> bool {
        self.0 & (1 << depth.bit_index()) != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Members present in both sets.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Members, in [`BitDepth::ALL`] order (shallowest first).
    #[must_use]
    pub fn iter(self) -> impl DoubleEndedIterator<Item = BitDepth> {
        BitDepth::ALL
            .iter()
            .copied()
            .filter(move |depth| self.contains(*depth))
    }

    /// The deepest member, or `None` when empty.
    ///
    /// Used when degrading: a backend that cannot serve the requested depth
    /// should fall to the deepest one it *can* serve rather than straight to
    /// eight bits.
    #[must_use]
    pub fn deepest(self) -> Option<BitDepth> {
        self.iter().next_back()
    }

    /// The deepest member no deeper than `ceiling`, or `None`.
    #[must_use]
    pub fn deepest_up_to(self, ceiling: BitDepth) -> Option<BitDepth> {
        self.iter().rfind(|depth| *depth <= ceiling)
    }
}

/// A set of [`ColorRange`] values. See [`CodecSet`] for the rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColorRangeSet(u32);

impl ColorRangeSet {
    /// The empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build a set in a `const` context, for capability tables.
    #[must_use]
    pub const fn from_slice(ranges: &[ColorRange]) -> Self {
        let mut bits = 0u32;
        let mut index = 0;
        while index < ranges.len() {
            bits |= 1 << ranges[index].bit_index();
            index += 1;
        }
        Self(bits)
    }

    /// This set plus `range`.
    #[must_use]
    pub const fn with(self, range: ColorRange) -> Self {
        Self(self.0 | (1 << range.bit_index()))
    }

    /// Whether `range` is present.
    #[must_use]
    pub const fn contains(self, range: ColorRange) -> bool {
        self.0 & (1 << range.bit_index()) != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Members present in both sets.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Members, in [`ColorRange::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = ColorRange> {
        ColorRange::ALL
            .iter()
            .copied()
            .filter(move |range| self.contains(*range))
    }
}

/// Supported coded component depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BitDepth {
    /// Eight bits per component.
    Eight,
    /// Ten bits per component.
    Ten,
    /// Twelve bits per component.
    Twelve,
}

impl BitDepth {
    /// Every depth in the vocabulary, shallowest first.
    pub const ALL: &'static [Self] = &[Self::Eight, Self::Ten, Self::Twelve];

    /// Bits per component.
    #[must_use]
    pub const fn bits(self) -> u16 {
        match self {
            Self::Eight => 8,
            Self::Ten => 10,
            Self::Twelve => 12,
        }
    }

    /// Bytes each component occupies in an unpacked sample buffer.
    ///
    /// Everything above eight bits is carried in 16-bit words, which is what
    /// both `NV_ENC_BUFFER_FORMAT_*_10BIT` and `CoreVideo`'s `x`-prefixed
    /// formats expect.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        if self.bits() > 8 { 2 } else { 1 }
    }

    /// Stable wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Eight => "8",
            Self::Ten => "10",
            Self::Twelve => "12",
        }
    }

    /// Parse a wire token; unknown values are `None`.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        let mut index = 0;
        while index < Self::ALL.len() {
            let depth = Self::ALL[index];
            if depth.token().as_bytes() == value.as_bytes() {
                return Some(depth);
            }
            index += 1;
        }
        None
    }

    /// Largest representable code value at this depth.
    #[must_use]
    pub const fn max_code(self) -> u16 {
        (1 << self.bits()) - 1
    }

    const fn bit_index(self) -> u32 {
        match self {
            Self::Eight => 0,
            Self::Ten => 1,
            Self::Twelve => 2,
        }
    }
}

/// What the encoder should optimise for.
///
/// Orthogonal to [`VideoConfiguration`], deliberately. The colour contract
/// says *what* is encoded and is negotiated, equality-checked and reported;
/// intent says *how hard the encoder works* and is a pure configuration knob.
/// Mixing them would make two sessions with identical colour compare unequal.
///
/// The distinction is real for Arcen's audience. Interactive desktop use is
/// latency-bound, so the encoder should avoid lookahead and B-frames. A
/// colourist studying a held frame is not latency-bound at all and wants
/// every bit the encoder can spend on that frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncodeIntent {
    /// Minimise latency: no lookahead, no B-frames, tight VBV.
    ///
    /// The correct default for remote desktop, and what Arcen has always done.
    #[default]
    Interactive,
    /// Maximise per-frame fidelity, accepting added latency.
    ///
    /// For grading and VFX review, where the image is mostly static and
    /// judged rather than driven.
    Quality,
}

impl EncodeIntent {
    /// Every intent in the vocabulary.
    pub const ALL: &'static [Self] = &[Self::Interactive, Self::Quality];

    /// Stable wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Quality => "quality",
        }
    }

    /// Parse a wire token; unknown values are `None`.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|intent| intent.token() == value)
    }

    /// Whether this intent permits the encoder to add latency for quality.
    #[must_use]
    pub const fn allows_added_latency(self) -> bool {
        matches!(self, Self::Quality)
    }

    /// NVENC `frameIntervalP` every Arcen session must be configured with.
    ///
    /// **One — IPPP, no B-frames — for every intent, deliberately.**
    ///
    /// This is the single most important constraint in this file, because it
    /// is not obvious and it was got wrong once. `Quality` selects preset P6
    /// with `NV_ENC_TUNING_INFO_HIGH_QUALITY`, and the driver fills that
    /// preset with B-frames and lookahead enabled. That is the correct default
    /// for encoding a *file*, and a serious defect for encoding a *session*:
    ///
    /// 1. A B-frame cannot be displayed until a later frame has arrived, so
    ///    coding order stops matching display order.
    /// 2. Arcen stamps a frame's timestamp when the encoded access unit is
    ///    *read out of the encoder*, which is coding order, not capture order.
    ///    There is no capture timestamp on the wire to recover the difference.
    /// 3. The client therefore has no way to reorder correctly, and playback
    ///    visibly runs forward, jumps back, and runs forward again.
    ///
    /// Reordering is also worthless here even when it is correct: a live
    /// desktop always wants the newest frame, so a reorder buffer would trade
    /// latency for compression the session cannot spend.
    ///
    /// The quality that `Quality` mode is actually for — better mode
    /// decision, finer RDO, a wider VBV — costs no reordering and is kept.
    ///
    /// Do not make this intent-dependent. If B-frames are ever genuinely
    /// wanted, the protocol must carry a real capture timestamp first, and
    /// [`Self::allows_added_latency`] is the flag to consult then.
    pub const REQUIRED_FRAME_INTERVAL_P: u32 = 1;

    /// Whether the encoder may hold frames to look ahead.
    ///
    /// Never. Lookahead does not reorder output, but it delays it by its
    /// depth — at 30 fps a depth of 8 is over a quarter of a second of pure
    /// added latency — and it is what lets the driver decide to insert
    /// B-frames adaptively. See [`Self::REQUIRED_FRAME_INTERVAL_P`].
    #[must_use]
    pub const fn allows_lookahead(self) -> bool {
        false
    }
}

/// Truth value for hardware acceleration discovered on the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareSupport {
    /// The endpoint probed and found support.
    Available,
    /// The endpoint probed and did not find support.
    Unavailable,
    /// The endpoint has not established support.
    Unknown,
}

/// Codec support and the hardware facts behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecCapability {
    /// Codec.
    pub codec: VideoCodec,
    /// Supported chroma formats.
    pub chroma: BTreeSet<ChromaSubsampling>,
    /// Supported bit depths.
    pub bit_depths: BTreeSet<BitDepth>,
    /// Hardware encoder availability.
    pub hardware_encode: HardwareSupport,
    /// Hardware decoder availability.
    pub hardware_decode: HardwareSupport,
}

/// Stable identity attributes for a physical monitor when the OS exposes them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorIdentity {
    /// Endpoint-local identifier used to correlate reconnects when stable.
    pub id: String,
    /// Human-readable display name.
    #[serde(default)]
    pub name: String,
    /// Manufacturer identifier, when reported by the OS.
    #[serde(default)]
    pub vendor: u32,
    /// Product identifier, when reported by the OS.
    #[serde(default)]
    pub model: u32,
    /// Serial identifier, when reported by the OS.
    #[serde(default)]
    pub serial: u32,
}

/// Clockwise monitor rotation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rotation {
    /// No rotation.
    #[default]
    Degrees0,
    /// 90 degrees clockwise.
    Degrees90,
    /// 180 degrees clockwise.
    Degrees180,
    /// 270 degrees clockwise.
    Degrees270,
}

impl Rotation {
    /// All domain rotations in their stable protocol order.
    pub const ALL: [Self; 4] = [
        Self::Degrees0,
        Self::Degrees90,
        Self::Degrees180,
        Self::Degrees270,
    ];
}

/// Monitor placement in a virtual desktop.
///
/// `x/y` stay in the endpoint's logical desktop space, while
/// `width_px/height_px` stay in native physical/backing pixels even when
/// `rotation` is 90 or 270 degrees. Aggregate requested or applied desktop
/// bounds therefore require the explicit multi-monitor wrappers in
/// `multi_monitor.rs`, which derive the rotation-aware desktop footprint
/// separately instead of mutating the underlying native mode dimensions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    /// Monitor identity.
    pub identity: MonitorIdentity,
    /// Horizontal virtual-desktop origin in logical pixels.
    pub x: i32,
    /// Vertical virtual-desktop origin in logical pixels.
    pub y: i32,
    /// Native physical framebuffer width before any desktop rotation.
    pub width_px: u32,
    /// Native physical framebuffer height before any desktop rotation.
    pub height_px: u32,
    /// Logical-to-physical backing scale.
    pub scale: f32,
    /// Nominal refresh rate.
    pub refresh_hz: u32,
    /// Clockwise rotation.
    #[serde(default)]
    pub rotation: Rotation,
    /// Whether this monitor anchors the desktop.
    pub primary: bool,
    /// Physical width in millimetres, or zero when unknown.
    #[serde(default)]
    pub width_mm: f32,
    /// Physical height in millimetres, or zero when unknown.
    #[serde(default)]
    pub height_mm: f32,
}

/// A validated endpoint monitor roster.
///
/// This validates identity, primary, size, and scale invariants only. It does
/// not imply one aggregate desktop coordinate space because `Monitor` keeps
/// logical origins separate from physical extents.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorTopology {
    monitors: Vec<Monitor>,
    primary_index: usize,
}

impl MonitorTopology {
    /// Validates a non-empty layout with unique identities and one primary.
    ///
    /// # Errors
    ///
    /// Returns the first topology invariant that is not satisfied.
    pub fn new(monitors: Vec<Monitor>) -> Result<Self, MediaContractError> {
        if monitors.is_empty() {
            return Err(MediaContractError::EmptyTopology);
        }
        let primary_count = monitors.iter().filter(|monitor| monitor.primary).count();
        if primary_count != 1 {
            return Err(MediaContractError::PrimaryMonitorCount(primary_count));
        }
        let mut ids = BTreeSet::new();
        for monitor in &monitors {
            if monitor.identity.id.is_empty() {
                return Err(MediaContractError::EmptyMonitorId);
            }
            if !ids.insert(&monitor.identity.id) {
                return Err(MediaContractError::DuplicateMonitorId(
                    monitor.identity.id.clone(),
                ));
            }
            if monitor.width_px == 0 || monitor.height_px == 0 {
                return Err(MediaContractError::InvalidMonitorDimensions(
                    monitor.identity.id.clone(),
                ));
            }
            if !monitor.scale.is_finite() || monitor.scale <= 0.0 {
                return Err(MediaContractError::InvalidMonitorScale(
                    monitor.identity.id.clone(),
                ));
            }
        }
        let primary_index = monitors
            .iter()
            .position(|monitor| monitor.primary)
            .ok_or(MediaContractError::PrimaryMonitorCount(0))?;
        Ok(Self {
            monitors,
            primary_index,
        })
    }

    /// Returns monitors in endpoint-defined layout order.
    #[must_use]
    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }

    /// Returns the primary monitor.
    #[must_use]
    pub fn primary(&self) -> &Monitor {
        &self.monitors[self.primary_index]
    }
}

/// Endpoint media capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaCapabilities {
    /// Video capabilities, ordered by endpoint preference.
    pub video: Vec<CodecCapability>,
    /// Legacy audio codec identifiers, ordered by endpoint preference.
    ///
    /// Active audio-output negotiation uses [`audio::AudioCodecCapability`].
    #[serde(default)]
    pub audio_codecs: Vec<String>,
}

/// Selected video contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoConfiguration {
    /// Selected codec.
    pub codec: VideoCodec,
    /// Selected chroma.
    pub chroma: ChromaSubsampling,
    /// Selected bit depth.
    pub bit_depth: BitDepth,
    /// Selected coded sample range.
    pub range: ColorRange,
    /// Selected matrix coefficients.
    pub matrix: ColorMatrix,
    /// Selected colour primaries.
    pub primaries: ColorPrimaries,
    /// Selected transfer characteristics.
    pub transfer: TransferCharacteristics,
}

impl VideoConfiguration {
    /// The historical Arcen contract: H.264, 4:2:0, 8-bit, BT.709 limited.
    ///
    /// Kept as a named constructor because a great many call sites only ever
    /// wanted "whatever we shipped before", and spelling out seven fields at
    /// each of them would bury the ones that genuinely differ.
    #[must_use]
    pub const fn legacy_h264() -> Self {
        Self {
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            range: ColorRange::Limited,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }

    /// The grading-reference contract: HEVC, 4:4:4, 10-bit, BT.709 full range.
    #[must_use]
    pub const fn grading_reference() -> Self {
        Self {
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: BitDepth::Ten,
            range: ColorRange::Full,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }

    /// Whether the coded planes carry G/B/R rather than Y/Cb/Cr.
    #[must_use]
    pub const fn is_identity_matrix(self) -> bool {
        self.matrix.is_identity()
    }
}

/// Media contract validation failure.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaContractError {
    /// A topology contained no monitors.
    EmptyTopology,
    /// A topology requested fewer than one or more than four monitors.
    UnsupportedMonitorCount(usize),
    /// The monitor identity was empty.
    EmptyMonitorId,
    /// The monitor identity could not serve as a bounded opaque client display id.
    InvalidClientDisplayId(ClientDisplayIdError),
    /// A monitor identity appeared more than once.
    DuplicateMonitorId(String),
    /// Session monitor id `0` is reserved for legacy single-monitor framing.
    ZeroSessionMonitorId,
    /// A session monitor identifier appeared more than once.
    DuplicateSessionMonitorId(u16),
    /// The topology did not contain exactly one primary monitor.
    PrimaryMonitorCount(usize),
    /// A monitor dimension was zero.
    InvalidMonitorDimensions(String),
    /// A monitor scale was non-positive or non-finite.
    InvalidMonitorScale(String),
    /// A layout rectangle had zero dimensions.
    InvalidLayoutDimensions,
    /// A topology generation must be nonzero.
    ZeroTopologyGeneration,
    /// A region media stream epoch must be nonzero.
    ZeroStreamEpoch,
    /// A region media plan had zero encoded dimensions.
    InvalidMediaDimensions,
    /// A region media plan had a zero frame rate.
    InvalidMediaFps,
    /// A checked coordinate or bounding-raster calculation overflowed.
    CoordinateOverflow,
    /// A per-monitor media plan declared no bitrate budget.
    InvalidBitrateKbps,
    /// A bitrate budget fell outside the supported per-region band. Carries
    /// the rejected kbps value.
    BitrateBudgetOutOfRange(u32),
    /// A checked aggregate budget calculation overflowed.
    BudgetOverflow(&'static str),
}

impl Display for MediaContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyTopology => formatter.write_str("monitor topology is empty"),
            Self::UnsupportedMonitorCount(count) => {
                write!(formatter, "expected 1..=4 monitors, found {count}")
            }
            Self::EmptyMonitorId => formatter.write_str("monitor identity is empty"),
            Self::InvalidClientDisplayId(error) => {
                write!(
                    formatter,
                    "monitor identity is not a valid client display id: {error}"
                )
            }
            Self::DuplicateMonitorId(id) => write!(formatter, "duplicate monitor identity: {id}"),
            Self::ZeroSessionMonitorId => formatter.write_str(
                "session monitor id must be 1..=65535; 0 is reserved for legacy single-monitor framing",
            ),
            Self::DuplicateSessionMonitorId(id) => {
                write!(formatter, "duplicate session monitor id: {id}")
            }
            Self::PrimaryMonitorCount(count) => {
                write!(formatter, "expected one primary monitor, found {count}")
            }
            Self::InvalidMonitorDimensions(id) => {
                write!(formatter, "monitor {id} has zero dimensions")
            }
            Self::InvalidMonitorScale(id) => write!(formatter, "monitor {id} has invalid scale"),
            Self::InvalidLayoutDimensions => {
                formatter.write_str("layout rectangle has zero dimensions")
            }
            Self::ZeroTopologyGeneration => {
                formatter.write_str("topology generation must be nonzero")
            }
            Self::ZeroStreamEpoch => formatter.write_str("media stream epoch must be nonzero"),
            Self::InvalidMediaDimensions => {
                formatter.write_str("media plan dimensions must be nonzero")
            }
            Self::InvalidMediaFps => formatter.write_str("media plan fps must be nonzero"),
            Self::CoordinateOverflow => {
                formatter.write_str("coordinate or bounding raster overflowed")
            }
            Self::InvalidBitrateKbps => formatter.write_str("bitrate budget must be nonzero"),
            Self::BitrateBudgetOutOfRange(kbps) => {
                write!(
                    formatter,
                    "bitrate budget {kbps} kbps is outside {}..={} kbps",
                    BitrateBudgetKbps::MIN_KBPS,
                    BitrateBudgetKbps::MAX_KBPS
                )
            }
            Self::BudgetOverflow(field) => {
                write!(formatter, "aggregate budget overflowed: {field}")
            }
        }
    }
}

impl Error for MediaContractError {}

#[cfg(test)]
mod tests {
    // ---- capability sets -------------------------------------------------

    #[test]
    fn codec_all_lists_every_variant() {
        // `ALL` is a hand-written list, so the compiler cannot force a new
        // variant into it the way it forces `token`, `bit_index` and
        // `offered_chroma`. This closes that one gap: the match below is
        // exhaustive, so a new variant does not compile until it is listed
        // here, and the assertion then proves it also reached `ALL`.
        let every = [
            VideoCodec::Jpeg,
            VideoCodec::H264,
            VideoCodec::H265,
            VideoCodec::Vp9,
            VideoCodec::Av1,
        ];
        for codec in every {
            match codec {
                VideoCodec::Jpeg
                | VideoCodec::H264
                | VideoCodec::H265
                | VideoCodec::Vp9
                | VideoCodec::Av1 => {}
            }
            assert!(
                VideoCodec::ALL.contains(&codec),
                "{} is missing from VideoCodec::ALL",
                codec.token()
            );
        }
        assert_eq!(every.len(), VideoCodec::ALL.len(), "ALL has a stale entry");
    }

    #[test]
    fn chroma_all_lists_every_variant() {
        let every = [
            ChromaSubsampling::Yuv420,
            ChromaSubsampling::Yuv422,
            ChromaSubsampling::Yuv444,
        ];
        for chroma in every {
            match chroma {
                ChromaSubsampling::Yuv420
                | ChromaSubsampling::Yuv422
                | ChromaSubsampling::Yuv444 => {}
            }
            assert!(ChromaSubsampling::ALL.contains(&chroma));
        }
        assert_eq!(every.len(), ChromaSubsampling::ALL.len());
    }

    #[test]
    fn every_codec_has_a_unique_bit_and_round_trips_through_its_token() {
        let mut seen = CodecSet::empty();
        for codec in VideoCodec::ALL.iter().copied() {
            assert!(
                !seen.contains(codec),
                "{} claims a bit already taken",
                codec.token()
            );
            seen = seen.with(codec);
            assert_eq!(VideoCodec::from_token(codec.token()), Some(codec));
        }
        // Every declared codec is representable, so adding one to ALL without
        // giving it a bit_index fails here rather than silently aliasing.
        assert_eq!(seen.iter().count(), VideoCodec::ALL.len());
    }

    /// A live session must never emit reordered output, whatever the intent.
    ///
    /// This is the invariant behind the grading-playback defect: `Quality`
    /// selects P6 + HIGH_QUALITY, whose driver defaults enable B-frames, and
    /// Arcen has no capture timestamp on the wire to reorder them with. The
    /// constant exists so both encoder backends read the same rule and a
    /// future "let Quality use B-frames" change has to come here and read why
    /// it must not.
    #[test]
    fn output_ordering_is_never_intent_dependent() {
        assert_eq!(
            EncodeIntent::REQUIRED_FRAME_INTERVAL_P,
            1,
            "IPPP only: a B-frame cannot be displayed until a later frame \
             arrives, and the wire carries no capture timestamp to reorder by"
        );
        for intent in EncodeIntent::ALL.iter().copied() {
            assert!(
                !intent.allows_lookahead(),
                "{} must not hold frames to look ahead in a live session",
                intent.token()
            );
        }
    }

    /// Intent may still buy quality — just never ordering or lookahead.
    #[test]
    fn quality_intent_still_differs_from_interactive() {
        assert!(EncodeIntent::Quality.allows_added_latency());
        assert!(!EncodeIntent::Interactive.allows_added_latency());
    }

    /// Intent tokens must round-trip, since they cross the wire as strings.
    #[test]
    fn every_intent_round_trips_through_its_token() {
        for intent in EncodeIntent::ALL.iter().copied() {
            assert_eq!(EncodeIntent::from_token(intent.token()), Some(intent));
        }
        assert_eq!(EncodeIntent::from_token("nonsense"), None);
    }

    /// The default must stay latency-first. A silent change here would make
    /// every existing interactive session start buying latency for quality.
    #[test]
    fn default_intent_is_interactive() {
        assert_eq!(EncodeIntent::default(), EncodeIntent::Interactive);
        assert!(!EncodeIntent::Interactive.allows_added_latency());
        assert!(EncodeIntent::Quality.allows_added_latency());
    }

    #[test]
    fn every_chroma_has_a_unique_bit_and_round_trips_through_its_token() {
        let mut seen = ChromaSet::empty();
        for chroma in ChromaSubsampling::ALL.iter().copied() {
            assert!(!seen.contains(chroma));
            seen = seen.with(chroma);
            assert_eq!(ChromaSubsampling::from_token(chroma.token()), Some(chroma));
        }
        assert_eq!(seen.iter().count(), ChromaSubsampling::ALL.len());
    }

    #[test]
    fn unknown_tokens_are_rejected_rather_than_defaulted() {
        // A peer announcing a codec we have never heard of must not be read as
        // some default we happen to support.
        assert_eq!(VideoCodec::from_token("mlvc"), None);
        assert_eq!(VideoCodec::from_token(""), None);
        assert_eq!(VideoCodec::from_token("H264"), None);
        assert_eq!(ChromaSubsampling::from_token("yuv411"), None);
    }

    #[test]
    fn codec_sets_are_const_constructible_and_iterate_in_declaration_order() {
        const SET: CodecSet = CodecSet::from_slice(&[VideoCodec::H265, VideoCodec::H264]);
        assert!(SET.contains(VideoCodec::H264));
        assert!(SET.contains(VideoCodec::H265));
        assert!(!SET.contains(VideoCodec::Av1));
        assert!(!SET.is_empty());
        assert!(CodecSet::empty().is_empty());
        // Declaration order, not insertion order, so tables are deterministic.
        assert_eq!(
            SET.iter().collect::<Vec<_>>(),
            vec![VideoCodec::H264, VideoCodec::H265]
        );
    }

    #[test]
    fn codec_set_intersection_is_the_negotiable_overlap() {
        const HOST: CodecSet = CodecSet::from_slice(&[VideoCodec::H264, VideoCodec::H265]);
        const CLIENT: CodecSet = CodecSet::from_slice(&[VideoCodec::H265, VideoCodec::Av1]);
        assert_eq!(
            HOST.intersection(CLIENT).iter().collect::<Vec<_>>(),
            vec![VideoCodec::H265]
        );
        assert!(HOST.intersection(CodecSet::empty()).is_empty());
    }

    use super::*;

    fn monitor(id: &str, primary: bool) -> Monitor {
        Monitor {
            identity: MonitorIdentity {
                id: id.to_owned(),
                ..MonitorIdentity::default()
            },
            x: 0,
            y: 0,
            width_px: 2560,
            height_px: 1440,
            scale: 2.0,
            refresh_hz: 60,
            rotation: Rotation::Degrees0,
            primary,
            width_mm: 0.0,
            height_mm: 0.0,
        }
    }

    #[test]
    fn topology_requires_one_primary_and_unique_ids() {
        let topology = MonitorTopology::new(vec![monitor("primary", true), monitor("side", false)])
            .expect("valid topology");
        assert_eq!(topology.primary().identity.id, "primary");

        assert_eq!(
            MonitorTopology::new(vec![monitor("same", true), monitor("same", false)]),
            Err(MediaContractError::DuplicateMonitorId("same".to_owned()))
        );
        assert_eq!(
            MonitorTopology::new(vec![monitor("one", false)]),
            Err(MediaContractError::PrimaryMonitorCount(0))
        );
    }

    #[test]
    fn hardware_support_preserves_unknown() {
        let json = serde_json::to_string(&HardwareSupport::Unknown).expect("serializes");
        assert_eq!(json, "\"unknown\"");
    }
}
