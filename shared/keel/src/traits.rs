// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Pixel traits of changed blocks: one colour, few colours, sharp edges.
//!
//! Pure measurements with no codec knowledge. A planner reads them to tell
//! user-interface and text content (few colours, hard edges: exact tiles
//! win) from photographic content (many colours: a lossy encoder wins).

use crate::damage::WORD_BITS;
use crate::{BgraFrame, BlockBounds, BlockGrid, DamageMap, KeelError};

/// Distinct colours counted exactly; more are reported as "many".
pub const MAX_COUNTED_COLOURS: u8 = 16;
/// Luma step between neighbouring pixels that counts as a sharp edge.
pub const SHARP_EDGE_LUMA_STEP: u8 = 64;

const RGB_MASK: u32 = 0x00ff_ffff;
const MANY: u8 = MAX_COUNTED_COLOURS + 1;

/// Measured traits of one block. Alpha is ignored: captures deliver BGRX
/// as often as BGRA.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockTraits {
    /// 0 = not measured, 1..=16 exact, 17 = more than 16.
    colours: u8,
    /// Neighbouring pixel pairs with a luma step of at least
    /// [`SHARP_EDGE_LUMA_STEP`]; measured only for low-colour blocks.
    sharp_edges: u16,
    /// First pixel's colour as `0x00RRGGBB`.
    first_colour: u32,
}

impl BlockTraits {
    /// Measures block `index` of `frame`'s grid; `None` outside the grid.
    #[must_use]
    pub fn measure_block(frame: BgraFrame<'_>, index: usize) -> Option<Self> {
        frame
            .grid()
            .block_bounds(index)
            .map(|bounds| Self::measure(frame, bounds))
    }

    /// `bounds` must come from `frame.grid()`.
    fn measure(frame: BgraFrame<'_>, bounds: BlockBounds) -> Self {
        let mut palette = [0u32; MAX_COUNTED_COLOURS as usize];
        let mut count = 0usize;
        let mut last = None;
        'rows: for row in 0..bounds.height {
            let segment = frame.block_row_segment(bounds.y + row, bounds);
            for pixel in segment.chunks_exact(4) {
                let colour = rgb(pixel);
                if last == Some(colour) {
                    continue;
                }
                last = Some(colour);
                if palette[..count].contains(&colour) {
                    continue;
                }
                if count == palette.len() {
                    count += 1;
                    break 'rows;
                }
                palette[count] = colour;
                count += 1;
            }
        }
        let colours = u8::try_from(count).unwrap_or(MANY).min(MANY);
        let sharp_edges = if (2..=usize::from(MAX_COUNTED_COLOURS)).contains(&count) {
            count_sharp_edges(frame, bounds)
        } else {
            0
        };
        Self {
            colours,
            sharp_edges,
            first_colour: palette[0],
        }
    }

    #[must_use]
    pub const fn is_measured(self) -> bool {
        self.colours != 0
    }

    /// Exact distinct colour count, or `None` for "more than 16" or an
    /// unmeasured block.
    #[must_use]
    pub const fn distinct_colours(self) -> Option<u8> {
        if self.colours == 0 || self.colours > MAX_COUNTED_COLOURS {
            None
        } else {
            Some(self.colours)
        }
    }

    #[must_use]
    pub const fn is_solid(self) -> bool {
        self.colours == 1
    }

    /// The block's single colour as `[b, g, r]`, if it is solid.
    #[must_use]
    pub const fn solid_bgr(self) -> Option<[u8; 3]> {
        if self.is_solid() {
            let [b, g, r, _] = self.first_colour.to_le_bytes();
            Some([b, g, r])
        } else {
            None
        }
    }

    /// At most [`MAX_COUNTED_COLOURS`] colours.
    #[must_use]
    pub const fn is_low_colour(self) -> bool {
        self.colours != 0 && self.colours <= MAX_COUNTED_COLOURS
    }

    /// Low colour, more than one colour, and at least one hard edge: text,
    /// icons and interface chrome rather than a gradient or a photograph.
    #[must_use]
    pub const fn is_text_like(self) -> bool {
        self.colours >= 2 && self.colours <= MAX_COUNTED_COLOURS && self.sharp_edges > 0
    }

    #[must_use]
    pub const fn sharp_edges(self) -> u16 {
        self.sharp_edges
    }
}

/// Traits for every block of one geometry, refreshed only where damaged.
#[derive(Debug)]
pub struct BlockTraitsMap {
    grid: BlockGrid,
    traits: Vec<BlockTraits>,
}

impl BlockTraitsMap {
    #[must_use]
    pub fn new(grid: BlockGrid) -> Self {
        Self {
            grid,
            traits: vec![BlockTraits::default(); grid.block_count()],
        }
    }

    #[must_use]
    pub const fn grid(&self) -> BlockGrid {
        self.grid
    }

    /// Forgets every measurement.
    pub fn reset(&mut self) {
        self.traits.fill(BlockTraits::default());
    }

    /// Measures the damaged blocks of `frame` and returns how many were
    /// measured. Undamaged blocks keep their earlier measurement, which
    /// still describes their unchanged content.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] without mutating state when the
    /// frame or map has another geometry.
    pub fn update(
        &mut self,
        frame: BgraFrame<'_>,
        damage: DamageMap<'_>,
    ) -> Result<usize, KeelError> {
        self.update_where(frame, damage, |_| true)
    }

    /// [`Self::update`] for the damaged blocks `measure` accepts. A caller
    /// that reproduces some damaged blocks without looking at their pixels
    /// (a verified copy) skips them; they read as not measured (neither
    /// solid nor low-colour) until damage measures them again.
    ///
    /// # Errors
    ///
    /// [`KeelError::GeometryChanged`] for another geometry.
    pub fn update_where(
        &mut self,
        frame: BgraFrame<'_>,
        damage: DamageMap<'_>,
        measure: impl Fn(usize) -> bool,
    ) -> Result<usize, KeelError> {
        for actual in [frame.grid(), damage.grid()] {
            if actual != self.grid {
                return Err(KeelError::GeometryChanged {
                    expected: self.grid,
                    actual,
                });
            }
        }
        let mut measured = 0usize;
        for (word_index, &word) in damage.bits.iter().enumerate() {
            let mut remaining = word;
            while remaining != 0 {
                let index = word_index * WORD_BITS + remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                if !measure(index) {
                    if let Some(skipped) = self.traits.get_mut(index) {
                        *skipped = BlockTraits::default();
                    }
                    continue;
                }
                let Some(bounds) = self.grid.block_bounds(index) else {
                    break;
                };
                self.traits[index] = BlockTraits::measure(frame, bounds);
                measured += 1;
            }
        }
        Ok(measured)
    }

    #[must_use]
    pub fn get(&self, index: usize) -> Option<BlockTraits> {
        self.traits.get(index).copied()
    }
}

fn rgb(pixel: &[u8]) -> u32 {
    u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]) & RGB_MASK
}

fn luma(pixel: &[u8]) -> i16 {
    // BT.601-like integer weights on B, G, R; enough to find hard edges.
    let weighted = u16::from(pixel[0]) + 5 * u16::from(pixel[1]) + 2 * u16::from(pixel[2]);
    i16::try_from(weighted / 8).unwrap_or(i16::MAX)
}

fn count_sharp_edges(frame: BgraFrame<'_>, bounds: BlockBounds) -> u16 {
    let step = i16::from(SHARP_EDGE_LUMA_STEP);
    let mut edges = 0u16;
    let mut previous_row: Option<&[u8]> = None;
    for row in 0..bounds.height {
        let segment = frame.block_row_segment(bounds.y + row, bounds);
        for (index, pixel) in segment.chunks_exact(4).enumerate() {
            let here = luma(pixel);
            if index > 0 && (here - luma(&segment[(index - 1) * 4..index * 4])).abs() >= step {
                edges = edges.saturating_add(1);
            }
            if previous_row
                .is_some_and(|above| (here - luma(&above[index * 4..index * 4 + 4])).abs() >= step)
            {
                edges = edges.saturating_add(1);
            }
        }
        previous_row = Some(segment);
    }
    edges
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{ExternalDamage, PixelRect};

    fn traits_of(pixels: &[u8]) -> BlockTraits {
        let frame = BgraFrame::new(pixels, 16, 16, 64).unwrap();
        assert!(BlockTraits::measure_block(frame, 1).is_none());
        BlockTraits::measure_block(frame, 0).unwrap()
    }

    #[test]
    fn solid_block_reports_its_colour_and_ignores_alpha() {
        let mut pixels = [10u8, 20, 30, 0xff].repeat(256);
        pixels[3] = 0;
        let traits = traits_of(&pixels);
        assert!(traits.is_solid());
        assert_eq!(traits.solid_bgr(), Some([10, 20, 30]));
        assert!(!traits.is_text_like());
        assert_eq!(traits.distinct_colours(), Some(1));
    }

    #[test]
    fn glyph_on_background_is_text_like() {
        let mut pixels = [250u8, 250, 250, 0xff].repeat(256);
        for y in 3..13 {
            let offset = (y * 16 + 7) * 4;
            pixels[offset..offset + 4].copy_from_slice(&[20, 20, 20, 0xff]);
        }
        let traits = traits_of(&pixels);
        assert_eq!(traits.distinct_colours(), Some(2));
        assert!(traits.is_text_like());
        assert!(traits.sharp_edges() >= 20);
    }

    #[test]
    fn gentle_gradient_is_low_colour_but_not_text_like() {
        let mut pixels = Vec::with_capacity(1024);
        for _y in 0..16u8 {
            for x in 0..16u8 {
                let value = 100 + x * 2;
                pixels.extend_from_slice(&[value, value, value, 0xff]);
            }
        }
        let traits = traits_of(&pixels);
        assert!(traits.is_low_colour());
        assert!(!traits.is_text_like());
    }

    #[test]
    fn photographic_block_has_many_colours() {
        let pixels = (0..1024u32)
            .map(|value| u8::try_from(value * 37 % 251).unwrap())
            .collect::<Vec<_>>();
        let traits = traits_of(&pixels);
        assert_eq!(traits.distinct_colours(), None);
        assert!(traits.is_measured());
        assert!(!traits.is_low_colour());
    }

    #[test]
    fn map_measures_only_damaged_blocks() {
        let pixels = [1u8, 2, 3, 0xff].repeat(64 * 32);
        let frame = BgraFrame::new(&pixels, 64, 32, 256).unwrap();
        let mut damage = ExternalDamage::new(64, 32).unwrap();
        damage.mark_rect(PixelRect {
            x: 16,
            y: 16,
            width: 1,
            height: 1,
        });
        let mut map = BlockTraitsMap::new(frame.grid());
        assert_eq!(map.update(frame, damage.damage_map()).unwrap(), 1);
        assert!(map.get(5).unwrap().is_solid());
        assert!(!map.get(0).unwrap().is_measured());
    }
}
