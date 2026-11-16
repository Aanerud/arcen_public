// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Y′CbCr 4:4:4 refinement source (ADR 0016 §1, R1): a capture that is
//! already Y′CbCr when the encoder takes it — the macOS Pier's
//! `ScreenCaptureKit` `xf44`, after the HDR white stage — is refined in the
//! Y′CbCr domain. The codes are the encoder's input codes as they are;
//! nothing here converts, so there is no rounding to be exact about.
//!
//! [`YCbCr444Source`] keeps an exact mirror of every plane (compare and
//! copy, so its damage is exact per 16×16 block) and reads codes out of the
//! mirror only for the rectangles it extracts. It takes any three-plane
//! (`Y`, `Cb`, `Cr`) or two-plane (`Y`, interleaved `CbCr`) layout of
//! 16-bit little-endian samples holding 8 to 12 significant bits, aligned
//! at the top (`xf44`, `P410`-style) or at the bottom of the container.
//! Tiles are planar `Y′`, `Cb`, `Cr`, the order of the canonical digest's
//! planes 0, 1, 2.

use super::RefineSourceKind;
use super::source::{RefineSource, SourceMirror};
use super::tile::{RefineGrid, TileCodes, TileError, TileRect};
use crate::{ColorMatrix, ColorRange};
use arcen_protocol::refine::RefineDomain;

/// How a Y′CbCr 4:4:4 picture's planes are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YCbCr444Planes {
    /// Three planes, `Y`, `Cb`, `Cr`, one 16-bit sample per pixel each.
    Planar,
    /// Two planes: `Y` (one sample per pixel) and `Cb`,`Cr` interleaved
    /// (two samples per pixel, `Cb` first).
    Biplanar,
}

impl YCbCr444Planes {
    /// Number of planes.
    #[must_use]
    pub const fn count(self) -> usize {
        match self {
            Self::Planar => 3,
            Self::Biplanar => 2,
        }
    }

    /// Bytes per pixel of plane `plane`.
    #[must_use]
    pub const fn pixel_bytes(self, plane: usize) -> usize {
        match (self, plane) {
            (Self::Biplanar, 1) => 4,
            _ => 2,
        }
    }
}

/// The sample format of a Y′CbCr 4:4:4 source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YCbCr444Format {
    /// Plane layout.
    pub planes: YCbCr444Planes,
    /// Significant bits per sample (8..=12).
    pub depth: u8,
    /// The significant bits sit at the top of each 16-bit sample (the low
    /// bits are padding); otherwise at the bottom.
    pub msb_aligned: bool,
}

impl YCbCr444Format {
    /// `kCVPixelFormatType_444YpCbCr10BiPlanarFullRange` (`xf44`) and its
    /// video-range twin `x444`: two planes, ten bits at the top of 16.
    pub const BIPLANAR_TEN_MSB: Self = Self {
        planes: YCbCr444Planes::Biplanar,
        depth: 10,
        msb_aligned: true,
    };

    /// The code a 16-bit sample holds.
    #[must_use]
    pub const fn code(self, sample: u16) -> u16 {
        if self.msb_aligned {
            sample >> (16 - self.depth as u32)
        } else {
            sample & ((1 << self.depth as u32) - 1)
        }
    }

    const fn valid(self) -> bool {
        self.depth >= 8 && self.depth <= 12
    }
}

/// One plane of a captured picture: its bytes from the top-left pixel of
/// the observed or extracted rectangle, and the bytes between its rows.
#[derive(Debug, Clone, Copy)]
pub struct PlaneRef<'a> {
    /// Plane bytes.
    pub bytes: &'a [u8],
    /// Bytes per row.
    pub stride: usize,
}

fn sample(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// The codes of `rect` of a picture of `grid` whose planes are `planes`
/// (each starting at the picture's top-left pixel), planar `Y′`, `Cb`, `Cr`.
///
/// # Errors
///
/// [`TileError::Geometry`] for a rectangle outside the grid,
/// [`TileError::Source`] for a missing or short plane, [`TileError::Depth`]
/// for a depth outside 8..=12.
pub fn extract_ycbcr444(
    planes: &[PlaneRef<'_>],
    grid: RefineGrid,
    rect: TileRect,
    format: YCbCr444Format,
) -> Result<TileCodes, TileError> {
    if !format.valid() {
        return Err(TileError::Depth);
    }
    grid.check(rect)?;
    if planes.len() != format.planes.count() {
        return Err(TileError::Source);
    }
    for (index, plane) in planes.iter().enumerate() {
        let row = grid.width as usize * format.planes.pixel_bytes(index);
        let needed = plane
            .stride
            .checked_mul(grid.height as usize - 1)
            .and_then(|bytes| bytes.checked_add(row))
            .ok_or(TileError::Source)?;
        if plane.stride < row || plane.bytes.len() < needed {
            return Err(TileError::Source);
        }
    }
    let pixels = rect.pixels();
    let mut samples = vec![0_u16; pixels * 3];
    let (y_plane, chroma) = samples.split_at_mut(pixels);
    let (blue_plane, red_plane) = chroma.split_at_mut(pixels);
    let mut out = 0;
    for row in rect.y as usize..(rect.y + rect.height) as usize {
        for column in rect.x as usize..(rect.x + rect.width) as usize {
            let luma = &planes[0];
            y_plane[out] = format.code(sample(luma.bytes, row * luma.stride + column * 2));
            let (cb, cr) = match format.planes {
                YCbCr444Planes::Planar => (
                    sample(planes[1].bytes, row * planes[1].stride + column * 2),
                    sample(planes[2].bytes, row * planes[2].stride + column * 2),
                ),
                YCbCr444Planes::Biplanar => {
                    let at = row * planes[1].stride + column * 4;
                    (sample(planes[1].bytes, at), sample(planes[1].bytes, at + 2))
                }
            };
            blue_plane[out] = format.code(cb);
            red_plane[out] = format.code(cr);
            out += 1;
        }
    }
    TileCodes::new(rect.width, rect.height, samples)
}

/// A Y′CbCr 4:4:4 source: an exact mirror of each plane, read as codes
/// only when a rectangle is extracted.
#[derive(Debug, Clone)]
pub struct YCbCr444Source {
    mirrors: Vec<SourceMirror>,
    format: YCbCr444Format,
    matrix: ColorMatrix,
    range: ColorRange,
}

impl YCbCr444Source {
    /// A source of `grid` pixels in `format`, whose codes were made with
    /// `matrix` and `range` (the stream's own: the Deck composites the
    /// codes through the matrix and range it decodes the video with).
    #[must_use]
    pub fn new(
        grid: RefineGrid,
        format: YCbCr444Format,
        matrix: ColorMatrix,
        range: ColorRange,
    ) -> Self {
        Self {
            mirrors: (0..format.planes.count())
                .map(|plane| SourceMirror::with_pixel_bytes(grid, format.planes.pixel_bytes(plane)))
                .collect(),
            format,
            matrix,
            range,
        }
    }

    /// The sample format.
    #[must_use]
    pub const fn format(&self) -> YCbCr444Format {
        self.format
    }

    /// Observes `rect` of a captured picture given as its planes, each
    /// starting at the rectangle's top-left pixel; returns the blocks whose
    /// samples changed in any plane, ascending. A plane count that does not
    /// match the format observes nothing.
    pub fn observe_planes(&mut self, planes: &[PlaneRef<'_>], rect: TileRect) -> Vec<usize> {
        if planes.len() != self.mirrors.len() {
            return Vec::new();
        }
        let mut dirty: Vec<usize> = Vec::new();
        for (mirror, plane) in self.mirrors.iter_mut().zip(planes) {
            dirty.extend(mirror.observe_rect(plane.bytes, plane.stride, rect));
        }
        dirty.sort_unstable();
        dirty.dedup();
        dirty
    }

    /// Observes a whole picture given as its planes.
    pub fn observe_whole_planes(&mut self, planes: &[PlaneRef<'_>]) -> Vec<usize> {
        let grid = self.grid();
        self.observe_planes(
            planes,
            TileRect {
                x: 0,
                y: 0,
                width: grid.width,
                height: grid.height,
            },
        )
    }
}

impl RefineSource for YCbCr444Source {
    fn grid(&self) -> RefineGrid {
        self.mirrors[0].grid()
    }

    fn depth(&self) -> u8 {
        self.format.depth
    }

    fn domain(&self) -> RefineDomain {
        RefineDomain::YCbCr
    }

    fn kind(&self) -> RefineSourceKind {
        RefineSourceKind::CpuYCbCr444 {
            matrix: self.matrix,
            range: self.range,
        }
    }

    /// `src` holds the format's planes one after another, each `rect.height`
    /// rows of `stride` bytes (a picture copied into one buffer); a capture
    /// loop that has the planes apart calls [`Self::observe_planes`].
    fn observe(&mut self, src: &[u8], stride: usize, rect: TileRect) -> Vec<usize> {
        let Some(plane_bytes) = stride.checked_mul(rect.height as usize) else {
            return Vec::new();
        };
        let planes: Vec<PlaneRef<'_>> = (0..self.mirrors.len())
            .map_while(|plane| {
                let start = plane.checked_mul(plane_bytes)?;
                let bytes = src.get(start..start.checked_add(plane_bytes)?)?;
                Some(PlaneRef { bytes, stride })
            })
            .collect();
        self.observe_planes(&planes, rect)
    }

    fn ready(&self) -> bool {
        self.mirrors.iter().all(SourceMirror::is_valid)
    }

    fn extract(&self, rect: TileRect) -> Result<TileCodes, TileError> {
        let planes: Vec<PlaneRef<'_>> = self
            .mirrors
            .iter()
            .map(|mirror| PlaneRef {
                bytes: mirror.bytes(),
                stride: mirror.stride(),
            })
            .collect();
        extract_ycbcr444(&planes, self.grid(), rect, self.format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRID: RefineGrid = RefineGrid {
        width: 40,
        height: 20,
    };

    fn whole() -> TileRect {
        TileRect {
            x: 0,
            y: 0,
            width: 32,
            height: 16,
        }
    }

    /// `xf44`-shaped planes: every ten-bit code, MSB-aligned, with padded
    /// rows; returns the planes and the codes per pixel.
    fn xf44(seed: u16) -> (Vec<u8>, Vec<u8>, usize, usize, Vec<[u16; 3]>) {
        let (luma_stride, chroma_stride) = (40 * 2 + 16, 40 * 4 + 32);
        let mut luma = vec![0xEE_u8; luma_stride * 20];
        let mut chroma = vec![0xEE_u8; chroma_stride * 20];
        let mut codes = Vec::new();
        for y in 0..20 {
            for x in 0..40 {
                let pixel = (y * 40 + x) as u16;
                let code = [
                    (pixel * 3 + seed) % 1024,
                    (pixel * 7 + 512) % 1024,
                    (1023 - pixel) % 1024,
                ];
                let at = y * luma_stride + x * 2;
                luma[at..at + 2].copy_from_slice(&(code[0] << 6).to_le_bytes());
                let at = y * chroma_stride + x * 4;
                chroma[at..at + 2].copy_from_slice(&(code[1] << 6).to_le_bytes());
                chroma[at + 2..at + 4].copy_from_slice(&(code[2] << 6).to_le_bytes());
                codes.push(code);
            }
        }
        (luma, chroma, luma_stride, chroma_stride, codes)
    }

    fn source() -> YCbCr444Source {
        YCbCr444Source::new(
            GRID,
            YCbCr444Format::BIPLANAR_TEN_MSB,
            ColorMatrix::Bt709,
            ColorRange::Full,
        )
    }

    #[test]
    fn biplanar_ten_bit_tiles_are_the_codes_the_encoder_is_given() {
        let (luma, chroma, ls, cs, codes) = xf44(0);
        let mut source = source();
        let planes = [
            PlaneRef {
                bytes: &luma,
                stride: ls,
            },
            PlaneRef {
                bytes: &chroma,
                stride: cs,
            },
        ];
        assert_eq!(
            source.observe_whole_planes(&planes),
            (0..6).collect::<Vec<_>>()
        );
        assert!(source.ready());
        let tile = source.extract(whole()).expect("tile");
        for y in 0..16 {
            for x in 0..32 {
                let code = codes[(y * 40 + x) as usize];
                assert_eq!(
                    [tile.at(0, x, y), tile.at(1, x, y), tile.at(2, x, y)],
                    code,
                    "({x}, {y})"
                );
            }
        }
        // The right/bottom edge block is clipped, not padded.
        let edge = GRID.block_rect(5).expect("edge");
        assert_eq!((edge.width, edge.height), (8, 4));
        let tile = source.extract(edge).expect("edge tile");
        assert_eq!(tile.at(2, 7, 3), codes[(19 * 40 + 39) as usize][2]);
        assert_eq!(
            source.kind(),
            RefineSourceKind::CpuYCbCr444 {
                matrix: ColorMatrix::Bt709,
                range: ColorRange::Full
            }
        );
    }

    #[test]
    fn damage_is_exact_per_block_in_every_plane() {
        let (mut luma, mut chroma, ls, cs, _) = xf44(0);
        let mut source = source();
        let observe = |source: &mut YCbCr444Source, luma: &[u8], chroma: &[u8]| {
            source.observe_whole_planes(&[
                PlaneRef {
                    bytes: luma,
                    stride: ls,
                },
                PlaneRef {
                    bytes: chroma,
                    stride: cs,
                },
            ])
        };
        observe(&mut source, &luma, &chroma);
        assert!(observe(&mut source, &luma, &chroma).is_empty());
        // One Cr sample in block (2, 1); one luma sample in block (0, 0).
        chroma[17 * cs + 33 * 4 + 3] ^= 0x40;
        luma[2] ^= 0x40;
        assert_eq!(observe(&mut source, &luma, &chroma), vec![0, 5]);
        // Row padding is not picture: changing it damages nothing.
        luma[ls - 1] ^= 1;
        chroma[cs - 1] ^= 1;
        assert!(observe(&mut source, &luma, &chroma).is_empty());
    }

    #[test]
    fn planar_and_bottom_aligned_samples_and_short_planes() {
        let format = YCbCr444Format {
            planes: YCbCr444Planes::Planar,
            depth: 12,
            msb_aligned: false,
        };
        let grid = RefineGrid {
            width: 16,
            height: 16,
        };
        let plane = |value: u16| -> Vec<u8> {
            (0..256_u16)
                .flat_map(|pixel| ((value + pixel) | 0xF000).to_le_bytes())
                .collect()
        };
        let (y, cb, cr) = (plane(1), plane(2000), plane(4000));
        let planes = [
            PlaneRef {
                bytes: &y,
                stride: 32,
            },
            PlaneRef {
                bytes: &cb,
                stride: 32,
            },
            PlaneRef {
                bytes: &cr,
                stride: 32,
            },
        ];
        let rect = grid.block_rect(0).expect("block");
        let tile = extract_ycbcr444(&planes, grid, rect, format).expect("tile");
        assert_eq!(
            [tile.at(0, 3, 0), tile.at(1, 3, 0), tile.at(2, 3, 0)],
            [4, 2003, (4003 & 0xFFF)]
        );
        assert_eq!(
            extract_ycbcr444(&planes[..2], grid, rect, format),
            Err(TileError::Source)
        );
        assert_eq!(
            extract_ycbcr444(
                &[
                    PlaneRef {
                        bytes: &y[..100],
                        stride: 32
                    },
                    planes[1],
                    planes[2]
                ],
                grid,
                rect,
                format
            ),
            Err(TileError::Source)
        );
        let deep = YCbCr444Format {
            depth: 16,
            ..format
        };
        assert_eq!(
            extract_ycbcr444(&planes, grid, rect, deep),
            Err(TileError::Depth)
        );
        // The single-buffer form: planes one after another.
        let mut joined = y.clone();
        joined.extend_from_slice(&cb);
        joined.extend_from_slice(&cr);
        let mut source =
            YCbCr444Source::new(grid, format, ColorMatrix::Bt2020Ncl, ColorRange::Full);
        assert_eq!(source.observe(&joined, 32, rect), vec![0]);
        assert_eq!(source.extract(rect).expect("tile"), tile);
    }
}
