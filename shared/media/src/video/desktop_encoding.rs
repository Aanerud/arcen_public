// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! How the pixels of a captured desktop are encoded, when the platform itself
//! cannot say.
//!
//! Windows reports whether Advanced Color composes the desktop in scRGB, and
//! a colour-managed Wayland compositor tags every screencast buffer with its
//! transfer and primaries. Xorg reports neither: a depth-30 root window holds
//! ten-bit code values and nothing about what they mean. Most applications
//! draw BT.709 SDR there. A colour-managed application can instead write
//! Rec.2100 PQ code values straight into its window -- Autodesk Flame's
//! "HDR UI" mode with the graphics monitor set to Rec.2100-PQ does exactly
//! that, and relies on the monitor being in PQ mode to show it.
//!
//! The encoding is therefore an operator declaration, the same promise a user
//! makes by switching a physical monitor into PQ mode. It never invents HDR
//! for a session that did not ask for it, and it never lets PQ code values
//! reach an SDR session unconverted: [`resolve_desktop_plan`] either converts
//! them or refuses the session.
//!
//! What an SDR desktop's code values are is not in doubt on any Pier: they
//! are sRGB (IEC 61966-2-1). [`sdr_desktop_transfer`] is the one rule every
//! host follows to say so on the wire, for the clients that can present it.

use super::Rgb10Signal;
use crate::{BitDepth, ColorMatrix, ColorPrimaries, TransferCharacteristics, VideoConfiguration};

/// The signal encoding of the desktop a host captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesktopSignalEncoding {
    /// BT.709 SDR code values: the default, and the only thing a desktop that
    /// nobody vouched for can be assumed to carry.
    #[default]
    Sdr,
    /// Rec.2100 PQ code values in BT.2020 primaries, written by a
    /// colour-managed application.
    Rec2100Pq,
}

impl DesktopSignalEncoding {
    pub const ALL: &'static [Self] = &[Self::Sdr, Self::Rec2100Pq];

    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Sdr => "sdr",
            Self::Rec2100Pq => "rec2100-pq",
        }
    }

    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|encoding| encoding.token() == value)
    }
}

/// What a session will encode from a desktop, and what must happen to the
/// desktop's pixels on the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopPlan {
    /// The contract the session encodes and signals.
    pub video: VideoConfiguration,
    /// What the desktop's code values mean. Travels to the capture helper
    /// whatever `video` says, because the source does not change when the
    /// output does.
    pub source: DesktopSignalEncoding,
    /// The conversion from `source` to `video`.
    pub conversion: Rgb10Signal,
}

/// A request a desktop cannot serve truthfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopPlanError {
    /// An eight-bit session on a Rec.2100 PQ desktop: the eight-bit capture
    /// paths have no conversion stage, so PQ code values would be shown as
    /// SDR.
    EightBitOnPqDesktop,
}

impl std::fmt::Display for DesktopPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EightBitOnPqDesktop => formatter.write_str(
                "this host's desktop carries HDR (Rec.2100 PQ); an 8-bit stream cannot show it \
                 correctly. Choose Grading for a converted SDR picture, or HDR",
            ),
        }
    }
}

impl std::error::Error for DesktopPlanError {}

/// The SDR transfer a host serves its desktop with.
///
/// Desktop applications draw sRGB code values on every platform Arcen hosts,
/// and the Deck presents every SDR stream on an sRGB layer without a transfer
/// conversion. So the exact answer to a `bt709` SDR request is the desktop's
/// own codes, said to be `srgb` (H.273 13). Each host gets there differently,
/// and none of them is allowed to re-encode with the BT.709 OETF for a client
/// that can take `srgb`:
///
/// * code-value sources pass the codes through and only the tag changes: a
///   depth-30 Xorg framebuffer ([`super::Rgb10Signal::Direct`]) and a
///   `ScreenCaptureKit` `xf44` surface captured in `kCGColorSpaceSRGB`;
/// * a linear-light source is re-encoded with the curve its compositor
///   linearised with: Windows FP16 scRGB ([`super::ScrgbSdrTransform`]);
/// * a Rec.2100 PQ desktop converted to SDR is re-encoded with the sRGB curve
///   ([`super::Rgb10Signal::PqBt2020ToSdrSrgb`]).
///
/// [`TransferCharacteristics::serves_request`] accepts the pair, so the
/// served pipeline and the Deck's negotiated truth stay Grading. A client
/// that predates `srgb_transfer` keeps the `bt709` stream it was built
/// against, and any other request, SDR or HDR, is returned unchanged.
#[must_use]
pub const fn sdr_desktop_transfer(
    requested: TransferCharacteristics,
    client_accepts_srgb: bool,
) -> TransferCharacteristics {
    match requested {
        TransferCharacteristics::Bt709 if client_accepts_srgb => TransferCharacteristics::Srgb,
        other => other,
    }
}

/// A resolved session contract with its SDR desktop served as
/// [`sdr_desktop_transfer`] says.
///
/// Only fidelity sessions change: a contract above eight bits that no
/// administrator pinned. Eight-bit sessions, the Auto/Speed fast paths and
/// their software fallbacks, keep the transfer they always signalled, and an
/// administrator's pinned variant is served exactly as written.
#[must_use]
pub const fn serve_sdr_desktop(
    mut video: VideoConfiguration,
    client_accepts_srgb: bool,
    administrator_pinned: bool,
) -> VideoConfiguration {
    video.transfer = fidelity_sdr_transfer(
        video.transfer,
        video.bit_depth,
        client_accepts_srgb,
        administrator_pinned,
    );
    video
}

/// [`serve_sdr_desktop`] on one transfer axis, for plans that are not a
/// [`VideoConfiguration`].
pub(crate) const fn fidelity_sdr_transfer(
    transfer: TransferCharacteristics,
    bit_depth: BitDepth,
    client_accepts_srgb: bool,
    administrator_pinned: bool,
) -> TransferCharacteristics {
    if administrator_pinned || matches!(bit_depth, BitDepth::Eight) {
        transfer
    } else {
        sdr_desktop_transfer(transfer, client_accepts_srgb)
    }
}

impl DesktopPlan {
    /// This plan with its SDR output served as [`serve_sdr_desktop`] says,
    /// and the conversion that output needs: an SDR desktop's codes still
    /// pass through, and a PQ desktop's SDR conversion re-encodes with the
    /// served curve.
    #[must_use]
    pub fn serving_sdr_desktop(
        self,
        client_accepts_srgb: bool,
        administrator_pinned: bool,
    ) -> Self {
        let video = serve_sdr_desktop(self.video, client_accepts_srgb, administrator_pinned);
        let conversion =
            conversion_for_output(self.source, video.transfer, video.primaries, video.matrix)
                .unwrap_or(self.conversion);
        Self {
            video,
            conversion,
            ..self
        }
    }
}

/// Resolve a video contract against what the desktop truthfully holds.
///
/// * An SDR desktop has no HDR composition space: PQ and HLG requests become
///   BT.709, with BT.709 primaries and matrix. SDR requests are unchanged.
/// * A Rec.2100 PQ desktop keeps a ten-bit PQ request as PQ / BT.2020 /
///   BT.2020 NCL, the only honest labelling of its pixels.
/// * A Rec.2100 PQ desktop serves any other ten-bit request as BT.709 SDR
///   and says so: the pixels are converted with
///   [`Rgb10Signal::PqBt2020ToSdrBt709`], never relabelled
///   ([`DesktopPlan::serving_sdr_desktop`] moves a capable client to the
///   sRGB curve).
/// * A Rec.2100 PQ desktop refuses eight-bit requests, which have no
///   conversion stage.
///
/// # Errors
///
/// [`DesktopPlanError::EightBitOnPqDesktop`] for an eight-bit request on a PQ
/// desktop.
pub fn resolve_desktop_plan(
    mut video: VideoConfiguration,
    source: DesktopSignalEncoding,
) -> Result<DesktopPlan, DesktopPlanError> {
    let to_sdr = |video: &mut VideoConfiguration| {
        video.matrix = if video.matrix == ColorMatrix::Bt2020Ncl {
            ColorMatrix::Bt709
        } else {
            video.matrix
        };
        video.primaries = ColorPrimaries::Bt709;
        video.transfer = TransferCharacteristics::Bt709;
    };
    let conversion = match source {
        DesktopSignalEncoding::Sdr => {
            if matches!(
                video.transfer,
                TransferCharacteristics::Pq | TransferCharacteristics::Hlg
            ) {
                video.matrix = ColorMatrix::Bt709;
                to_sdr(&mut video);
            }
            Rgb10Signal::Direct
        }
        DesktopSignalEncoding::Rec2100Pq => {
            if video.bit_depth == BitDepth::Eight {
                return Err(DesktopPlanError::EightBitOnPqDesktop);
            }
            if video.transfer == TransferCharacteristics::Pq {
                video.primaries = ColorPrimaries::Bt2020;
                video.matrix = ColorMatrix::Bt2020Ncl;
                Rgb10Signal::Direct
            } else {
                to_sdr(&mut video);
                Rgb10Signal::PqBt2020ToSdrBt709
            }
        }
    };
    Ok(DesktopPlan {
        video,
        source,
        conversion,
    })
}

/// The conversion a capture helper applies for a contract that was already
/// resolved by [`resolve_desktop_plan`], or `None` for a contract that rule
/// never produces from this source.
///
/// The helper receives the resolved axes on its command line, not the plan,
/// so it re-derives the conversion here rather than restating the rule.
#[must_use]
pub fn conversion_for_output(
    source: DesktopSignalEncoding,
    transfer: TransferCharacteristics,
    primaries: ColorPrimaries,
    matrix: ColorMatrix,
) -> Option<Rgb10Signal> {
    let sdr = matches!(
        transfer,
        TransferCharacteristics::Bt709 | TransferCharacteristics::Srgb
    ) && primaries == ColorPrimaries::Bt709;
    match source {
        DesktopSignalEncoding::Sdr => sdr.then_some(Rgb10Signal::Direct),
        DesktopSignalEncoding::Rec2100Pq => {
            if transfer == TransferCharacteristics::Pq
                && primaries == ColorPrimaries::Bt2020
                && matrix == ColorMatrix::Bt2020Ncl
            {
                Some(Rgb10Signal::Direct)
            } else if sdr && matrix != ColorMatrix::Bt2020Ncl {
                Some(if transfer == TransferCharacteristics::Srgb {
                    Rgb10Signal::PqBt2020ToSdrSrgb
                } else {
                    Rgb10Signal::PqBt2020ToSdrBt709
                })
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChromaSubsampling, ColorRange, VideoCodec};

    fn hdr_request() -> VideoConfiguration {
        VideoConfiguration {
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: BitDepth::Ten,
            range: ColorRange::Full,
            matrix: ColorMatrix::Bt2020Ncl,
            primaries: ColorPrimaries::Bt2020,
            transfer: TransferCharacteristics::Pq,
        }
    }

    #[test]
    fn tokens_round_trip_and_default_is_sdr() {
        for encoding in DesktopSignalEncoding::ALL {
            assert_eq!(
                DesktopSignalEncoding::from_token(encoding.token()),
                Some(*encoding)
            );
        }
        assert_eq!(DesktopSignalEncoding::default(), DesktopSignalEncoding::Sdr);
        assert_eq!(DesktopSignalEncoding::from_token("hdr"), None);
    }

    fn plan(video: VideoConfiguration, source: DesktopSignalEncoding) -> DesktopPlan {
        resolve_desktop_plan(video, source).expect("servable")
    }

    #[test]
    fn sdr_desktop_reduces_hdr_to_bt709_grading() {
        let resolved = plan(hdr_request(), DesktopSignalEncoding::Sdr);
        assert_eq!(resolved.video.transfer, TransferCharacteristics::Bt709);
        assert_eq!(resolved.video.primaries, ColorPrimaries::Bt709);
        assert_eq!(resolved.video.matrix, ColorMatrix::Bt709);
        assert_eq!(resolved.video.bit_depth, BitDepth::Ten);
        assert_eq!(resolved.video.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(resolved.conversion, Rgb10Signal::Direct);
    }

    #[test]
    fn pq_desktop_keeps_ten_bit_pq_as_bt2020() {
        let mut request = hdr_request();
        request.matrix = ColorMatrix::Bt709;
        request.primaries = ColorPrimaries::Bt709;
        let resolved = plan(request, DesktopSignalEncoding::Rec2100Pq);
        assert_eq!(resolved.video.transfer, TransferCharacteristics::Pq);
        assert_eq!(resolved.video.primaries, ColorPrimaries::Bt2020);
        assert_eq!(resolved.video.matrix, ColorMatrix::Bt2020Ncl);
        assert_eq!(resolved.conversion, Rgb10Signal::Direct);
        assert_eq!(resolved.source, DesktopSignalEncoding::Rec2100Pq);
    }

    #[test]
    fn pq_desktop_converts_sdr_and_hlg_requests_instead_of_relabelling() {
        let grading = VideoConfiguration::grading_reference();
        let resolved = plan(grading, DesktopSignalEncoding::Rec2100Pq);
        assert_eq!(
            resolved.video, grading,
            "the SDR contract is what is encoded"
        );
        assert_eq!(
            resolved.source,
            DesktopSignalEncoding::Rec2100Pq,
            "the source survives"
        );
        assert_eq!(resolved.conversion, Rgb10Signal::PqBt2020ToSdrBt709);

        let mut hlg = hdr_request();
        hlg.transfer = TransferCharacteristics::Hlg;
        let resolved = plan(hlg, DesktopSignalEncoding::Rec2100Pq);
        assert_eq!(resolved.video.transfer, TransferCharacteristics::Bt709);
        assert_eq!(resolved.video.primaries, ColorPrimaries::Bt709);
        assert_eq!(resolved.video.matrix, ColorMatrix::Bt709);
        assert_eq!(resolved.conversion, Rgb10Signal::PqBt2020ToSdrBt709);
    }

    #[test]
    fn pq_desktop_refuses_eight_bit_sessions() {
        let mut eight = hdr_request();
        eight.bit_depth = BitDepth::Eight;
        assert_eq!(
            resolve_desktop_plan(eight, DesktopSignalEncoding::Rec2100Pq),
            Err(DesktopPlanError::EightBitOnPqDesktop)
        );
        let auto = VideoConfiguration::legacy_h264();
        let error = resolve_desktop_plan(auto, DesktopSignalEncoding::Rec2100Pq).unwrap_err();
        assert!(error.to_string().contains("Choose Grading"), "{error}");
    }

    #[test]
    fn a_helper_derives_the_same_conversion_the_plan_chose() {
        let mut hlg = hdr_request();
        hlg.transfer = TransferCharacteristics::Hlg;
        for source in DesktopSignalEncoding::ALL {
            for video in [
                hdr_request(),
                hlg,
                VideoConfiguration::grading_reference(),
                VideoConfiguration::legacy_h264(),
            ] {
                let Ok(plain) = resolve_desktop_plan(video, *source) else {
                    continue;
                };
                for resolved in [plain, plain.serving_sdr_desktop(true, false)] {
                    assert_eq!(
                        conversion_for_output(
                            resolved.source,
                            resolved.video.transfer,
                            resolved.video.primaries,
                            resolved.video.matrix,
                        ),
                        Some(resolved.conversion),
                        "{source:?} {video:?}"
                    );
                }
            }
        }
        assert_eq!(
            conversion_for_output(
                DesktopSignalEncoding::Sdr,
                TransferCharacteristics::Pq,
                ColorPrimaries::Bt2020,
                ColorMatrix::Bt2020Ncl,
            ),
            None,
            "an SDR desktop is never encoded as PQ"
        );
    }

    #[test]
    fn only_a_capable_client_moves_an_sdr_desktop_to_srgb() {
        use TransferCharacteristics::{Bt709, Hlg, Pq, Srgb};
        assert_eq!(sdr_desktop_transfer(Bt709, true), Srgb);
        assert_eq!(sdr_desktop_transfer(Bt709, false), Bt709);
        for requested in [Srgb, Pq, Hlg] {
            for capable in [false, true] {
                assert_eq!(sdr_desktop_transfer(requested, capable), requested);
            }
        }
        assert!(Srgb.serves_request(Bt709));
        assert!(!Bt709.serves_request(Srgb));
        assert!(!Pq.serves_request(Bt709) && !Srgb.serves_request(Pq));
    }

    #[test]
    fn only_unpinned_fidelity_sessions_are_served_as_srgb() {
        let grading = VideoConfiguration::grading_reference();
        let srgb = TransferCharacteristics::Srgb;
        assert_eq!(serve_sdr_desktop(grading, true, false).transfer, srgb);
        assert_eq!(serve_sdr_desktop(grading, false, false), grading);
        assert_eq!(serve_sdr_desktop(grading, true, true), grading, "pinned");
        let eight = VideoConfiguration::legacy_h264();
        assert_eq!(serve_sdr_desktop(eight, true, false), eight, "fast path");
        assert_eq!(serve_sdr_desktop(hdr_request(), true, false), hdr_request());
        let served = serve_sdr_desktop(grading, true, false);
        assert_eq!(
            VideoConfiguration {
                transfer: grading.transfer,
                ..served
            },
            grading,
            "only the transfer changes"
        );
    }

    #[test]
    fn a_capable_client_gets_srgb_codes_from_both_desktops() {
        let grading = VideoConfiguration::grading_reference();
        let sdr = plan(grading, DesktopSignalEncoding::Sdr).serving_sdr_desktop(true, false);
        assert_eq!(sdr.video.transfer, TransferCharacteristics::Srgb);
        assert_eq!(sdr.conversion, Rgb10Signal::Direct, "codes pass through");

        let pq = plan(grading, DesktopSignalEncoding::Rec2100Pq).serving_sdr_desktop(true, false);
        assert_eq!(pq.video.transfer, TransferCharacteristics::Srgb);
        assert_eq!(pq.conversion, Rgb10Signal::PqBt2020ToSdrSrgb);
        assert_eq!(pq.source, DesktopSignalEncoding::Rec2100Pq);

        let legacy =
            plan(grading, DesktopSignalEncoding::Rec2100Pq).serving_sdr_desktop(false, false);
        assert_eq!(legacy.video.transfer, TransferCharacteristics::Bt709);
        assert_eq!(legacy.conversion, Rgb10Signal::PqBt2020ToSdrBt709);

        let hdr =
            plan(hdr_request(), DesktopSignalEncoding::Rec2100Pq).serving_sdr_desktop(true, false);
        assert_eq!(hdr.video.transfer, TransferCharacteristics::Pq);
        assert_eq!(hdr.conversion, Rgb10Signal::Direct);
    }

    #[test]
    fn sdr_desktop_never_changes_sdr_requests() {
        for video in [
            VideoConfiguration::grading_reference(),
            VideoConfiguration::legacy_h264(),
        ] {
            let resolved = plan(video, DesktopSignalEncoding::Sdr);
            assert_eq!(resolved.video, video);
            assert_eq!(resolved.conversion, Rgb10Signal::Direct);
        }
    }
}
