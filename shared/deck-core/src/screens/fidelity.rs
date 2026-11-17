// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Why a streaming preset or codec is unavailable, in words a user can act
//! on (PARITY Q-13): the GPU, the exact profile it lacks,
//! what still works, and what to do about it. One table for both Decks;
//! only the machine's name and the Windows "Use HDR" hint differ. The
//! sentences about the decoder come from
//! [`crate::decode_report::DecodeReport`], which also decides what a
//! connection may ask for, so what Settings greys out is what is never
//! requested.

use super::settings::no_hdr_screen;
use super::wording::DeckPlatform;
use crate::decode_report::{DecodeReport, FIDELITY_PROFILE, adaptive_codecs, profile_name};
use crate::settings::{StreamingPreset, color_fidelity_codec_label};

/// Why each fidelity preset is unavailable; `None` where it is available.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FidelityAvailability {
    /// Why Grading cannot be chosen.
    pub grading: Option<String>,
    /// Why HDR cannot be chosen.
    pub hdr: Option<String>,
    /// Which codecs Auto and Speed use here, when AV1 or HEVC is missing,
    /// and why.
    pub codecs: Option<String>,
}

/// The reasons for this Deck: `hdr_screen` from the displays' colour
/// facts, `decode` from [`crate::DeckServices::decode_report`] (`None`
/// while it is unknown, which blocks nothing).
#[must_use]
pub fn fidelity_availability(
    platform: DeckPlatform,
    hdr_screen: bool,
    decode: Option<&DecodeReport>,
) -> FidelityAvailability {
    let grading =
        decode.and_then(|report| decoder_reason(platform, report, StreamingPreset::Grading));
    let hdr_decoder =
        decode.and_then(|report| decoder_reason(platform, report, StreamingPreset::Hdr));
    let hdr = match (hdr_decoder, hdr_screen) {
        (Some(reason), true) => Some(reason),
        (Some(reason), false) => Some(format!("{reason} {}", hdr_display_hint(platform))),
        (None, false) => Some(no_hdr_screen_reason(platform)),
        (None, true) => None,
    };
    FidelityAvailability {
        grading,
        hdr,
        codecs: decode.and_then(|report| codec_reason(platform, report)),
    }
}

/// HDR without an HDR-capable screen. The macOS Deck keeps its sentence
/// unchanged (it already says what to do; its Settings screen stays
/// pixel-identical); the PC adds what still works, because Windows' usual
/// cause is a switch the user can turn on.
fn no_hdr_screen_reason(platform: DeckPlatform) -> String {
    match platform {
        DeckPlatform::Mac => no_hdr_screen(platform).to_owned(),
        DeckPlatform::Pc => format!(
            "{} Auto, Speed and Grading still work.",
            no_hdr_screen(platform)
        ),
    }
}

fn decoder_reason(
    platform: DeckPlatform,
    report: &DecodeReport,
    preset: StreamingPreset,
) -> Option<String> {
    let refusal = report.preset_refusal(preset)?;
    Some(format!(
        "{refusal} Auto and Speed still work. To use {title}, connect from a {machine} whose \
         GPU decodes {profile}.",
        title = preset.title(),
        machine = platform.machine(),
        profile = profile_name(FIDELITY_PROFILE),
    ))
}

/// "On this PC, Auto and Speed stream HEVC or H.264. AV1 is not available:
/// …": only when AV1 or HEVC is missing.
fn codec_reason(platform: DeckPlatform, report: &DecodeReport) -> Option<String> {
    let note = report.codec_note()?;
    let codecs = adaptive_codecs(&report.capabilities);
    if codecs.is_empty() {
        return Some(note);
    }
    let names: Vec<&str> = codecs
        .into_iter()
        .map(|codec| match codec {
            arcen_media::VideoCodec::H265 => "HEVC",
            other => color_fidelity_codec_label(other),
        })
        .collect();
    Some(format!(
        "On {this}, Auto and Speed stream {}. {note}",
        names.join(" or "),
        this = platform.this_machine(),
    ))
}

fn hdr_display_hint(platform: DeckPlatform) -> &'static str {
    match platform {
        DeckPlatform::Mac => "HDR also needs an HDR-capable display.",
        DeckPlatform::Pc => {
            "HDR also needs an HDR-capable display with Use HDR turned on in Windows display \
             settings."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::DecodeCapabilities;
    use crate::decode_report::{AV1_PROFILE, DecodeRefusal};

    fn grid(yuv444: bool) -> DecodeReport {
        DecodeReport {
            decoder: "NVIDIA GRID V100D-16Q".to_owned(),
            capabilities: DecodeCapabilities {
                h264: true,
                h265: true,
                main10: true,
                yuv444,
                full_range: true,
                bt601_matrix: true,
                bt2020_ncl_matrix: true,
                ..DecodeCapabilities::default()
            },
            refusals: vec![
                DecodeRefusal::not_offered(FIDELITY_PROFILE, "NVIDIA GRID V100D-16Q"),
                DecodeRefusal::not_offered(AV1_PROFILE, "NVIDIA GRID V100D-16Q"),
            ],
        }
    }

    #[test]
    fn a_missing_decoder_names_the_gpu_profile_what_works_and_what_to_do() {
        let reasons = fidelity_availability(DeckPlatform::Pc, true, Some(&grid(false)));
        assert_eq!(
            reasons.grading.as_deref(),
            Some(
                "Grading needs HEVC Main 4:4:4 10-bit decoding. NVIDIA GRID V100D-16Q decodes \
                 HEVC Main and Main 10, but not 4:4:4. Auto and Speed still work. To use \
                 Grading, connect from a PC whose GPU decodes HEVC Main 4:4:4 10-bit."
            )
        );
        assert_eq!(
            reasons.hdr.as_deref(),
            Some(
                "HDR needs HEVC Main 4:4:4 10-bit decoding. NVIDIA GRID V100D-16Q decodes HEVC \
                 Main and Main 10, but not 4:4:4. Auto and Speed still work. To use HDR, \
                 connect from a PC whose GPU decodes HEVC Main 4:4:4 10-bit."
            )
        );
        assert_eq!(
            reasons.codecs.as_deref(),
            Some(
                "On this PC, Auto and Speed stream HEVC or H.264. AV1 is not available: NVIDIA \
                 GRID V100D-16Q has no AV1 decoder."
            )
        );
    }

    #[test]
    fn without_an_hdr_screen_the_pc_hint_names_use_hdr() {
        let reasons = fidelity_availability(DeckPlatform::Pc, false, Some(&grid(true)));
        assert_eq!(reasons.grading, None);
        assert_eq!(
            reasons.hdr.as_deref(),
            Some(
                "No HDR-capable screen found. Turn on Use HDR in Windows display settings, or \
                 connect an HDR display. Auto, Speed and Grading still work."
            )
        );
        let both = fidelity_availability(DeckPlatform::Pc, false, Some(&grid(false)));
        assert!(
            both.hdr
                .unwrap()
                .ends_with("turned on in Windows display settings.")
        );
        let mac = fidelity_availability(DeckPlatform::Mac, false, None);
        assert_eq!(
            mac.hdr.as_deref(),
            Some("No HDR-capable screen found. Connect an HDR display to use this preset.")
        );
    }

    #[test]
    fn an_unknown_decoder_blocks_nothing() {
        assert_eq!(
            fidelity_availability(DeckPlatform::Mac, true, None),
            FidelityAvailability::default()
        );
    }
}
