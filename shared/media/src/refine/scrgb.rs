// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The FP16 scRGB refinement source (ADR 0016 §1, Windows Grading and HDR).
//!
//! Windows captures every ten-bit contract as WGC `R16G16B16A16Float`
//! linear scRGB. Half-floats have no integer truth, so the exact codes of a
//! still block are the stream's own quantisation of them: the R′G′B′
//! ten-bit codes the shared conversion hands the matrix
//! ([`ScrgbConversion::rgb_codes`], the very conversion the encoder converts
//! the video with, so a tile and the video around it come from one value).
//! [`ScrgbHalfSource`] mirrors the half-floats exactly (compare and copy, 8
//! bytes per pixel) and converts only the rectangles it extracts, so a still
//! picture costs one conversion per tile, never one per frame.
//!
//! A capture whose pixels reach the CPU only through a readback that lags the
//! encoder (a GPU conversion path) cannot use the mirror's own damage for the
//! access unit being sent: the mirror sees a change after the Deck has it.
//! Such a loop reports the capture API's dirty regions instead
//! ([`super::runtime::RefineRuntime::observe_blocks`]), marks the source
//! stale until the readback lands ([`ScrgbHalfSource::set_stale`]), and lets
//! [`DamageWitness`] check every readback against what was reported: a
//! change the regions missed means they are not trustworthy here.

use super::source::{RefineSource, SourceMirror};
use super::tile::{RefineGrid, TileCodes, TileError, TileRect};
use crate::video::ScrgbConversion;
use arcen_protocol::refine::RefineDomain;

/// Bytes of one FP16 RGBA pixel.
pub const SCRGB_PIXEL_BYTES: usize = 8;

/// The codes of `rect` of an FP16 RGBA frame whose rows are `stride` bytes
/// apart, planar R, G, B.
///
/// # Errors
///
/// [`TileError`] for a rectangle outside the grid or a short source.
pub fn extract_scrgb(
    src: &[u8],
    stride: usize,
    grid: RefineGrid,
    rect: TileRect,
    codes: ScrgbConversion,
) -> Result<TileCodes, TileError> {
    grid.check(rect)?;
    let needed = stride
        .checked_mul(grid.height as usize)
        .ok_or(TileError::Source)?;
    if stride < grid.width as usize * SCRGB_PIXEL_BYTES || src.len() < needed {
        return Err(TileError::Source);
    }
    let pixels = rect.pixels();
    let mut samples = vec![0_u16; pixels * 3];
    let (r_plane, g_and_b) = samples.split_at_mut(pixels);
    let (g_plane, b_plane) = g_and_b.split_at_mut(pixels);
    let mut out = 0;
    for row in rect.y..rect.y + rect.height {
        let row_start = row as usize * stride + rect.x as usize * SCRGB_PIXEL_BYTES;
        let row_bytes = &src[row_start..row_start + rect.width as usize * SCRGB_PIXEL_BYTES];
        for pixel in row_bytes.chunks_exact(SCRGB_PIXEL_BYTES) {
            let half = |at: usize| u16::from_le_bytes([pixel[at], pixel[at + 1]]);
            // RGBA order; alpha is ignored, as the encoder conversion does.
            let [r, g, b] = codes.rgb_codes(half(0), half(2), half(4));
            r_plane[out] = r;
            g_plane[out] = g;
            b_plane[out] = b;
            out += 1;
        }
    }
    TileCodes::new(rect.width, rect.height, samples)
}

/// The Windows FP16 scRGB source: an exact half-float mirror, converted to
/// the stream's R′G′B′ codes on extraction.
#[derive(Debug, Clone)]
pub struct ScrgbHalfSource {
    mirror: SourceMirror,
    codes: ScrgbConversion,
    stale: bool,
}

impl ScrgbHalfSource {
    /// A source of `grid` pixels carrying `codes`' codes.
    #[must_use]
    pub fn new(grid: RefineGrid, codes: ScrgbConversion) -> Self {
        Self {
            mirror: SourceMirror::with_pixel_bytes(grid, SCRGB_PIXEL_BYTES),
            codes,
            stale: false,
        }
    }

    /// The capture holds frames the mirror has not seen yet (a readback is
    /// pending): nothing is extracted until it lands.
    pub fn set_stale(&mut self, stale: bool) {
        self.stale = stale;
    }

    /// Whether the mirror lags the capture.
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        self.stale
    }
}

impl RefineSource for ScrgbHalfSource {
    fn grid(&self) -> RefineGrid {
        self.mirror.grid()
    }

    fn depth(&self) -> u8 {
        10
    }

    fn domain(&self) -> RefineDomain {
        RefineDomain::Rgb
    }

    fn observe(&mut self, src: &[u8], stride: usize, rect: TileRect) -> Vec<usize> {
        self.mirror.observe_rect(src, stride, rect)
    }

    fn ready(&self) -> bool {
        self.mirror.is_valid() && !self.stale
    }

    fn extract(&self, rect: TileRect) -> Result<TileCodes, TileError> {
        extract_scrgb(
            self.mirror.bytes(),
            self.mirror.stride(),
            self.mirror.grid(),
            rect,
            self.codes,
        )
    }
}

/// What [`DamageWitness::finish_readback`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WitnessVerdict {
    /// Every block the readback changed had been reported.
    Trusted,
    /// The first readback: nothing to compare yet.
    First,
    /// `unreported` changed blocks had not been reported: the reported
    /// damage is not exact on this host.
    Untrusted {
        /// Changed blocks the reports missed.
        unreported: usize,
    },
}

/// Checks reported damage (a capture API's dirty regions) against the exact
/// mirror, one readback at a time.
///
/// Reports accumulate until a readback starts; that readback's mirror damage
/// must lie inside them. Reports that arrive while it is in flight belong to
/// the next one.
#[derive(Debug, Clone)]
pub struct DamageWitness {
    reported: Vec<bool>,
    checking: Option<Vec<bool>>,
    readbacks: u64,
    untrusted: u64,
}

impl DamageWitness {
    /// A witness over `grid`.
    #[must_use]
    pub fn new(grid: RefineGrid) -> Self {
        Self {
            reported: vec![false; grid.block_count() as usize],
            checking: None,
            readbacks: 0,
            untrusted: 0,
        }
    }

    /// Damage reported for a frame the capture delivered.
    pub fn note_reported(&mut self, blocks: impl IntoIterator<Item = usize>) {
        for block in blocks {
            if let Some(slot) = self.reported.get_mut(block) {
                *slot = true;
            }
        }
    }

    /// Whether a readback is in flight.
    #[must_use]
    pub const fn in_flight(&self) -> bool {
        self.checking.is_some()
    }

    /// A readback of everything reported so far started.
    pub fn begin_readback(&mut self) {
        let fresh = vec![false; self.reported.len()];
        self.checking = Some(std::mem::replace(&mut self.reported, fresh));
    }

    /// The readback landed and changed `mirror_damage` (block indices).
    pub fn finish_readback(&mut self, mirror_damage: &[usize]) -> WitnessVerdict {
        let Some(checking) = self.checking.take() else {
            return WitnessVerdict::Trusted;
        };
        self.readbacks += 1;
        if self.readbacks == 1 {
            return WitnessVerdict::First;
        }
        let unreported = mirror_damage
            .iter()
            .filter(|block| !checking.get(**block).copied().unwrap_or(false))
            .count();
        if unreported == 0 {
            WitnessVerdict::Trusted
        } else {
            self.untrusted += 1;
            WitnessVerdict::Untrusted { unreported }
        }
    }

    /// The readback was abandoned (its copy failed): its reports return to
    /// the next one.
    pub fn abandon_readback(&mut self) {
        if let Some(checking) = self.checking.take() {
            for (slot, was) in self.reported.iter_mut().zip(checking) {
                *slot |= was;
            }
        }
    }

    /// Readbacks completed.
    #[must_use]
    pub const fn readbacks(&self) -> u64 {
        self.readbacks
    }

    /// Readbacks that found unreported damage.
    #[must_use]
    pub const fn untrusted(&self) -> u64 {
        self.untrusted
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "test fixtures build small half-floats and pixel coordinates"
)]
mod tests {
    use super::*;
    use crate::video::{ColorTransform, ScrgbPqTransform, ScrgbSdrTransform};
    use crate::{BitDepth, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics};

    fn half(value: f32) -> u16 {
        // Exact for the test values (small dyadic fractions and integers).
        let bits = value.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        if value == 0.0 {
            return sign;
        }
        let exponent = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
        let mantissa = ((bits >> 13) & 0x3FF) as u16;
        sign | ((exponent as u16) << 10) | mantissa
    }

    /// A `width × height` FP16 frame of a smooth ramp with HDR highlights.
    fn frame(width: usize, height: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(width * height * 8);
        for y in 0..height {
            for x in 0..width {
                let r = (x as f32) / 64.0;
                let g = (y as f32) / 32.0;
                let b = ((x + y) % 7) as f32 / 4.0;
                for value in [r, g, b, 1.0] {
                    bytes.extend_from_slice(&half(value).to_le_bytes());
                }
            }
        }
        bytes
    }

    fn sdr() -> ScrgbSdrTransform {
        ScrgbSdrTransform::new(
            ColorMatrix::Bt709,
            ColorRange::Full,
            BitDepth::Ten,
            TransferCharacteristics::Srgb,
        )
        .expect("SDR transform")
    }

    fn pq() -> ScrgbPqTransform {
        ScrgbPqTransform::new(
            ColorMatrix::Bt2020Ncl,
            ColorPrimaries::Bt2020,
            ColorRange::Full,
            BitDepth::Ten,
            Some(240.0),
        )
    }

    #[test]
    fn extracted_codes_are_the_codes_the_encoder_conversion_gave_its_matrix() {
        let (width, height) = (40_usize, 20_usize);
        let bytes = frame(width, height);
        let grid = RefineGrid {
            width: 40,
            height: 20,
        };
        let rect = TileRect {
            x: 16,
            y: 0,
            width: 24,
            height: 20,
        };
        let cases = [
            (ScrgbConversion::Sdr(sdr()), ColorMatrix::Bt709),
            (ScrgbConversion::Pq(pq()), ColorMatrix::Bt2020Ncl),
        ];
        for (codes, matrix) in cases {
            let mut luma = vec![0_u16; width * height];
            let mut cb = vec![0_u16; width * height];
            let mut cr = vec![0_u16; width * height];
            codes
                .convert_i444_p16(
                    &bytes,
                    width * 8,
                    [&mut luma, &mut cb, &mut cr],
                    [width; 3],
                    width,
                    height,
                )
                .expect("encoder conversion");
            // The encoder's matrix, as the conversions build it.
            let ycbcr = ColorTransform::for_input_max(
                matrix,
                ColorRange::Full,
                BitDepth::Ten,
                f64::from(crate::video::WIDE_INPUT_MAX),
            );
            let tile = extract_scrgb(&bytes, width * 8, grid, rect, codes).expect("tile");
            for row in 0..rect.height as usize {
                for column in 0..rect.width as usize {
                    let at = |plane| tile.at(plane, column as u32, row as u32);
                    let [r, g, b] = [at(0), at(1), at(2)];
                    assert!([r, g, b].iter().all(|code| *code <= 1023));
                    let index = row * width + rect.x as usize + column;
                    assert_eq!(
                        [luma[index], cb[index], cr[index]],
                        [
                            ycbcr.pack_p16(ycbcr.luma_wide(b, g, r)),
                            ycbcr.pack_p16(ycbcr.cb_wide(b, g, r)),
                            ycbcr.pack_p16(ycbcr.cr_wide(b, g, r)),
                        ],
                        "pixel ({column}, {row}) of {codes:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn rgb_codes_feed_the_matrix_unchanged() {
        // Grading in sRGB keeps a desktop's code values: scRGB 1.0 is white.
        let white = half(1.0);
        assert_eq!(sdr().rgb_codes(white, white, white), [1023; 3]);
        assert_eq!(sdr().rgb_codes(0, 0, 0), [0; 3]);
        // Above SDR white is clamped in Grading but survives PQ.
        let bright = half(4.0);
        assert_eq!(sdr().rgb_codes(bright, 0, 0)[0], 1023);
        let pq_white = pq().rgb_codes(white, white, white);
        let pq_bright = pq().rgb_codes(bright, bright, bright);
        assert!(pq_bright[0] > pq_white[0], "{pq_white:?} {pq_bright:?}");
        assert!(pq_white[0] < 1023);
    }

    #[test]
    fn the_half_float_mirror_reports_exact_damage_and_waits_while_stale() {
        let grid = RefineGrid {
            width: 40,
            height: 20,
        };
        let mut source = ScrgbHalfSource::new(grid, ScrgbConversion::Sdr(sdr()));
        let mut bytes = frame(40, 20);
        let whole = TileRect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        assert!(!source.ready());
        assert_eq!(source.observe(&bytes, 320, whole).len(), 6);
        assert!(source.ready());
        assert!(source.observe(&bytes, 320, whole).is_empty());
        // One half-float byte in block (2, 1) — only its high byte changes.
        bytes[(17 * 40 + 33) * 8 + 1] ^= 0x01;
        assert_eq!(source.observe(&bytes, 320, whole), vec![5]);
        source.set_stale(true);
        assert!(!source.ready());
        source.set_stale(false);
        let tile = source
            .extract(TileRect {
                x: 32,
                y: 16,
                width: 8,
                height: 4,
            })
            .expect("tile");
        assert_eq!(tile.width(), 8);
    }

    #[test]
    fn the_witness_trusts_only_reports_that_cover_the_mirror_damage() {
        let grid = RefineGrid {
            width: 64,
            height: 16,
        };
        let mut witness = DamageWitness::new(grid);
        witness.begin_readback();
        assert_eq!(
            witness.finish_readback(&[0, 1, 2, 3]),
            WitnessVerdict::First
        );
        witness.note_reported([1, 2]);
        witness.begin_readback();
        // A report during the readback belongs to the next one.
        witness.note_reported([3]);
        assert!(witness.in_flight());
        assert_eq!(witness.finish_readback(&[1, 2]), WitnessVerdict::Trusted);
        witness.begin_readback();
        assert_eq!(witness.finish_readback(&[3]), WitnessVerdict::Trusted);
        witness.begin_readback();
        assert_eq!(
            witness.finish_readback(&[0]),
            WitnessVerdict::Untrusted { unreported: 1 }
        );
        assert_eq!(witness.untrusted(), 1);
        // An abandoned readback hands its reports to the next one.
        witness.note_reported([2]);
        witness.begin_readback();
        witness.abandon_readback();
        witness.begin_readback();
        assert_eq!(witness.finish_readback(&[2]), WitnessVerdict::Trusted);
        assert_eq!(witness.readbacks(), 5);
    }
}
