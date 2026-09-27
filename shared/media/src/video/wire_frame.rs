//! Framing one encoded access unit for the video wire.
//!
//! Every Pier does the same five things to a frame before it leaves: pick the
//! frame type from the codec and whether this is a region, translate the plan's
//! colour description into the protocol's, pack the flags, encode the header,
//! and put the payload behind it. All three wrote that out separately, and the
//! Deck rejects a frame whose header disagrees with the plan it was promised —
//! so three copies were three chances to describe the same picture differently.
//!
//! The translation is the part worth centralising. It is pure, it is
//! mechanical, and getting one arm of a match wrong produces a frame that
//! decodes into the wrong colours rather than failing outright.

use arcen_protocol::wire::{
    BitDepth as WireBitDepth, ChromaSubsampling as WireChroma, ColorMatrix as WireMatrix,
    ColorRange as WireRange, VideoHeader, encode_video_header,
};
use arcen_protocol::{FrameType, VideoCodec as WireCodec};

use crate::{BitDepth, ChromaSubsampling, ColorMatrix, ColorRange, VideoCodec};

/// A codec the video frame types can actually describe.
///
/// The negotiation codec enum also carries JPEG and VP9, which no video frame
/// type names. Narrowing here rather than returning an error from every call
/// means a host that has chosen its encoder cannot be handed a failure it has
/// no way to act on — the only place the question can be answered is the one
/// place it is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramedVideoCodec {
    /// H.264/AVC.
    H264,
    /// H.265/HEVC.
    H265,
    /// AV1.
    Av1,
}

impl FramedVideoCodec {
    /// Narrows a negotiated codec to one the frame types describe.
    ///
    /// Returns `None` for JPEG and VP9, which exist for older negotiation
    /// paths and have no video frame type.
    #[must_use]
    pub const fn from_codec(codec: VideoCodec) -> Option<Self> {
        Some(match codec {
            VideoCodec::H264 => Self::H264,
            VideoCodec::H265 => Self::H265,
            VideoCodec::Av1 => Self::Av1,
            VideoCodec::Jpeg | VideoCodec::Vp9 => return None,
        })
    }

    /// Returns the frame type that carries this codec.
    #[must_use]
    pub const fn frame_type(self, region: bool) -> FrameType {
        match (self, region) {
            (Self::H264, false) => FrameType::VideoH264,
            (Self::H264, true) => FrameType::RegionVideoH264,
            (Self::H265, false) => FrameType::VideoH265,
            (Self::H265, true) => FrameType::RegionVideoH265,
            (Self::Av1, false) => FrameType::VideoAv1,
            (Self::Av1, true) => FrameType::RegionVideoAv1,
        }
    }

    /// Returns the codec as the wire names it.
    #[must_use]
    pub const fn wire(self) -> WireCodec {
        match self {
            Self::H264 => WireCodec::H264,
            Self::H265 => WireCodec::H265,
            Self::Av1 => WireCodec::Av1,
        }
    }
}

/// What a receiver needs to know to decode the frames of one stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoWireProfile {
    /// The codec the payload is encoded with.
    pub codec: FramedVideoCodec,
    /// The chroma subsampling of the coded frame.
    pub chroma: ChromaSubsampling,
    /// The bit depth of the coded frame.
    pub bit_depth: BitDepth,
    /// Whether the luma range is limited or full.
    pub range: ColorRange,
    /// The matrix used to reach YCbCr.
    pub matrix: ColorMatrix,
}

/// Which monitor a frame belongs to, and which topology it was captured under.
///
/// `monitor_id` zero is not a monitor: it means a legacy single-monitor frame,
/// which is why a zero-based capture index must never be used here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoWireRoute {
    /// The session monitor id, or zero for a legacy single-monitor frame.
    pub monitor_id: u16,
    /// The topology generation this frame was captured under.
    pub topology_generation: u64,
    /// The media stream epoch this frame belongs to.
    pub stream_epoch: u64,
}

/// Translates the planned chroma subsampling into the wire's.
#[must_use]
pub const fn wire_chroma(chroma: ChromaSubsampling) -> WireChroma {
    match chroma {
        ChromaSubsampling::Yuv420 => WireChroma::Yuv420,
        // The wire has carried 4:2:2 all along. A host that collapsed anything
        // that was not 4:4:4 into 4:2:0 would label a 4:2:2 frame as something
        // it is not, and the receiver would read the chroma planes at the
        // wrong size rather than refuse it.
        ChromaSubsampling::Yuv422 => WireChroma::Yuv422,
        ChromaSubsampling::Yuv444 => WireChroma::Yuv444,
    }
}

/// Translates the planned bit depth into the wire's.
#[must_use]
pub const fn wire_bit_depth(depth: BitDepth) -> WireBitDepth {
    match depth {
        BitDepth::Eight => WireBitDepth::Eight,
        BitDepth::Ten => WireBitDepth::Ten,
        BitDepth::Twelve => WireBitDepth::Twelve,
    }
}

/// Translates the planned colour range into the wire's.
#[must_use]
pub const fn wire_range(range: ColorRange) -> WireRange {
    match range {
        ColorRange::Limited => WireRange::Limited,
        ColorRange::Full => WireRange::Full,
    }
}

/// Translates the planned colour matrix into the wire's.
#[must_use]
pub const fn wire_matrix(matrix: ColorMatrix) -> WireMatrix {
    match matrix {
        ColorMatrix::Identity => WireMatrix::Identity,
        ColorMatrix::Bt709 => WireMatrix::Bt709,
        ColorMatrix::Bt601 => WireMatrix::Bt601,
        ColorMatrix::Bt2020Ncl => WireMatrix::Bt2020Ncl,
    }
}

/// Builds the header that describes one encoded access unit.
#[must_use]
pub fn video_header(
    profile: VideoWireProfile,
    route: VideoWireRoute,
    keyframe: bool,
    timestamp_ms: u32,
) -> VideoHeader {
    VideoHeader {
        frame_type: profile.codec.frame_type(route.monitor_id != 0),
        codec: profile.codec.wire(),
        chroma: wire_chroma(profile.chroma),
        flags: VideoHeader::encode_flags(
            keyframe,
            wire_bit_depth(profile.bit_depth),
            wire_range(profile.range),
            wire_matrix(profile.matrix),
        ),
        timestamp_ms,
        monitor_id: route.monitor_id,
        topology_generation: route.topology_generation,
        stream_epoch: route.stream_epoch,
    }
}

/// Builds the complete wire message for one encoded access unit.
#[must_use]
pub fn video_frame_message(
    profile: VideoWireProfile,
    route: VideoWireRoute,
    keyframe: bool,
    timestamp_ms: u32,
    payload: &[u8],
) -> Vec<u8> {
    let header = encode_video_header(video_header(profile, route, keyframe, timestamp_ms));
    let mut message = Vec::with_capacity(header.len() + payload.len());
    message.extend_from_slice(&header);
    message.extend_from_slice(payload);
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDR: VideoWireProfile = VideoWireProfile {
        codec: FramedVideoCodec::H265,
        chroma: ChromaSubsampling::Yuv420,
        bit_depth: BitDepth::Eight,
        range: ColorRange::Limited,
        matrix: ColorMatrix::Bt709,
    };

    #[test]
    fn monitor_zero_is_a_legacy_frame_and_anything_else_is_a_region() {
        // A zero-based capture index used as a wire id silently produces
        // legacy frames, which a multi-monitor Deck cannot route.
        assert_eq!(
            FramedVideoCodec::H265.frame_type(false),
            FrameType::VideoH265,
        );
        assert_eq!(
            FramedVideoCodec::H265.frame_type(true),
            FrameType::RegionVideoH265,
        );
    }

    #[test]
    fn a_codec_with_no_frame_type_is_refused_where_it_is_chosen() {
        // JPEG and VP9 are in the negotiation enum for older paths, and no
        // video frame type names them. Narrowing once, here, is what lets
        // every framing call afterwards be total: a host that has already
        // picked its encoder is never handed a failure it cannot act on.
        assert!(FramedVideoCodec::from_codec(VideoCodec::Jpeg).is_none());
        assert!(FramedVideoCodec::from_codec(VideoCodec::Vp9).is_none());
        assert_eq!(
            FramedVideoCodec::from_codec(VideoCodec::H265),
            Some(FramedVideoCodec::H265),
        );
    }

    #[test]
    fn the_header_describes_the_frame_that_follows_it() {
        let message = video_frame_message(
            VideoWireProfile {
                codec: FramedVideoCodec::H265,
                chroma: ChromaSubsampling::Yuv444,
                bit_depth: BitDepth::Ten,
                range: ColorRange::Full,
                matrix: ColorMatrix::Bt2020Ncl,
            },
            VideoWireRoute {
                monitor_id: 3,
                topology_generation: 7,
                stream_epoch: 11,
            },
            true,
            1_234,
            b"access unit",
        );

        let header = arcen_protocol::wire::decode_video_header(&message).expect("decodable");
        let payload = &message[arcen_protocol::wire::REGION_VIDEO_HEADER_SIZE..];
        assert_eq!(header.frame_type, FrameType::RegionVideoH265);
        assert_eq!(header.codec, WireCodec::H265);
        assert_eq!(header.chroma, WireChroma::Yuv444);
        assert_eq!(header.timestamp_ms, 1_234);
        assert_eq!(header.monitor_id, 3);
        assert_eq!(header.topology_generation, 7);
        assert_eq!(header.stream_epoch, 11);
        assert_eq!(payload, b"access unit");
    }

    #[test]
    fn the_colour_description_survives_the_round_trip() {
        // Getting one arm of these matches wrong does not fail: it produces a
        // picture in the wrong colours, which is far harder to notice.
        for matrix in [
            ColorMatrix::Identity,
            ColorMatrix::Bt709,
            ColorMatrix::Bt601,
            ColorMatrix::Bt2020Ncl,
        ] {
            for depth in [BitDepth::Eight, BitDepth::Ten, BitDepth::Twelve] {
                for range in [ColorRange::Limited, ColorRange::Full] {
                    let header = video_header(
                        VideoWireProfile {
                            matrix,
                            bit_depth: depth,
                            range,
                            ..SDR
                        },
                        VideoWireRoute::default(),
                        false,
                        0,
                    );
                    assert_eq!(header.bit_depth().expect("depth"), wire_bit_depth(depth));
                    assert_eq!(header.color_range(), wire_range(range));
                    assert_eq!(header.color_matrix().expect("matrix"), wire_matrix(matrix));
                }
            }
        }
    }

    #[test]
    fn a_keyframe_says_so_and_a_delta_does_not() {
        // A Deck waits for a keyframe before it will show anything, so a
        // keyframe flagged as a delta frame is a session that never starts.
        let key = video_header(SDR, VideoWireRoute::default(), true, 0);
        let delta = video_header(SDR, VideoWireRoute::default(), false, 0);
        assert!(key.is_keyframe());
        assert!(!delta.is_keyframe());
    }
}
