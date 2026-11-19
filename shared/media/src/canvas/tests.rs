// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The canvas gate (ADR 0028 §8): corpus replay, where the Pier's mirror
//! digest equals an independent replay of the wire bytes for every frame,
//! plus the state owner's refusals and the executor's semantics.

use super::pixels::ResidualTileDecoder;
use super::tile::{ResidualTilePacker, TilePacker, pack_tiles};
use super::*;
use crate::refine::source::{Bgra8Source, RefineSource};
use crate::refine::tile::{RefineGrid, TileRect};
use arcen_protocol::canvas::{
    CanvasCommand, CanvasFrame, CanvasPixelFormat, CanvasRecord, CanvasRect, CanvasReset,
    CanvasResetReason, CanvasScope, CanvasTerms, SURFACE_CODEC_H264, SurfacePaint,
    canvas_block_count,
};
use arcen_protocol::refine::BlockRuns;
use std::num::NonZeroU32;

const WIDTH: u32 = 330;
const HEIGHT: u32 = 206;

fn terms() -> CanvasTerms {
    CanvasTerms {
        max_canvas_px: 16_384,
        max_surfaces_per_monitor: 2,
        cache_bytes: 1 << 20,
        cache_slots: 16,
    }
}

fn scope(generation: u32) -> CanvasScope {
    CanvasScope {
        monitor_id: 1,
        topology_generation: 4,
        stream_epoch: 9,
        generation: NonZeroU32::new(generation).unwrap(),
    }
}

fn reset_bytes(generation: u32, width: u32, height: u32) -> Vec<u8> {
    CanvasReset {
        scope: scope(generation),
        reason: CanvasResetReason::Start,
        format: CanvasPixelFormat::Bgra8,
        width,
        height,
    }
    .encode()
    .unwrap()
}

fn canvas_rect(rect: TileRect) -> CanvasRect {
    CanvasRect {
        x: u16::try_from(rect.x).unwrap(),
        y: u16::try_from(rect.y).unwrap(),
        width: u16::try_from(rect.width).unwrap(),
        height: u16::try_from(rect.height).unwrap(),
    }
}

fn whole(width: u32, height: u32) -> TileRect {
    TileRect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

// ---------------------------------------------------------------- corpus

fn noise(seed: u32) -> u8 {
    let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(0x9E37_79B9);
    state ^= state >> 15;
    state = state.wrapping_mul(0x2C1B_3C6D);
    state ^= state >> 12;
    state.to_le_bytes()[0]
}

/// A desktop: a soft gradient with a text-like page and solid panels.
fn desktop(width: u32, height: u32, page_offset: u32, glyph_seed: u32) -> Vec<u8> {
    let mut frame = vec![0_u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let at = ((y * width + x) * 4) as usize;
            let page_y = y + page_offset;
            let pixel = if x < 40 {
                // A solid side panel.
                [60, 50, 40, 255]
            } else if (page_y / 12) % 2 == 0 && x % 9 < 6 {
                // Text-like rows: glyph cells change with the seed.
                let cell = (page_y / 12) * 64 + x / 9;
                let ink = noise(cell ^ glyph_seed.wrapping_mul(7919)) % 3 == 0;
                if ink {
                    [20, 20, 20, 255]
                } else {
                    [250, 250, 245, 255]
                }
            } else {
                [
                    u8::try_from((x * 3 + page_y) % 256).unwrap(),
                    u8::try_from((page_y * 2) % 256).unwrap(),
                    200,
                    255,
                ]
            };
            frame[at..at + 4].copy_from_slice(&pixel);
        }
    }
    frame
}

fn paint_rect(frame: &mut [u8], width: u32, rect: TileRect, pixel: impl Fn(u32, u32) -> [u8; 4]) {
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            let at = ((y * width + x) * 4) as usize;
            frame[at..at + 4].copy_from_slice(&pixel(x - rect.x, y - rect.y));
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Scene {
    Typing,
    Scroll,
    Drag,
    Menu,
    Video,
}

const MENU: TileRect = TileRect {
    x: 96,
    y: 48,
    width: 80,
    height: 64,
};
const VIDEO: TileRect = TileRect {
    x: 160,
    y: 96,
    width: 128,
    height: 80,
};
const SCROLL_ROWS: u32 = 24;

/// Frame `index` of `scene`. The planner below knows each scene's
/// structure (a scroll vector, a cached menu, a video region), as Keel's
/// move list, tile ledger and motion regions will tell the product one.
fn scene_frame(scene: Scene, index: u32) -> Vec<u8> {
    match scene {
        Scene::Typing => desktop(WIDTH, HEIGHT, 0, index),
        Scene::Scroll => desktop(WIDTH, HEIGHT, index * SCROLL_ROWS, 0),
        Scene::Drag => {
            let mut frame = desktop(WIDTH, HEIGHT, 0, 0);
            let window = TileRect {
                x: 50 + index * 7,
                y: 20 + index * 5,
                width: 90,
                height: 70,
            };
            paint_rect(&mut frame, WIDTH, window, |x, y| {
                if y < 12 {
                    [180, 90, 30, 255]
                } else {
                    [noise(x * 131 + y), 230, noise(y * 17 + x), 255]
                }
            });
            frame
        }
        Scene::Menu => {
            let mut frame = desktop(WIDTH, HEIGHT, 0, 0);
            if index % 2 == 1 {
                paint_rect(&mut frame, WIDTH, MENU, |x, y| {
                    if y % 16 < 14 && x > 4 {
                        [240, 240, 240, 255]
                    } else {
                        [noise(x + y * 80), 128, 64, 255]
                    }
                });
            }
            frame
        }
        Scene::Video => {
            let mut frame = desktop(WIDTH, HEIGHT, 0, index / 3);
            paint_rect(&mut frame, WIDTH, VIDEO, |x, y| {
                [
                    noise(x + y * 128 + index * 977),
                    noise(x * y + index),
                    90,
                    255,
                ]
            });
            frame
        }
    }
}

/// A Pier for one monitor: the eight-bit source, the mirror, and a plain
/// planner that turns every frame into canvas commands (M2's planner is
/// the product one; this one only has to produce every command kind).
struct TestPier {
    source: Bgra8Source,
    mirror: CanvasMirror,
    packer: Box<dyn TilePacker>,
    fence: u64,
    menu_cached: bool,
    video_open: bool,
}

fn block_differs(a: &[u8], b: &[u8], width: u32, block: TileRect) -> bool {
    (block.y..block.y + block.height).any(|y| {
        let start = ((y * width + block.x) * 4) as usize;
        let end = start + block.width as usize * 4;
        a[start..end] != b[start..end]
    })
}

fn block_colour(frame: &[u8], width: u32, block: TileRect) -> Option<[u8; 4]> {
    let first = ((block.y * width + block.x) * 4) as usize;
    let colour: [u8; 4] = frame[first..first + 4].try_into().ok()?;
    (block.y..block.y + block.height)
        .all(|y| {
            let start = ((y * width + block.x) * 4) as usize;
            frame[start..start + block.width as usize * 4]
                .chunks_exact(4)
                .all(|pixel| pixel == colour)
        })
        .then_some(colour)
}

impl TestPier {
    fn new(packer: Box<dyn TilePacker>) -> (Self, Vec<u8>) {
        let mut mirror = CanvasMirror::new(1, terms());
        let reset = reset_bytes(1, WIDTH, HEIGHT);
        mirror
            .apply(
                &CanvasRecord::decode(&reset).unwrap(),
                &mut PackedTileCodes::default(),
                &mut NoSurfacePixels,
            )
            .unwrap();
        let pier = Self {
            source: Bgra8Source::new(RefineGrid {
                width: WIDTH,
                height: HEIGHT,
            }),
            mirror,
            packer,
            fence: 0,
            menu_cached: false,
            video_open: false,
        };
        (pier, reset)
    }

    fn grid(&self) -> RefineGrid {
        self.source.grid()
    }

    /// What the planner knows besides pixels: a scroll vector, a cached
    /// menu, a video region.
    fn structural<'a>(
        &self,
        scene: Scene,
        index: u32,
        au: &'a [u8],
        video_blocks: &[u32],
    ) -> Vec<CanvasCommand<'a>> {
        let mut structural = Vec::new();
        match scene {
            Scene::Scroll if index > 0 => structural.push(CanvasCommand::Copy {
                src: canvas_rect(TileRect {
                    x: 0,
                    y: SCROLL_ROWS,
                    width: WIDTH,
                    height: HEIGHT - SCROLL_ROWS,
                }),
                dst_x: 0,
                dst_y: 0,
            }),
            Scene::Menu if index % 2 == 1 && self.menu_cached => {
                structural.push(CanvasCommand::CachePaint {
                    slot: 3,
                    dst_x: 96,
                    dst_y: 48,
                });
            }
            Scene::Video => {
                if !self.video_open {
                    structural.push(CanvasCommand::SurfaceOpen {
                        surface: 1,
                        codec: SURFACE_CODEC_H264,
                        width: 128,
                        height: 80,
                    });
                }
                structural.push(CanvasCommand::SurfacePaint(SurfacePaint {
                    surface: 1,
                    idr: !self.video_open,
                    dest: canvas_rect(VIDEO),
                    blocks: BlockRuns::from_sorted_indices(video_blocks.iter().copied()),
                    access_unit: au,
                }));
            }
            _ => {}
        }
        structural
    }

    /// Plans, encodes and mirrors one frame; returns the wire bytes.
    fn step(&mut self, scene: Scene, index: u32, next: &[u8]) -> Vec<u8> {
        let grid = self.grid();
        self.source
            .observe(next, (WIDTH * 4) as usize, whole(WIDTH, HEIGHT));
        let block_count = canvas_block_count(WIDTH, HEIGHT);
        let au = [0, 0, 0, 1, 0x65, u8::try_from(index % 256).unwrap()];
        let video_blocks: Vec<u32> = grid.blocks_of(VIDEO);
        let structural = self.structural(scene, index, &au, &video_blocks);

        // Predict the canvas after the structural commands, then send what
        // still differs from the source.
        let mut predicted = self.mirror.clone();
        let probe = CanvasFrame {
            scope: scope(1),
            fence: self.fence + 1,
            timestamp_ms: 0,
            block_count,
            commands: structural.clone(),
        };
        predicted
            .apply(
                &CanvasRecord::Frame(probe),
                &mut PackedTileCodes::default(),
                &mut NoSurfacePixels,
            )
            .unwrap();
        let mut fills = Vec::new();
        let mut tile_blocks = Vec::new();
        for index in 0..grid.block_count() {
            let block = grid.block_rect(index).unwrap();
            let in_video = matches!(scene, Scene::Video) && video_blocks.contains(&index);
            if in_video || !block_differs(predicted.pixels().bgra(), next, WIDTH, block) {
                continue;
            }
            match block_colour(next, WIDTH, block) {
                Some([b, g, r, _]) => fills.push(CanvasCommand::Fill {
                    rect: canvas_rect(block),
                    rgb: [r, g, b],
                }),
                None => tile_blocks.push(index),
            }
        }
        let tiles = pack_tiles(&self.source, &tile_blocks, self.packer.as_mut()).unwrap();
        let mut commands = structural;
        commands.extend(fills);
        commands.extend(tiles.iter().map(super::tile::PackedTile::command));
        if matches!(scene, Scene::Menu) && index % 2 == 1 && !self.menu_cached {
            commands.push(CanvasCommand::CacheStore {
                slot: 3,
                rect: canvas_rect(MENU),
            });
            self.menu_cached = true;
        }
        if matches!(scene, Scene::Video) {
            self.video_open = true;
        }
        self.fence += 1;
        let frame = CanvasFrame {
            scope: scope(1),
            fence: self.fence,
            timestamp_ms: index,
            block_count,
            commands,
        };
        let bytes = frame.encode().unwrap();
        let mut codes = PackedTileCodes::default();
        for tile in &tiles {
            codes.push(tile.codes.clone());
        }
        self.mirror
            .apply(
                &CanvasRecord::Frame(frame),
                &mut codes,
                &mut NoSurfacePixels,
            )
            .unwrap();
        assert!(codes.is_empty(), "every packed tile was applied");
        bytes
    }
}

/// Every lossless block of the mirror equals the source.
fn assert_exact(mirror: &CanvasMirror, next: &[u8], what: &str) {
    let grid = mirror.state().grid().unwrap();
    for index in mirror.state().lossless_blocks() {
        let block = grid.block_rect(index).unwrap();
        assert!(
            !block_differs(mirror.pixels().bgra(), next, WIDTH, block),
            "{what}: block {index} differs from the source"
        );
    }
}

fn replay_corpus(
    packer: &dyn Fn() -> Box<dyn TilePacker>,
    decoder: &mut dyn FnMut() -> Box<dyn TileDecoder>,
) {
    for scene in [
        Scene::Typing,
        Scene::Scroll,
        Scene::Drag,
        Scene::Menu,
        Scene::Video,
    ] {
        let (mut pier, reset) = TestPier::new(packer());
        // An independent Deck: only the wire bytes reach it.
        let mut replay = CanvasMirror::new(1, terms());
        let mut tiles = decoder();
        replay
            .apply(
                &CanvasRecord::decode(&reset).unwrap(),
                tiles.as_mut(),
                &mut NoSurfacePixels,
            )
            .unwrap();
        assert_eq!(pier.mirror.digest(), replay.digest());
        let mut kinds = std::collections::BTreeSet::new();
        for index in 0..8 {
            let next = scene_frame(scene, index);
            let bytes = pier.step(scene, index, &next);
            let record = CanvasRecord::decode(&bytes).unwrap();
            if let CanvasRecord::Frame(frame) = &record {
                kinds.extend(frame.commands.iter().map(CanvasCommand::token));
            }
            replay
                .apply(&record, tiles.as_mut(), &mut NoSurfacePixels)
                .unwrap();
            let what = format!("{scene:?} frame {index}");
            let mirror_digest = pier.mirror.digest().unwrap();
            assert_eq!(Some(mirror_digest), replay.digest(), "{what}");
            assert_eq!(mirror_digest.fence, u64::from(index) + 1, "{what}");
            assert_exact(&pier.mirror, &next, &what);
            let lossy = pier.state_lossy_count();
            assert_eq!(
                mirror_digest.covered + lossy,
                canvas_block_count(WIDTH, HEIGHT),
                "{what}"
            );
            if matches!(scene, Scene::Video) {
                assert_eq!(
                    lossy,
                    u32::try_from(pier.grid().blocks_of(VIDEO).len()).unwrap()
                );
            } else {
                assert_eq!(lossy, 0, "{what}");
            }
        }
        let expected: &[&str] = match scene {
            Scene::Typing | Scene::Drag => &["tile"],
            Scene::Scroll => &["copy", "tile"],
            Scene::Menu => &["cache_store", "cache_paint", "tile"],
            Scene::Video => &["surface_open", "surface_paint", "tile"],
        };
        for kind in expected {
            assert!(
                kinds.contains(kind),
                "{scene:?} never sent {kind}: {kinds:?}"
            );
        }
        assert!(
            kinds.contains("fill"),
            "{scene:?} never sent a fill: {kinds:?}"
        );
    }
}

impl TestPier {
    fn state_lossy_count(&self) -> u32 {
        let state = self.mirror.state();
        (0..canvas_block_count(WIDTH, HEIGHT))
            .filter(|index| state.is_lossy(*index))
            .count()
            .try_into()
            .unwrap()
    }
}

#[test]
fn the_mirror_digest_equals_an_independent_replay_for_every_frame() {
    replay_corpus(&|| Box::new(ResidualTilePacker), &mut || {
        Box::new(ResidualTileDecoder)
    });
}

#[cfg(all(feature = "refine-zstd-encode", feature = "refine-zstd-decode"))]
#[test]
fn the_mirror_digest_equals_an_independent_replay_with_delta_zstd1() {
    replay_corpus(
        &|| Box::new(super::tile::ZstdTilePacker::new().unwrap()),
        &mut || Box::new(super::pixels::ZstdTileDecoder),
    );
}

#[test]
fn a_corrupted_tile_changes_the_replay_digest() {
    let (mut pier, reset) = TestPier::new(Box::new(ResidualTilePacker));
    let mut replay = CanvasMirror::new(1, terms());
    replay
        .apply(
            &CanvasRecord::decode(&reset).unwrap(),
            &mut ResidualTileDecoder,
            &mut NoSurfacePixels,
        )
        .unwrap();
    let next = scene_frame(Scene::Typing, 0);
    let mut bytes = pier.step(Scene::Typing, 0, &next);
    let offset = {
        let CanvasRecord::Frame(frame) = CanvasRecord::decode(&bytes).unwrap() else {
            panic!("a frame");
        };
        let payload = frame
            .commands
            .iter()
            .find_map(|command| match command {
                CanvasCommand::Tile(tile) => Some(tile.payload),
                _ => None,
            })
            .unwrap();
        payload.as_ptr() as usize - bytes.as_ptr() as usize
    };
    // The first residual of the first plane: the tile's first red code.
    bytes[offset] ^= 0x02;
    replay
        .apply(
            &CanvasRecord::decode(&bytes).unwrap(),
            &mut ResidualTileDecoder,
            &mut NoSurfacePixels,
        )
        .unwrap();
    assert_ne!(pier.mirror.digest(), replay.digest());
}

// ------------------------------------------------------------- the state

fn frame_bytes(fence: u64, commands: Vec<CanvasCommand<'_>>) -> Vec<u8> {
    CanvasFrame {
        scope: scope(1),
        fence,
        timestamp_ms: 0,
        block_count: canvas_block_count(WIDTH, HEIGHT),
        commands,
    }
    .encode()
    .unwrap()
}

fn started() -> CanvasMirror {
    let mut mirror = CanvasMirror::new(1, terms());
    mirror
        .apply(
            &CanvasRecord::decode(&reset_bytes(1, WIDTH, HEIGHT)).unwrap(),
            &mut ResidualTileDecoder,
            &mut NoSurfacePixels,
        )
        .unwrap();
    mirror
}

fn apply(mirror: &mut CanvasMirror, bytes: &[u8]) -> Result<(), CanvasStateError> {
    mirror.apply(
        &CanvasRecord::decode(bytes).unwrap(),
        &mut ResidualTileDecoder,
        &mut NoSurfacePixels,
    )
}

fn fill(x: u16, y: u16, width: u16, height: u16) -> CanvasCommand<'static> {
    CanvasCommand::Fill {
        rect: CanvasRect {
            x,
            y,
            width,
            height,
        },
        rgb: [1, 2, 3],
    }
}

#[test]
fn a_refused_record_breaks_the_state_until_the_next_reset() {
    let mut mirror = CanvasMirror::new(1, terms());
    assert_eq!(
        apply(&mut mirror, &frame_bytes(1, vec![fill(0, 0, 1, 1)])),
        Err(CanvasStateError::NoGeneration)
    );
    let mut mirror = started();
    assert_eq!(
        apply(&mut mirror, &frame_bytes(1, vec![fill(0, 0, 1, 1)])),
        Ok(())
    );
    assert_eq!(
        apply(&mut mirror, &frame_bytes(1, vec![fill(0, 0, 1, 1)])),
        Err(CanvasStateError::FenceOrder)
    );
    assert!(mirror.state().needs_reset());
    assert_eq!(
        apply(&mut mirror, &frame_bytes(5, vec![fill(0, 0, 1, 1)])),
        Err(CanvasStateError::Broken)
    );
    assert_eq!(mirror.digest().unwrap().fence, 1);
    assert_eq!(apply(&mut mirror, &reset_bytes(2, WIDTH, HEIGHT)), Ok(()));
    assert!(!mirror.state().needs_reset());
    // The old generation's frames are refused after the reset.
    assert_eq!(
        apply(&mut mirror, &frame_bytes(9, vec![fill(0, 0, 1, 1)])),
        Err(CanvasStateError::Scope)
    );
    // Another monitor's record never touches this state.
    let mut mirror = started();
    let other = CanvasReset {
        scope: CanvasScope {
            monitor_id: 2,
            ..scope(1)
        },
        reason: CanvasResetReason::Start,
        format: CanvasPixelFormat::Bgra8,
        width: 16,
        height: 16,
    }
    .encode()
    .unwrap();
    assert_eq!(apply(&mut mirror, &other), Err(CanvasStateError::Monitor));
    assert!(!mirror.state().needs_reset());
}

#[test]
fn geometry_needing_the_canvas_size_is_checked_by_the_state() {
    let refused = |commands: Vec<CanvasCommand<'_>>, error: CanvasStateError| {
        let mut mirror = started();
        assert_eq!(apply(&mut mirror, &frame_bytes(1, commands)), Err(error));
    };
    refused(vec![fill(320, 0, 11, 1)], CanvasStateError::Bounds);
    refused(
        vec![CanvasCommand::Copy {
            src: CanvasRect {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
            },
            dst_x: 320,
            dst_y: 0,
        }],
        CanvasStateError::Bounds,
    );
    let residuals = |width: u32, height: u32| vec![0_u8; (width * height * 6) as usize];
    let short = residuals(8, 16);
    refused(
        vec![CanvasCommand::Tile(arcen_protocol::canvas::CanvasTile {
            rect: CanvasRect {
                x: 16,
                y: 0,
                width: 8,
                height: 16,
            },
            codec: arcen_protocol::refine::CODEC_DELTA_ZSTD1,
            depth: CANVAS_TILE_DEPTH,
            payload: &short,
        })],
        CanvasStateError::Alignment,
    );
    // The same narrow tile at the right edge is the canvas's last column.
    let edge = residuals(10, 14);
    let mut mirror = started();
    assert_eq!(
        apply(
            &mut mirror,
            &frame_bytes(
                1,
                vec![CanvasCommand::Tile(arcen_protocol::canvas::CanvasTile {
                    rect: CanvasRect {
                        x: 320,
                        y: 192,
                        width: 10,
                        height: 14,
                    },
                    codec: arcen_protocol::refine::CODEC_DELTA_ZSTD1,
                    depth: CANVAS_TILE_DEPTH,
                    payload: &edge,
                })]
            )
        ),
        Ok(())
    );
    // A canvas larger than the Deck offered.
    let mut mirror = CanvasMirror::new(
        1,
        CanvasTerms {
            max_canvas_px: 256,
            ..terms()
        },
    );
    assert_eq!(
        apply(&mut mirror, &reset_bytes(1, 257, 16)),
        Err(CanvasStateError::CanvasTooLarge)
    );
}

fn paint<'a>(
    surface: u8,
    idr: bool,
    dest: TileRect,
    blocks: &[u32],
    au: &'a [u8],
) -> CanvasCommand<'a> {
    CanvasCommand::SurfacePaint(SurfacePaint {
        surface,
        idr,
        dest: canvas_rect(dest),
        blocks: BlockRuns::from_sorted_indices(blocks.iter().copied()),
        access_unit: au,
    })
}

fn open(surface: u8) -> CanvasCommand<'static> {
    CanvasCommand::SurfaceOpen {
        surface,
        codec: SURFACE_CODEC_H264,
        width: 32,
        height: 32,
    }
}

#[test]
fn surfaces_open_with_an_idr_and_paint_only_inside_their_destination() {
    let au = [0, 0, 0, 1, 0x65];
    let dest = TileRect {
        x: 0,
        y: 0,
        width: 32,
        height: 32,
    };
    let cols = RefineGrid {
        width: WIDTH,
        height: HEIGHT,
    }
    .cols();
    let refused = |commands: Vec<CanvasCommand<'_>>, error: CanvasStateError| {
        let mut mirror = started();
        assert_eq!(apply(&mut mirror, &frame_bytes(1, commands)), Err(error));
    };
    refused(
        vec![paint(0, true, dest, &[0], &au)],
        CanvasStateError::SurfaceClosed,
    );
    refused(
        vec![open(0), paint(0, false, dest, &[0], &au)],
        CanvasStateError::NeedsIdr,
    );
    refused(vec![open(2)], CanvasStateError::SurfaceLimit);
    refused(
        vec![CanvasCommand::SurfaceClose { surface: 1 }],
        CanvasStateError::SurfaceClosed,
    );
    // Block 2 lies right of the 32-pixel destination.
    refused(
        vec![open(0), paint(0, true, dest, &[0, 2], &au)],
        CanvasStateError::Alignment,
    );
    let mut mirror = started();
    assert_eq!(
        apply(
            &mut mirror,
            &frame_bytes(
                1,
                vec![
                    open(0),
                    paint(0, true, dest, &[0, 1, cols, cols + 1], &au),
                    paint(0, false, dest, &[], &au),
                ]
            )
        ),
        Ok(())
    );
    for index in [0, 1, cols, cols + 1] {
        assert!(mirror.state().is_lossy(index));
    }
    assert!(!mirror.state().is_lossy(2));
    // Re-opening asks for a new IDR.
    assert_eq!(
        apply(
            &mut mirror,
            &frame_bytes(2, vec![open(0), paint(0, false, dest, &[0], &au)])
        ),
        Err(CanvasStateError::NeedsIdr)
    );
}

#[test]
fn lossiness_moves_with_copies_and_cache_entries() {
    let au = [0, 0, 0, 1, 0x65];
    let dest = TileRect {
        x: 0,
        y: 0,
        width: 16,
        height: 16,
    };
    let mut mirror = started();
    let cols = mirror.state().grid().unwrap().cols();
    apply(
        &mut mirror,
        &frame_bytes(
            1,
            vec![
                open(0),
                paint(0, true, dest, &[0], &au),
                // A copy of the lossy block to block 5, half a block down:
                // blocks 5 and cols + 5 take its lossiness.
                CanvasCommand::Copy {
                    src: canvas_rect(dest),
                    dst_x: 80,
                    dst_y: 8,
                },
                CanvasCommand::CacheStore {
                    slot: 0,
                    rect: canvas_rect(TileRect {
                        x: 0,
                        y: 0,
                        width: 32,
                        height: 16,
                    }),
                },
                CanvasCommand::CachePaint {
                    slot: 0,
                    dst_x: 160,
                    dst_y: 160,
                },
            ],
        ),
    )
    .unwrap();
    let state = mirror.state();
    assert!(state.is_lossy(5) && state.is_lossy(cols + 5));
    assert!(!state.is_lossy(6));
    let painted = 10 * cols + 10;
    assert!(state.is_lossy(painted), "the lossy half of the entry");
    assert!(!state.is_lossy(painted + 1), "the exact half of the entry");
    assert_eq!(state.cache_bytes(), 32 * 16 * 4);
    // A fill over a whole block makes it exact again; over part of one it
    // does not.
    apply(
        &mut mirror,
        &frame_bytes(2, vec![fill(80, 0, 16, 16), fill(80, 16, 16, 4)]),
    )
    .unwrap();
    assert!(!mirror.state().is_lossy(5));
    assert!(mirror.state().is_lossy(cols + 5));
}

#[test]
fn the_cache_keeps_to_the_decks_slots_and_bytes() {
    let store = |slot: u32, width: u16| CanvasCommand::CacheStore {
        slot,
        rect: CanvasRect {
            x: 0,
            y: 0,
            width,
            height: 64,
        },
    };
    let small = CanvasTerms {
        cache_bytes: 2 * 256 * 64 * 4,
        cache_slots: 2,
        ..terms()
    };
    let mut mirror = CanvasMirror::new(1, small);
    apply(&mut mirror, &reset_bytes(1, WIDTH, HEIGHT)).unwrap();
    apply(
        &mut mirror,
        &frame_bytes(1, vec![store(0, 256), store(1, 256), store(1, 128)]),
    )
    .unwrap();
    assert_eq!(mirror.state().cache_bytes(), (256 + 128) * 64 * 4);
    apply(
        &mut mirror,
        &frame_bytes(2, vec![CanvasCommand::CacheEvict { slot: 1 }]),
    )
    .unwrap();
    assert_eq!(mirror.state().cache_bytes(), 256 * 64 * 4);
    let refused = |commands: Vec<CanvasCommand<'_>>, error: CanvasStateError| {
        let mut mirror = CanvasMirror::new(1, small);
        apply(&mut mirror, &reset_bytes(1, WIDTH, HEIGHT)).unwrap();
        apply(
            &mut mirror,
            &frame_bytes(1, vec![store(0, 256), store(1, 256)]),
        )
        .unwrap();
        assert_eq!(apply(&mut mirror, &frame_bytes(2, commands)), Err(error));
    };
    refused(vec![store(2, 16)], CanvasStateError::SlotLimit);
    refused(
        vec![
            CanvasCommand::CacheEvict { slot: 1 },
            CanvasCommand::CacheEvict { slot: 1 },
        ],
        CanvasStateError::SlotEmpty,
    );
    let mut mirror = CanvasMirror::new(
        1,
        CanvasTerms {
            cache_bytes: 256 * 64 * 4,
            ..small
        },
    );
    apply(&mut mirror, &reset_bytes(1, WIDTH, HEIGHT)).unwrap();
    assert_eq!(
        apply(
            &mut mirror,
            &frame_bytes(1, vec![store(0, 256), store(1, 16)])
        ),
        Err(CanvasStateError::CacheBudget)
    );
}

// ---------------------------------------------------------- the executor

#[test]
fn overlapping_copies_move_pixels_as_if_through_a_temporary() {
    for (dst_y, src_y) in [(0_u16, 5_u16), (5, 0)] {
        let mut mirror = started();
        let before = scene_frame(Scene::Typing, 3);
        let codes = {
            let mut source = Bgra8Source::new(RefineGrid {
                width: WIDTH,
                height: HEIGHT,
            });
            source.observe(&before, (WIDTH * 4) as usize, whole(WIDTH, HEIGHT));
            let all: Vec<u32> = (0..canvas_block_count(WIDTH, HEIGHT)).collect();
            pack_tiles(&source, &all, &mut ResidualTilePacker).unwrap()
        };
        let commands: Vec<_> = codes.iter().map(super::tile::PackedTile::command).collect();
        apply(&mut mirror, &frame_bytes(1, commands)).unwrap();
        assert_eq!(mirror.pixels().bgra(), &before[..]);
        let src = TileRect {
            x: 3,
            y: u32::from(src_y),
            width: 200,
            height: 150,
        };
        apply(
            &mut mirror,
            &frame_bytes(
                2,
                vec![CanvasCommand::Copy {
                    src: canvas_rect(src),
                    dst_x: 7,
                    dst_y,
                }],
            ),
        )
        .unwrap();
        let mut expected = before.clone();
        for row in 0..src.height {
            let from = (((src.y + row) * WIDTH + src.x) * 4) as usize;
            let to = (((u32::from(dst_y) + row) * WIDTH + 7) * 4) as usize;
            let len = (src.width * 4) as usize;
            expected[to..to + len].copy_from_slice(&before[from..from + len]);
        }
        assert_eq!(mirror.pixels().bgra(), &expected[..], "dst_y {dst_y}");
    }
}

/// A test decoder whose access units name a solid colour.
struct SolidSurfaces {
    picture: Vec<u8>,
    size: (u32, u32),
}

impl SurfaceDecoder for SolidSurfaces {
    fn open(&mut self, _surface: u8, width: u32, height: u32) {
        self.size = (width, height);
    }

    fn close(&mut self, _surface: u8) {}

    fn decode(
        &mut self,
        _surface: u8,
        access_unit: &[u8],
        _idr: bool,
    ) -> Result<Option<SurfacePicture<'_>>, CanvasExecError> {
        let colour = access_unit.get(4..7).ok_or(CanvasExecError::Surface)?;
        let (width, height) = self.size;
        // A left half and a right half, to see the scaling.
        self.picture = (0..height)
            .flat_map(|_| {
                (0..width).flat_map(move |x| {
                    if x < width / 2 {
                        [colour[0], colour[1], colour[2], 0]
                    } else {
                        [9, 9, 9, 0]
                    }
                })
            })
            .collect();
        Ok(Some(SurfacePicture {
            width,
            height,
            bgra: &self.picture,
        }))
    }
}

#[test]
fn a_surface_paint_writes_only_its_blocks_scaled_into_the_destination() {
    let mut mirror = started();
    let cols = mirror.state().grid().unwrap().cols();
    let au = [0, 0, 0, 1, 10, 20, 30];
    let dest = TileRect {
        x: 32,
        y: 16,
        width: 64,
        height: 32,
    };
    let bytes = frame_bytes(
        1,
        vec![
            open(1),
            // Blocks (2, 1), (3, 1) and (4, 2): a run that is split by rows.
            paint(1, true, dest, &[cols + 2, cols + 3, 2 * cols + 4], &au),
        ],
    );
    let mut decoder = SolidSurfaces {
        picture: Vec::new(),
        size: (0, 0),
    };
    mirror
        .apply(
            &CanvasRecord::decode(&bytes).unwrap(),
            &mut ResidualTileDecoder,
            &mut decoder,
        )
        .unwrap();
    let pixel = |x: u32, y: u32| {
        let at = ((y * WIDTH + x) * 4) as usize;
        <[u8; 4]>::try_from(&mirror.pixels().bgra()[at..at + 4]).unwrap()
    };
    // The 32-pixel picture is scaled ×2 into the 64-pixel destination: its
    // left half covers blocks 2 and 3 of the row, its right half block 4.
    assert_eq!(pixel(32, 16), [10, 20, 30, 255]);
    assert_eq!(pixel(63, 31), [10, 20, 30, 255]);
    assert_eq!(pixel(64, 32), [9, 9, 9, 255]);
    // Unlisted blocks inside the destination keep the canvas.
    assert_eq!(pixel(32, 32), [0, 0, 0, 255]);
    assert_eq!(pixel(80, 16), [0, 0, 0, 255]);
}

#[test]
fn work_lists_runs_as_row_spans_for_gpu_executors() {
    let mut state = CanvasState::new(1, terms());
    state
        .apply(&CanvasRecord::decode(&reset_bytes(1, 40, 40)).unwrap())
        .unwrap();
    // A 40 × 40 canvas has 3 × 3 blocks; the run 1..=4 crosses a row.
    let au = [0, 0, 0, 1, 0x65];
    let bytes = CanvasFrame {
        scope: scope(1),
        fence: 1,
        timestamp_ms: 7,
        block_count: 9,
        commands: vec![open(0), paint(0, true, whole(40, 40), &[1, 2, 3, 4], &au)],
    }
    .encode()
    .unwrap();
    let work = state.apply(&CanvasRecord::decode(&bytes).unwrap()).unwrap();
    assert_eq!((work.fence, work.timestamp_ms), (1, 7));
    let CanvasOp::SurfacePaint { blocks, .. } = &work.ops[1] else {
        panic!("a paint");
    };
    assert_eq!(
        blocks,
        &vec![
            TileRect {
                x: 16,
                y: 0,
                width: 24,
                height: 16
            },
            TileRect {
                x: 0,
                y: 16,
                width: 32,
                height: 16
            },
        ]
    );
}

#[test]
fn the_canvas_contract_is_eight_bit_srgb_rgb() {
    let bytes = canvas_contract_bytes();
    assert_eq!(
        (
            bytes.domain,
            bytes.depth,
            bytes.transfer,
            bytes.primaries,
            bytes.matrix,
            bytes.range,
            bytes.chroma
        ),
        (1, 8, 13, 1, 0, 1, 2)
    );
    // The canvas digest is refine's digest: a reset canvas is all black and
    // exact.
    let mirror = started();
    let digest = mirror.digest().unwrap();
    assert_eq!(digest.covered, canvas_block_count(WIDTH, HEIGHT));
    assert_eq!(digest.fence, 0);
    let mut black = started();
    apply(&mut black, &frame_bytes(1, vec![fill(0, 0, 330, 206)])).unwrap();
    assert_ne!(black.digest().unwrap().digest, digest.digest);
}

#[test]
fn a_frame_may_write_at_most_four_canvases_of_area() {
    let whole_fill = || fill(0, 0, 330, 206);
    let mut mirror = started();
    assert_eq!(
        apply(&mut mirror, &frame_bytes(1, vec![whole_fill(); 4])),
        Ok(())
    );
    let mut mirror = started();
    assert_eq!(
        apply(&mut mirror, &frame_bytes(1, vec![whole_fill(); 5])),
        Err(CanvasStateError::WorkBudget)
    );
    // Painted blocks count too, at a whole block each.
    let au = [0, 0, 0, 1, 0x65];
    let all: Vec<u32> = (0..canvas_block_count(WIDTH, HEIGHT)).collect();
    let paints = (0..5).map(|index| paint(0, index == 0, whole(WIDTH, HEIGHT), &all, &au));
    let mut mirror = started();
    assert_eq!(
        apply(
            &mut mirror,
            &frame_bytes(1, std::iter::once(open(0)).chain(paints).collect())
        ),
        Err(CanvasStateError::WorkBudget)
    );
}

/// Records what the executor tells a surface decoder.
#[derive(Default)]
struct SurfaceLog {
    events: Vec<(char, u8)>,
}

impl SurfaceDecoder for SurfaceLog {
    fn open(&mut self, surface: u8, _width: u32, _height: u32) {
        self.events.push(('o', surface));
    }

    fn close(&mut self, surface: u8) {
        self.events.push(('c', surface));
    }

    fn decode(
        &mut self,
        surface: u8,
        _access_unit: &[u8],
        _idr: bool,
    ) -> Result<Option<SurfacePicture<'_>>, CanvasExecError> {
        self.events.push(('d', surface));
        Ok(None)
    }
}

#[test]
fn a_reset_closes_every_surface_decoder() {
    let mut log = SurfaceLog::default();
    let mut mirror = CanvasMirror::new(1, terms());
    let au = [0, 0, 0, 1, 0x65];
    let dest = whole(32, 32);
    for record in [
        reset_bytes(1, WIDTH, HEIGHT),
        frame_bytes(1, vec![open(1), paint(1, true, dest, &[0], &au)]),
        reset_bytes(2, WIDTH, HEIGHT),
    ] {
        mirror
            .apply(
                &CanvasRecord::decode(&record).unwrap(),
                &mut ResidualTileDecoder,
                &mut log,
            )
            .unwrap();
    }
    let closes = |events: &[(char, u8)]| {
        events
            .iter()
            .filter(|(kind, _)| *kind == 'c')
            .map(|(_, surface)| *surface)
            .collect::<Vec<_>>()
    };
    assert_eq!(&log.events[4..6], &[('o', 1), ('d', 1)]);
    assert_eq!(closes(&log.events[6..]), vec![0, 1, 2, 3]);
}
