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

/// Resolve a video contract against what the desktop truthfully holds.
///
/// * An SDR desktop has no HDR composition space: PQ and HLG requests become
///   BT.709, with BT.709 primaries and matrix. SDR requests are unchanged.
/// * A Rec.2100 PQ desktop keeps a ten-bit PQ request as PQ / BT.2020 /
///   BT.2020 NCL, the only honest labelling of its pixels.
/// * A Rec.2100 PQ desktop serves any other ten-bit request as BT.709 SDR
///   and says so: the pixels are converted with
///   [`Rgb10Signal::PqBt2020ToSdrBt709`], never relabelled.
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
                Some(Rgb10Signal::PqBt2020ToSdrBt709)
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
                let Ok(resolved) = resolve_desktop_plan(video, *source) else {
                    continue;
                };
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
