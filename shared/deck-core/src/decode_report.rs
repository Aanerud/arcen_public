// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What the decoder proved, with the GPU's name and why each refused
//! profile was refused, and what that lets this Deck ask for.
//!
//! [`crate::decode::DecodeCapabilities`] is what the hello needs: flags.
//! A person needs more: which GPU answered, and the exact profile it did
//! not offer ("NVIDIA GRID V100D-16Q decodes HEVC Main and Main 10, but not
//! 4:4:4"). The Settings screen explains an unavailable Grading or HDR
//! preset from it (PARITY Q-13), and the support bundle records it.
//!
//! The rule for what a Deck may ask for is here too, so both Decks follow
//! one: a preset is offered only when the Pier's own decode check
//! ([`arcen_media::video::client_can_decode`]) passes for this decoder's
//! flags, and a connection never requests one that does not
//! ([`decodable_preset`]). Only the probes that fill the flags stay per
//! operating system.

use arcen_media::VideoCodec;
use arcen_media::video::{PipelineId, client_can_decode, pipeline_codec_ladder};
use arcen_protocol::messages::VideoSelectionIntent;

use crate::decode::DecodeCapabilities;
use crate::settings::{
    ColorFidelitySettings, PerformanceMode, StreamingPreset, effective_color_fidelity_variant,
};

/// The profile Grading and HDR need: HEVC `RExt` 4:4:4 10-bit. Both
/// fidelity presets stream it (never AV1), so a decoder without it serves
/// only Auto and Speed.
pub const FIDELITY_PROFILE: &str = "hevc-main10-444";

/// The AV1 profile token in a decoder's refusals.
pub const AV1_PROFILE: &str = "av1";

/// One profile the decoder refused, in the decoder's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeRefusal {
    /// The decoder's profile token, for example `hevc-main10-444`.
    pub profile: String,
    /// Why, for example `hevc-main10-444 is not offered by NVIDIA GRID
    /// V100D-16Q`.
    pub reason: String,
}

const NOT_DECODED_YET: &str = ", but this Deck does not decode it yet";

impl DecodeRefusal {
    /// The GPU does not list `profile` at all: `<profile> is not offered by
    /// <decoder>`.
    #[must_use]
    pub fn not_offered(profile: &str, decoder: &str) -> Self {
        Self {
            profile: profile.to_owned(),
            reason: format!("{profile} is not offered by {decoder}"),
        }
    }

    /// The GPU lists `profile`, but this Deck has no decoder for it yet.
    #[must_use]
    pub fn not_decoded_yet(profile: &str, decoder: &str) -> Self {
        Self {
            profile: profile.to_owned(),
            reason: format!("{profile} is offered by {decoder}{NOT_DECODED_YET}"),
        }
    }

    /// Whether the GPU simply does not offer the profile (rather than
    /// offering one that failed Arcen's check).
    #[must_use]
    pub fn is_absent(&self) -> bool {
        self.reason
            .strip_prefix(self.profile.as_str())
            .is_some_and(|rest| rest.starts_with(" is not offered by "))
    }

    /// Whether the GPU offers the profile and only this Deck lacks it.
    #[must_use]
    pub fn is_not_decoded_yet(&self) -> bool {
        self.reason.ends_with(NOT_DECODED_YET)
    }
}

/// What one decoder proved on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DecodeReport {
    /// The GPU (or decoder) that answered, for example `NVIDIA GRID
    /// V100D-16Q` or `Apple M4 (VideoToolbox)`. Empty when unknown.
    pub decoder: String,
    /// The hello's flags.
    pub capabilities: DecodeCapabilities,
    /// Every profile it refused, with the reason.
    pub refusals: Vec<DecodeRefusal>,
}

/// The codecs Auto and Speed may be served in, best first: the Pier's own
/// ladder (AV1, HEVC, H.264) less what this decoder cannot decode. The
/// hello advertises exactly these, and the Pier's ladder picks from them.
#[must_use]
pub fn adaptive_codecs(capabilities: &DecodeCapabilities) -> Vec<VideoCodec> {
    pipeline_codec_ladder(PipelineId::Auto, VideoCodec::Av1)
        .iter()
        .copied()
        .filter(|codec| match codec {
            VideoCodec::Av1 => capabilities.av1,
            VideoCodec::H265 => capabilities.h265,
            VideoCodec::H264 => capabilities.h264,
            VideoCodec::Jpeg | VideoCodec::Vp9 => false,
        })
        .collect()
}

/// Whether a request for `video` with `selection` decodes here, by the
/// check the Pier applies to it: an adaptive (Auto/Speed) request on any
/// codec of [`adaptive_codecs`], any other on its own codec.
#[must_use]
pub fn request_decodes(
    capabilities: &DecodeCapabilities,
    selection: VideoSelectionIntent,
    video: arcen_media::VideoConfiguration,
) -> bool {
    let flags = capabilities.video_capabilities();
    if selection == VideoSelectionIntent::AdaptivePerformance {
        adaptive_codecs(capabilities).into_iter().any(|codec| {
            client_can_decode(arcen_media::VideoConfiguration { codec, ..video }, flags)
        })
    } else {
        client_can_decode(video, flags)
    }
}

/// Whether `preset`'s request decodes here. Custom (a developer's exact
/// axes) is never second-guessed.
#[must_use]
pub fn preset_decodes(capabilities: &DecodeCapabilities, preset: StreamingPreset) -> bool {
    if preset == StreamingPreset::Custom {
        return true;
    }
    let mut performance = PerformanceMode::Standard;
    let mut color = ColorFidelitySettings::default();
    preset.apply_to(&mut performance, &mut color);
    request_decodes(
        capabilities,
        color.video_selection(),
        effective_color_fidelity_variant(color).video,
    )
}

/// The preset a connection asks for: `preset`, or Auto when this decoder
/// cannot decode it (Grading or HDR without HEVC Main 4:4:4 10-bit). Auto
/// and Speed are adaptive: the Pier picks their codec from what the hello
/// advertises.
#[must_use]
pub fn decodable_preset(
    capabilities: &DecodeCapabilities,
    preset: StreamingPreset,
) -> StreamingPreset {
    if matches!(preset, StreamingPreset::Grading | StreamingPreset::Hdr)
        && !preset_decodes(capabilities, preset)
    {
        StreamingPreset::Auto
    } else {
        preset
    }
}

impl DecodeReport {
    /// A report that knows only the flags (a platform without per-profile
    /// reasons).
    #[must_use]
    pub fn from_capabilities(decoder: impl Into<String>, capabilities: DecodeCapabilities) -> Self {
        Self {
            decoder: decoder.into(),
            capabilities,
            refusals: Vec::new(),
        }
    }

    /// Whether Grading and HDR can both be decoded.
    #[must_use]
    pub fn decodes_fidelity(&self) -> bool {
        preset_decodes(&self.capabilities, StreamingPreset::Grading)
            && preset_decodes(&self.capabilities, StreamingPreset::Hdr)
    }

    fn refusal(&self, profile: &str) -> Option<&DecodeRefusal> {
        self.refusals
            .iter()
            .find(|refusal| refusal.profile == profile)
    }

    /// The GPU's name, or "This GPU" when the decoder did not say.
    fn subject(&self) -> &str {
        if self.decoder.is_empty() {
            "This GPU"
        } else {
            &self.decoder
        }
    }

    /// The GPU's name, or "this GPU", inside a sentence.
    fn subject_in_sentence(&self) -> &str {
        if self.decoder.is_empty() {
            "this GPU"
        } else {
            &self.decoder
        }
    }

    /// Why `preset` cannot be decoded here, naming the profile it needs, the
    /// GPU, what that GPU decodes and what it lacks: "Grading needs HEVC
    /// Main 4:4:4 10-bit decoding. NVIDIA GRID V100D-16Q decodes HEVC Main
    /// and Main 10, but not 4:4:4." `None` when it decodes.
    #[must_use]
    pub fn preset_refusal(&self, preset: StreamingPreset) -> Option<String> {
        if preset_decodes(&self.capabilities, preset) {
            return None;
        }
        let title = preset.title();
        if matches!(preset, StreamingPreset::Auto | StreamingPreset::Speed) {
            return Some(format!(
                "{title} needs an H.264, HEVC or AV1 decoder. {} offered none.",
                self.subject()
            ));
        }
        let caps = &self.capabilities;
        let subject = self.subject();
        let need = profile_name(FIDELITY_PROFILE);
        if !caps.h265 {
            return Some(format!(
                "{title} needs {need} decoding. {subject} has no HEVC decoder."
            ));
        }
        let mut decodes = vec![profile_name("hevc-main")];
        let mut lacks = Vec::new();
        if caps.main10 {
            decodes.push("Main 10");
        } else {
            lacks.push("10-bit");
        }
        if caps.yuv444 {
            decodes.push("4:4:4");
        } else {
            lacks.push("4:4:4");
        }
        if !caps.full_range {
            lacks.push("full-range video");
        }
        if preset == StreamingPreset::Hdr && !caps.bt2020_ncl_matrix {
            lacks.push("the BT.2020 matrix");
        }
        let detail = self
            .refusal(FIDELITY_PROFILE)
            .filter(|refusal| !refusal.is_absent() && !caps.yuv444)
            .map(|refusal| format!(" ({})", humanize_reason(&refusal.profile, &refusal.reason)))
            .unwrap_or_default();
        Some(if lacks.is_empty() {
            format!("{title} needs {need} decoding, which {subject} does not offer.")
        } else {
            format!(
                "{title} needs {need} decoding. {subject} decodes {}, but not {}{detail}.",
                join(&decodes, "and"),
                join(&lacks, "or"),
            )
        })
    }

    /// The codecs Auto and Speed cannot use here and why, as sentences:
    /// "AV1 is not available: NVIDIA GRID V100D-16Q has no AV1 decoder."
    /// and, rarely, "This GPU has no HEVC decoder; Arcen uses H.264."
    /// `None` when AV1 and HEVC both decode.
    #[must_use]
    pub fn codec_note(&self) -> Option<String> {
        let caps = &self.capabilities;
        if !caps.h264 && !caps.h265 && !caps.av1 {
            return Some(format!(
                "{} offered no video decoder Arcen can use (H.264, HEVC or AV1).",
                self.subject()
            ));
        }
        let mut sentences = Vec::new();
        if !caps.av1 {
            sentences.push(match self.refusal(AV1_PROFILE) {
                Some(refusal) if refusal.is_not_decoded_yet() => format!(
                    "AV1 is not available: {} has an AV1 decoder, but this Deck does not use \
                     AV1 yet.",
                    self.subject_in_sentence()
                ),
                _ => format!(
                    "AV1 is not available: {} has no AV1 decoder.",
                    self.subject_in_sentence()
                ),
            });
        }
        if !caps.h265 {
            sentences.push(format!(
                "{} has no HEVC decoder; Arcen uses {}.",
                self.subject(),
                if caps.av1 { "AV1 or H.264" } else { "H.264" }
            ));
        }
        (!sentences.is_empty()).then(|| sentences.join(" "))
    }

    /// The report as the support bundle records it.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let caps = &self.capabilities;
        serde_json::json!({
            "decoder": self.decoder,
            "backend": caps.decoder_backend_label(),
            "capabilities": {
                "h264": caps.h264,
                "h265": caps.h265,
                "av1": caps.av1,
                "yuv444": caps.yuv444,
                "main10": caps.main10,
                "main12": caps.main12,
                "full_range": caps.full_range,
                "identity_matrix": caps.identity_matrix,
                "bt601_matrix": caps.bt601_matrix,
                "bt2020_ncl_matrix": caps.bt2020_ncl_matrix,
                "hardware_accelerated": caps.hardware_accelerated,
            },
            "grading_and_hdr_decode": self.decodes_fidelity(),
            "refusals": self
                .refusals
                .iter()
                .map(|refusal| serde_json::json!({
                    "profile": refusal.profile,
                    "reason": refusal.reason,
                }))
                .collect::<Vec<_>>(),
        })
    }
}

/// `a`, `a and b`, `a, b and c` (or `or`).
fn join(items: &[&str], conjunction: &str) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [init @ .., last] => format!("{} {conjunction} {last}", init.join(", ")),
    }
}

/// A decoder profile token as a person reads it.
#[must_use]
pub fn profile_name(token: &str) -> &str {
    match token {
        "h264" => "H.264 High",
        "hevc-main" => "HEVC Main",
        "hevc-main10" => "HEVC Main 10",
        "hevc-main-444" => "HEVC Main 4:4:4",
        "hevc-main10-444" => "HEVC Main 4:4:4 10-bit",
        "av1" => "AV1 Main",
        "adapter" => "A video decoder",
        other => other,
    }
}

/// The decoder's reason with its leading profile token spelled out:
/// `hevc-main10-444 is not offered by X` → `HEVC Main 4:4:4 10-bit is not
/// offered by X`.
#[must_use]
pub fn humanize_reason(token: &str, reason: &str) -> String {
    match reason.strip_prefix(token) {
        Some(rest) if token != "adapter" => format!("{}{rest}", profile_name(token)),
        _ => reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows Deck's D3D11 flags for a GPU (full range, BT.601 and
    /// BT.2020 are always honoured there).
    fn d3d11(h265: bool, main10: bool, yuv444: bool) -> DecodeCapabilities {
        DecodeCapabilities {
            h264: true,
            h265,
            main10,
            yuv444,
            full_range: true,
            bt601_matrix: true,
            bt2020_ncl_matrix: true,
            ..DecodeCapabilities::default()
        }
    }

    fn v100d() -> DecodeReport {
        DecodeReport {
            decoder: "NVIDIA GRID V100D-16Q".to_owned(),
            capabilities: d3d11(true, true, false),
            refusals: vec![
                DecodeRefusal::not_offered("hevc-main-444", "NVIDIA GRID V100D-16Q"),
                DecodeRefusal::not_offered(FIDELITY_PROFILE, "NVIDIA GRID V100D-16Q"),
                DecodeRefusal::not_offered(AV1_PROFILE, "NVIDIA GRID V100D-16Q"),
            ],
        }
    }

    #[test]
    fn grading_and_hdr_name_the_profile_the_gpu_and_what_it_lacks() {
        let report = v100d();
        assert_eq!(
            report.preset_refusal(StreamingPreset::Grading).as_deref(),
            Some(
                "Grading needs HEVC Main 4:4:4 10-bit decoding. NVIDIA GRID V100D-16Q decodes \
                 HEVC Main and Main 10, but not 4:4:4."
            )
        );
        assert_eq!(
            report.preset_refusal(StreamingPreset::Hdr).as_deref(),
            Some(
                "HDR needs HEVC Main 4:4:4 10-bit decoding. NVIDIA GRID V100D-16Q decodes HEVC \
                 Main and Main 10, but not 4:4:4."
            )
        );
        assert_eq!(report.preset_refusal(StreamingPreset::Auto), None);
        assert_eq!(report.preset_refusal(StreamingPreset::Speed), None);
        assert!(!report.decodes_fidelity());
    }

    #[test]
    fn a_profile_that_failed_its_check_says_how() {
        let mut report = DecodeReport::from_capabilities("Example GPU", d3d11(true, false, false));
        report.refusals.push(DecodeRefusal {
            profile: FIDELITY_PROFILE.to_owned(),
            reason: "hevc-main10-444 probe keyframe decoded wrongly on Example GPU".to_owned(),
        });
        assert_eq!(
            report.preset_refusal(StreamingPreset::Grading).as_deref(),
            Some(
                "Grading needs HEVC Main 4:4:4 10-bit decoding. Example GPU decodes HEVC Main, \
                 but not 10-bit or 4:4:4 (HEVC Main 4:4:4 10-bit probe keyframe decoded wrongly \
                 on Example GPU)."
            )
        );
    }

    #[test]
    fn a_gpu_with_the_profile_offers_every_preset() {
        let report =
            DecodeReport::from_capabilities("NVIDIA GRID RTX6000-8Q", d3d11(true, true, true));
        for preset in StreamingPreset::PRIMARY {
            assert_eq!(report.preset_refusal(preset), None, "{preset:?}");
            assert_eq!(decodable_preset(&report.capabilities, preset), preset);
        }
        assert!(report.decodes_fidelity());
    }

    #[test]
    fn a_connection_never_asks_for_a_preset_the_decoder_lacks() {
        let caps = v100d().capabilities;
        assert_eq!(
            decodable_preset(&caps, StreamingPreset::Grading),
            StreamingPreset::Auto
        );
        assert_eq!(
            decodable_preset(&caps, StreamingPreset::Hdr),
            StreamingPreset::Auto
        );
        assert_eq!(
            decodable_preset(&caps, StreamingPreset::Speed),
            StreamingPreset::Speed
        );
        assert_eq!(
            decodable_preset(&caps, StreamingPreset::Custom),
            StreamingPreset::Custom
        );
        // HDR also needs the BT.2020 matrix; Grading does not.
        let no_bt2020 = DecodeCapabilities {
            bt2020_ncl_matrix: false,
            ..d3d11(true, true, true)
        };
        assert_eq!(
            decodable_preset(&no_bt2020, StreamingPreset::Grading),
            StreamingPreset::Grading
        );
        assert_eq!(
            decodable_preset(&no_bt2020, StreamingPreset::Hdr),
            StreamingPreset::Auto
        );
    }

    #[test]
    fn auto_and_speed_offer_only_the_codecs_that_decode() {
        assert_eq!(
            adaptive_codecs(&v100d().capabilities),
            [VideoCodec::H265, VideoCodec::H264]
        );
        let h264_only = d3d11(false, false, false);
        assert_eq!(adaptive_codecs(&h264_only), [VideoCodec::H264]);
        let all = DecodeCapabilities {
            av1: true,
            ..d3d11(true, true, true)
        };
        assert_eq!(
            adaptive_codecs(&all),
            [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264]
        );
        assert!(!preset_decodes(
            &DecodeCapabilities::default(),
            StreamingPreset::Auto
        ));
    }

    #[test]
    fn the_codec_note_says_why_av1_or_hevc_is_missing() {
        assert_eq!(
            v100d().codec_note().as_deref(),
            Some("AV1 is not available: NVIDIA GRID V100D-16Q has no AV1 decoder.")
        );
        let mut listed = DecodeReport::from_capabilities("Example GPU", d3d11(true, true, true));
        listed
            .refusals
            .push(DecodeRefusal::not_decoded_yet(AV1_PROFILE, "Example GPU"));
        assert_eq!(
            listed.codec_note().as_deref(),
            Some(
                "AV1 is not available: Example GPU has an AV1 decoder, but this Deck does not \
                 use AV1 yet."
            )
        );
        let h264_only = DecodeReport::from_capabilities("", d3d11(false, false, false));
        assert_eq!(
            h264_only.codec_note().as_deref(),
            Some(
                "AV1 is not available: this GPU has no AV1 decoder. This GPU has no HEVC \
                 decoder; Arcen uses H.264."
            )
        );
        assert_eq!(
            h264_only
                .preset_refusal(StreamingPreset::Grading)
                .as_deref(),
            Some("Grading needs HEVC Main 4:4:4 10-bit decoding. This GPU has no HEVC decoder.")
        );
        let everything = DecodeCapabilities {
            av1: true,
            ..d3d11(true, true, true)
        };
        assert_eq!(
            DecodeReport::from_capabilities("Apple M4", everything).codec_note(),
            None
        );
    }

    #[test]
    fn refusals_tell_absence_from_failure() {
        assert!(DecodeRefusal::not_offered(AV1_PROFILE, "X").is_absent());
        assert!(!DecodeRefusal::not_decoded_yet(AV1_PROFILE, "X").is_absent());
        assert!(DecodeRefusal::not_decoded_yet(AV1_PROFILE, "X").is_not_decoded_yet());
        assert!(
            !DecodeRefusal {
                profile: FIDELITY_PROFILE.to_owned(),
                reason: "hevc-main10-444 cannot output Y410 on X".to_owned(),
            }
            .is_absent()
        );
    }
}
