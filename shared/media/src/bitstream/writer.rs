// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Writing RBSP syntax elements and escaping them into NAL payloads.
//!
//! Used to synthesise parameter sets for decoder capability probes and to
//! build test streams; nothing here reads peer input.

/// Big-endian, MSB-first bit writer for Annex-B RBSP syntax elements.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    partial: u8,
    /// Number of valid bits already shifted into `partial`, 0..=7.
    bit_pos: u8,
}

impl BitWriter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes one bit, most-significant-bit-first within each byte.
    pub fn write_bit(&mut self, bit: bool) {
        self.partial = (self.partial << 1) | u8::from(bit);
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bytes.push(self.partial);
            self.partial = 0;
            self.bit_pos = 0;
        }
    }

    /// `n_bits` zero bits -- shorthand for a run of reserved/unused
    /// flag bits.
    pub fn write_zeros(&mut self, n_bits: u32) {
        for _ in 0..n_bits {
            self.write_bit(false);
        }
    }

    /// `u(n)`: the low `n_bits` of `value`, most-significant-bit-first.
    pub fn write_bits(&mut self, value: u32, n_bits: u32) {
        debug_assert!(n_bits <= 32);
        for i in (0..n_bits).rev() {
            self.write_bit((value >> i) & 1 != 0);
        }
    }

    /// `ue(v)`: unsigned Exp-Golomb code (ITU-T H.264/H.265 9.1).
    pub fn write_ue(&mut self, value: u32) {
        let code_num = value + 1;
        let leading_zero_bits = code_num.ilog2();
        self.write_zeros(leading_zero_bits);
        self.write_bits(code_num, leading_zero_bits + 1);
    }

    /// `se(v)`: signed Exp-Golomb code (ITU-T H.264/H.265 9.1.1's
    /// mapping from a signed value to the `ue(v)` `codeNum`).
    pub fn write_se(&mut self, value: i32) {
        let doubled = i64::from(value) * 2;
        let code_num = if value <= 0 { -doubled } else { doubled - 1 };
        self.write_ue(u32::try_from(code_num).unwrap_or(u32::MAX));
    }

    /// `rbsp_trailing_bits()`: a single `1` stop bit, then zero
    /// padding to the next byte boundary.
    pub fn rbsp_trailing_bits(&mut self) {
        self.write_bit(true);
        while self.bit_pos != 0 {
            self.write_bit(false);
        }
    }

    /// Number of bits written so far, including any not-yet-flushed
    /// partial byte.
    #[must_use]
    pub fn bit_len(&self) -> usize {
        self.bytes.len() * 8 + usize::from(self.bit_pos)
    }

    /// Consumes the writer, returning the byte-aligned RBSP.
    #[must_use]
    pub fn into_rbsp(self) -> Vec<u8> {
        debug_assert_eq!(self.bit_pos, 0, "RBSP must be byte-aligned before use");
        self.bytes
    }
}

/// Applies Annex-B "emulation prevention": inserts `0x03` after any
/// two consecutive zero bytes immediately followed by a byte `<=
/// 0x03`, so the result never contains a real start-code prefix
/// (`0x000000`, `0x000001`, `0x000002`) or an unescaped `0x000003`
/// once wrapped in a NAL header.
#[must_use]
pub fn escape_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len());
    let mut zero_run = 0u8;
    for &byte in rbsp {
        if zero_run >= 2 && byte <= 3 {
            out.push(0x03);
            zero_run = 0;
        }
        out.push(byte);
        zero_run = if byte == 0 { zero_run + 1 } else { 0 };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstream::BitReader;

    #[test]
    fn exp_golomb_ue_matches_known_reference_codes() {
        // ITU-T H.264/H.265 9.1, Table 9-2: ue(0)="1", ue(1)="010",
        // ue(2)="011", ue(3)="00100", ue(4)="00101", ue(5)="00110",
        // ue(6)="00111", ue(7)="0001000".
        let cases: &[(u32, &str)] = &[
            (0, "1"),
            (1, "010"),
            (2, "011"),
            (3, "00100"),
            (4, "00101"),
            (5, "00110"),
            (6, "00111"),
            (7, "0001000"),
        ];
        for &(value, expected_bits) in cases {
            let mut w = BitWriter::new();
            w.write_ue(value);
            let bit_len = w.bit_len();
            assert_eq!(bit_len, expected_bits.len(), "ue({value}) bit length");
            w.write_zeros(((8 - bit_len % 8) % 8) as u32);
            let bytes = w.into_rbsp();
            let mut reader = BitReader::new(&bytes);
            let actual: String = (0..bit_len)
                .map(|_| if reader.flag().unwrap() { '1' } else { '0' })
                .collect();
            assert_eq!(actual, expected_bits, "ue({value})");
        }
    }

    #[test]
    fn exp_golomb_se_matches_the_signed_mapping() {
        // ITU-T H.264/H.265 Table 9-3: se(0)=ue(0), se(1)=ue(1),
        // se(-1)=ue(2), se(2)=ue(3), se(-2)=ue(4). Compares full bit
        // content (not just length) against an independently-built ue()
        // writer for the mapped codeNum.
        let cases: &[(i32, u32)] = &[(0, 0), (1, 1), (-1, 2), (2, 3), (-2, 4)];
        for &(value, expected_ue) in cases {
            let mut actual = BitWriter::new();
            actual.write_se(value);
            let bit_len = actual.bit_len();
            let mut expected = BitWriter::new();
            expected.write_ue(expected_ue);
            assert_eq!(bit_len, expected.bit_len(), "se({value}) bit length");

            let padding = ((8 - bit_len % 8) % 8) as u32;
            actual.write_zeros(padding);
            expected.write_zeros(padding);
            assert_eq!(actual.into_rbsp(), expected.into_rbsp(), "se({value}) bits");
        }
    }

    #[test]
    fn emulation_prevention_escapes_every_start_code_like_run_but_nothing_else() {
        assert_eq!(escape_emulation_prevention(&[0, 0, 0]), vec![0, 0, 3, 0]);
        assert_eq!(escape_emulation_prevention(&[0, 0, 1]), vec![0, 0, 3, 1]);
        assert_eq!(escape_emulation_prevention(&[0, 0, 2]), vec![0, 0, 3, 2]);
        assert_eq!(escape_emulation_prevention(&[0, 0, 3]), vec![0, 0, 3, 3]);
        // 0x04 and above never need escaping.
        assert_eq!(escape_emulation_prevention(&[0, 0, 4]), vec![0, 0, 4]);
        // A run of more than two zeros only re-triggers after the
        // inserted 0x03 resets the count.
        assert_eq!(
            escape_emulation_prevention(&[0, 0, 0, 0, 1]),
            vec![0, 0, 3, 0, 0, 3, 1]
        );
    }
}
