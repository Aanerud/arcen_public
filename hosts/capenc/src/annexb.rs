// Convert MF's length-prefixed (AVCC) NAL unit stream into Annex-B start-code
// framing, matching NVENC's on-wire output and what arcen-protocol expects.
//
// Media Foundation's H.264 encoder emits samples whose IMFMediaBuffer payload
// is one or more NAL units, each preceded by a big-endian 32-bit length. NVENC
// on our other host emits Annex-B directly (each NAL prefixed by 00 00 00 01
// or 00 00 01). To keep exactly one encoder-output contract in the wire path,
// the MF path converts AVCC -> Annex-B before writing to stdout.
//
// SPS + PPS are not carried in the per-sample payload; MF exposes them once
// through the media type attribute `MF_MT_MPEG_SEQUENCE_HEADER` (also AVCC-
// framed). The encoder module prepends the parsed SPS/PPS to every IDR access
// unit — decoders on Apple VideoToolbox etc. require them right before the IDR
// slice.

/// Convert a single AVCC (4-byte big-endian length + NALU) buffer into Annex-B
/// framing (00 00 00 01 || NALU). Returns Err if the buffer is malformed.
pub(crate) fn avcc_to_annexb(mut input: &[u8], out: &mut Vec<u8>) -> Result<(), &'static str> {
    while !input.is_empty() {
        if input.len() < 4 {
            return Err("AVCC buffer ended mid-length-prefix");
        }
        let (len_bytes, rest) = input.split_at(4);
        let len =
            u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
        if rest.len() < len {
            return Err("AVCC length exceeds remaining buffer");
        }
        if len == 0 {
            // Zero-length NALU is illegal but has been observed on some MFTs
            // when the encoder emits an empty tail; skip cleanly rather than
            // producing a truncated Annex-B AU.
            input = &rest[len..];
            continue;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&rest[..len]);
        input = &rest[len..];
    }
    Ok(())
}

/// Parse the AVCDecoderConfigurationRecord layout that MF stores in
/// `MF_MT_MPEG_SEQUENCE_HEADER` on some builds, extracting each SPS and PPS as
/// a raw NAL unit (no start code, no length prefix). Returns an empty vector
/// when the attribute is absent or malformed — the caller then falls back to
/// scanning the first output sample for NALU types 7/8.
///
/// Layout per ISO/IEC 14496-15 §5.3.3.1:
///   [1] configurationVersion (=1)
///   [1] AVCProfileIndication
///   [1] profile_compatibility
///   [1] AVCLevelIndication
///   [1] 0b111111 | (lengthSizeMinusOne : 2)
///   [1] 0b111    | (numOfSequenceParameterSets : 5)
///   for each SPS: [2] u16_be length, [length] NALU
///   [1] numOfPictureParameterSets
///   for each PPS: [2] u16_be length, [length] NALU
#[allow(dead_code)]
pub(crate) fn parse_avc_decoder_config(input: &[u8]) -> Vec<Vec<u8>> {
    fn read_u16(bytes: &[u8], off: &mut usize) -> Option<usize> {
        if *off + 2 > bytes.len() {
            return None;
        }
        let v = u16::from_be_bytes([bytes[*off], bytes[*off + 1]]) as usize;
        *off += 2;
        Some(v)
    }
    let mut params = Vec::new();
    if input.len() < 7 || input[0] != 1 {
        return params;
    }
    let mut off = 5usize;
    let num_sps = (input[off] & 0x1f) as usize;
    off += 1;
    for _ in 0..num_sps {
        let Some(len) = read_u16(input, &mut off) else {
            return params;
        };
        if off + len > input.len() {
            return params;
        }
        params.push(input[off..off + len].to_vec());
        off += len;
    }
    if off >= input.len() {
        return params;
    }
    let num_pps = input[off] as usize;
    off += 1;
    for _ in 0..num_pps {
        let Some(len) = read_u16(input, &mut off) else {
            return params;
        };
        if off + len > input.len() {
            return params;
        }
        params.push(input[off..off + len].to_vec());
        off += len;
    }
    params
}

/// Prepend Annex-B-framed SPS/PPS to an access unit that starts with an IDR
/// slice. Called once at stream start and before every keyframe so decoders
/// that lost the initial parameter sets can recover.
#[allow(dead_code)]
pub(crate) fn prepend_parameter_sets(params: &[Vec<u8>], au: &mut Vec<u8>) {
    if params.is_empty() {
        return;
    }
    let mut prefix = Vec::with_capacity(params.iter().map(|p| p.len() + 4).sum::<usize>());
    for p in params {
        prefix.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        prefix.extend_from_slice(p);
    }
    let existing = std::mem::take(au);
    au.reserve(prefix.len() + existing.len());
    au.extend_from_slice(&prefix);
    au.extend_from_slice(&existing);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_single_nalu() {
        let mut input = Vec::new();
        input.extend_from_slice(&5u32.to_be_bytes());
        input.extend_from_slice(&[0x65, 0xaa, 0xbb, 0xcc, 0xdd]);
        let mut out = Vec::new();
        avcc_to_annexb(&input, &mut out).unwrap();
        assert_eq!(
            out,
            vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xaa, 0xbb, 0xcc, 0xdd]
        );
    }

    #[test]
    fn converts_multiple_nalus_in_one_buffer() {
        let mut input = Vec::new();
        input.extend_from_slice(&2u32.to_be_bytes());
        input.extend_from_slice(&[0x67, 0x42]);
        input.extend_from_slice(&3u32.to_be_bytes());
        input.extend_from_slice(&[0x68, 0xce, 0x38]);
        let mut out = Vec::new();
        avcc_to_annexb(&input, &mut out).unwrap();
        assert_eq!(
            out,
            vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x38,]
        );
    }

    #[test]
    fn rejects_truncated_length_prefix() {
        let input = [0x00, 0x00, 0x00];
        let mut out = Vec::new();
        assert!(avcc_to_annexb(&input, &mut out).is_err());
    }

    #[test]
    fn rejects_length_exceeding_buffer() {
        let mut input = Vec::new();
        input.extend_from_slice(&10u32.to_be_bytes());
        input.extend_from_slice(&[0x65, 0xaa]);
        let mut out = Vec::new();
        assert!(avcc_to_annexb(&input, &mut out).is_err());
    }

    #[test]
    fn skips_zero_length_tail() {
        let mut input = Vec::new();
        input.extend_from_slice(&2u32.to_be_bytes());
        input.extend_from_slice(&[0x65, 0xaa]);
        input.extend_from_slice(&0u32.to_be_bytes());
        let mut out = Vec::new();
        avcc_to_annexb(&input, &mut out).unwrap();
        assert_eq!(out, vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xaa]);
    }

    #[test]
    fn parses_avc_decoder_config_record() {
        let record = vec![
            0x01, // configurationVersion
            0x42, 0x00, 0x1f, // profile/compat/level
            0xff, // lengthSizeMinusOne (bits 0-1) = 3
            0xe1, // 111 | numSPS=1
            0x00, 0x03, 0x67, 0x42, 0x1f, // SPS len=3, bytes
            0x01, // numPPS=1
            0x00, 0x02, 0x68, 0xce, // PPS len=2, bytes
        ];
        let params = parse_avc_decoder_config(&record);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], vec![0x67, 0x42, 0x1f]);
        assert_eq!(params[1], vec![0x68, 0xce]);
    }

    #[test]
    fn prepend_parameter_sets_wraps_each_in_start_code() {
        // The AU passed to `prepend_parameter_sets` in production is already
        // Annex-B framed by `avcc_to_annexb` (each NAL preceded by 00 00 00 01).
        let params = vec![vec![0x67, 0x42], vec![0x68, 0xce]];
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xaa];
        prepend_parameter_sets(&params, &mut au);
        assert_eq!(
            au,
            vec![
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x00, 0x00,
                0x00, 0x01, 0x65, 0xaa,
            ]
        );
    }
}
