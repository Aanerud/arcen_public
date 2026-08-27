//! Annex-B access-unit metadata helpers.
//!
//! Access-unit boundaries come exclusively from capenc's explicit `framed-v1`
//! length prefix. This module only classifies exact payloads; it never infers a
//! frame boundary from pipe read sizes.

/// Which codec's NAL rules to apply when classifying keyframe NALs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalCodec {
    H264,
    H265,
    Av1,
}

impl NalCodec {
    pub fn from_codec_token(codec: &str) -> Option<Self> {
        match codec.to_ascii_lowercase().as_str() {
            "h264" => Some(Self::H264),
            "h265" | "hevc" => Some(Self::H265),
            "av1" => Some(Self::Av1),
            _ => None,
        }
    }

    pub const fn as_codec_token(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Av1 => "av1",
        }
    }
}

/// One complete encoded frame, ready to be wire-framed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

const START_CODE_3: [u8; 3] = [0x00, 0x00, 0x01];
const START_CODE_4: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

fn start_code_len(data: &[u8], index: usize) -> Option<usize> {
    if data.get(index..index.saturating_add(START_CODE_4.len())) == Some(&START_CODE_4) {
        Some(START_CODE_4.len())
    } else if data.get(index..index.saturating_add(START_CODE_3.len())) == Some(&START_CODE_3) {
        Some(START_CODE_3.len())
    } else {
        None
    }
}

fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..data.len()).find_map(|index| start_code_len(data, index).map(|len| (index, len)))
}

/// Scan every NAL because an IDR can follow VPS/SPS/PPS/SEI in the same AU.
pub(crate) fn access_unit_is_keyframe(au: &[u8], codec: NalCodec) -> bool {
    if codec == NalCodec::Av1 {
        return arcen_media::video::av1_low_overhead_has_sequence_header(au);
    }
    access_unit_nal_flags(au, codec).is_some_and(|flags| flags.keyframe)
}

#[derive(Default)]
struct NalFlags {
    keyframe: bool,
    vps: bool,
    sps: bool,
    pps: bool,
}

/// A generation may become authoritative only on a self-contained recovery AU.
pub(crate) fn access_unit_is_recovery_point(au: &[u8], codec: NalCodec) -> bool {
    if codec == NalCodec::Av1 {
        return arcen_media::video::av1_low_overhead_has_sequence_header(au);
    }
    access_unit_nal_flags(au, codec).is_some_and(|flags| {
        flags.keyframe && flags.sps && flags.pps && (codec == NalCodec::H264 || flags.vps)
    })
}

fn classify_nal(nal: &[u8], codec: NalCodec, flags: &mut NalFlags) -> Option<()> {
    match codec {
        NalCodec::H264 => {
            let (&header, body) = nal.split_first()?;
            if header & 0x80 != 0 || body.is_empty() {
                return None;
            }
            let nal_ref_idc = (header >> 5) & 0x03;
            match header & 0x1F {
                0 | 24..=31 => return None,
                5 if nal_ref_idc != 0 => flags.keyframe = true,
                7 if nal_ref_idc != 0 => flags.sps = true,
                8 if nal_ref_idc != 0 => flags.pps = true,
                _ => {}
            }
        }
        NalCodec::H265 => {
            if nal.len() < 3 {
                return None;
            }
            let first = nal[0];
            let second = nal[1];
            let nal_type = (first >> 1) & 0x3F;
            let temporal_id_plus_one = second & 0x07;
            if first & 0x80 != 0 || temporal_id_plus_one == 0 || nal_type > 47 {
                return None;
            }
            match nal_type {
                16..=21 if temporal_id_plus_one == 1 => flags.keyframe = true,
                32 => flags.vps = true,
                33 => flags.sps = true,
                34 => flags.pps = true,
                _ => {}
            }
        }
        NalCodec::Av1 => return None,
    }
    Some(())
}

fn access_unit_nal_flags(au: &[u8], codec: NalCodec) -> Option<NalFlags> {
    let mut flags = NalFlags::default();
    let mut position = 0usize;
    let mut saw_nal = false;
    while position < au.len() {
        let (index, prefix_len) = find_start_code(au, position)?;
        if index != position {
            return None;
        }
        let nal_start = index.checked_add(prefix_len)?;
        let nal_end = find_start_code(au, nal_start)
            .map(|(next, _)| next)
            .unwrap_or(au.len());
        if nal_start >= nal_end {
            return None;
        }
        classify_nal(&au[nal_start..nal_end], codec, &mut flags)?;
        saw_nal = true;
        position = nal_end;
    }
    saw_nal.then_some(flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264_nal(prefix: &[u8], header: u8, body: &[u8]) -> Vec<u8> {
        let mut output = prefix.to_vec();
        output.push(header);
        output.extend_from_slice(body);
        output
    }

    fn h265_nal(prefix: &[u8], nal_type: u8, temporal_id_plus_one: u8, body: &[u8]) -> Vec<u8> {
        let mut output = prefix.to_vec();
        output.push(nal_type << 1);
        output.push(temporal_id_plus_one & 0x07);
        output.extend_from_slice(body);
        output
    }

    #[test]
    fn h264_mixed_prefix_recovery_is_accepted() {
        let mut au = h264_nal(&START_CODE_4, 0x67, &[1, 2, 3]);
        au.extend(h264_nal(&START_CODE_3, 0x68, &[4, 5]));
        au.extend(h264_nal(&START_CODE_4, 0x65, &[6, 7, 8]));
        assert!(access_unit_is_keyframe(&au, NalCodec::H264));
        assert!(access_unit_is_recovery_point(&au, NalCodec::H264));
    }

    #[test]
    fn h264_non_idr_is_not_keyframe() {
        assert!(!access_unit_is_keyframe(
            &h264_nal(&START_CODE_3, 0x41, &[1, 2, 3]),
            NalCodec::H264
        ));
    }

    #[test]
    fn idr_without_parameter_sets_is_not_a_recovery_point() {
        assert!(access_unit_is_keyframe(
            &h264_nal(&START_CODE_4, 0x65, &[1, 2, 3]),
            NalCodec::H264
        ));
        assert!(!access_unit_is_recovery_point(
            &h264_nal(&START_CODE_4, 0x65, &[1, 2, 3]),
            NalCodec::H264
        ));
    }

    #[test]
    fn every_h265_irap_type_is_a_recovery_keyframe() {
        for nal_type in 16..=21 {
            let mut au = h265_nal(&START_CODE_4, 32, 1, &[0x80]);
            au.extend(h265_nal(&START_CODE_3, 33, 1, &[0x80]));
            au.extend(h265_nal(&START_CODE_4, 34, 1, &[0x80]));
            au.extend(h265_nal(&START_CODE_3, nal_type, 1, &[1, 2, 3]));
            assert!(access_unit_is_keyframe(&au, NalCodec::H265));
            assert!(access_unit_is_recovery_point(&au, NalCodec::H265));
        }
    }

    #[test]
    fn h265_trail_slice_is_not_keyframe() {
        assert!(!access_unit_is_keyframe(
            &h265_nal(&START_CODE_4, 1, 1, &[5, 5]),
            NalCodec::H265
        ));
    }

    fn av1_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 128);
        let mut obu = vec![(obu_type << 3) | 0x02, payload.len() as u8];
        obu.extend_from_slice(payload);
        obu
    }

    #[test]
    fn av1_sequence_header_marks_a_repeated_header_keyframe_and_recovery_point() {
        let mut temporal_unit = av1_obu(2, &[]);
        temporal_unit.extend(av1_obu(1, &[0x10, 0x20]));
        temporal_unit.extend(av1_obu(6, &[0x30, 0x40]));
        assert!(access_unit_is_keyframe(&temporal_unit, NalCodec::Av1));
        assert!(access_unit_is_recovery_point(&temporal_unit, NalCodec::Av1));
    }

    #[test]
    fn av1_frame_without_sequence_header_is_not_a_recovery_keyframe() {
        let frame = av1_obu(6, &[0x30, 0x40]);
        assert!(!access_unit_is_keyframe(&frame, NalCodec::Av1));
        assert!(!access_unit_is_recovery_point(&frame, NalCodec::Av1));
        assert!(!access_unit_is_keyframe(&[0x81], NalCodec::Av1));
    }

    #[test]
    fn h264_recovery_nals_with_zero_ref_idc_do_not_set_flags() {
        for header in [0x05, 0x07, 0x08] {
            assert!(!access_unit_is_keyframe(
                &h264_nal(&START_CODE_4, header, &[0x80]),
                NalCodec::H264
            ));
        }
        let mut au = h264_nal(&START_CODE_4, 0x07, &[0x80]);
        au.extend(h264_nal(&START_CODE_3, 0x08, &[0x80]));
        au.extend(h264_nal(&START_CODE_4, 0x05, &[0x80]));
        assert!(!access_unit_is_recovery_point(&au, NalCodec::H264));
    }

    #[test]
    fn h265_irap_requires_base_temporal_layer() {
        for nal_type in 16..=21 {
            let mut au = h265_nal(&START_CODE_4, 32, 1, &[0x80]);
            au.extend(h265_nal(&START_CODE_3, 33, 1, &[0x80]));
            au.extend(h265_nal(&START_CODE_4, 34, 1, &[0x80]));
            au.extend(h265_nal(&START_CODE_3, nal_type, 2, &[0x80]));
            assert!(!access_unit_is_keyframe(&au, NalCodec::H265));
            assert!(!access_unit_is_recovery_point(&au, NalCodec::H265));
        }
    }

    #[test]
    fn malformed_and_mixed_access_units_fail_closed() {
        let cases = [
            START_CODE_4.to_vec(),
            vec![0, 0, 0, 1, 19 << 1],
            h265_nal(&START_CODE_4, 19, 0, &[1]),
            h265_nal(&START_CODE_4, 19, 1, &[]),
            h264_nal(&START_CODE_4, 0xE5, &[1]),
            h264_nal(&START_CODE_4, 0, &[1]),
        ];
        for au in cases {
            assert!(!access_unit_is_keyframe(&au, NalCodec::H264));
            assert!(!access_unit_is_keyframe(&au, NalCodec::H265));
        }

        let mut mixed = h265_nal(&START_CODE_4, 32, 1, &[0]);
        mixed.extend(h264_nal(&START_CODE_3, 0x67, &[1]));
        assert!(!access_unit_is_recovery_point(&mixed, NalCodec::H265));
        assert!(!access_unit_is_recovery_point(&mixed, NalCodec::H264));
    }
}
