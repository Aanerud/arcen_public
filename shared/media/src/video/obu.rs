fn read_leb128(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for (index, byte) in bytes.iter().copied().enumerate().take(8) {
        value |= usize::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

/// Whether one AV1 low-overhead temporal unit carries a Sequence Header OBU.
///
/// Arcen configures NVENC with `repeatSeqHdr=1`, so this is also the
/// self-contained recovery/keyframe signal used by both hosts.
#[must_use]
pub fn av1_low_overhead_has_sequence_header(data: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset < data.len() {
        let header = data[offset];
        if header & 0x81 != 0 {
            return false;
        }
        let obu_type = (header >> 3) & 0x0f;
        let extension_flag = header & 0x04 != 0;
        let has_size_field = header & 0x02 != 0;
        let header_len = if extension_flag { 2 } else { 1 };
        let Some(size_start) = offset.checked_add(header_len) else {
            return false;
        };
        if size_start > data.len() {
            return false;
        }
        if !has_size_field {
            return false;
        }
        let Some((payload_len, size_len)) = read_leb128(&data[size_start..]) else {
            return false;
        };
        let Some(payload_start) = size_start.checked_add(size_len) else {
            return false;
        };
        let Some(next) = payload_start.checked_add(payload_len) else {
            return false;
        };
        if next > data.len() || next <= offset {
            return false;
        }
        if obu_type == 1 {
            return true;
        }
        offset = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut output = vec![(obu_type << 3) | 0x02, payload.len() as u8];
        output.extend_from_slice(payload);
        output
    }

    #[test]
    fn finds_sequence_header_and_rejects_malformed_units() {
        let mut temporal_unit = obu(2, &[]);
        temporal_unit.extend(obu(1, &[0x10, 0x20]));
        temporal_unit.extend(obu(6, &[0x30]));
        assert!(av1_low_overhead_has_sequence_header(&temporal_unit));
        assert!(!av1_low_overhead_has_sequence_header(&obu(6, &[0x30])));
        assert!(!av1_low_overhead_has_sequence_header(&[0x81]));
        assert!(!av1_low_overhead_has_sequence_header(&[0x08]));
        assert!(!av1_low_overhead_has_sequence_header(&[0x0a]));
        assert!(!av1_low_overhead_has_sequence_header(&[0x0a, 0x02, 0x10]));
        assert!(!av1_low_overhead_has_sequence_header(&[0x12, 0x80]));
    }
}
