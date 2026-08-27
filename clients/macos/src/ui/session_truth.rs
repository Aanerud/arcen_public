//! Client-side derivation of "what is actually happening in this session",
//! straight from a `ServerHelloMsg`, plus the live pixel-exactness readout
//! against a declared `arcen_media::test_pattern::TestPattern`.
//!
//! Every function here is pure (plain data in, plain data out): no `egui`,
//! no socket, no GPU, no `wgpu`. `app.rs` owns the two panels this feeds --
//! `w5-negotiated-truth`'s session-info panel and `w4-exactness-readout`'s
//! live per-channel error readout -- and is the only thing that keeps
//! state: which pattern the user declared the host is streaming, and the
//! last computed [`NegotiatedTruth`]/[`ExactnessReadout`].
//!
//! # Why a colourist cannot just trust `server_hello`
//!
//! A session can silently ask for one contract and be served another (a
//! backend that cannot serve 4:4:4 falling back to 4:2:0, for instance), and
//! it can be hardware-encoded while decoding entirely in software (or the
//! reverse) -- two independent axes that neither the encode nor the decode
//! side can see about the other. Nothing about either fact is visible from
//! the video itself; both must be read back out and shown, not assumed.

use arcen_media::test_pattern::{
    measure_rgb_roundtrip, measure_transform_roundtrip, ColorAccuracy, TestPattern,
};
use arcen_media::video::{AcceleratorClass, ColorTransform, PlanDegradation};
use arcen_media::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
    VideoCodec, VideoConfiguration,
};

use crate::protocol::messages::{ServerHelloMsg, VideoSelectionIntent};

// ============================================================================
// w5-negotiated-truth: the negotiated colour/encode contract
// ============================================================================

/// Whether `encoder_backend` runs on dedicated silicon, preferring the class
/// the host declares (`encoder_class`) and falling back to a name-based
/// guess only for hosts that predate that field: a name-based guess cannot
/// classify a vendor it has never heard of, and would show a future
/// hardware encoder as a fallback.
///
/// Pulled out so the hello-arrival status line
/// (`ArcenApp::sync_media_state`) and the negotiated-truth panel share one
/// answer and can never disagree about which this session is.
#[must_use]
pub fn encoder_is_hardware(encoder_backend: &str, encoder_class: &str) -> bool {
    match AcceleratorClass::from_token(encoder_class) {
        Some(class) => class == AcceleratorClass::Hardware,
        None => {
            let lowered = encoder_backend.to_ascii_lowercase();
            lowered.contains("native") || lowered.contains("capenc")
        }
    }
}

/// Best-effort chroma read from `ServerColorCaps::advertised_pix_fmt`.
///
/// Unlike every other colour axis, chroma has no dedicated `active_*` wire
/// field yet (see `arcen_protocol::messages::ServerColorCaps`): it has to be
/// read out of this free-form pixel-format string instead, and the two host
/// implementations do not even agree on its shape -- read directly out of
/// both, not guessed: `hosts/linux/src/session/handshake.rs` sends plain
/// `"yuv444p"`/`"yuv420p"`, while `hosts/windows/src/session.rs` sends
/// `ResolvedMediaPlan::chroma_token`'s own `"yuv420"`/`"yuv422"`/`"yuv444"`
/// (no `p` suffix, no bit-depth suffix). Matching on the
/// `"420"`/`"422"`/`"444"` substring, plus the `p010`/`nv12`/`gbrp` prefixes
/// real fourCCs use for 4:2:0/4:4:4 with no such digit substring at all
/// (`ServerColorCaps::default`'s own `"p010le"` included), covers every
/// value either host actually sends today. Anything else is reported as
/// `None` (unknown) rather than guessed at.
#[must_use]
pub fn parse_chroma_from_pix_fmt(pix_fmt: &str) -> Option<ChromaSubsampling> {
    let lowered = pix_fmt.to_ascii_lowercase();
    if lowered.contains("444") || lowered.starts_with("gbrp") {
        Some(ChromaSubsampling::Yuv444)
    } else if lowered.contains("422") {
        Some(ChromaSubsampling::Yuv422)
    } else if lowered.contains("420") || lowered.starts_with("p010") || lowered.starts_with("nv12")
    {
        Some(ChromaSubsampling::Yuv420)
    } else {
        None
    }
}

/// The fully-parsed "actual" colour contract, read back out of
/// `ServerColorCaps`'s wire-string `active_*` fields (and
/// `ServerHelloMsg::codec` for the codec axis) into the same typed
/// vocabulary `arcen_media::VideoConfiguration` uses for the client's own
/// request. Any field the wire string did not parse to a recognised token
/// is `None` (unknown), never guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ActiveContract {
    pub codec: Option<VideoCodec>,
    pub chroma: Option<ChromaSubsampling>,
    pub bit_depth: Option<BitDepth>,
    pub range: Option<ColorRange>,
    pub matrix: Option<ColorMatrix>,
    pub primaries: Option<ColorPrimaries>,
    pub transfer: Option<TransferCharacteristics>,
}

impl ActiveContract {
    #[must_use]
    pub fn from_hello(hello: &ServerHelloMsg) -> Self {
        let caps = &hello.color_caps;
        Self {
            codec: VideoCodec::from_token(&hello.codec),
            chroma: parse_chroma_from_pix_fmt(&caps.advertised_pix_fmt),
            bit_depth: BitDepth::from_token(&caps.active_bit_depth),
            range: ColorRange::from_token(&caps.active_range),
            matrix: ColorMatrix::from_token(&caps.active_matrix),
            primaries: ColorPrimaries::from_token(&caps.active_primaries),
            transfer: TransferCharacteristics::from_token(&caps.active_transfer),
        }
    }

    /// A [`ColorTransform`] for this contract's matrix/range/bit-depth, if
    /// every one of those three axes parsed. `None` (rather than assuming a
    /// default) when any of them did not: a guessed axis could make the
    /// w4-exactness-readout's "no codec involved" figure claim a transform
    /// the session is not actually using.
    #[must_use]
    #[cfg_attr(not(feature = "dev-tools"), allow(dead_code))]
    pub fn color_transform(&self) -> Option<ColorTransform> {
        Some(ColorTransform::new(
            self.matrix?,
            self.range?,
            self.bit_depth?,
        ))
    }
}

/// Which axes changed between what was requested and what
/// [`ActiveContract::from_hello`] read back, in the exact vocabulary
/// `arcen_media::video::PlanDegradation` already defines server-side for
/// backend-plan resolution -- reused here rather than inventing a parallel
/// type, so this client's notion of "degraded" can never quietly drift from
/// the one the resolver itself uses.
///
/// `fps_clamped`, `geometry_clamped` and `cursor_moved_to_local` are always
/// `false` here: none of the three has a wire field in `server_hello` today
/// (no requested-vs-resolved fps or geometry pair is transmitted), so this
/// client cannot observe them independently of the colour axes. If a future
/// wire change adds them, only this function needs to grow the extra
/// comparison; [`PlanDegradation::is_exact`]/[`PlanDegradation::colour_degraded`]
/// downstream already handle distinguishing them from a colour axis.
#[must_use]
pub fn negotiated_degradation(
    requested: VideoConfiguration,
    active: &ActiveContract,
) -> PlanDegradation {
    PlanDegradation {
        codec_changed: active.codec.is_some_and(|codec| codec != requested.codec),
        chroma_changed: active
            .chroma
            .is_some_and(|chroma| chroma != requested.chroma),
        bit_depth_reduced: active
            .bit_depth
            .is_some_and(|depth| depth < requested.bit_depth),
        range_changed: active.range.is_some_and(|range| range != requested.range),
        matrix_changed: active
            .matrix
            .is_some_and(|matrix| matrix != requested.matrix),
        fps_clamped: false,
        geometry_clamped: false,
        cursor_moved_to_local: false,
    }
}

/// Everything the negotiated-truth panel needs, computed once when
/// `server_hello` arrives (`ArcenApp::sync_media_state`) and cached for the
/// rest of the session: the request side
/// (`ClientSettings`/`ColorFidelitySettings`) cannot change mid-session (the
/// Settings screen is unreachable from `AppScreen::InSession`), so
/// recomputing this every frame would be pure waste.
#[derive(Debug, Clone, PartialEq)]
pub struct NegotiatedTruth {
    /// The concrete colour axes and compatibility codec preference this
    /// connection sent. `selection` determines whether codec differences are
    /// authorized host ranking or a genuine degradation.
    pub requested: VideoConfiguration,
    pub selection: VideoSelectionIntent,
    pub active: ActiveContract,
    pub degradation: PlanDegradation,
    /// `hello.encoder_backend`, or `"unknown"` when the host sent none.
    pub encoder_backend: String,
    pub encoder_hardware: bool,
}

impl NegotiatedTruth {
    #[must_use]
    pub fn from_hello_with_selection(
        hello: &ServerHelloMsg,
        requested: VideoConfiguration,
        selection: VideoSelectionIntent,
    ) -> Self {
        let active = ActiveContract::from_hello(hello);
        let mut degradation = negotiated_degradation(requested, &active);
        if selection == VideoSelectionIntent::AdaptivePerformance {
            // Codec choice is the point of this intent: AV1/HEVC/H.264 are
            // equivalent successful resolutions when the independently
            // requested chroma/depth/range/matrix remain intact.
            degradation.codec_changed = false;
        }
        let encoder_backend = if hello.encoder_backend.is_empty() {
            "unknown".to_string()
        } else {
            hello.encoder_backend.clone()
        };
        let encoder_hardware = encoder_is_hardware(&hello.encoder_backend, &hello.encoder_class);
        Self {
            requested,
            selection,
            active,
            degradation,
            encoder_backend,
            encoder_hardware,
        }
    }
}

/// `"hardware"`/`"software"`/`"unknown"` label for an `Option<bool>`
/// acceleration flag -- shared by the encode side (always `Some`, see
/// [`encoder_is_hardware`]) and the decode side
/// (`NativeVideoDecoder::is_hardware_accelerated`, genuinely `None` before
/// any decode session has been created).
#[must_use]
pub const fn hardware_label(hardware: Option<bool>) -> &'static str {
    match hardware {
        Some(true) => "hardware",
        Some(false) => "software",
        None => "unknown",
    }
}

/// One-line summary of a [`PlanDegradation`], naming every changed axis --
/// used by both the always-on banner and the detail panel, so the two can
/// never disagree. Distinguishes a colour downgrade
/// ([`PlanDegradation::colour_degraded`]) from a non-colour one via the
/// leading label: today the two happen to coincide for this client (see
/// [`negotiated_degradation`]'s doc for why), but the distinction is kept
/// live so a future fps/geometry wire signal only has to flip a bool here,
/// not change this function's contract.
#[must_use]
pub fn degradation_summary(degradation: PlanDegradation) -> String {
    if degradation.is_exact() {
        return "negotiated exactly as requested".to_string();
    }
    let mut axes = Vec::new();
    if degradation.codec_changed {
        axes.push("codec");
    }
    if degradation.chroma_changed {
        axes.push("chroma");
    }
    if degradation.bit_depth_reduced {
        axes.push("bit depth");
    }
    if degradation.range_changed {
        axes.push("range");
    }
    if degradation.matrix_changed {
        axes.push("matrix");
    }
    if degradation.fps_clamped {
        axes.push("fps");
    }
    if degradation.geometry_clamped {
        axes.push("geometry");
    }
    if degradation.cursor_moved_to_local {
        axes.push("cursor");
    }
    let label = if degradation.colour_degraded() {
        "COLOUR DEGRADED"
    } else {
        "DEGRADED"
    };
    format!("{label}: {}", axes.join(", "))
}

/// Whether the viewer should reserve permanent on-video space for negotiation
/// status. Exact sessions remain inspectable through the on-demand detail
/// panel, while any observable downgrade stays permanently visible.
#[must_use]
pub fn should_show_degradation_badge(degradation: PlanDegradation) -> bool {
    !degradation.is_exact()
}

#[must_use]
pub fn selection_summary(selection: VideoSelectionIntent, degradation: PlanDegradation) -> String {
    if selection == VideoSelectionIntent::AdaptivePerformance && degradation.is_exact() {
        "adaptive codec selected".to_string()
    } else {
        degradation_summary(degradation)
    }
}

// ============================================================================
// w4-exactness-readout: live pixel-exactness against a declared test pattern
// ============================================================================

/// Cycles `None -> TestPattern::ALL[0] -> ... -> TestPattern::ALL[last] ->
/// None`, matching `TestPattern::ALL`'s own declaration order so a
/// maintainer adding a pattern there needs no second edit here.
#[must_use]
#[cfg_attr(not(feature = "dev-tools"), allow(dead_code))]
pub fn next_test_pattern(current: Option<TestPattern>) -> Option<TestPattern> {
    match current {
        None => TestPattern::ALL.first().copied(),
        Some(pattern) => TestPattern::ALL
            .iter()
            .position(|candidate| *candidate == pattern)
            .and_then(|index| TestPattern::ALL.get(index + 1))
            .copied(),
    }
}

/// Swaps the R/B byte of every pixel, adapting this crate's own
/// `DecodedVideoFrame::rgba` (genuinely RGBA -- see
/// `pipeline::video_decoder::copy_locked_pixels`'s explicit B<->R swap out
/// of `CVPixelBuffer`'s native BGRA) to the BGRA layout
/// `arcen_media::test_pattern::measure_rgb_roundtrip` requires. Allocates a
/// fresh buffer rather than mutating in place: the caller's frame is a live
/// decode result, not a scratch buffer this module owns.
#[must_use]
#[cfg_attr(not(feature = "dev-tools"), allow(dead_code))]
pub fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for pixel in out.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    out
}

/// The two distinct exactness claims for one decoded frame against a
/// declared reference pattern. See the module doc and
/// `arcen_media::test_pattern`'s own doc for why they must never be
/// conflated: `colour_only` is pure colour-space maths with no codec
/// involved at all (what `arcen-media`'s own unit tests already prove exact
/// for 10-bit 4:4:4 full range); `end_to_end` is the same pattern compared
/// against this session's actual decoded pixels, so it also carries
/// whatever quantisation loss the negotiated codec added. Neither field
/// being `Some` and exact implies anything about the other.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExactnessReadout {
    pub pattern: TestPattern,
    /// `None` when the active contract's matrix/range/bit-depth could not
    /// all be parsed (see [`ActiveContract::color_transform`]), never
    /// because the comparison merely found error.
    pub colour_only: Option<ColorAccuracy>,
    /// `None` when the decoded buffer was shorter than `width * height * 4`
    /// bytes, never because the comparison merely found error.
    pub end_to_end: Option<ColorAccuracy>,
}

/// Measures `pattern` against one decoded frame, for both claims described
/// on [`ExactnessReadout`]. `decoded_rgba` must be this crate's own
/// `DecodedVideoFrame::rgba` layout: interleaved RGBA, row-major, tightly
/// packed (no row padding), at exactly `width * height * 4` bytes.
#[must_use]
#[cfg_attr(not(feature = "dev-tools"), allow(dead_code))]
pub fn measure_exactness(
    pattern: TestPattern,
    width: usize,
    height: usize,
    decoded_rgba: &[u8],
    active_transform: Option<ColorTransform>,
) -> ExactnessReadout {
    let colour_only = active_transform
        .map(|transform| measure_transform_roundtrip(pattern, transform, width, height));
    let bgra = rgba_to_bgra(decoded_rgba);
    let end_to_end = measure_rgb_roundtrip(pattern, width, height, &bgra);
    ExactnessReadout {
        pattern,
        colour_only,
        end_to_end,
    }
}

/// One line for one claim, clearly labelled. Never states or implies a
/// codec is lossless: callers must pass an `end_to_end`-style label
/// something like "end-to-end (includes codec)" so this never reads as a
/// pure-maths figure.
#[must_use]
#[cfg_attr(not(feature = "dev-tools"), allow(dead_code))]
pub fn format_accuracy(label: &str, accuracy: Option<ColorAccuracy>) -> String {
    match accuracy {
        None => format!("{label}: unavailable"),
        Some(accuracy) if accuracy.is_exact() => {
            format!("{label}: exact (0/255 over {} px)", accuracy.pixels)
        }
        Some(accuracy) => format!(
            "{label}: max {}/255, mean {:.2}/255, worst px ({}, {}), over {} px",
            accuracy.max_error,
            accuracy.mean_error,
            accuracy.worst_at.0,
            accuracy.worst_at.1,
            accuracy.pixels
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_with_color_caps(color_caps: serde_json::Value, codec: &str) -> ServerHelloMsg {
        serde_json::from_value(serde_json::json!({
            "type": "server_hello",
            "codec": codec,
            "color_caps": color_caps,
        }))
        .expect("minimal server_hello with color_caps parses")
    }

    // ---- encoder_is_hardware ----

    #[test]
    fn encoder_is_hardware_prefers_the_declared_class() {
        assert!(encoder_is_hardware("some-unnamed-backend", "hardware"));
        assert!(!encoder_is_hardware("native-nvenc", "software"));
    }

    #[test]
    fn encoder_is_hardware_falls_back_to_name_guess_when_class_is_unknown() {
        assert!(encoder_is_hardware("native-nvenc", ""));
        assert!(encoder_is_hardware("host-capenc-thing", "not-a-real-class"));
        assert!(!encoder_is_hardware("openh264-sw-h264", ""));
    }

    // ---- parse_chroma_from_pix_fmt ----

    #[test]
    fn parse_chroma_from_pix_fmt_covers_both_hosts_conventions() {
        // hosts/windows/src/session.rs: ResolvedMediaPlan::chroma_token().
        assert_eq!(
            parse_chroma_from_pix_fmt("yuv420"),
            Some(ChromaSubsampling::Yuv420)
        );
        assert_eq!(
            parse_chroma_from_pix_fmt("yuv422"),
            Some(ChromaSubsampling::Yuv422)
        );
        assert_eq!(
            parse_chroma_from_pix_fmt("yuv444"),
            Some(ChromaSubsampling::Yuv444)
        );
        // hosts/linux/src/session/handshake.rs: plain ffmpeg-style pix_fmt.
        assert_eq!(
            parse_chroma_from_pix_fmt("yuv420p"),
            Some(ChromaSubsampling::Yuv420)
        );
        assert_eq!(
            parse_chroma_from_pix_fmt("yuv444p"),
            Some(ChromaSubsampling::Yuv444)
        );
        assert_eq!(
            parse_chroma_from_pix_fmt("yuv422p10le"),
            Some(ChromaSubsampling::Yuv422)
        );
    }

    #[test]
    fn parse_chroma_from_pix_fmt_handles_fourccs_with_no_digit_substring() {
        // ServerColorCaps::default()'s own value.
        assert_eq!(
            parse_chroma_from_pix_fmt("p010le"),
            Some(ChromaSubsampling::Yuv420)
        );
        assert_eq!(
            parse_chroma_from_pix_fmt("nv12"),
            Some(ChromaSubsampling::Yuv420)
        );
        assert_eq!(
            parse_chroma_from_pix_fmt("gbrp10le"),
            Some(ChromaSubsampling::Yuv444)
        );
    }

    #[test]
    fn parse_chroma_from_pix_fmt_reports_unknown_rather_than_guessing() {
        assert_eq!(parse_chroma_from_pix_fmt("completely-unknown"), None);
        assert_eq!(parse_chroma_from_pix_fmt(""), None);
    }

    // ---- ActiveContract ----

    #[test]
    fn active_contract_parses_every_axis_from_a_real_hello() {
        let hello = hello_with_color_caps(
            serde_json::json!({
                "active_bit_depth": "10",
                "active_range": "full",
                "active_matrix": "bt709",
                "active_primaries": "bt709",
                "active_transfer": "bt709",
                "advertised_pix_fmt": "yuv444p10le",
            }),
            "h265",
        );
        let active = ActiveContract::from_hello(&hello);
        assert_eq!(active.codec, Some(VideoCodec::H265));
        assert_eq!(active.chroma, Some(ChromaSubsampling::Yuv444));
        assert_eq!(active.bit_depth, Some(BitDepth::Ten));
        assert_eq!(active.range, Some(ColorRange::Full));
        assert_eq!(active.matrix, Some(ColorMatrix::Bt709));
        assert_eq!(active.primaries, Some(ColorPrimaries::Bt709));
        assert_eq!(active.transfer, Some(TransferCharacteristics::Bt709));
        assert_eq!(
            active.color_transform(),
            Some(ColorTransform::new(
                ColorMatrix::Bt709,
                ColorRange::Full,
                BitDepth::Ten
            ))
        );
    }

    #[test]
    fn active_contract_color_transform_is_none_when_any_axis_is_unparseable() {
        let hello = hello_with_color_caps(
            serde_json::json!({
                "active_bit_depth": "not-a-depth",
                "active_range": "full",
                "active_matrix": "bt709",
            }),
            "h265",
        );
        let active = ActiveContract::from_hello(&hello);
        assert_eq!(active.bit_depth, None);
        assert_eq!(active.color_transform(), None);
    }

    // ---- negotiated_degradation ----

    #[test]
    fn negotiated_degradation_is_exact_when_active_matches_requested() {
        let requested = VideoConfiguration::grading_reference();
        let active = ActiveContract {
            codec: Some(requested.codec),
            chroma: Some(requested.chroma),
            bit_depth: Some(requested.bit_depth),
            range: Some(requested.range),
            matrix: Some(requested.matrix),
            primaries: Some(requested.primaries),
            transfer: Some(requested.transfer),
        };
        let degradation = negotiated_degradation(requested, &active);
        assert!(degradation.is_exact());
        assert!(!degradation.colour_degraded());
    }

    #[test]
    fn negotiated_degradation_flags_a_chroma_fallback() {
        let requested = VideoConfiguration::grading_reference(); // 4:4:4
        let active = ActiveContract {
            codec: Some(requested.codec),
            chroma: Some(ChromaSubsampling::Yuv420),
            bit_depth: Some(requested.bit_depth),
            range: Some(requested.range),
            matrix: Some(requested.matrix),
            primaries: Some(requested.primaries),
            transfer: Some(requested.transfer),
        };
        let degradation = negotiated_degradation(requested, &active);
        assert!(!degradation.is_exact());
        assert!(degradation.colour_degraded());
        assert!(degradation.chroma_changed);
        assert!(!degradation.bit_depth_reduced);
    }

    #[test]
    fn negotiated_degradation_bit_depth_reduced_is_directional_not_just_changed() {
        let requested = VideoConfiguration::legacy_h264(); // eight-bit
                                                           // Active came back *richer* than requested: never a "reduction".
        let richer = ActiveContract {
            bit_depth: Some(BitDepth::Ten),
            ..ActiveContract::from_hello(&hello_with_color_caps(serde_json::json!({}), "h264"))
        };
        assert!(!negotiated_degradation(requested, &richer).bit_depth_reduced);

        let requested_ten = VideoConfiguration::grading_reference(); // ten-bit
        let reduced = ActiveContract {
            bit_depth: Some(BitDepth::Eight),
            ..ActiveContract::from_hello(&hello_with_color_caps(serde_json::json!({}), "h265"))
        };
        assert!(negotiated_degradation(requested_ten, &reduced).bit_depth_reduced);
    }

    #[test]
    fn negotiated_degradation_never_claims_fps_or_geometry_or_cursor_axes() {
        // Documented gap: server_hello carries no requested-vs-resolved fps
        // or geometry pair, so this client can never observe them.
        let requested = VideoConfiguration::legacy_h264();
        let active = ActiveContract::default();
        let degradation = negotiated_degradation(requested, &active);
        assert!(!degradation.fps_clamped);
        assert!(!degradation.geometry_clamped);
        assert!(!degradation.cursor_moved_to_local);
    }

    #[test]
    fn negotiated_degradation_treats_unparseable_axes_as_not_shown_as_changed() {
        // An axis this client could not parse must never itself manufacture
        // a "degraded" claim -- see the module doc's honesty note.
        let requested = VideoConfiguration::grading_reference();
        let active = ActiveContract::default(); // every axis None
        let degradation = negotiated_degradation(requested, &active);
        assert!(degradation.is_exact());
    }

    #[test]
    fn adaptive_performance_accepts_host_ranked_codec_but_not_colour_changes() {
        let requested = VideoConfiguration::legacy_h264();
        let hello = hello_with_color_caps(serde_json::json!({}), "av1");
        let adaptive = NegotiatedTruth::from_hello_with_selection(
            &hello,
            requested,
            VideoSelectionIntent::AdaptivePerformance,
        );
        assert!(adaptive.degradation.is_exact());
        assert_eq!(adaptive.active.codec, Some(VideoCodec::Av1));

        let exact = NegotiatedTruth::from_hello_with_selection(
            &hello,
            requested,
            VideoSelectionIntent::Exact,
        );
        assert!(exact.degradation.codec_changed);

        let changed_range = hello_with_color_caps(
            serde_json::json!({
                "active_range": "full",
                "advertised_pix_fmt": "yuv420p"
            }),
            "av1",
        );
        let adaptive = NegotiatedTruth::from_hello_with_selection(
            &changed_range,
            requested,
            VideoSelectionIntent::AdaptivePerformance,
        );
        assert!(
            adaptive.degradation.range_changed,
            "adaptive applies only to codec choice, never to colour axes"
        );
    }

    // ---- hardware_label ----

    #[test]
    fn hardware_label_covers_every_state() {
        assert_eq!(hardware_label(Some(true)), "hardware");
        assert_eq!(hardware_label(Some(false)), "software");
        assert_eq!(hardware_label(None), "unknown");
    }

    // ---- degradation_summary ----

    #[test]
    fn degradation_summary_reports_exact() {
        assert_eq!(
            degradation_summary(PlanDegradation::default()),
            "negotiated exactly as requested"
        );
    }

    #[test]
    fn only_degraded_sessions_need_a_permanent_badge() {
        assert!(!should_show_degradation_badge(PlanDegradation::default()));
        assert!(should_show_degradation_badge(PlanDegradation {
            matrix_changed: true,
            ..PlanDegradation::default()
        }));
    }

    #[test]
    fn degradation_summary_names_every_changed_colour_axis() {
        let degradation = PlanDegradation {
            chroma_changed: true,
            bit_depth_reduced: true,
            ..PlanDegradation::default()
        };
        let summary = degradation_summary(degradation);
        assert!(summary.starts_with("COLOUR DEGRADED"));
        assert!(summary.contains("chroma"));
        assert!(summary.contains("bit depth"));
        assert!(!summary.contains("matrix"));
    }

    #[test]
    fn degradation_summary_distinguishes_non_colour_degradation() {
        let degradation = PlanDegradation {
            fps_clamped: true,
            ..PlanDegradation::default()
        };
        let summary = degradation_summary(degradation);
        assert!(summary.starts_with("DEGRADED"));
        assert!(!summary.starts_with("COLOUR DEGRADED"));
        assert!(summary.contains("fps"));
    }

    // ---- next_test_pattern ----

    #[test]
    fn next_test_pattern_cycles_through_every_pattern_and_back_to_none() {
        let mut current = None;
        let mut seen = Vec::new();
        for _ in 0..TestPattern::ALL.len() {
            current = next_test_pattern(current);
            seen.push(current.expect("every step but the last stays Some"));
        }
        assert_eq!(seen, TestPattern::ALL.to_vec());
        assert_eq!(next_test_pattern(current), None);
    }

    // ---- rgba_to_bgra ----

    #[test]
    fn rgba_to_bgra_swaps_only_red_and_blue() {
        let rgba = [10u8, 20, 30, 40, 50, 60, 70, 80];
        let bgra = rgba_to_bgra(&rgba);
        assert_eq!(bgra, vec![30, 20, 10, 40, 70, 60, 50, 80]);
        // Round-trips: swapping twice is the identity.
        assert_eq!(rgba_to_bgra(&bgra), rgba);
    }

    // ---- measure_exactness / format_accuracy ----

    /// `DecodedVideoFrame::rgba`'s own layout: interleaved RGBA, row-major.
    fn render_rgba(pattern: TestPattern, width: usize, height: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = pattern.pixel(x, y, width, height);
                out.extend_from_slice(&[r, g, b, 0xff]);
            }
        }
        out
    }

    #[test]
    fn measure_exactness_is_exact_both_ways_for_an_untouched_frame_at_ten_bit_full_range() {
        let pattern = TestPattern::SaturatedPrimaries;
        let (width, height) = (64, 32);
        let decoded = render_rgba(pattern, width, height);
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        let readout = measure_exactness(pattern, width, height, &decoded, Some(transform));
        assert_eq!(readout.pattern, pattern);
        assert!(readout.colour_only.expect("transform was Some").is_exact());
        assert!(readout
            .end_to_end
            .expect("buffer was correctly sized")
            .is_exact());
    }

    #[test]
    fn measure_exactness_end_to_end_detects_a_perturbed_pixel() {
        let pattern = TestPattern::GreyRamp;
        let (width, height) = (64, 32);
        let mut decoded = render_rgba(pattern, width, height);
        // Perturb one pixel's green channel by a known amount.
        decoded[1] = decoded[1].wrapping_add(7);
        let readout = measure_exactness(pattern, width, height, &decoded, None);
        assert_eq!(readout.colour_only, None);
        let end_to_end = readout.end_to_end.expect("buffer was correctly sized");
        assert_eq!(end_to_end.max_error, 7);
        assert!(!end_to_end.is_exact());
    }

    #[test]
    fn measure_exactness_end_to_end_is_none_for_a_truncated_buffer() {
        let pattern = TestPattern::GreyRamp;
        let readout = measure_exactness(pattern, 64, 32, &[0u8; 4], None);
        assert_eq!(readout.end_to_end, None);
    }

    #[test]
    fn format_accuracy_never_hides_which_measurement_is_unavailable() {
        assert_eq!(
            format_accuracy("colour-only", None),
            "colour-only: unavailable"
        );
    }

    #[test]
    fn format_accuracy_reports_exact_distinctly_from_measured_error() {
        let exact = ColorAccuracy {
            max_error: 0,
            mean_error: 0.0,
            pixels: 100,
            worst_at: (0, 0),
        };
        assert_eq!(
            format_accuracy("end-to-end", Some(exact)),
            "end-to-end: exact (0/255 over 100 px)"
        );
        let inexact = ColorAccuracy {
            max_error: 5,
            mean_error: 1.5,
            pixels: 100,
            worst_at: (3, 4),
        };
        let formatted = format_accuracy("end-to-end", Some(inexact));
        assert!(formatted.contains("max 5/255"));
        assert!(formatted.contains("worst px (3, 4)"));
    }
}
