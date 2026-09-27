//! Reading and rewriting H.264 and H.265 access units.
//!
//! Two encoders in this repository produce *length-prefixed* samples — macOS
//! `VideoToolbox` and Windows Media Foundation both emit four-byte lengths —
//! while the wire format, and Linux's capture path, use Annex B start codes.
//! Converting between them and deciding whether a unit is a keyframe is byte
//! arithmetic over a documented bitstream layout. None of it touches an
//! operating system, and getting it wrong is not obvious from the outside: a
//! stream whose keyframes are misreported still plays, right up until a client
//! reconnects and has nothing to start from.
//!
//! There is one trap here worth stating plainly, because it was a real bug.
//! The two codecs' NAL headers overlap. An H.264 P-slice header of `0x21`
//! reads as HEVC `nal_unit_type` 16, which is inside the IRAP range — so code
//! that tries both layouts marks every H.264 frame a keyframe. Classification
//! must follow the codec the session actually negotiated, which is why every
//! function here takes one.

/// The codec whose NAL layout applies.
///
/// Not defaulted, and not inferred from the bytes: the two layouts alias, so
/// guessing produces a confident wrong answer rather than an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalCodec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    H265,
}

/// The four-byte Annex B start code.
///
/// Three-byte start codes are accepted when scanning, but only the four-byte
/// form is written: it is what every decoder in this repository's test matrix
/// has been exercised against.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Returns whether a single NAL unit begins an intra refresh.
///
/// `nal` must start at the NAL header byte, with no start code or length
/// prefix.
///
/// The header is validated rather than just masked. A byte with the forbidden
/// zero bit set, a reserved type, or an impossible temporal id is not a NAL
/// unit this code should be drawing conclusions from — and the alternative,
/// masking whatever arrives and trusting the result, is how a corrupt byte
/// becomes a confidently reported keyframe.
///
/// These are the rules the Linux host already applied. The shared contract
/// takes the stricter of the implementations it replaces, so migrating a
/// platform onto it is never a downgrade.
#[must_use]
pub fn nal_is_irap(nal: &[u8], codec: NalCodec) -> bool {
    match codec {
        NalCodec::H264 => {
            let Some((&header, body)) = nal.split_first() else {
                return false;
            };
            // forbidden_zero_bit must be clear, and a slice has a payload.
            if header & 0x80 != 0 || body.is_empty() {
                return false;
            }
            // An IDR slice is always referenced; nal_ref_idc of zero means
            // the encoder marked it disposable, which an IDR cannot be.
            let nal_ref_idc = (header >> 5) & 0x03;
            header & 0x1F == 5 && nal_ref_idc != 0
        }
        NalCodec::H265 => {
            // Two-byte header plus at least one payload byte.
            if nal.len() < 3 {
                return false;
            }
            let nal_type = (nal[0] >> 1) & 0x3F;
            let temporal_id_plus_one = nal[1] & 0x07;
            if nal[0] & 0x80 != 0 || temporal_id_plus_one == 0 || nal_type > 47 {
                return false;
            }
            // 16..=21 are the IRAP types a real encoder emits: BLA, IDR and
            // CRA. 22 and 23 are reserved, and treating a reserved type as a
            // keyframe would tell a client to start decoding at a picture
            // whose meaning is not defined. An IRAP is always at the base
            // temporal layer.
            (16..=21).contains(&nal_type) && temporal_id_plus_one == 1
        }
    }
}

/// Returns whether a single NAL unit is a parameter set.
///
/// Parameter sets have to precede the keyframe they describe, or a client that
/// joins mid-stream cannot decode the picture it was given.
#[must_use]
pub fn nal_is_parameter_set(nal: &[u8], codec: NalCodec) -> bool {
    let Some(&header) = nal.first() else {
        return false;
    };
    match codec {
        // 7 is SPS, 8 is PPS. Both are referenced by definition.
        NalCodec::H264 => {
            header & 0x80 == 0 && matches!(header & 0x1F, 7 | 8) && (header >> 5) & 0x03 != 0
        }
        // 32 is VPS, 33 is SPS, 34 is PPS.
        NalCodec::H265 => header & 0x80 == 0 && matches!((header >> 1) & 0x3F, 32..=34),
    }
}

/// Walks the NAL units of a length-prefixed sample.
///
/// Stops at the first malformed length rather than guessing, because a sample
/// that does not describe itself is not one to interpret further.
fn for_each_length_prefixed(sample: &[u8], mut visit: impl FnMut(&[u8])) {
    let mut offset = 0_usize;
    while offset + 4 <= sample.len() {
        let length = u32::from_be_bytes([
            sample[offset],
            sample[offset + 1],
            sample[offset + 2],
            sample[offset + 3],
        ]) as usize;
        offset += 4;
        let Some(end) = offset.checked_add(length) else {
            break;
        };
        if length == 0 || end > sample.len() {
            break;
        }
        visit(&sample[offset..end]);
        offset = end;
    }
}

/// Rewrites a length-prefixed sample into Annex B, appending to `out`.
pub fn append_length_prefixed_as_annex_b(out: &mut Vec<u8>, sample: &[u8]) {
    for_each_length_prefixed(sample, |nal| {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    });
}

/// Returns whether a length-prefixed sample contains an IRAP/IDR unit.
#[must_use]
pub fn length_prefixed_is_keyframe(sample: &[u8], codec: NalCodec) -> bool {
    let mut found = false;
    for_each_length_prefixed(sample, |nal| {
        if !found && nal_is_irap(nal, codec) {
            found = true;
        }
    });
    found
}

/// Returns the length of the start code at `index`, if there is one.
#[must_use]
fn start_code_len(data: &[u8], index: usize) -> Option<usize> {
    if data[index..].starts_with(&[0, 0, 0, 1]) {
        Some(4)
    } else if data[index..].starts_with(&[0, 0, 1]) {
        Some(3)
    } else {
        None
    }
}

/// Walks the NAL units of an Annex B access unit.
///
/// Three- and four-byte start codes are both accepted, because a real
/// bitstream mixes them.
pub fn for_each_annex_b(access_unit: &[u8], mut visit: impl FnMut(&[u8])) {
    let mut index = 0_usize;
    // Find the first start code; anything before it is not a NAL unit.
    while index < access_unit.len() {
        if let Some(prefix) = start_code_len(access_unit, index) {
            index += prefix;
            break;
        }
        index += 1;
    }

    while index < access_unit.len() {
        let start = index;
        let mut end = access_unit.len();
        let mut next = access_unit.len();
        let mut scan = index;
        while scan < access_unit.len() {
            if let Some(prefix) = start_code_len(access_unit, scan) {
                end = scan;
                next = scan + prefix;
                break;
            }
            scan += 1;
        }
        if end > start {
            visit(&access_unit[start..end]);
        }
        if next <= index {
            break;
        }
        index = next;
    }
}

/// Returns whether an Annex B access unit contains an IRAP/IDR unit.
#[must_use]
pub fn annex_b_is_keyframe(access_unit: &[u8], codec: NalCodec) -> bool {
    let mut found = false;
    for_each_annex_b(access_unit, |nal| {
        if !found && nal_is_irap(nal, codec) {
            found = true;
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn length_prefixed(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(&u32::try_from(unit.len()).expect("fits").to_be_bytes());
            out.extend_from_slice(unit);
        }
        out
    }

    #[test]
    fn an_h264_p_slice_is_not_a_keyframe_despite_aliasing_an_hevc_irap() {
        // This is the bug this module exists to prevent. 0x21 is an ordinary
        // H.264 P-slice header, and it reads as HEVC nal_unit_type 16, inside
        // the IRAP range. Code that tries both layouts reports every H.264
        // frame as a keyframe, which looks fine until a client reconnects.
        // As H.264: forbidden bit clear, nal_ref_idc 1, type 1 - a P-slice.
        // As H.265: type (0x21 >> 1) & 0x3F = 16, temporal_id_plus_one 1 - an
        // IRAP at the base layer. The same three bytes, two valid readings.
        let sample = length_prefixed(&[&[0x21, 0xA9, 0xBB]]);
        assert!(!length_prefixed_is_keyframe(&sample, NalCodec::H264));
        assert!(length_prefixed_is_keyframe(&sample, NalCodec::H265));
    }

    #[test]
    fn h264_idr_and_hevc_irap_are_keyframes() {
        let idr = length_prefixed(&[&[0x65, 0x01]]);
        assert!(length_prefixed_is_keyframe(&idr, NalCodec::H264));

        // nal_unit_type 19 (IDR_W_RADL) is 19 << 1 = 0x26, with
        // temporal_id_plus_one 1 in the second header byte.
        let irap = length_prefixed(&[&[0x26, 0x01, 0x00]]);
        assert!(length_prefixed_is_keyframe(&irap, NalCodec::H265));
    }

    #[test]
    fn conversion_preserves_every_unit_and_its_bytes() {
        let sample = length_prefixed(&[&[0x67, 0x01, 0x02], &[0x68, 0x03], &[0x65, 0x04]]);
        let mut out = Vec::new();
        append_length_prefixed_as_annex_b(&mut out, &sample);

        let mut units: Vec<Vec<u8>> = Vec::new();
        for_each_annex_b(&out, |nal| units.push(nal.to_vec()));
        assert_eq!(
            units,
            vec![vec![0x67, 0x01, 0x02], vec![0x68, 0x03], vec![0x65, 0x04],]
        );
        assert!(annex_b_is_keyframe(&out, NalCodec::H264));
    }

    #[test]
    fn a_truncated_length_stops_rather_than_reading_past_the_sample() {
        // A length claiming more bytes than exist must not panic or read
        // beyond the buffer; the valid prefix is kept.
        let mut sample = length_prefixed(&[&[0x65, 0x01]]);
        sample.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF, 0x41]);
        let mut out = Vec::new();
        append_length_prefixed_as_annex_b(&mut out, &sample);
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0x01]);
        assert!(length_prefixed_is_keyframe(&sample, NalCodec::H264));
    }

    #[test]
    fn a_zero_length_unit_ends_the_walk() {
        let mut sample = vec![0, 0, 0, 0];
        sample.extend_from_slice(&length_prefixed(&[&[0x65]]));
        let mut out = Vec::new();
        append_length_prefixed_as_annex_b(&mut out, &sample);
        assert!(out.is_empty(), "a zero length is malformed, not empty");
    }

    #[test]
    fn three_and_four_byte_start_codes_are_both_read() {
        // Real bitstreams mix them, and a scanner that only knows one silently
        // merges two units into one.
        let mut stream = vec![0, 0, 0, 1, 0x67, 0x11];
        stream.extend_from_slice(&[0, 0, 1, 0x65, 0x22]);
        let mut units: Vec<Vec<u8>> = Vec::new();
        for_each_annex_b(&stream, |nal| units.push(nal.to_vec()));
        assert_eq!(units, vec![vec![0x67, 0x11], vec![0x65, 0x22]]);
        assert!(annex_b_is_keyframe(&stream, NalCodec::H264));
    }

    #[test]
    fn malformed_headers_are_not_read_as_keyframes() {
        // The forbidden zero bit set means this is not a NAL unit. Masking it
        // away and trusting the result turns a corrupt byte into a confidently
        // reported keyframe.
        let forbidden = length_prefixed(&[&[0xE5, 0x01]]);
        assert!(!length_prefixed_is_keyframe(&forbidden, NalCodec::H264));

        // An IDR marked disposable is a contradiction.
        let disposable = length_prefixed(&[&[0x05, 0x01]]);
        assert!(!length_prefixed_is_keyframe(&disposable, NalCodec::H264));

        // A slice header with no payload.
        let empty_body = length_prefixed(&[&[0x65]]);
        assert!(!length_prefixed_is_keyframe(&empty_body, NalCodec::H264));

        // HEVC reserved IRAP types 22 and 23 are not keyframes to start at:
        // their meaning is not defined, so telling a client to begin decoding
        // there is worse than waiting for a real one. 22 << 1 = 0x2C.
        let reserved = length_prefixed(&[&[0x2C, 0x01, 0x00]]);
        assert!(!length_prefixed_is_keyframe(&reserved, NalCodec::H265));

        // An IRAP is always at the base temporal layer.
        let higher_layer = length_prefixed(&[&[0x26, 0x02, 0x00]]);
        assert!(!length_prefixed_is_keyframe(&higher_layer, NalCodec::H265));
    }

    #[test]
    fn parameter_sets_are_recognised_for_both_codecs() {
        assert!(nal_is_parameter_set(&[0x67], NalCodec::H264));
        assert!(nal_is_parameter_set(&[0x68], NalCodec::H264));
        assert!(!nal_is_parameter_set(&[0x65], NalCodec::H264));

        // 32 << 1 = 0x40 (VPS), 33 << 1 = 0x42 (SPS), 34 << 1 = 0x44 (PPS).
        assert!(nal_is_parameter_set(&[0x40], NalCodec::H265));
        assert!(nal_is_parameter_set(&[0x42], NalCodec::H265));
        assert!(nal_is_parameter_set(&[0x44], NalCodec::H265));
        assert!(!nal_is_parameter_set(&[0x26], NalCodec::H265));
    }

    #[test]
    fn an_empty_unit_is_not_classified_as_anything() {
        assert!(!nal_is_irap(&[], NalCodec::H264));
        assert!(!nal_is_irap(&[], NalCodec::H265));
        assert!(!nal_is_parameter_set(&[], NalCodec::H264));
    }
}
