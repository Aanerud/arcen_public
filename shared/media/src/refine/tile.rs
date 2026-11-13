// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Tile geometry, extraction from packed RGB10 frames, and the codec-free
//! residual transform of `delta-zstd1` (ADR 0016 §6): per plane, the
//! residual against the left neighbour (the pixel above for a row's first
//! pixel, zero for the tile's first), modulo `2^depth`, zigzag-folded, then
//! split into a low-byte plane and a high-byte plane.

use super::BLOCK;
use crate::video::{PackedRgb10Layout, Rgb10Signal};
use arcen_protocol::refine::{MAX_TILE_EXPANDED_BYTES, MAX_TILE_HEIGHT, MAX_TILE_WIDTH};

/// [`BLOCK`] as `u32`.
pub const BLOCK_U32: u32 = 16;
const _: () = assert!(BLOCK_U32 as usize == BLOCK);

/// Largest tile in blocks across.
pub const MAX_TILE_BLOCKS_WIDE: usize = MAX_TILE_WIDTH as usize / BLOCK;
/// Largest tile in blocks down.
pub const MAX_TILE_BLOCKS_TALL: usize = MAX_TILE_HEIGHT as usize / BLOCK;
const MAX_TILE_BLOCKS_WIDE_U32: u32 = MAX_TILE_WIDTH as u32 / BLOCK_U32;
const MAX_TILE_BLOCKS_TALL_U32: u32 = MAX_TILE_HEIGHT as u32 / BLOCK_U32;

/// Why a tile operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileError {
    /// The rectangle leaves the source or breaks the grid rules.
    Geometry,
    /// The source buffer is smaller than its stated geometry.
    Source,
    /// Expanded bytes disagree with the rectangle or exceed the limit.
    Size,
    /// Depth outside 8..=12.
    Depth,
}

/// Block grid of one monitor's source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RefineGrid {
    /// Source width in pixels.
    pub width: u32,
    /// Source height in pixels.
    pub height: u32,
}

impl RefineGrid {
    /// Blocks across.
    #[must_use]
    pub fn cols(self) -> u32 {
        self.width.div_ceil(BLOCK_U32)
    }

    /// Blocks down.
    #[must_use]
    pub fn rows(self) -> u32 {
        self.height.div_ceil(BLOCK_U32)
    }

    /// Blocks in the grid.
    #[must_use]
    pub fn block_count(self) -> u32 {
        self.cols().saturating_mul(self.rows())
    }

    /// Pixel rectangle of block `index`, clipped to the source.
    #[must_use]
    pub fn block_rect(self, index: u32) -> Option<TileRect> {
        if index >= self.block_count() {
            return None;
        }
        let bx = index % self.cols();
        let by = index / self.cols();
        let x = bx * BLOCK_U32;
        let y = by * BLOCK_U32;
        Some(TileRect {
            x,
            y,
            width: (self.width - x).min(BLOCK_U32),
            height: (self.height - y).min(BLOCK_U32),
        })
    }

    /// Checks a tile rectangle: grid-aligned, within the per-tile limits,
    /// inside the source, and only short of whole blocks at the source's
    /// right/bottom edge.
    ///
    /// # Errors
    ///
    /// [`TileError::Geometry`].
    pub fn check(self, rect: TileRect) -> Result<(), TileError> {
        let block = BLOCK_U32;
        let right = rect.x.checked_add(rect.width).ok_or(TileError::Geometry)?;
        let bottom = rect.y.checked_add(rect.height).ok_or(TileError::Geometry)?;
        let aligned_or_edge = |end: u32, len: u32, limit: u32| len % block == 0 || end == limit;
        if rect.x % block != 0
            || rect.y % block != 0
            || rect.width == 0
            || rect.height == 0
            || rect.width > u32::from(MAX_TILE_WIDTH)
            || rect.height > u32::from(MAX_TILE_HEIGHT)
            || right > self.width
            || bottom > self.height
            || !aligned_or_edge(right, rect.width, self.width)
            || !aligned_or_edge(bottom, rect.height, self.height)
        {
            return Err(TileError::Geometry);
        }
        Ok(())
    }

    /// Block indices a rectangle covers, ascending.
    #[must_use]
    pub fn blocks_of(self, rect: TileRect) -> Vec<u32> {
        let block = BLOCK_U32;
        let (x0, y0) = (rect.x / block, rect.y / block);
        let x1 = (rect.x + rect.width).div_ceil(block);
        let y1 = (rect.y + rect.height).div_ceil(block);
        let mut out = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
        for by in y0..y1 {
            for bx in x0..x1 {
                out.push(by * self.cols() + bx);
            }
        }
        out
    }
}

/// A rectangle in visible source pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileRect {
    /// Left edge (multiple of 16).
    pub x: u32,
    /// Top edge (multiple of 16).
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl TileRect {
    /// Pixels in the rectangle.
    #[must_use]
    pub fn pixels(self) -> usize {
        self.width as usize * self.height as usize
    }

    /// Expanded bytes (three `u16` planes).
    #[must_use]
    pub fn expanded_bytes(self) -> usize {
        self.pixels() * 6
    }
}

/// Planar `u16` codes of one tile: plane 0, 1, 2 (R′, G′, B′), row-major,
/// codes right-aligned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileCodes {
    width: u32,
    height: u32,
    samples: Vec<u16>,
}

impl TileCodes {
    /// Wraps planar samples.
    ///
    /// # Errors
    ///
    /// [`TileError::Size`] when `samples` is not `3 × width × height`.
    pub fn new(width: u32, height: u32, samples: Vec<u16>) -> Result<Self, TileError> {
        if samples.len() != width as usize * height as usize * 3 {
            return Err(TileError::Size);
        }
        Ok(Self {
            width,
            height,
            samples,
        })
    }

    /// Width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// One plane.
    #[must_use]
    pub fn plane(&self, plane: usize) -> &[u16] {
        let len = self.width as usize * self.height as usize;
        &self.samples[plane * len..(plane + 1) * len]
    }

    /// Code at `(x, y)` in `plane`.
    #[must_use]
    pub fn at(&self, plane: usize, x: u32, y: u32) -> u16 {
        self.plane(plane)[(y * self.width + x) as usize]
    }

    /// The three codes of a sub-rectangle `(x, y, w, h)` (tile-relative),
    /// planar, as the canonical block layout wants them.
    #[must_use]
    pub fn sub_planes(&self, x: u32, y: u32, width: u32, height: u32) -> Vec<u16> {
        let mut out = Vec::with_capacity(width as usize * height as usize * 3);
        for plane in 0..3 {
            for row in y..y + height {
                let start = (row * self.width + x) as usize;
                out.extend_from_slice(&self.plane(plane)[start..start + width as usize]);
            }
        }
        out
    }
}

/// Copies `rect` out of a packed 32-bit RGB10 frame (`stride` bytes per row,
/// little-endian words) into planar R′G′B′ codes, applying `signal` — the
/// same per-pixel stage the encoder's conversion applies before its matrix,
/// so tiles carry exactly the codes the stream describes.
///
/// # Errors
///
/// [`TileError::Geometry`] or [`TileError::Source`].
pub fn extract_packed_rgb10(
    src: &[u8],
    stride: usize,
    grid: RefineGrid,
    rect: TileRect,
    layout: PackedRgb10Layout,
    signal: Rgb10Signal,
) -> Result<TileCodes, TileError> {
    grid.check(rect)?;
    let needed = stride
        .checked_mul(grid.height as usize)
        .ok_or(TileError::Source)?;
    if stride < grid.width as usize * 4 || src.len() < needed {
        return Err(TileError::Source);
    }
    let pixels = rect.pixels();
    let mut samples = vec![0_u16; pixels * 3];
    let (r_plane, g_and_b) = samples.split_at_mut(pixels);
    let (g_plane, b_plane) = g_and_b.split_at_mut(pixels);
    let mut out = 0;
    for row in rect.y..rect.y + rect.height {
        let row_start = row as usize * stride + rect.x as usize * 4;
        let row_bytes = &src[row_start..row_start + rect.width as usize * 4];
        for word in row_bytes.chunks_exact(4) {
            let word = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            let [r, g, b] = signal.map(layout.components(word));
            r_plane[out] = r;
            g_plane[out] = g;
            b_plane[out] = b;
            out += 1;
        }
    }
    TileCodes::new(rect.width, rect.height, samples)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "residuals lie in -2^11..2^11, so the fold fits 12 bits"
)]
const fn zigzag(residual: i32) -> u16 {
    ((residual << 1) ^ (residual >> 31)) as u16
}

const fn unzigzag(value: u16) -> i32 {
    let value = value as i32;
    (value >> 1) ^ -(value & 1)
}

fn check_depth(depth: u8) -> Result<(), TileError> {
    if (8..=12).contains(&depth) {
        Ok(())
    } else {
        Err(TileError::Depth)
    }
}

/// Applies the residual transform: `6 × width × height` bytes, plane by
/// plane, low bytes then high bytes.
///
/// # Errors
///
/// [`TileError::Depth`] or [`TileError::Size`].
pub fn encode_residuals(codes: &TileCodes, depth: u8) -> Result<Vec<u8>, TileError> {
    check_depth(depth)?;
    let (width, height) = (codes.width as usize, codes.height as usize);
    let pixels = width * height;
    if pixels * 6 > MAX_TILE_EXPANDED_BYTES {
        return Err(TileError::Size);
    }
    let modulus = 1_i32 << depth;
    let half = modulus >> 1;
    let mut out = vec![0_u8; pixels * 6];
    for plane in 0..3 {
        let samples = codes.plane(plane);
        let (low, high) = out[plane * pixels * 2..(plane + 1) * pixels * 2].split_at_mut(pixels);
        for y in 0..height {
            for x in 0..width {
                let at = y * width + x;
                let prediction = if x > 0 {
                    samples[at - 1]
                } else if y > 0 {
                    samples[at - width]
                } else {
                    0
                };
                let mut residual =
                    (i32::from(samples[at]) - i32::from(prediction)).rem_euclid(modulus);
                if residual >= half {
                    residual -= modulus;
                }
                let folded = zigzag(residual);
                low[at] = folded.to_le_bytes()[0];
                high[at] = folded.to_le_bytes()[1];
            }
        }
    }
    Ok(out)
}

/// Inverts [`encode_residuals`]. The expanded size is checked against the
/// rectangle before anything is allocated; codes above `2^depth − 1` cannot
/// result, by construction.
///
/// # Errors
///
/// [`TileError::Depth`] or [`TileError::Size`].
pub fn decode_residuals(
    bytes: &[u8],
    width: u32,
    height: u32,
    depth: u8,
) -> Result<TileCodes, TileError> {
    check_depth(depth)?;
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or(TileError::Size)?;
    let expected = pixels.checked_mul(6).ok_or(TileError::Size)?;
    if expected > MAX_TILE_EXPANDED_BYTES || bytes.len() != expected || pixels == 0 {
        return Err(TileError::Size);
    }
    let mask = (1_i32 << depth) - 1;
    let width = width as usize;
    let mut samples = vec![0_u16; pixels * 3];
    for plane in 0..3 {
        let encoded = &bytes[plane * pixels * 2..(plane + 1) * pixels * 2];
        let (low, high) = encoded.split_at(pixels);
        let out = &mut samples[plane * pixels..(plane + 1) * pixels];
        for at in 0..pixels {
            let prediction = if at % width > 0 {
                out[at - 1]
            } else if at >= width {
                out[at - width]
            } else {
                0
            };
            let residual = unzigzag(u16::from_le_bytes([low[at], high[at]]));
            // The modulus is a power of two: wrap and mask.
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            {
                out[at] = (i32::from(prediction).wrapping_add(residual) & mask) as u16;
            }
        }
    }
    TileCodes::new(u32::try_from(width).unwrap_or(0), height, samples)
}

/// Groups eligible blocks into rectangles of at most 16 × 4 blocks: greedy,
/// in the order given, extending right then down over eligible, unclaimed
/// blocks.
#[must_use]
pub fn group_blocks(grid: RefineGrid, eligible: &[u32]) -> Vec<TileRect> {
    let count = grid.block_count() as usize;
    let mut wanted = vec![false; count];
    for &index in eligible {
        if let Some(slot) = wanted.get_mut(index as usize) {
            *slot = true;
        }
    }
    let cols = grid.cols();
    let mut rects = Vec::new();
    for &index in eligible {
        if !wanted.get(index as usize).copied().unwrap_or(false) {
            continue;
        }
        let (bx, by) = (index % cols, index / cols);
        let mut wide = 1;
        while wide < MAX_TILE_BLOCKS_WIDE_U32
            && bx + wide < cols
            && wanted[(by * cols + bx + wide) as usize]
        {
            wide += 1;
        }
        let mut tall = 1;
        while tall < MAX_TILE_BLOCKS_TALL_U32
            && by + tall < grid.rows()
            && (0..wide).all(|dx| wanted[((by + tall) * cols + bx + dx) as usize])
        {
            tall += 1;
        }
        for dy in 0..tall {
            for dx in 0..wide {
                wanted[((by + dy) * cols + bx + dx) as usize] = false;
            }
        }
        let x = bx * BLOCK_U32;
        let y = by * BLOCK_U32;
        rects.push(TileRect {
            x,
            y,
            width: (wide * BLOCK_U32).min(grid.width - x),
            height: (tall * BLOCK_U32).min(grid.height - y),
        });
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let r = (x * 7 + y) % 1024;
                let g = (x * 3 + y * 5) % 1024;
                let b = (x ^ y) % 1024;
                out.extend_from_slice(&(r << 20 | g << 10 | b).to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn residuals_round_trip_at_edges_and_depths() {
        let grid = RefineGrid {
            width: 37,
            height: 21,
        };
        let frame = pattern(37, 21);
        for rect in [
            TileRect {
                x: 0,
                y: 0,
                width: 32,
                height: 16,
            },
            TileRect {
                x: 32,
                y: 16,
                width: 5,
                height: 5,
            },
        ] {
            let codes = extract_packed_rgb10(
                &frame,
                37 * 4,
                grid,
                rect,
                PackedRgb10Layout::XRGB2101010,
                Rgb10Signal::Direct,
            )
            .unwrap();
            let bytes = encode_residuals(&codes, 10).unwrap();
            assert_eq!(bytes.len(), rect.expanded_bytes());
            assert_eq!(
                decode_residuals(&bytes, rect.width, rect.height, 10).unwrap(),
                codes
            );
        }
        let extreme = TileCodes::new(2, 1, vec![0, 4095, 4095, 0, 0, 4095]).unwrap();
        let bytes = encode_residuals(&extreme, 12).unwrap();
        assert_eq!(decode_residuals(&bytes, 2, 1, 12).unwrap(), extreme);
    }

    #[test]
    fn geometry_is_checked_before_reading() {
        let grid = RefineGrid {
            width: 1920,
            height: 1080,
        };
        let ok = TileRect {
            x: 1664,
            y: 1024,
            width: 256,
            height: 56,
        };
        assert_eq!(grid.check(ok), Ok(()));
        for bad in [
            TileRect { x: 8, ..ok },
            TileRect { width: 272, ..ok },
            TileRect { height: 64, ..ok },
            TileRect {
                y: 1008,
                height: 40,
                ..ok
            },
        ] {
            assert_eq!(grid.check(bad), Err(TileError::Geometry), "{bad:?}");
        }
        assert_eq!(decode_residuals(&[0; 10], 16, 16, 10), Err(TileError::Size));
        assert_eq!(
            decode_residuals(&vec![0; 300 * 64 * 6], 300, 64, 10),
            Err(TileError::Size)
        );
    }

    #[test]
    fn grouping_makes_rectangles_of_at_most_16_by_4_blocks() {
        let grid = RefineGrid {
            width: 1920,
            height: 1080,
        };
        let all: Vec<u32> = (0..grid.block_count()).collect();
        let rects = group_blocks(grid, &all);
        let covered: usize = rects.iter().map(|rect| grid.blocks_of(*rect).len()).sum();
        assert_eq!(covered, grid.block_count() as usize);
        assert!(rects.iter().all(|rect| grid.check(*rect).is_ok()));
        assert_eq!(rects[0].width, 256);
        assert_eq!(rects[0].height, 64);
        let sparse = group_blocks(grid, &[5, 6, 125]);
        assert_eq!(sparse.len(), 2);
    }
}
