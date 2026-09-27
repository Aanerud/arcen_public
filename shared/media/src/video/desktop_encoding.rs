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
//! makes by switching a physical monitor into PQ mode. It only ever widens
//! what an HDR request may keep; it never invents HDR for a session that did
//! not ask for it.

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

    /// Whether an SDR session sees this desktop as it was meant to be seen.
    ///
    /// A PQ desktop streamed as BT.709 shows PQ code values as SDR: dim and
    /// flat, exactly as an SDR monitor would show them. Hosts warn rather
    /// than guess a tone map nobody asked for.
    #[must_use]
    pub const fn sdr_sessions_are_faithful(self) -> bool {
        matches!(self, Self::Sdr)
    }
}

/// Constrain a resolved video contract to what the desktop can truthfully
/// supply.
///
/// * An SDR desktop has no HDR composition space: PQ and HLG requests become
///   BT.709, with BT.709 primaries and matrix.
/// * A Rec.2100 PQ desktop keeps a ten-bit PQ request as PQ / BT.2020 /
///   BT.2020 NCL -- the only honest labelling of its pixels. HLG cannot be
///   produced from PQ code values without a conversion nobody asked for, and
///   eight-bit PQ is not a contract Arcen offers, so both become BT.709.
/// * SDR requests are never changed.
#[must_use]
pub fn constrain_to_desktop_encoding(
    mut video: VideoConfiguration,
    encoding: DesktopSignalEncoding,
) -> VideoConfiguration {
    let keeps_pq = encoding == DesktopSignalEncoding::Rec2100Pq
        && video.transfer == TransferCharacteristics::Pq
        && video.bit_depth == BitDepth::Ten;
    if keeps_pq {
        video.primaries = ColorPrimaries::Bt2020;
        video.matrix = ColorMatrix::Bt2020Ncl;
    } else if matches!(
        video.transfer,
        TransferCharacteristics::Pq | TransferCharacteristics::Hlg
    ) {
        video.matrix = ColorMatrix::Bt709;
        video.primaries = ColorPrimaries::Bt709;
        video.transfer = TransferCharacteristics::Bt709;
    }
    video
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

    #[test]
    fn sdr_desktop_reduces_hdr_to_bt709_grading() {
        let video = constrain_to_desktop_encoding(hdr_request(), DesktopSignalEncoding::Sdr);
        assert_eq!(video.transfer, TransferCharacteristics::Bt709);
        assert_eq!(video.primaries, ColorPrimaries::Bt709);
        assert_eq!(video.matrix, ColorMatrix::Bt709);
        assert_eq!(video.bit_depth, BitDepth::Ten);
        assert_eq!(video.chroma, ChromaSubsampling::Yuv444);
    }

    #[test]
    fn pq_desktop_keeps_ten_bit_pq_as_bt2020() {
        let mut request = hdr_request();
        request.matrix = ColorMatrix::Bt709;
        request.primaries = ColorPrimaries::Bt709;
        let video = constrain_to_desktop_encoding(request, DesktopSignalEncoding::Rec2100Pq);
        assert_eq!(video.transfer, TransferCharacteristics::Pq);
        assert_eq!(video.primaries, ColorPrimaries::Bt2020);
        assert_eq!(video.matrix, ColorMatrix::Bt2020Ncl);
    }

    #[test]
    fn pq_desktop_cannot_serve_hlg_or_eight_bit_pq() {
        let mut hlg = hdr_request();
        hlg.transfer = TransferCharacteristics::Hlg;
        let video = constrain_to_desktop_encoding(hlg, DesktopSignalEncoding::Rec2100Pq);
        assert_eq!(video.transfer, TransferCharacteristics::Bt709);

        let mut eight = hdr_request();
        eight.bit_depth = BitDepth::Eight;
        let video = constrain_to_desktop_encoding(eight, DesktopSignalEncoding::Rec2100Pq);
        assert_eq!(video.transfer, TransferCharacteristics::Bt709);
        assert_eq!(video.primaries, ColorPrimaries::Bt709);
    }

    #[test]
    fn sdr_requests_are_never_changed() {
        let grading = VideoConfiguration::grading_reference();
        for encoding in DesktopSignalEncoding::ALL {
            assert_eq!(constrain_to_desktop_encoding(grading, *encoding), grading);
        }
        assert!(DesktopSignalEncoding::Sdr.sdr_sessions_are_faithful());
        assert!(!DesktopSignalEncoding::Rec2100Pq.sdr_sessions_are_faithful());
    }
}
