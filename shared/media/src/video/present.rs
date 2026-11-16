// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's decoded-picture → R′G′B′ contract, shared by every presenter.
//!
//! Each Deck converts decoded Y′CbCr (or GBR) codes in its own shading
//! language — WGSL and Metal on the Mac, HLSL on Windows — but all of them
//! read the same [`VideoUniform`], derived here from the negotiated
//! [`VideoColorContract`], and all of them are tested against the same CPU
//! references ([`VideoUniform::reference_rgb`],
//! [`VideoUniform::chroma_reference_codes`]). The maths mirrors
//! [`super::ColorTransform::to_bgr8`]; the shaders keep their formulas in
//! step with these functions (ADR 0016 R1: a refined Y′CbCr pixel passes
//! exactly the presenter's own range and matrix).
//!
//! Transfer, not depth, decides HDR ([`presents_pq`]): ten-bit BT.709
//! Grading is SDR.

use crate::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
    VideoConfiguration,
};

/// The negotiated axes that decide how a decoded picture is converted and
/// presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoColorContract {
    /// Chroma layout.
    pub chroma: ChromaSubsampling,
    /// Coded sample range.
    pub range: ColorRange,
    /// Coded bit depth.
    pub depth: BitDepth,
    /// Matrix coefficients.
    pub matrix: ColorMatrix,
    /// Colour primaries (a presentation-surface input, not a conversion
    /// one).
    pub primaries: ColorPrimaries,
    /// Transfer characteristics: the only axis that separates HDR from SDR.
    pub transfer: TransferCharacteristics,
}

impl Default for VideoColorContract {
    /// The legacy contract: BT.709 limited 8-bit 4:2:0.
    fn default() -> Self {
        Self {
            chroma: ChromaSubsampling::Yuv420,
            range: ColorRange::Limited,
            depth: BitDepth::Eight,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }
}

impl From<VideoConfiguration> for VideoColorContract {
    fn from(configuration: VideoConfiguration) -> Self {
        Self {
            chroma: configuration.chroma,
            range: configuration.range,
            depth: configuration.bit_depth,
            matrix: configuration.matrix,
            primaries: configuration.primaries,
            transfer: configuration.transfer,
        }
    }
}

/// Whether a presenter tags its surface PQ / BT.2100 and enables HDR
/// (EDR on the Mac, HDR10 metadata on Windows): only a PQ transfer does.
#[must_use]
pub const fn presents_pq(transfer: TransferCharacteristics) -> bool {
    matches!(transfer, TransferCharacteristics::Pq)
}

/// Minimum mastering luminance, in cd/m², of the HDR10 grade a presenter
/// signals when the stream carries no ST 2086 metadata of its own — which
/// Arcen's never does: a desktop has no mastering display. With
/// [`HDR10_MASTERING_MAX_NITS`] this is the reference HDR10 grade, and what
/// Windows composites its own HDR desktop against.
pub const HDR10_MASTERING_MIN_NITS: f32 = 0.005;
/// Maximum mastering luminance, in cd/m², of that grade.
pub const HDR10_MASTERING_MAX_NITS: f32 = 1000.0;

/// Luma coefficients `(Kr, Kb)` of a matrix (`Kg = 1 − Kr − Kb`).
#[must_use]
pub const fn luma_weights(matrix: ColorMatrix) -> (f32, f32) {
    match matrix {
        ColorMatrix::Identity | ColorMatrix::Bt709 => (0.2126, 0.0722),
        ColorMatrix::Bt601 => (0.299, 0.114),
        ColorMatrix::Bt2020Ncl => (0.2627, 0.0593),
    }
}

/// [`VideoUniform::mode`]: BT.709/601/2020 NCL Y′CbCr → R′G′B′.
pub const MODE_MATRIX: u32 = 0;
/// [`VideoUniform::mode`]: ITU-T H.273 identity/GBR (plane 0 → G, Cb slot →
/// B, Cr slot → R, all scaled by the luma range).
pub const MODE_IDENTITY_GBR: u32 = 1;
/// [`VideoUniform::mode`]: already-decoded RGB (the Mac's portable path).
pub const MODE_PACKED_RGBA: u32 = 2;

/// Bytes of [`VideoUniform::to_bytes`]: twelve 4-byte scalars, no padding
/// (WGSL, MSL and HLSL `cbuffer` packing agree on a run of scalars).
pub const VIDEO_UNIFORM_BYTES: usize = 48;

/// The conversion uniform every presenter's shader reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoUniform {
    /// Luma code of black.
    pub luma_offset: f32,
    /// `1 / (white − black)` in luma codes.
    pub luma_span_inv: f32,
    /// Neutral chroma code (`1 << (depth − 1)` at full range).
    pub chroma_center: f32,
    /// `1 / chroma span`.
    pub chroma_span_inv: f32,
    /// `Kr`.
    pub kr: f32,
    /// `Kb`.
    pub kb: f32,
    /// `1 / Kg`.
    pub kg_inv: f32,
    /// [`MODE_MATRIX`], [`MODE_IDENTITY_GBR`] or [`MODE_PACKED_RGBA`].
    pub mode: u32,
    /// Luma plane width.
    pub luma_width: f32,
    /// Luma plane height.
    pub luma_height: f32,
    /// Chroma plane width.
    pub chroma_width: f32,
    /// Chroma plane height.
    pub chroma_height: f32,
}

impl VideoUniform {
    /// Already-decoded RGB: no conversion; identity-safe range fields.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "plane sizes are far below 2^24")]
    pub fn passthrough(width: u32, height: u32) -> Self {
        Self {
            luma_offset: 0.0,
            luma_span_inv: 1.0,
            chroma_center: 0.0,
            chroma_span_inv: 1.0,
            kr: 0.0,
            kb: 0.0,
            kg_inv: 1.0,
            mode: MODE_PACKED_RGBA,
            luma_width: width as f32,
            luma_height: height as f32,
            chroma_width: width as f32,
            chroma_height: height as f32,
        }
    }

    /// The uniform for `contract` and the planes' pixel sizes, mirroring
    /// [`super::ColorTransform::new`]'s offset, span and centre.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "plane sizes are far below 2^24")]
    pub fn from_contract(
        contract: VideoColorContract,
        luma_size: (u32, u32),
        chroma_size: (u32, u32),
    ) -> Self {
        let depth = contract.depth;
        let (luma_lo, luma_hi) = contract.range.luma_bounds(depth);
        let (chroma_lo, chroma_hi) = contract.range.chroma_bounds(depth);
        // Full range centres chroma on the ITU convention `1 << (bits − 1)`,
        // not the arithmetic mean of the bounds (511.5 at ten bits).
        let chroma_center = match contract.range {
            ColorRange::Limited => f32::from(chroma_lo + chroma_hi) / 2.0,
            ColorRange::Full => f32::from(1_u16 << (depth.bits() - 1)),
        };
        let (kr, kb) = luma_weights(contract.matrix);
        Self {
            luma_offset: f32::from(luma_lo),
            luma_span_inv: 1.0 / f32::from(luma_hi - luma_lo),
            chroma_center,
            chroma_span_inv: 1.0 / f32::from(chroma_hi - chroma_lo),
            kr,
            kb,
            kg_inv: 1.0 / (1.0 - kr - kb),
            mode: if contract.matrix.is_identity() {
                MODE_IDENTITY_GBR
            } else {
                MODE_MATRIX
            },
            luma_width: luma_size.0 as f32,
            luma_height: luma_size.1 as f32,
            chroma_width: chroma_size.0 as f32,
            chroma_height: chroma_size.1 as f32,
        }
    }

    /// The uniform's bytes: twelve little-endian scalars in field order.
    #[must_use]
    pub fn to_bytes(self) -> [u8; VIDEO_UNIFORM_BYTES] {
        let words = [
            self.luma_offset.to_bits(),
            self.luma_span_inv.to_bits(),
            self.chroma_center.to_bits(),
            self.chroma_span_inv.to_bits(),
            self.kr.to_bits(),
            self.kb.to_bits(),
            self.kg_inv.to_bits(),
            self.mode,
            self.luma_width.to_bits(),
            self.luma_height.to_bits(),
            self.chroma_width.to_bits(),
            self.chroma_height.to_bits(),
        ];
        let mut bytes = [0_u8; VIDEO_UNIFORM_BYTES];
        for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// CPU reference of the shaders' range and matrix step, from the three
    /// codes of one pixel: normalised R′G′B′, clamped to `0..=1`.
    #[must_use]
    #[allow(clippy::similar_names, reason = "Cb and Cr are the standard names")]
    pub fn reference_rgb(&self, luma_code: f32, cb_code: f32, cr_code: f32) -> [f32; 3] {
        let clamp = |v: f32| v.clamp(0.0, 1.0);
        if self.mode == MODE_IDENTITY_GBR {
            let scale = |code: f32| clamp((code - self.luma_offset) * self.luma_span_inv);
            return [scale(cr_code), scale(luma_code), scale(cb_code)];
        }
        let luma_norm = (luma_code - self.luma_offset) * self.luma_span_inv;
        let blue_diff = (cb_code - self.chroma_center) * self.chroma_span_inv;
        let red_diff = (cr_code - self.chroma_center) * self.chroma_span_inv;
        let red = luma_norm + 2.0 * (1.0 - self.kr) * red_diff;
        let blue = luma_norm + 2.0 * (1.0 - self.kb) * blue_diff;
        let green = (luma_norm - self.kr * red - self.kb * blue) * self.kg_inv;
        [clamp(red), clamp(green), clamp(blue)]
    }

    /// CPU reference of the shaders' luma-guided chroma reconstruction: a
    /// fit of chroma against block-mean luma over the four nearest
    /// centre-sited chroma samples, blended towards the pixel's own sample
    /// by the fit's r²; the exact fetch for 4:4:4. `luma(x, y)` returns the
    /// luma code at a luma texel and `chroma(x, y)` the `(Cb, Cr)` codes at
    /// a chroma texel; `uv` is the pixel centre in `0..1`.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::similar_names,
        reason = "the shaders' own float/int conversions, reproduced exactly"
    )]
    pub fn chroma_reference_codes(
        &self,
        uv: [f32; 2],
        pixel_luma: f32,
        luma: impl Fn(u32, u32) -> f32,
        chroma: impl Fn(u32, u32) -> [f32; 2],
    ) -> [f32; 2] {
        const EPSILON: f32 = 3.0e-4;
        let luma_size = [self.luma_width, self.luma_height];
        let chroma_size = [self.chroma_width, self.chroma_height];
        let exact = [
            (uv[0] * chroma_size[0]) as i64,
            (uv[1] * chroma_size[1]) as i64,
        ];
        let own = chroma(exact[0] as u32, exact[1] as u32);
        let full = [
            chroma_size[0] >= luma_size[0],
            chroma_size[1] >= luma_size[1],
        ];
        if full[0] && full[1] {
            return own;
        }
        let ratio = [
            (luma_size[0] / chroma_size[0]).round() as i64,
            (luma_size[1] / chroma_size[1]).round() as i64,
        ];
        let mut c0 = [0_i64; 2];
        let mut c1 = [0_i64; 2];
        let mut weight = [0.0_f32; 2];
        for axis in 0..2 {
            let last = chroma_size[axis] as i64 - 1;
            if full[axis] {
                c0[axis] = exact[axis].clamp(0, last);
                c1[axis] = c0[axis];
            } else {
                let position = uv[axis] * chroma_size[axis] - 0.5;
                let base = position.floor();
                weight[axis] = position - base;
                c0[axis] = (base as i64).clamp(0, last);
                c1[axis] = (base as i64 + 1).clamp(0, last);
            }
        }
        let normalise = |code: f32| (code - self.luma_offset) * self.luma_span_inv;
        let block_mean = |cx: i64, cy: i64| {
            let mut sum = 0.0;
            for dy in 0..ratio[1] {
                for dx in 0..ratio[0] {
                    let x = (cx * ratio[0] + dx).clamp(0, luma_size[0] as i64 - 1);
                    let y = (cy * ratio[1] + dy).clamp(0, luma_size[1] as i64 - 1);
                    sum += luma(x as u32, y as u32);
                }
            }
            normalise(sum / (ratio[0] * ratio[1]) as f32)
        };
        let taps = [
            ((c0[0], c0[1]), (1.0 - weight[0]) * (1.0 - weight[1])),
            ((c1[0], c0[1]), weight[0] * (1.0 - weight[1])),
            ((c0[0], c1[1]), (1.0 - weight[0]) * weight[1]),
            ((c1[0], c1[1]), weight[0] * weight[1]),
        ];
        let samples: Vec<(f32, f32, [f32; 2])> = taps
            .iter()
            .map(|&((x, y), w)| (w, block_mean(x, y), chroma(x as u32, y as u32)))
            .collect();
        let mean_luma: f32 = samples.iter().map(|(w, y, _)| w * y).sum();
        let luma_variance: f32 = samples
            .iter()
            .map(|(w, y, _)| w * (y - mean_luma) * (y - mean_luma))
            .sum();
        let luma_here = normalise(pixel_luma);
        let mut out = [0.0; 2];
        for channel in 0..2 {
            let mean: f32 = samples.iter().map(|(w, _, c)| w * c[channel]).sum();
            let variance: f32 = samples
                .iter()
                .map(|(w, _, c)| w * (c[channel] - mean) * (c[channel] - mean))
                .sum();
            let covariance: f32 = samples
                .iter()
                .map(|(w, y, c)| w * (y - mean_luma) * (c[channel] - mean))
                .sum();
            let low = samples
                .iter()
                .map(|s| s.2[channel])
                .fold(f32::INFINITY, f32::min);
            let high = samples
                .iter()
                .map(|s| s.2[channel])
                .fold(f32::NEG_INFINITY, f32::max);
            let slope = covariance / (luma_variance + EPSILON);
            let fitted = (mean + slope * (luma_here - mean_luma)).clamp(low, high);
            let denominator = luma_variance * variance;
            let confidence = if denominator > 0.0 {
                (covariance * covariance / denominator.max(1.0e-12)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            out[channel] = own[channel] + (fitted - own[channel]) * confidence;
        }
        out
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    clippy::float_cmp,
    clippy::too_many_lines,
    reason = "exact codes are compared on purpose"
)]
mod tests {
    use super::*;

    const GRADING: VideoColorContract = VideoColorContract {
        chroma: ChromaSubsampling::Yuv444,
        range: ColorRange::Full,
        depth: BitDepth::Ten,
        matrix: ColorMatrix::Bt709,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristics::Srgb,
    };

    #[test]
    fn full_range_ten_bit_grey_is_the_luma_code() {
        let uniform = VideoUniform::from_contract(GRADING, (4, 4), (4, 4));
        assert_eq!(uniform.chroma_center, 512.0);
        for code in [0_u16, 1, 511, 512, 1022, 1023] {
            for channel in uniform.reference_rgb(f32::from(code), 512.0, 512.0) {
                assert_eq!((channel * 1023.0).round(), f32::from(code));
            }
        }
    }

    #[test]
    fn the_reference_agrees_with_the_shared_color_transform() {
        let transform = super::super::ColorTransform::new(
            ColorMatrix::Bt709,
            ColorRange::Limited,
            BitDepth::Eight,
        );
        let uniform = VideoUniform::from_contract(VideoColorContract::default(), (2, 2), (1, 1));
        for (y, cb, cr) in [
            (16, 128, 128),
            (235, 128, 128),
            (81, 90, 240),
            (145, 54, 34),
        ] {
            let (b, g, r) = transform.to_bgr8(y, cb, cr);
            #[allow(clippy::cast_precision_loss)]
            let [red, green, blue] = uniform.reference_rgb(y as f32, cb as f32, cr as f32);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let to_u8 = |v: f32| (v * 255.0).round() as u8;
            assert_eq!((to_u8(blue), to_u8(green), to_u8(red)), (b, g, r));
        }
    }

    #[test]
    fn a_flat_four_four_four_picture_keeps_its_own_chroma() {
        let uniform = VideoUniform::from_contract(GRADING, (8, 8), (8, 8));
        let got = uniform.chroma_reference_codes(
            [0.3, 0.6],
            500.0,
            |_, _| 500.0,
            |x, y| {
                [
                    f32::from(u16::try_from(x).unwrap_or(0)),
                    f32::from(u16::try_from(y).unwrap_or(0)),
                ]
            },
        );
        assert_eq!(got, [2.0, 4.0]);
    }

    #[test]
    fn only_pq_presents_hdr() {
        assert!(presents_pq(TransferCharacteristics::Pq));
        assert!(!presents_pq(TransferCharacteristics::Srgb));
        assert!(!presents_pq(TransferCharacteristics::Bt709));
    }

    #[test]
    fn the_uniform_is_twelve_little_endian_scalars() {
        let bytes = VideoUniform::from_contract(GRADING, (64, 32), (64, 32)).to_bytes();
        assert_eq!(bytes[28..32], MODE_MATRIX.to_le_bytes());
        assert_eq!(bytes[32..36], 64.0_f32.to_le_bytes());
        assert_eq!(bytes[44..48], 32.0_f32.to_le_bytes());
    }

    // Moved from the macOS Deck (`clients/macos/src/ui/video_render.rs`)
    // with its copy of this uniform: every presenter reads these values.

    fn assert_f32_near(actual: f32, expected: f32, context: &str) {
        let tolerance = expected.abs().max(1.0) * f32::EPSILON;
        assert!(
            (actual - expected).abs() <= tolerance,
            "{context}: expected {expected}, got {actual}"
        );
    }

    fn contract(
        chroma: ChromaSubsampling,
        range: ColorRange,
        depth: BitDepth,
        matrix: ColorMatrix,
    ) -> VideoColorContract {
        VideoColorContract {
            chroma,
            range,
            depth,
            matrix,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }

    // ---- VideoUniform::from_contract: range bounds at each depth --------

    #[test]
    fn bt709_limited_eight_bit_matches_classic_16_235_240_bounds() {
        let c = contract(
            ChromaSubsampling::Yuv420,
            ColorRange::Limited,
            BitDepth::Eight,
            ColorMatrix::Bt709,
        );
        let u = VideoUniform::from_contract(c, (1920, 1080), (960, 540));
        assert_eq!(u.luma_offset, 16.0);
        assert_f32_near(1.0 / u.luma_span_inv, 219.0, "235 - 16");
        assert_eq!(u.chroma_center, 128.0, "(16 + 240) / 2");
        assert_f32_near(1.0 / u.chroma_span_inv, 224.0, "240 - 16");
        assert_eq!(u.mode, MODE_MATRIX);
    }

    #[test]
    fn limited_range_bounds_scale_linearly_with_depth() {
        let eight = contract(
            ChromaSubsampling::Yuv444,
            ColorRange::Limited,
            BitDepth::Eight,
            ColorMatrix::Bt709,
        );
        let ten = contract(
            ChromaSubsampling::Yuv444,
            ColorRange::Limited,
            BitDepth::Ten,
            ColorMatrix::Bt709,
        );
        let u8_bit = VideoUniform::from_contract(eight, (100, 100), (100, 100));
        let u10_bit = VideoUniform::from_contract(ten, (100, 100), (100, 100));
        assert_eq!(u10_bit.luma_offset, u8_bit.luma_offset * 4.0, "16<<2 = 64");
        assert_eq!(
            1.0 / u10_bit.luma_span_inv,
            (1.0 / u8_bit.luma_span_inv) * 4.0,
            "219<<2"
        );
        assert_eq!(u10_bit.chroma_center, u8_bit.chroma_center * 4.0);
        assert_eq!(
            1.0 / u10_bit.chroma_span_inv,
            (1.0 / u8_bit.chroma_span_inv) * 4.0
        );
    }

    #[test]
    fn full_range_chroma_centre_is_the_itu_midpoint_not_the_bounds_average() {
        let ten_bit_full = contract(
            ChromaSubsampling::Yuv444,
            ColorRange::Full,
            BitDepth::Ten,
            ColorMatrix::Bt709,
        );
        let u = VideoUniform::from_contract(ten_bit_full, (100, 100), (100, 100));
        // The naive mean of `chroma_bounds()` (0, 1023) is 511.5 -- the ITU
        // convention this must use instead is `1 << (10 - 1) = 512`.
        assert_eq!(u.chroma_center, 512.0);
        assert_eq!(1.0 / u.chroma_span_inv, 1023.0, "max_code at ten bits");
        assert_eq!(u.luma_offset, 0.0);
        assert_eq!(1.0 / u.luma_span_inv, 1023.0);
    }

    #[test]
    fn twelve_bit_full_range_uses_the_4095_code_space() {
        let c = contract(
            ChromaSubsampling::Yuv444,
            ColorRange::Full,
            BitDepth::Twelve,
            ColorMatrix::Bt2020Ncl,
        );
        let u = VideoUniform::from_contract(c, (100, 100), (100, 100));
        assert_f32_near(
            1.0 / u.luma_span_inv,
            4095.0,
            "full-range twelve-bit luma span",
        );
        assert_eq!(u.chroma_center, 2048.0, "1 << 11");
    }

    // ---- matrix coefficients ---------------------------------------------

    #[test]
    fn matrix_coefficients_match_the_itu_r_constants_per_matrix() {
        let bt709 = VideoUniform::from_contract(
            contract(
                ChromaSubsampling::Yuv444,
                ColorRange::Full,
                BitDepth::Eight,
                ColorMatrix::Bt709,
            ),
            (10, 10),
            (10, 10),
        );
        assert!((bt709.kr - 0.2126).abs() < 1e-6);
        assert!((bt709.kb - 0.0722).abs() < 1e-6);

        let bt601 = VideoUniform::from_contract(
            contract(
                ChromaSubsampling::Yuv444,
                ColorRange::Full,
                BitDepth::Eight,
                ColorMatrix::Bt601,
            ),
            (10, 10),
            (10, 10),
        );
        assert!((bt601.kr - 0.299).abs() < 1e-6);
        assert!((bt601.kb - 0.114).abs() < 1e-6);

        let bt2020 = VideoUniform::from_contract(
            contract(
                ChromaSubsampling::Yuv444,
                ColorRange::Full,
                BitDepth::Eight,
                ColorMatrix::Bt2020Ncl,
            ),
            (10, 10),
            (10, 10),
        );
        assert!((bt2020.kr - 0.2627).abs() < 1e-6);
        assert!((bt2020.kb - 0.0593).abs() < 1e-6);

        // Kg = 1 - Kr - Kb for BT.709: 1 - 0.2126 - 0.0722 = 0.7152.
        assert!((1.0 / bt709.kg_inv - 0.7152).abs() < 1e-6);
    }

    // ---- identity/GBR branch selection -----------------------------------

    #[test]
    fn identity_matrix_selects_the_gbr_branch_and_uses_only_luma_scaling() {
        let identity = contract(
            ChromaSubsampling::Yuv444,
            ColorRange::Full,
            BitDepth::Ten,
            ColorMatrix::Identity,
        );
        let u = VideoUniform::from_contract(identity, (100, 100), (100, 100));
        assert_eq!(u.mode, MODE_IDENTITY_GBR);
        // Kr/Kb are meaningless for identity but must still be finite/safe.
        assert!(u.kg_inv.is_finite() && u.kg_inv != 0.0);
    }

    #[test]
    fn non_identity_matrices_select_the_matrix_branch() {
        for matrix in [
            ColorMatrix::Bt709,
            ColorMatrix::Bt601,
            ColorMatrix::Bt2020Ncl,
        ] {
            let c = contract(
                ChromaSubsampling::Yuv444,
                ColorRange::Full,
                BitDepth::Eight,
                matrix,
            );
            let u = VideoUniform::from_contract(c, (10, 10), (10, 10));
            assert_eq!(u.mode, MODE_MATRIX, "{matrix:?}");
        }
    }

    #[test]
    fn packed_rgba_passthrough_always_selects_the_passthrough_mode() {
        let u = VideoUniform::passthrough(1920, 1080);
        assert_eq!(u.mode, MODE_PACKED_RGBA);
        assert_eq!(u.luma_width, 1920.0);
        assert_eq!(u.luma_height, 1080.0);
        assert_eq!(u.chroma_width, 1920.0);
        assert_eq!(u.chroma_height, 1080.0);
    }

    // ---- chroma/luma plane dimensions carried through --------------------

    #[test]
    fn subsampled_chroma_dimensions_are_carried_through_independently_of_luma() {
        let c = contract(
            ChromaSubsampling::Yuv420,
            ColorRange::Limited,
            BitDepth::Eight,
            ColorMatrix::Bt709,
        );
        let u = VideoUniform::from_contract(c, (1920, 1080), (960, 540));
        assert_eq!(u.luma_width, 1920.0);
        assert_eq!(u.luma_height, 1080.0);
        assert_eq!(u.chroma_width, 960.0);
        assert_eq!(u.chroma_height, 540.0);
    }

    // ---- uniform byte layout ----------------------------------------------

    #[test]
    fn to_bytes_is_48_bytes_and_round_trips_every_field_in_declaration_order() {
        let u = VideoUniform::from_contract(
            contract(
                ChromaSubsampling::Yuv444,
                ColorRange::Full,
                BitDepth::Ten,
                ColorMatrix::Bt2020Ncl,
            ),
            (3840, 2160),
            (3840, 2160),
        );
        let bytes = u.to_bytes();
        assert_eq!(bytes.len(), 48);
        let f32_at = |index: usize| -> f32 {
            f32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
        };
        assert_eq!(f32_at(0), u.luma_offset);
        assert_eq!(f32_at(1), u.luma_span_inv);
        assert_eq!(f32_at(2), u.chroma_center);
        assert_eq!(f32_at(3), u.chroma_span_inv);
        assert_eq!(f32_at(4), u.kr);
        assert_eq!(f32_at(5), u.kb);
        assert_eq!(f32_at(6), u.kg_inv);
        let mode = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
        assert_eq!(mode, u.mode);
        assert_eq!(f32_at(8), u.luma_width);
        assert_eq!(f32_at(9), u.luma_height);
        assert_eq!(f32_at(10), u.chroma_width);
        assert_eq!(f32_at(11), u.chroma_height);
    }

    // ---- chroma reconstruction (4:2:0 / 4:2:2) -----------------------------

    fn uniform_for(chroma: ChromaSubsampling, luma: (u32, u32)) -> VideoUniform {
        let chroma_size = match chroma {
            ChromaSubsampling::Yuv420 => (luma.0 / 2, luma.1 / 2),
            ChromaSubsampling::Yuv422 => (luma.0 / 2, luma.1),
            ChromaSubsampling::Yuv444 => luma,
        };
        VideoUniform::from_contract(
            contract(
                chroma,
                ColorRange::Limited,
                BitDepth::Eight,
                ColorMatrix::Bt709,
            ),
            luma,
            chroma_size,
        )
    }

    fn pixel_uv(x: u32, y: u32, luma: (u32, u32)) -> [f32; 2] {
        [
            (x as f32 + 0.5) / luma.0 as f32,
            (y as f32 + 0.5) / luma.1 as f32,
        ]
    }

    /// A two-colour edge: a dark background (luma 30, neutral chroma) meets
    /// a bright coloured stroke (luma 200, Cb 60) inside a 2x2 block, so the
    /// block that straddles the edge carries their average chroma.
    #[test]
    fn a_two_colour_edge_is_rebuilt_at_the_luma_edge() {
        let size = (8, 2);
        let u = uniform_for(ChromaSubsampling::Yuv420, size);
        // Luma columns 0..=2 dark, 3..=7 bright: the edge falls inside
        // chroma block 1 (luma columns 2 and 3).
        let luma = |x: u32, _y: u32| if x <= 2 { 30.0 } else { 200.0 };
        let chroma = |x: u32, _y: u32| match x {
            0 => [128.0, 128.0],
            1 => [94.0, 128.0], // half dark (128), half stroke (60)
            _ => [60.0, 128.0],
        };
        let cb: Vec<f32> = (0..8)
            .map(|x| u.chroma_reference_codes(pixel_uv(x, 0, size), luma(x, 0), luma, chroma)[0])
            .collect();
        // Nearest repeated 94 over columns 2 and 3: the dark pixel turned
        // blue-ish and the stroke pixel lost colour. The fit restores both.
        assert!(
            (cb[2] - 128.0).abs() < 4.0,
            "dark pixel keeps neutral chroma: {cb:?}"
        );
        assert!(
            (cb[3] - 60.0).abs() < 4.0,
            "stroke pixel keeps its colour: {cb:?}"
        );
        assert_eq!(cb[0], 128.0);
        assert_eq!(cb[7], 60.0);
    }

    #[test]
    fn chroma_unrelated_to_luma_keeps_each_blocks_own_sample() {
        // Flat luma: the fit explains nothing, so every pixel keeps exactly
        // the sample its block transmitted, as the nearest fetch did.
        let size = (4, 4);
        let u = uniform_for(ChromaSubsampling::Yuv420, size);
        let luma = |_x: u32, _y: u32| 100.0;
        let chroma = |x: u32, y: u32| [(x * 50 + y * 7) as f32, 128.0];
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(
                    u.chroma_reference_codes(pixel_uv(x, y, size), 100.0, luma, chroma),
                    chroma(x / 2, y / 2)
                );
            }
        }
    }

    #[test]
    fn four_four_four_chroma_keeps_the_exact_texel() {
        let u = uniform_for(ChromaSubsampling::Yuv444, (4, 4));
        let luma = |x: u32, _y: u32| (x * 60) as f32;
        for y in 0..4 {
            for x in 0..4 {
                let chroma = |cx: u32, cy: u32| [(cx * 10 + cy) as f32, 0.0];
                assert_eq!(
                    u.chroma_reference_codes(pixel_uv(x, y, (4, 4)), luma(x, y), luma, chroma)[0],
                    (x * 10 + y) as f32
                );
            }
        }
    }

    #[test]
    fn four_two_two_fits_along_rows_only() {
        let size = (8, 2);
        let u = uniform_for(ChromaSubsampling::Yuv422, size);
        let luma = |x: u32, _y: u32| if x <= 2 { 30.0 } else { 200.0 };
        let chroma = |x: u32, y: u32| match x {
            0 => [128.0, 100.0 + y as f32],
            1 => [94.0, 100.0 + y as f32],
            _ => [60.0, 100.0 + y as f32],
        };
        for y in 0..2 {
            let codes = u.chroma_reference_codes(pixel_uv(3, y, size), 200.0, luma, chroma);
            assert!((codes[0] - 60.0).abs() < 4.0);
            assert_eq!(
                codes[1],
                100.0 + y as f32,
                "rows keep their own exact sample"
            );
        }
    }
}
