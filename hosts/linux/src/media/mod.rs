//! Media pipeline: `capenc` supervision + Annex-B access-unit framing +
//! wire-frame construction.

pub mod annexb;
pub mod audio;
pub mod capenc;
pub mod encoder_admission;
pub mod multi_capenc;

use arcen_protocol::wire::{
    encode_audio_header, encode_video_header, AudioCodec, AudioHeader, BitDepth, ChromaSubsampling,
    ColorMatrix, ColorRange, FrameType, VideoCodec, VideoHeader, AUDIO_HEADER_SIZE,
};

use annexb::AccessUnit;
use capenc::ResolvedMediaPlan;

pub(crate) fn now_ms_u32() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_millis() & 0xFFFF_FFFF) as u32)
        .unwrap_or(0)
}

pub fn build_audio_frame(codec: AudioCodec, payload: &[u8], timestamp_ms: u32) -> Vec<u8> {
    let header = encode_audio_header(AudioHeader {
        codec,
        timestamp_ms,
    });
    let mut frame = Vec::with_capacity(AUDIO_HEADER_SIZE + payload.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(payload);
    frame
}

/// Build one on-the-wire video frame. Legacy single-monitor frames use the
/// 10-byte header; negotiated region frames use the 26-byte v1 header with
/// explicit topology generation and stream epoch.
///
/// The resolved plan is the single source for codec, chroma, depth, range and
/// matrix. `monitor_id` defaults to 0 for the single active head (matches
/// `common/messages.py`).
pub fn build_video_frame(
    plan: &ResolvedMediaPlan,
    au: &AccessUnit,
    timestamp_ms: u32,
    monitor_id: u16,
    topology_generation: u64,
    stream_epoch: u64,
) -> Vec<u8> {
    let region = monitor_id != 0;
    let (frame_type, vcodec) = match plan.video.codec {
        arcen_media::VideoCodec::H265 => (
            if region {
                FrameType::RegionVideoH265
            } else {
                FrameType::VideoH265
            },
            VideoCodec::H265,
        ),
        arcen_media::VideoCodec::Av1 => (
            if region {
                FrameType::RegionVideoAv1
            } else {
                FrameType::VideoAv1
            },
            VideoCodec::Av1,
        ),
        _ => (
            if region {
                FrameType::RegionVideoH264
            } else {
                FrameType::VideoH264
            },
            VideoCodec::H264,
        ),
    };
    let chroma = if matches!(plan.video.chroma, arcen_media::ChromaSubsampling::Yuv444) {
        ChromaSubsampling::Yuv444
    } else {
        ChromaSubsampling::Yuv420
    };
    let bit_depth = match plan.video.bit_depth {
        arcen_media::BitDepth::Eight => BitDepth::Eight,
        arcen_media::BitDepth::Ten => BitDepth::Ten,
        arcen_media::BitDepth::Twelve => BitDepth::Twelve,
    };
    let range = match plan.video.range {
        arcen_media::ColorRange::Limited => ColorRange::Limited,
        arcen_media::ColorRange::Full => ColorRange::Full,
    };
    let matrix = match plan.video.matrix {
        arcen_media::ColorMatrix::Bt709 => ColorMatrix::Bt709,
        arcen_media::ColorMatrix::Identity => ColorMatrix::Identity,
        arcen_media::ColorMatrix::Bt601 => ColorMatrix::Bt601,
        arcen_media::ColorMatrix::Bt2020Ncl => ColorMatrix::Bt2020Ncl,
    };
    let flags = VideoHeader::encode_flags(au.is_keyframe, bit_depth, range, matrix);

    let header = encode_video_header(VideoHeader {
        frame_type,
        codec: vcodec,
        chroma,
        flags,
        timestamp_ms,
        monitor_id,
        topology_generation,
        stream_epoch,
    });

    let mut out = Vec::with_capacity(header.len() + au.data.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&au.data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::capenc::ResolvedMediaPlan;
    use arcen_media::video::EncoderBackend;
    use arcen_media::VideoConfiguration;
    use arcen_protocol::wire::{
        decode_video_header, REGION_VIDEO_HEADER_SIZE, VIDEO_HEADER_SIZE, VIDEO_KEYFRAME_FLAG,
    };

    fn plan(codec: arcen_media::VideoCodec, yuv444: bool) -> ResolvedMediaPlan {
        ResolvedMediaPlan {
            backend: EncoderBackend::NativeNvenc,
            video: VideoConfiguration {
                codec,
                chroma: if yuv444 {
                    arcen_media::ChromaSubsampling::Yuv444
                } else {
                    arcen_media::ChromaSubsampling::Yuv420
                },
                ..VideoConfiguration::legacy_h264()
            },
            width: 1920,
            height: 1080,
            fps: 60,
            codecs: arcen_media::CodecSet::from_slice(&[
                arcen_media::VideoCodec::H264,
                arcen_media::VideoCodec::H265,
                arcen_media::VideoCodec::Av1,
            ]),
            chroma: arcen_media::ChromaSet::from_slice(&[
                arcen_media::ChromaSubsampling::Yuv420,
                arcen_media::ChromaSubsampling::Yuv444,
            ]),
            bit_depths: arcen_media::BitDepthSet::from_slice(&[arcen_media::BitDepth::Eight]),
            ranges: arcen_media::ColorRangeSet::from_slice(&[arcen_media::ColorRange::Limited]),
            cursor_mode: arcen_protocol::messages::CursorMode::Local,
            cursor_in_video: false,
        }
    }

    #[test]
    fn h265_yuv444_keyframe_header_is_byte_exact() {
        let au = AccessUnit {
            data: vec![0xAA, 0xBB, 0xCC],
            is_keyframe: true,
        };
        let frame = build_video_frame(
            &plan(arcen_media::VideoCodec::H265, true),
            &au,
            0x0102_0304,
            0,
            0,
            0,
        );
        assert_eq!(frame.len(), VIDEO_HEADER_SIZE + 3);
        // Byte-for-byte against the wire contract.
        assert_eq!(frame[0], 0x04, "frame_type VIDEO_H265");
        assert_eq!(frame[1], 0x02, "codec H265");
        assert_eq!(frame[2], 0x02, "chroma YUV444");
        assert_eq!(frame[3], VIDEO_KEYFRAME_FLAG, "KEYFRAME flag");
        assert_eq!(&frame[4..8], &0x0102_0304u32.to_be_bytes(), "ts BE");
        assert_eq!(&frame[8..10], &0u16.to_be_bytes(), "monitor 0 BE");
        assert_eq!(&frame[10..], &au.data[..], "payload verbatim");

        let h = decode_video_header(&frame).unwrap();
        assert_eq!(h.frame_type, FrameType::VideoH265);
        assert_eq!(h.chroma, ChromaSubsampling::Yuv444);
    }

    #[test]
    fn h264_yuv420_pframe_has_no_keyframe_flag() {
        let au = AccessUnit {
            data: vec![1, 2],
            is_keyframe: false,
        };
        let frame = build_video_frame(&plan(arcen_media::VideoCodec::H264, false), &au, 7, 0, 0, 0);
        assert_eq!(frame[0], 0x03, "frame_type VIDEO_H264");
        assert_eq!(frame[1], 0x01, "codec H264");
        assert_eq!(frame[2], 0x00, "chroma YUV420");
        assert_eq!(frame[3], 0x00, "no keyframe flag");
    }

    #[test]
    fn av1_yuv420_keyframe_uses_the_av1_wire_identity() {
        let au = AccessUnit {
            data: vec![0x0a, 0x01, 0x00],
            is_keyframe: true,
        };
        let frame = build_video_frame(&plan(arcen_media::VideoCodec::Av1, false), &au, 9, 0, 0, 0);
        let header = decode_video_header(&frame).expect("AV1 video header");
        assert_eq!(header.frame_type, FrameType::VideoAv1);
        assert_eq!(header.codec, VideoCodec::Av1);
        assert!(header.is_keyframe());
    }

    #[test]
    fn region_frame_carries_generation_and_epoch() {
        let au = AccessUnit {
            data: vec![1, 2],
            is_keyframe: true,
        };
        let frame = build_video_frame(
            &plan(arcen_media::VideoCodec::H264, false),
            &au,
            7,
            2,
            9,
            11,
        );
        assert_eq!(frame.len(), REGION_VIDEO_HEADER_SIZE + 2);
        let header = decode_video_header(&frame).expect("region header");
        assert_eq!(header.frame_type, FrameType::RegionVideoH264);
        assert_eq!(header.monitor_id, 2);
        assert_eq!(header.topology_generation, 9);
        assert_eq!(header.stream_epoch, 11);
    }

    #[test]
    fn every_frame_carries_the_resolved_color_contract() {
        let au = AccessUnit {
            data: vec![1, 2],
            is_keyframe: false,
        };
        let mut plan = plan(arcen_media::VideoCodec::H265, true);
        plan.video = VideoConfiguration::grading_reference();
        let frame = build_video_frame(&plan, &au, 7, 0, 0, 0);
        let header = decode_video_header(&frame).expect("video header");
        assert!(!header.is_keyframe());
        assert_eq!(header.bit_depth(), Ok(BitDepth::Ten));
        assert_eq!(header.color_range(), ColorRange::Full);
        assert_eq!(header.color_matrix(), Ok(ColorMatrix::Bt709));
    }

    #[test]
    fn pcm_audio_header_is_byte_exact() {
        let frame = build_audio_frame(AudioCodec::Pcm, &[1, 2, 3, 4], 0x0102_0304);
        assert_eq!(&frame[..8], &[0x10, 0x01, 0, 0, 1, 2, 3, 4]);
        assert_eq!(&frame[8..], &[1, 2, 3, 4]);
    }
}
