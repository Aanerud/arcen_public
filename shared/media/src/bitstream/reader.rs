// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Reading RBSP syntax elements from untrusted NAL payloads.
//!
//! Every read is bounds-checked and returns [`BitstreamError`] instead of
//! panicking: these bytes come from a peer.

use super::BitstreamError;

/// Removes emulation-prevention bytes (`00 00 03` becomes `00 00`) from a NAL
/// payload, producing its RBSP.
///
/// The `03` is only removed when it follows two zero bytes; any other `03` is
/// data.
#[must_use]
pub fn unescape_rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0_u8;
    for &byte in ebsp {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 {
            zeros.saturating_add(1)
        } else {
            0
        };
        out.push(byte);
    }
    out
}

/// Big-endian, MSB-first reader over an RBSP (emulation prevention already
/// removed, see [`unescape_rbsp`]).
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Next bit to read, counted from the first bit of `data`.
    position: usize,
}

impl<'a> BitReader<'a> {
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// Bits consumed so far.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.position
    }

    /// Bits left before the end of the buffer.
    #[must_use]
    pub const fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.position)
    }

    #[must_use]
    pub const fn byte_aligned(&self) -> bool {
        self.position % 8 == 0
    }

    /// One bit as `0` or `1`.
    ///
    /// # Errors
    ///
    /// [`BitstreamError::Truncated`] at the end of the buffer.
    pub fn bit(&mut self) -> Result<u32, BitstreamError> {
        let byte = *self
            .data
            .get(self.position / 8)
            .ok_or(BitstreamError::Truncated)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Ok(u32::from(bit))
    }

    /// `u(1)` as a flag.
    ///
    /// # Errors
    ///
    /// [`BitstreamError::Truncated`] at the end of the buffer.
    pub fn flag(&mut self) -> Result<bool, BitstreamError> {
        Ok(self.bit()? == 1)
    }

    /// `u(n)` for `n <= 32`.
    ///
    /// # Errors
    ///
    /// [`BitstreamError::Truncated`] at the end of the buffer, and
    /// [`BitstreamError::Invalid`] for `n > 32` (a caller bug surfaced as an
    /// error rather than a panic).
    pub fn u(&mut self, count: u32) -> Result<u32, BitstreamError> {
        if count > 32 {
            return Err(BitstreamError::Invalid("bit count"));
        }
        if self.bits_left() < count as usize {
            return Err(BitstreamError::Truncated);
        }
        let mut value = 0_u64;
        for _ in 0..count {
            value = (value << 1) | u64::from(self.bit()?);
        }
        u32::try_from(value).map_err(|_| BitstreamError::Invalid("bit count"))
    }

    /// `u(n)` narrowed to a `u8` (`n <= 8`).
    ///
    /// # Errors
    ///
    /// As [`Self::u`].
    pub fn u8(&mut self, count: u32) -> Result<u8, BitstreamError> {
        if count > 8 {
            return Err(BitstreamError::Invalid("bit count"));
        }
        u8::try_from(self.u(count)?).map_err(|_| BitstreamError::Invalid("bit count"))
    }

    /// Skips `count` bits.
    ///
    /// # Errors
    ///
    /// [`BitstreamError::Truncated`] when fewer bits remain.
    pub fn skip(&mut self, count: usize) -> Result<(), BitstreamError> {
        if self.bits_left() < count {
            return Err(BitstreamError::Truncated);
        }
        self.position += count;
        Ok(())
    }

    /// `ue(v)`, unsigned Exp-Golomb (H.264/H.265 §9.1). Values up to
    /// `u32::MAX - 1` are representable; longer prefixes are refused.
    ///
    /// # Errors
    ///
    /// [`BitstreamError::Truncated`] or [`BitstreamError::Invalid`] for a
    /// prefix longer than 31 zero bits.
    pub fn ue(&mut self) -> Result<u32, BitstreamError> {
        let mut leading = 0_u32;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return Err(BitstreamError::Invalid("exp-golomb code"));
            }
        }
        let suffix = self.u(leading)?;
        let base = (1_u64 << leading) - 1;
        u32::try_from(base + u64::from(suffix))
            .map_err(|_| BitstreamError::Invalid("exp-golomb code"))
    }

    /// `ue(v)` that must not exceed `max`; `field` names it in the error.
    ///
    /// # Errors
    ///
    /// As [`Self::ue`], plus [`BitstreamError::Invalid`] above `max`.
    pub fn ue_max(&mut self, max: u32, field: &'static str) -> Result<u32, BitstreamError> {
        let value = self.ue()?;
        if value > max {
            return Err(BitstreamError::Invalid(field));
        }
        Ok(value)
    }

    /// `se(v)`, signed Exp-Golomb (H.264/H.265 §9.1.1).
    ///
    /// # Errors
    ///
    /// As [`Self::ue`].
    pub fn se(&mut self) -> Result<i32, BitstreamError> {
        let code = i64::from(self.ue()?);
        let magnitude = (code + 1) / 2;
        let value = if code % 2 == 1 { magnitude } else { -magnitude };
        i32::try_from(value).map_err(|_| BitstreamError::Invalid("exp-golomb code"))
    }

    /// `se(v)` within `min..=max`; `field` names it in the error.
    ///
    /// # Errors
    ///
    /// As [`Self::se`], plus [`BitstreamError::Invalid`] outside the range.
    pub fn se_range(
        &mut self,
        min: i32,
        max: i32,
        field: &'static str,
    ) -> Result<i32, BitstreamError> {
        let value = self.se()?;
        if value < min || value > max {
            return Err(BitstreamError::Invalid(field));
        }
        Ok(value)
    }

    /// `more_rbsp_data()` (H.264 §7.2, H.265 §7.2): whether any bit other
    /// than the RBSP stop bit and its zero padding remains.
    #[must_use]
    pub fn more_rbsp_data(&self) -> bool {
        let Some(last_one) = self.last_set_bit() else {
            return false;
        };
        self.position < last_one
    }

    /// Bit index of the RBSP stop bit (the last `1` in the buffer).
    fn last_set_bit(&self) -> Option<usize> {
        let (index, &byte) = self
            .data
            .iter()
            .enumerate()
            .rev()
            .find(|(_, byte)| **byte != 0)?;
        Some(index * 8 + 7 - byte.trailing_zeros() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_removes_only_emulation_prevention_bytes() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1]), vec![0, 0, 1]);
        assert_eq!(unescape_rbsp(&[0, 0, 3, 0, 0, 3]), vec![0, 0, 0, 0]);
        assert_eq!(unescape_rbsp(&[0, 3, 3]), vec![0, 3, 3]);
        assert_eq!(unescape_rbsp(&[]), Vec::<u8>::new());
    }

    #[test]
    fn reads_fixed_width_and_exp_golomb_fields() {
        // 1 | 010 | 011 | 00100 | 1  (ue 0,1,2,3 then a flag)
        let bits = [0b1010_0110, 0b0100_1000];
        let mut reader = BitReader::new(&bits);
        assert_eq!(reader.ue().unwrap(), 0);
        assert_eq!(reader.ue().unwrap(), 1);
        assert_eq!(reader.ue().unwrap(), 2);
        assert_eq!(reader.ue().unwrap(), 3);
        assert!(reader.flag().unwrap());
        assert_eq!(reader.bits_left(), 3);
        assert_eq!(reader.u(4), Err(BitstreamError::Truncated));
    }

    #[test]
    fn signed_exp_golomb_follows_the_alternating_mapping() {
        // ue codes 1,2,3,4 → se 1,-1,2,-2
        let bits = [0b0100_1100, 0b1000_0101];
        let mut reader = BitReader::new(&bits);
        assert_eq!(reader.se().unwrap(), 1);
        assert_eq!(reader.se().unwrap(), -1);
        assert_eq!(reader.se().unwrap(), 2);
        assert_eq!(reader.se().unwrap(), -2);
    }

    #[test]
    fn overlong_exp_golomb_prefix_is_an_error_not_a_panic() {
        let zeros = [0_u8; 8];
        assert!(BitReader::new(&zeros).ue().is_err());
        let max = [0, 0, 0, 1, 0xff, 0xff, 0xff, 0xfe];
        let mut reader = BitReader::new(&max);
        assert_eq!(reader.ue().unwrap(), u32::MAX - 1);
    }

    #[test]
    fn more_rbsp_data_stops_at_the_stop_bit() {
        let rbsp = [0b1011_0000];
        let mut reader = BitReader::new(&rbsp);
        assert!(reader.more_rbsp_data());
        reader.skip(2).unwrap();
        assert!(reader.more_rbsp_data());
        reader.skip(1).unwrap();
        assert!(!reader.more_rbsp_data());
        assert!(!BitReader::new(&[0, 0]).more_rbsp_data());
    }
}
