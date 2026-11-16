// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Pier-side seam of lossless refinement: a [`RefineSource`] is the only
//! thing a product implements to let a capture loop refine (ADR 0016).
//!
//! A source answers two questions about one monitor's captured picture:
//! *which 16×16 blocks changed* in what was just captured, and *what are the
//! exact codes* of a still rectangle **in the stream's own code values** —
//! the last integer stage before the encoder (ADR 0016 §1). Everything else
//! (scheduling, fences, generations, budget, compression, framing) is
//! [`super::producer::RefineProducer`]'s and the same for every source.
//!
//! The trait assumes neither ten bits nor RGB: [`RefineSource::depth`] and
//! [`RefineSource::domain`] travel in every tile header and in the digest.
//!
//! # Adding an eight-bit source (slice 2)
//!
//! An Auto capture that already lives in CPU memory as 8-bit RGB — Windows
//! DDA/WGC BGRA8, a Linux GPU readback of eligible blocks — implements this
//! trait with `depth() == 8`, `domain() == RefineDomain::Rgb`, and an
//! `extract` that widens each 8-bit sample into a `u16` code (no scaling: an
//! 8-bit code stays an 8-bit code). The plan rule that refuses
//! [`super::RefineOffReason::EightBit`] becomes a per-contract decision, and
//! the Deck's eight-bit presenter implements
//! [`super::hub::RefineCompositor`]. No record, digest or scheduler change
//! is needed. `NvFBC` and every other GPU-only capture stays
//! [`super::RefineSourceKind::GpuOnly`] and is never refined.

use super::BLOCK;
use super::RefineSourceKind;
use super::tile::{RefineGrid, TileCodes, TileError, TileRect, extract_packed_rgb10};
use crate::video::{PackedRgb10Layout, Rgb10Signal};
use arcen_protocol::refine::RefineDomain;

/// Exact source pixels of one monitor, in the stream's code values.
pub trait RefineSource: Send {
    /// The monitor's block grid (its source size).
    fn grid(&self) -> RefineGrid;
    /// Code depth of [`Self::extract`]'s samples (8..=12).
    fn depth(&self) -> u8;
    /// Tile domain.
    fn domain(&self) -> RefineDomain;
    /// Where the codes come from, for the shared plan: R′G′B′ sources are
    /// [`RefineSourceKind::CpuRgb`]; a Y′CbCr source names its matrix and
    /// range (the default names none, which the plan refuses).
    fn kind(&self) -> RefineSourceKind {
        match self.domain() {
            RefineDomain::Rgb => RefineSourceKind::CpuRgb,
            RefineDomain::YCbCr => RefineSourceKind::CpuYCbCr444 {
                matrix: crate::ColorMatrix::Identity,
                range: crate::ColorRange::Full,
            },
        }
    }
    /// Observes a captured rectangle whose top-left pixel is `src[0]` (rows
    /// `stride` bytes apart) and returns the indices of the blocks whose
    /// pixels changed, ascending. Damage must be **exact**: a block whose
    /// pixels changed must be reported, or a refined block could be shown
    /// over different content.
    fn observe(&mut self, src: &[u8], stride: usize, rect: TileRect) -> Vec<usize>;
    /// Whether a whole picture has been observed; nothing is extracted
    /// before.
    fn ready(&self) -> bool;
    /// The codes of `rect` (aligned to the grid, at most 256×64), as the
    /// encoder was given them, planar.
    ///
    /// # Errors
    ///
    /// [`TileError`] for a rectangle outside the grid or a short source.
    fn extract(&self, rect: TileRect) -> Result<TileCodes, TileError>;
}

/// An exact CPU copy of a source (4 bytes per pixel unless built with
/// [`Self::with_pixel_bytes`]), updated by compare-and-copy so the damage it
/// reports is exact per 16×16 block.
#[derive(Debug, Clone)]
pub struct SourceMirror {
    grid: RefineGrid,
    pixel_bytes: usize,
    bytes: Vec<u8>,
    valid: bool,
}

impl SourceMirror {
    /// An empty mirror for `grid` of a 4-byte-per-pixel source.
    #[must_use]
    pub fn new(grid: RefineGrid) -> Self {
        Self::with_pixel_bytes(grid, 4)
    }

    /// An empty mirror for `grid` of a source with `pixel_bytes` bytes per
    /// pixel (8 for FP16 RGBA), at least 1.
    #[must_use]
    pub fn with_pixel_bytes(grid: RefineGrid, pixel_bytes: usize) -> Self {
        let pixel_bytes = pixel_bytes.max(1);
        Self {
            grid,
            pixel_bytes,
            bytes: vec![0; grid.width as usize * grid.height as usize * pixel_bytes],
            valid: false,
        }
    }

    /// The mirror's grid.
    #[must_use]
    pub const fn grid(&self) -> RefineGrid {
        self.grid
    }

    /// Bytes per pixel.
    #[must_use]
    pub const fn pixel_bytes(&self) -> usize {
        self.pixel_bytes
    }

    /// Bytes per mirror row.
    #[must_use]
    pub fn stride(&self) -> usize {
        self.grid.width as usize * self.pixel_bytes
    }

    /// The mirror.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Copies a rectangle whose top-left pixel is `src[0]` (rows `stride`
    /// bytes apart) and returns the blocks whose bytes changed, ascending.
    /// The first observation marks every block it covers. A rectangle
    /// outside the source, or a source too short for it, observes nothing.
    pub fn observe_rect(&mut self, src: &[u8], stride: usize, rect: TileRect) -> Vec<usize> {
        let pixel = self.pixel_bytes;
        let (x, y, width, height) = (
            rect.x as usize,
            rect.y as usize,
            rect.width as usize,
            rect.height as usize,
        );
        if width == 0
            || height == 0
            || x + width > self.grid.width as usize
            || y + height > self.grid.height as usize
            || stride < width * pixel
            || src.len() < stride * (height - 1) + width * pixel
        {
            return Vec::new();
        }
        let first = !self.valid;
        let cols = self.grid.cols() as usize;
        let mut dirty = vec![false; self.grid.block_count() as usize];
        let mirror_stride = self.stride();
        for row in 0..height {
            let source_row = &src[row * stride..row * stride + width * pixel];
            let mirror_start = (y + row) * mirror_stride + x * pixel;
            // Most rows of a picture are unchanged: one comparison of the
            // whole row decides them before any per-block work.
            if !first && self.bytes[mirror_start..mirror_start + width * pixel] == *source_row {
                continue;
            }
            let block_row = (y + row) / BLOCK;
            let mut column = x;
            while column < x + width {
                let block_end = ((column / BLOCK) + 1) * BLOCK;
                let end = block_end.min(x + width);
                let from = (column - x) * pixel;
                let to = (end - x) * pixel;
                let mirror = &mut self.bytes[mirror_start + from..mirror_start + to];
                if first || mirror != &source_row[from..to] {
                    mirror.copy_from_slice(&source_row[from..to]);
                    dirty[block_row * cols + column / BLOCK] = true;
                }
                column = end;
            }
        }
        if first {
            self.valid = x == 0
                && y == 0
                && width == self.grid.width as usize
                && height == self.grid.height as usize;
        }
        dirty
            .iter()
            .enumerate()
            .filter_map(|(index, dirty)| dirty.then_some(index))
            .collect()
    }

    /// Observes a whole frame.
    pub fn observe_whole(&mut self, src: &[u8], stride: usize) -> Vec<usize> {
        let rect = TileRect {
            x: 0,
            y: 0,
            width: self.grid.width,
            height: self.grid.height,
        };
        self.observe_rect(src, stride, rect)
    }

    /// Whether a whole frame has been observed.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.valid
    }
}

/// The Linux Xorg depth-30 `XShm` source: packed 32-bit RGB10 words as the
/// root visual lays them out, mapped by the same [`Rgb10Signal`] stage the
/// encoder's conversion applies before its matrix.
#[derive(Debug, Clone)]
pub struct PackedRgb10Source {
    mirror: SourceMirror,
    layout: PackedRgb10Layout,
    signal: Rgb10Signal,
}

impl PackedRgb10Source {
    /// A source of `grid` pixels in `layout`, carrying `signal`'s codes.
    #[must_use]
    pub fn new(grid: RefineGrid, layout: PackedRgb10Layout, signal: Rgb10Signal) -> Self {
        Self {
            mirror: SourceMirror::new(grid),
            layout,
            signal,
        }
    }
}

impl RefineSource for PackedRgb10Source {
    fn grid(&self) -> RefineGrid {
        self.mirror.grid
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
        self.mirror.is_valid()
    }

    fn extract(&self, rect: TileRect) -> Result<TileCodes, TileError> {
        extract_packed_rgb10(
            self.mirror.bytes(),
            self.mirror.stride(),
            self.mirror.grid,
            rect,
            self.layout,
            self.signal,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> RefineGrid {
        RefineGrid {
            width: 32,
            height: 16,
        }
    }

    fn whole() -> TileRect {
        TileRect {
            x: 0,
            y: 0,
            width: 32,
            height: 16,
        }
    }

    /// Every ten-bit code once per channel, rotated so each pixel differs.
    fn pq_words(layout: PackedRgb10Layout) -> (Vec<u8>, Vec<[u16; 3]>) {
        let mut bytes = Vec::new();
        let mut codes = Vec::new();
        for pixel in 0..32 * 16_u16 {
            let rgb = [
                pixel * 2 % 1024,
                (pixel * 2 + 341) % 1024,
                (1023 - pixel * 2) % 1024,
            ];
            let word = if layout == PackedRgb10Layout::XRGB2101010 {
                (u32::from(rgb[0]) << 20) | (u32::from(rgb[1]) << 10) | u32::from(rgb[2])
            } else {
                (u32::from(rgb[2]) << 20) | (u32::from(rgb[1]) << 10) | u32::from(rgb[0])
            };
            bytes.extend_from_slice(&word.to_le_bytes());
            codes.push(rgb);
        }
        (bytes, codes)
    }

    fn extracted(source: &PackedRgb10Source) -> Vec<[u16; 3]> {
        let codes = source.extract(whole()).expect("tile");
        (0..16)
            .flat_map(|y| (0..32).map(move |x| (x, y)))
            .map(|(x, y)| [codes.at(0, x, y), codes.at(1, x, y), codes.at(2, x, y)])
            .collect()
    }

    #[test]
    fn hdr_tiles_carry_the_pq_codes_the_encoder_is_given() {
        // The Wayland HDR read-back (xBGR words, no conversion before the
        // matrix): tiles are the PQ codes themselves.
        for layout in [
            PackedRgb10Layout::XBGR2101010,
            PackedRgb10Layout::XRGB2101010,
        ] {
            let (bytes, codes) = pq_words(layout);
            let mut source = PackedRgb10Source::new(grid(), layout, Rgb10Signal::Direct);
            assert_eq!(source.observe(&bytes, 128, whole()), vec![0, 1]);
            assert!(source.ready());
            assert_eq!(extracted(&source), codes, "{layout:?}");
        }
        // Grading from the same PQ composition: the codes after the
        // encoder's own SDR conversion, never the PQ ones.
        let (bytes, codes) = pq_words(PackedRgb10Layout::XBGR2101010);
        let signal = Rgb10Signal::PqBt2020ToSdrGamma22;
        let mut source = PackedRgb10Source::new(grid(), PackedRgb10Layout::XBGR2101010, signal);
        source.observe(&bytes, 128, whole());
        let expected: Vec<[u16; 3]> = codes.into_iter().map(|rgb| signal.map(rgb)).collect();
        assert_eq!(extracted(&source), expected);
    }

    #[test]
    fn half_float_tiles_are_the_normalised_codes() {
        // The Wayland half-float route normalises once, by the shared
        // conversion, into the words the encoder stages; the source mirrors
        // those words, so a tile is the code the encoder saw.
        let mut half = Vec::new();
        let mut expected = Vec::new();
        for pixel in 0..32 * 16_u16 {
            // Halves from 0.0 to just under 1.0 (0x0000..0x3bff), and one
            // above SDR range.
            let rgb = [pixel * 29 % 0x3c00, pixel * 7 % 0x3c00, 0x3c00 + pixel % 3];
            for channel in rgb {
                half.extend_from_slice(&channel.to_le_bytes());
            }
            half.extend_from_slice(&0x3c00_u16.to_le_bytes());
            expected.push(rgb);
        }
        for signal in [Rgb10Signal::Direct, Rgb10Signal::PqBt2020ToSdrSrgb] {
            let mut packed = vec![0_u8; 32 * 16 * 4];
            crate::video::convert_rgba_f16_pq_to_packed_rgb10(
                &half,
                32 * 8,
                &mut packed,
                128,
                32,
                16,
                signal,
            )
            .expect("convert");
            let mut source =
                PackedRgb10Source::new(grid(), PackedRgb10Layout::XRGB2101010, Rgb10Signal::Direct);
            source.observe(&packed, 128, whole());
            let want: Vec<[u16; 3]> = expected
                .iter()
                .map(|rgb| signal.map_pq_half(*rgb))
                .collect();
            assert_eq!(extracted(&source), want, "{signal:?}");
        }
    }

    #[test]
    fn the_mirror_reports_exact_block_damage() {
        let grid = RefineGrid {
            width: 40,
            height: 20,
        };
        let mut mirror = SourceMirror::new(grid);
        let mut frame = vec![7_u8; 40 * 20 * 4];
        assert_eq!(mirror.observe_whole(&frame, 160).len(), 6);
        assert!(mirror.observe_whole(&frame, 160).is_empty());
        // One pixel in block (2, 1).
        frame[(17 * 40 + 33) * 4] = 9;
        assert_eq!(mirror.observe_whole(&frame, 160), vec![5]);
        // A region read: only its blocks are compared, and a block the
        // region touches but did not change is not damaged.
        frame[(18 * 40 + 31) * 4] = 1;
        let rect = TileRect {
            x: 30,
            y: 16,
            width: 4,
            height: 4,
        };
        let src = frame[(16 * 40 + 30) * 4..].to_vec();
        assert_eq!(mirror.observe_rect(&src, 160, rect), vec![4]);
        assert_eq!(&mirror.bytes()[(18 * 40 + 31) * 4], &1);
    }
}
