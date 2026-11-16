// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The portable half of every Deck's [`RefineCompositor`] (ADR 0016 R1, R2,
//! R4): what one presented frame must change in the overlay, the overlay's
//! texel format, the bounds guard, and the domain guard that decides
//! whether a generation's tiles may be composited over a decoded picture at
//! all. The GPU half (Metal blits, D3D11 `UpdateSubresource`) stays in each
//! Deck and only executes [`OverlayWork`].

use super::RefineContract;
use super::deck::{OverlayDelta, VerificationLevel};
use super::hub::{ReadbackRequest, RefineCompositor};
use super::tile::{RefineGrid, TileRect};
use crate::video::present::VideoColorContract;
use crate::{BitDepth, ChromaSubsampling};
use arcen_protocol::refine::RefineContractBytes;

/// Bytes of one overlay texel (`RGB10A2Uint` / `R10G10B10A2_UINT`).
pub const OVERLAY_TEXEL_BYTES: usize = 4;

/// One valid overlay texel: three 10-bit codes and the 2-bit "valid" flag.
#[must_use]
pub fn pack_texel(r: u16, g: u16, b: u16) -> u32 {
    u32::from(r & 0x3ff) | u32::from(g & 0x3ff) << 10 | u32::from(b & 0x3ff) << 20 | 3 << 30
}

/// A block's planar codes (`R…`, `G…`, `B…`, or `Y′…`, `Cb…`, `Cr…`) as
/// little-endian texel words; empty when `planar` is short.
#[must_use]
pub fn pack_block(planar: &[u16], pixels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels * OVERLAY_TEXEL_BYTES);
    if planar.len() < pixels * 3 {
        return out;
    }
    for index in 0..pixels {
        out.extend_from_slice(
            &pack_texel(
                planar[index],
                planar[pixels + index],
                planar[2 * pixels + index],
            )
            .to_le_bytes(),
        );
    }
    out
}

/// Texel words read back from the overlay (`UINT`, level 2) or an unscaled
/// composite (`UNORM`, level 3: a stored `code / 1023` is `code`) as planar
/// codes.
#[must_use]
pub fn unpack_block(bytes: &[u8], pixels: usize) -> Vec<u16> {
    let mut planar = vec![0_u16; pixels * 3];
    for (index, word) in bytes
        .chunks_exact(OVERLAY_TEXEL_BYTES)
        .take(pixels)
        .enumerate()
    {
        let word = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        let code = |shift: u32| u16::try_from((word >> shift) & 0x3ff).unwrap_or(0);
        planar[index] = code(0);
        planar[pixels + index] = code(10);
        planar[2 * pixels + index] = code(20);
    }
    planar
}

/// Whether every rectangle lies inside `grid` and is at most one 16×16
/// block (all overlay work is per block). A compositor never writes or
/// reads outside its overlay.
#[must_use]
pub fn inside<'a>(rects: impl IntoIterator<Item = &'a TileRect>, grid: RefineGrid) -> bool {
    rects.into_iter().all(|rect| {
        rect.width <= 16
            && rect.height <= 16
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|end| end <= grid.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|end| end <= grid.height)
    })
}

/// Whether a readback needs an unscaled composite (level 3) rather than
/// the overlay itself (level 2).
#[must_use]
pub fn needs_composite(request: &ReadbackRequest) -> bool {
    request.level == VerificationLevel::RenderedRgb
}

/// Whether a generation (its grid, code depth and barrier contract) may be
/// composited over a decoded picture of `source` size under `stream`: the
/// same size, 10-bit 4:4:4, and the picture's own coding domain — so the
/// overlay is never larger than the decoded video whatever a barrier
/// claimed, and PQ codes never land on an SDR picture (nor SDR codes on a
/// PQ one). Y′CbCr tiles also need the picture's own matrix and range.
#[must_use]
pub fn composites_over(
    generation: Option<(RefineGrid, u8)>,
    contract: Option<RefineContractBytes>,
    stream: VideoColorContract,
    source: RefineGrid,
) -> bool {
    let same_domain = contract.is_some_and(|bytes| {
        RefineContract::describes_coded_stream(
            bytes,
            stream.depth,
            stream.transfer,
            stream.primaries,
            stream.matrix,
            stream.range,
        ) && stream.chroma == ChromaSubsampling::Yuv444
    });
    generation == Some((source, 10)) && stream.depth == BitDepth::Ten && same_domain
}

/// The overlay work of one presented frame, collected from the shared hub
/// ([`super::hub::DeckRefineHub::compose`]); a Deck's GPU code executes it
/// before the draw that samples the overlay.
#[derive(Debug, Default)]
pub struct OverlayWork {
    /// A new generation: start from an empty overlay of this grid.
    pub reset: Option<RefineGrid>,
    /// Blocks to clear (after a reset, if any).
    pub clears: Vec<TileRect>,
    /// Blocks to write: rectangle and planar codes.
    pub sets: Vec<(TileRect, Vec<u16>)>,
    /// Readbacks to perform after the composite.
    pub readbacks: Vec<ReadbackRequest>,
}

impl OverlayWork {
    /// Whether the overlay changes at all.
    #[must_use]
    pub fn changes_overlay(&self) -> bool {
        self.reset.is_some() || !self.clears.is_empty() || !self.sets.is_empty()
    }
}

impl RefineCompositor for OverlayWork {
    fn apply_overlay(&mut self, grid: RefineGrid, delta: &OverlayDelta) {
        if delta.clear_all {
            self.reset = Some(grid);
            self.clears.clear();
            self.sets.clear();
        }
        self.clears.extend(delta.clear.iter().copied());
        self.sets.extend(delta.set.iter().cloned());
    }

    fn request_readback(&mut self, request: ReadbackRequest) {
        self.readbacks.push(request);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics};
    use arcen_protocol::refine::RefineDomain;

    #[test]
    fn texels_round_trip_and_carry_the_valid_flag() {
        let planar: Vec<u16> = vec![1, 1023, 0, 512, 7, 9];
        let packed = pack_block(&planar, 2);
        assert_eq!(packed.len(), 8);
        let word = u32::from_le_bytes([packed[0], packed[1], packed[2], packed[3]]);
        assert_eq!(word >> 30, 3, "valid");
        assert_eq!(unpack_block(&packed, 2), planar);
        assert!(pack_block(&planar[..5], 2).is_empty());
    }

    #[test]
    fn work_outside_the_overlay_is_refused() {
        let grid = RefineGrid {
            width: 40,
            height: 20,
        };
        assert!(inside(&[grid.block_rect(5).unwrap()], grid));
        for rect in [
            TileRect {
                x: 4000,
                y: 0,
                width: 16,
                height: 16,
            },
            TileRect {
                x: 0,
                y: 0,
                width: 32,
                height: 16,
            },
            TileRect {
                x: u32::MAX,
                y: 0,
                width: 16,
                height: 16,
            },
        ] {
            assert!(!inside(&[rect], grid), "{rect:?}");
        }
    }

    #[test]
    fn a_reset_drops_earlier_changes_of_the_same_frame() {
        let grid = RefineGrid {
            width: 32,
            height: 16,
        };
        let block = grid.block_rect(0).unwrap();
        let mut work = OverlayWork::default();
        work.apply_overlay(
            grid,
            &OverlayDelta {
                clear_all: false,
                clear: vec![block],
                set: vec![(block, vec![0; 768])],
            },
        );
        work.apply_overlay(
            grid,
            &OverlayDelta {
                clear_all: true,
                clear: Vec::new(),
                set: Vec::new(),
            },
        );
        assert_eq!(work.reset, Some(grid));
        assert!(work.clears.is_empty() && work.sets.is_empty());
        assert!(work.changes_overlay());
    }

    #[test]
    fn tiles_composite_only_over_their_own_picture() {
        let grading = VideoColorContract {
            chroma: ChromaSubsampling::Yuv444,
            range: ColorRange::Full,
            depth: BitDepth::Ten,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Srgb,
        };
        let grid = RefineGrid {
            width: 64,
            height: 32,
        };
        let rgb = RefineContract {
            domain: RefineDomain::Rgb,
            depth: BitDepth::Ten,
            transfer: grading.transfer,
            primaries: grading.primaries,
            range: ColorRange::Full,
            chroma: ChromaSubsampling::Yuv444,
            matrix: ColorMatrix::Identity,
        }
        .to_bytes();
        assert!(composites_over(Some((grid, 10)), Some(rgb), grading, grid));
        // Another size, an 8-bit or 4:2:0 picture, a PQ picture: refused.
        let other = RefineGrid {
            width: 65,
            height: 32,
        };
        assert!(!composites_over(
            Some((grid, 10)),
            Some(rgb),
            grading,
            other
        ));
        let eight = VideoColorContract {
            depth: BitDepth::Eight,
            ..grading
        };
        assert!(!composites_over(Some((grid, 10)), Some(rgb), eight, grid));
        let subsampled = VideoColorContract {
            chroma: ChromaSubsampling::Yuv420,
            ..grading
        };
        assert!(!composites_over(
            Some((grid, 10)),
            Some(rgb),
            subsampled,
            grid
        ));
        let pq = VideoColorContract {
            transfer: TransferCharacteristics::Pq,
            primaries: ColorPrimaries::Bt2020,
            matrix: ColorMatrix::Bt2020Ncl,
            ..grading
        };
        assert!(!composites_over(Some((grid, 10)), Some(rgb), pq, grid));
        assert!(!composites_over(None, Some(rgb), grading, grid));
    }
}
