//! Resolving a client's video request into a plan a host can actually serve.
//!
//! A client asks for a codec, a chroma layout, a bit depth, and says how
//! strictly it means it. A host can serve some of that. Deciding what the
//! session actually gets is arithmetic over two capability sets and one
//! intent, with no operating system in it, so it lives here rather than being
//! written once per platform and drifting.
//!
//! The rule this module exists to enforce is that **a plan carries its own
//! truth**. Every resolved plan states its transfer function, primaries and
//! matrix explicitly, and records whether it is what was asked for. A host
//! that resolves "10-bit" and leaves the transfer implied is how a stream ends
//! up being presented as HDR because it happened to be ten bits deep; bit
//! depth is not a transfer function and never implies one.
//!
//! Grading and Auto are separate contracts, not one contract with a depth
//! switch. [`VideoTier`] names which one a plan belongs to so a host can pick
//! the matching capture provider rather than widening a fast path it should
//! leave alone.

use serde::{Deserialize, Serialize};

use crate::{BitDepth, ChromaSubsampling, VideoCodec};

/// Which complete contract a session is running.
///
/// These are not points on a quality slider. Each names a different capture
/// path with a different cost and different correctness requirements, and a
/// host is expected to have separate providers for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoTier {
    /// Ordinary 8-bit 4:2:0 desktop. The fast path.
    Standard,
    /// 10-bit 4:4:4 SDR. Colour-critical work, still BT.709 transfer.
    Grading,
    /// 10-bit with a PQ or HLG transfer. Requires proven HDR output.
    HighDynamicRange,
}

/// How strictly the client meant its request.
///
/// Mirrors `arcen_protocol::messages::VideoSelectionIntent` without this crate
/// depending on the protocol crate, in the same way the pen types are mirrored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionIntent {
    /// Preserve the requested contract exactly where possible.
    Exact,
    /// Ordinary desktop; rank for throughput without changing colour axes.
    AdaptivePerformance,
    /// Prefer the fidelity contract and report any fallback explicitly.
    ColorFidelity,
}

/// Coded component depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanBitDepth {
    /// Eight bits per component.
    Eight,
    /// Ten bits per component.
    Ten,
}

/// Chroma layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanChroma {
    /// 4:2:0.
    Yuv420,
    /// 4:4:4.
    Yuv444,
}

/// What the host is able to serve.
///
/// Every field is something the host has established about itself, not
/// something it hopes is true. A host that has not proven HDR output sets
/// `hdr_output` false and gets a Grading plan instead of an HDR one, which is
/// a degradation it can report rather than a picture that is silently wrong.
// Capability sets are independent yes/no facts about hardware, and naming
// each one is clearer than packing them into flags nobody can read at a call
// site. The same allow is used for the other capability structs in shared/.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostVideoCapabilities {
    /// H.264 encode is available.
    pub h264: bool,
    /// HEVC encode is available.
    pub hevc: bool,
    /// 10-bit encode is available.
    pub ten_bit: bool,
    /// 4:4:4 capture and encode are available.
    pub chroma_444: bool,
    /// A display with a proven PQ or HLG transfer is attached.
    pub hdr_output: bool,
}

/// What the client asked for.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientVideoRequest {
    /// How strictly the request is meant.
    pub intent: SelectionIntent,
    /// Requested codec.
    pub codec: VideoCodec,
    /// Requested depth.
    pub bit_depth: BitDepth,
    /// Requested chroma.
    pub chroma: ChromaSubsampling,
    /// Requested range token.
    pub range: &'static str,
    /// Requested transfer function token.
    pub transfer: &'static str,
    /// Requested colour primaries token.
    pub primaries: &'static str,
    /// Requested matrix coefficients token.
    pub matrix: &'static str,
    /// Whether the client can decode HEVC at all.
    pub hevc: bool,
    /// Whether the client can decode 4:4:4.
    pub chroma_444: bool,
    /// Whether the client can decode 10-bit.
    pub main10: bool,
    /// Whether the client asked for an HDR transfer.
    pub hdr: bool,
}

/// Why a plan is not what was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Degradation {
    /// The host cannot encode HEVC, which every fidelity tier needs here.
    HostLacksHevc,
    /// The host cannot encode ten-bit.
    HostLacksTenBit,
    /// The host cannot capture or encode 4:4:4.
    HostLacksChroma444,
    /// The client cannot decode what was requested.
    ClientLacksSupport,
    /// HDR was requested and no display with a proven HDR transfer is attached.
    NoProvenHdrOutput,
    /// The exact request names a contract this host does not have a provider for.
    HostLacksExactContract,
}

/// A resolved, self-describing video plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedVideoPlan {
    /// Which contract this plan belongs to.
    pub tier: VideoTier,
    /// Selected codec.
    pub codec: VideoCodec,
    /// Coded depth.
    pub bit_depth: PlanBitDepth,
    /// Chroma layout.
    pub chroma: PlanChroma,
    /// Transfer function token, stated rather than implied.
    pub transfer: &'static str,
    /// Range token.
    pub range: &'static str,
    /// Colour primaries token.
    pub primaries: &'static str,
    /// Matrix coefficients token.
    pub matrix: &'static str,
    /// Why this is not what was asked for, when it is not.
    pub degraded: Option<Degradation>,
}

impl ResolvedVideoPlan {
    /// Returns whether the plan is what the client asked for.
    #[must_use]
    pub const fn is_exact(&self) -> bool {
        self.degraded.is_none()
    }

    /// The ordinary 8-bit 4:2:0 BT.709 desktop plan.
    #[must_use]
    pub const fn standard(degraded: Option<Degradation>) -> Self {
        Self::standard_for_codec(VideoCodec::H265, degraded)
    }

    const fn standard_for_codec(codec: VideoCodec, degraded: Option<Degradation>) -> Self {
        Self {
            tier: VideoTier::Standard,
            codec,
            bit_depth: PlanBitDepth::Eight,
            chroma: PlanChroma::Yuv420,
            transfer: "bt709",
            range: "limited",
            primaries: "bt709",
            matrix: "bt709",
            degraded,
        }
    }

    /// The 10-bit 4:4:4 SDR plan.
    ///
    /// Still BT.709 in every colour axis. Grading is about precision and
    /// chroma resolution, not about a wider gamut or a different transfer, and
    /// saying so here is what stops it being mistaken for HDR downstream.
    ///
    /// Full range, as `VideoConfiguration::grading_reference` is: a grader's
    /// eyedropper reads code values, and limited range spends a tenth of them
    /// on headroom no desktop pixel occupies. This plan said limited while the
    /// contract it names said full, and every Grading session was reported
    /// to the Deck as range-degraded.
    #[must_use]
    pub const fn grading(degraded: Option<Degradation>) -> Self {
        Self {
            tier: VideoTier::Grading,
            codec: VideoCodec::H265,
            bit_depth: PlanBitDepth::Ten,
            chroma: PlanChroma::Yuv444,
            transfer: "bt709",
            range: "full",
            primaries: "bt709",
            matrix: "bt709",
            degraded,
        }
    }

    /// The 10-bit PQ BT.2020 plan.
    ///
    /// Full range, as the HDR request is and as the capture is: the same
    /// 4:4:4 full-range surface Grading uses, with a PQ transfer.
    #[must_use]
    pub const fn hdr() -> Self {
        Self {
            tier: VideoTier::HighDynamicRange,
            codec: VideoCodec::H265,
            bit_depth: PlanBitDepth::Ten,
            chroma: PlanChroma::Yuv444,
            transfer: "pq",
            range: "full",
            primaries: "bt2020",
            matrix: "bt2020ncl",
            degraded: None,
        }
    }
}

/// Resolves a client request against what the host can serve.
///
/// The result is always serveable: this never returns a plan the host cannot
/// produce. When the request cannot be met the plan degrades to the nearest
/// contract the host does have and names why, because a client told "you asked
/// for 4:4:4 and the host has none" can explain itself to a user, whereas one
/// silently given 4:2:0 cannot.
#[must_use]
pub fn resolve_video_plan(
    request: &ClientVideoRequest,
    host: &HostVideoCapabilities,
) -> ResolvedVideoPlan {
    let standard = standard_codec(request, *host);

    if let Some(reason) = exact_codec_degradation(request, *host, standard) {
        return contract_checked(
            request,
            ResolvedVideoPlan::standard_for_codec(standard, Some(reason)),
        );
    }

    // Adaptive performance is a request for an ordinary desktop. It is not a
    // weaker form of fidelity, so it does not get promoted to one even when
    // both ends could manage it.
    if request.intent == SelectionIntent::AdaptivePerformance {
        return contract_checked(
            request,
            ResolvedVideoPlan::standard_for_codec(standard, None),
        );
    }

    let wants_fidelity = request.intent == SelectionIntent::ColorFidelity
        || request.bit_depth >= BitDepth::Ten
        || request.chroma == ChromaSubsampling::Yuv444;
    if !wants_fidelity {
        return contract_checked(
            request,
            ResolvedVideoPlan::standard_for_codec(standard, None),
        );
    }

    if !host.hevc {
        return contract_checked(
            request,
            ResolvedVideoPlan::standard_for_codec(standard, Some(Degradation::HostLacksHevc)),
        );
    }
    if !request.hevc {
        return contract_checked(
            request,
            ResolvedVideoPlan::standard_for_codec(standard, Some(Degradation::ClientLacksSupport)),
        );
    }

    if request.intent == SelectionIntent::Exact
        && (request.codec != VideoCodec::H265
            || request.bit_depth != BitDepth::Ten
            || request.chroma != ChromaSubsampling::Yuv444)
    {
        return contract_checked(
            request,
            ResolvedVideoPlan::standard_for_codec(
                standard,
                Some(Degradation::HostLacksExactContract),
            ),
        );
    }

    if !request.main10 || !request.chroma_444 {
        return contract_checked(
            request,
            ResolvedVideoPlan::standard_for_codec(standard, Some(Degradation::ClientLacksSupport)),
        );
    }
    if !host.ten_bit {
        return contract_checked(
            request,
            ResolvedVideoPlan::standard_for_codec(standard, Some(Degradation::HostLacksTenBit)),
        );
    }
    if !host.chroma_444 {
        return contract_checked(
            request,
            ResolvedVideoPlan::standard_for_codec(standard, Some(Degradation::HostLacksChroma444)),
        );
    }

    if request.hdr {
        // Ten bits is not HDR. Without a display whose transfer has actually
        // been read back, the honest answer is a Grading plan that says HDR
        // was refused, not a PQ plan over an SDR panel.
        if host.hdr_output {
            return contract_checked(request, ResolvedVideoPlan::hdr());
        }
        return contract_checked(
            request,
            ResolvedVideoPlan::grading(Some(Degradation::NoProvenHdrOutput)),
        );
    }

    contract_checked(request, ResolvedVideoPlan::grading(None))
}

const fn plan_depth(depth: PlanBitDepth) -> BitDepth {
    match depth {
        PlanBitDepth::Eight => BitDepth::Eight,
        PlanBitDepth::Ten => BitDepth::Ten,
    }
}

const fn plan_chroma(chroma: PlanChroma) -> ChromaSubsampling {
    match chroma {
        PlanChroma::Yuv420 => ChromaSubsampling::Yuv420,
        PlanChroma::Yuv444 => ChromaSubsampling::Yuv444,
    }
}

fn contract_checked(
    request: &ClientVideoRequest,
    mut plan: ResolvedVideoPlan,
) -> ResolvedVideoPlan {
    if plan.degraded.is_none() && contract_degradation_required(request, plan) {
        plan.degraded = Some(Degradation::HostLacksExactContract);
    }
    plan
}

fn contract_degradation_required(request: &ClientVideoRequest, plan: ResolvedVideoPlan) -> bool {
    let colour_changed = request.bit_depth != plan_depth(plan.bit_depth)
        || request.chroma != plan_chroma(plan.chroma)
        || request.range != plan.range
        || request.transfer != plan.transfer
        || request.primaries != plan.primaries
        || request.matrix != plan.matrix;
    match request.intent {
        SelectionIntent::Exact => request.codec != plan.codec || colour_changed,
        SelectionIntent::AdaptivePerformance => colour_changed,
        SelectionIntent::ColorFidelity => false,
    }
}

const fn host_supports_codec(host: HostVideoCapabilities, codec: VideoCodec) -> bool {
    match codec {
        VideoCodec::H264 => host.h264,
        VideoCodec::H265 => host.hevc,
        VideoCodec::Av1 | VideoCodec::Jpeg | VideoCodec::Vp9 => false,
    }
}

const fn client_supports_codec(request: &ClientVideoRequest, codec: VideoCodec) -> bool {
    match codec {
        VideoCodec::H264 => true,
        VideoCodec::H265 => request.hevc,
        VideoCodec::Av1 | VideoCodec::Jpeg | VideoCodec::Vp9 => false,
    }
}

fn standard_codec(request: &ClientVideoRequest, host: HostVideoCapabilities) -> VideoCodec {
    if host_supports_codec(host, request.codec) && client_supports_codec(request, request.codec) {
        return request.codec;
    }
    if host.hevc && request.hevc {
        VideoCodec::H265
    } else {
        VideoCodec::H264
    }
}

fn exact_codec_degradation(
    request: &ClientVideoRequest,
    host: HostVideoCapabilities,
    standard: VideoCodec,
) -> Option<Degradation> {
    if request.intent != SelectionIntent::Exact || request.codec == standard {
        return None;
    }
    Some(match request.codec {
        VideoCodec::H265 if !host.hevc => Degradation::HostLacksHevc,
        VideoCodec::H265 if !request.hevc => Degradation::ClientLacksSupport,
        _ => Degradation::HostLacksExactContract,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPABLE_HOST: HostVideoCapabilities = HostVideoCapabilities {
        h264: true,
        hevc: true,
        ten_bit: true,
        chroma_444: true,
        hdr_output: false,
    };

    const fn request(intent: SelectionIntent) -> ClientVideoRequest {
        ClientVideoRequest {
            intent,
            codec: VideoCodec::H265,
            bit_depth: BitDepth::Eight,
            chroma: ChromaSubsampling::Yuv420,
            range: "limited",
            transfer: "bt709",
            primaries: "bt709",
            matrix: "bt709",
            hevc: true,
            chroma_444: true,
            main10: true,
            hdr: false,
        }
    }

    #[test]
    fn colour_fidelity_against_a_capable_host_is_ten_bit_four_four_four() {
        let plan = resolve_video_plan(&request(SelectionIntent::ColorFidelity), &CAPABLE_HOST);
        assert_eq!(plan.tier, VideoTier::Grading);
        assert_eq!(plan.bit_depth, PlanBitDepth::Ten);
        assert_eq!(plan.chroma, PlanChroma::Yuv444);
        assert!(plan.is_exact());
    }

    #[test]
    fn grading_stays_bt709_in_every_colour_axis() {
        // The bug this guards: treating ten bits as a licence to relabel the
        // transfer, which presents an SDR desktop as HDR.
        let plan = resolve_video_plan(&request(SelectionIntent::ColorFidelity), &CAPABLE_HOST);
        assert_eq!(plan.transfer, "bt709");
        assert_eq!(plan.primaries, "bt709");
        assert_eq!(plan.matrix, "bt709");
        assert_eq!(
            plan.range,
            crate::VideoConfiguration::grading_reference().range.token(),
            "the Grading plan is the grading-reference contract's range"
        );
    }

    #[test]
    fn adaptive_performance_is_not_promoted_to_fidelity() {
        // A request for an ordinary desktop is a request, not a lower bound.
        let plan = resolve_video_plan(
            &request(SelectionIntent::AdaptivePerformance),
            &CAPABLE_HOST,
        );
        assert_eq!(plan.tier, VideoTier::Standard);
        assert!(
            plan.is_exact(),
            "serving what was asked for is not a degradation"
        );
    }

    #[test]
    fn hdr_without_a_proven_display_degrades_to_grading_and_says_so() {
        let mut asked = request(SelectionIntent::ColorFidelity);
        asked.hdr = true;
        let plan = resolve_video_plan(&asked, &CAPABLE_HOST);
        assert_eq!(plan.tier, VideoTier::Grading);
        assert_eq!(plan.transfer, "bt709", "no PQ over an unproven panel");
        assert_eq!(plan.degraded, Some(Degradation::NoProvenHdrOutput));
    }

    #[test]
    fn hdr_with_a_proven_display_is_pq_bt2020() {
        let mut asked = request(SelectionIntent::ColorFidelity);
        asked.hdr = true;
        let host = HostVideoCapabilities {
            hdr_output: true,
            ..CAPABLE_HOST
        };
        let plan = resolve_video_plan(&asked, &host);
        assert_eq!(plan.tier, VideoTier::HighDynamicRange);
        assert_eq!(plan.transfer, "pq");
        assert_eq!(plan.primaries, "bt2020");
    }

    #[test]
    fn a_host_without_four_four_four_names_itself_as_the_reason() {
        let host = HostVideoCapabilities {
            chroma_444: false,
            ..CAPABLE_HOST
        };
        let plan = resolve_video_plan(&request(SelectionIntent::ColorFidelity), &host);
        assert_eq!(plan.tier, VideoTier::Standard);
        assert_eq!(plan.degraded, Some(Degradation::HostLacksChroma444));
    }

    #[test]
    fn a_client_that_cannot_decode_fidelity_is_not_sent_it() {
        let mut asked = request(SelectionIntent::ColorFidelity);
        asked.main10 = false;
        let plan = resolve_video_plan(&asked, &CAPABLE_HOST);
        assert_eq!(plan.tier, VideoTier::Standard);
        assert_eq!(plan.degraded, Some(Degradation::ClientLacksSupport));
    }

    #[test]
    fn no_hevc_anywhere_is_one_answer_not_a_gradient() {
        let host = HostVideoCapabilities {
            hevc: false,
            ..CAPABLE_HOST
        };
        let plan = resolve_video_plan(&request(SelectionIntent::ColorFidelity), &host);
        assert_eq!(plan.degraded, Some(Degradation::HostLacksHevc));
        assert_eq!(plan.bit_depth, PlanBitDepth::Eight);
    }

    #[test]
    fn an_exact_request_for_ten_bit_is_honoured_without_the_fidelity_intent() {
        // A probe asking for exactly 10-bit 4:4:4 means it, and must not be
        // answered with an ordinary desktop because the intent token said
        // "exact" rather than "color-fidelity".
        let mut asked = request(SelectionIntent::Exact);
        asked.bit_depth = BitDepth::Ten;
        asked.chroma = ChromaSubsampling::Yuv444;
        asked.range = "full";
        let plan = resolve_video_plan(&asked, &CAPABLE_HOST);
        assert_eq!(plan.tier, VideoTier::Grading);
        assert!(plan.is_exact());

        // Exact means every axis. A limited-range 4:4:4 request is served the
        // full-range Grading contract, and told so rather than marked exact.
        asked.range = "limited";
        let plan = resolve_video_plan(&asked, &CAPABLE_HOST);
        assert_eq!(plan.tier, VideoTier::Grading);
        assert_eq!(plan.degraded, Some(Degradation::HostLacksExactContract));
    }

    #[test]
    fn exact_ten_bit_four_two_zero_is_degraded_not_rewritten_to_four_four_four() {
        let mut asked = request(SelectionIntent::Exact);
        asked.bit_depth = BitDepth::Ten;
        asked.chroma = ChromaSubsampling::Yuv420;
        let plan = resolve_video_plan(&asked, &CAPABLE_HOST);
        assert_eq!(plan.tier, VideoTier::Standard);
        assert_eq!(plan.bit_depth, PlanBitDepth::Eight);
        assert_eq!(plan.chroma, PlanChroma::Yuv420);
        assert_eq!(plan.degraded, Some(Degradation::HostLacksExactContract));
    }

    #[test]
    fn exact_four_two_two_is_reported_as_degraded_against_unprojected_request() {
        let mut asked = request(SelectionIntent::Exact);
        asked.chroma = ChromaSubsampling::Yuv422;
        let plan = resolve_video_plan(&asked, &CAPABLE_HOST);
        assert_eq!(plan.chroma, PlanChroma::Yuv420);
        assert_eq!(plan.degraded, Some(Degradation::HostLacksExactContract));
    }

    #[test]
    fn exact_av1_is_reported_as_degraded_against_requested_codec() {
        let mut asked = request(SelectionIntent::Exact);
        asked.codec = VideoCodec::Av1;
        let plan = resolve_video_plan(&asked, &CAPABLE_HOST);
        assert_eq!(plan.codec, VideoCodec::H265);
        assert_eq!(plan.degraded, Some(Degradation::HostLacksExactContract));
    }

    #[test]
    fn exact_colour_axis_changes_are_degraded_against_the_final_plan() {
        let mut asked = request(SelectionIntent::Exact);
        asked.matrix = "bt601";
        let plan = resolve_video_plan(&asked, &CAPABLE_HOST);
        assert_eq!(plan.matrix, "bt709");
        assert_eq!(plan.degraded, Some(Degradation::HostLacksExactContract));

        let mut srgb = request(SelectionIntent::Exact);
        srgb.transfer = "srgb";
        let plan = resolve_video_plan(&srgb, &CAPABLE_HOST);
        assert_eq!(plan.transfer, "bt709");
        assert_eq!(plan.degraded, Some(Degradation::HostLacksExactContract));
    }

    #[test]
    fn adaptive_full_range_srgb_records_the_colour_contract_change() {
        let mut asked = request(SelectionIntent::AdaptivePerformance);
        asked.range = "full";
        asked.transfer = "srgb";

        let plan = resolve_video_plan(&asked, &CAPABLE_HOST);

        assert_eq!(plan.codec, VideoCodec::H265);
        assert_eq!(plan.chroma, PlanChroma::Yuv420);
        assert_eq!(plan.bit_depth, PlanBitDepth::Eight);
        assert_eq!(plan.range, "limited");
        assert_eq!(plan.transfer, "bt709");
        assert_eq!(plan.degraded, Some(Degradation::HostLacksExactContract));
    }
}
