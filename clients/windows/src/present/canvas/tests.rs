// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The D3D11 canvas against its oracle, the shared CPU executor
//! (`arcen_media::canvas::CanvasPixels`): every record goes through the
//! Deck's own `CanvasState`, then through both executors, and the GPU
//! canvas read back must equal the CPU canvas exactly on every lossless
//! block and within one code on surface (lossy) blocks.
//!
//! Two corpora: hand-made records aimed at the executor's edges
//! (overlapping copies in every direction, fills in order, edge tiles, the
//! cache, scaled surface paints of listed blocks, a resize), and the canvas
//! planner over Keel's scenarios, packing real `delta-zstd1` tiles. Surface
//! access units name a synthetic NV12 picture, which the GPU side uploads
//! as the decoder would hand it over and the CPU side converts with the
//! shared reference (`arcen_media::video::present`).
//!
//! `cargo test` runs them on WARP; the ignored `..._on_every_hardware_adapter`
//! test runs them on each GPU of the machine.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "test rigs over records and pictures they build themselves"
)]

use super::*;
use crate::present::device::AdapterChoice;
use crate::present::selftest::{AUTO, Picture};
use arcen_deck_core::decode::{NativeDecodedVideoFrame, NativePictureFormat};
use arcen_keel::BgraFrame;
use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_media::VideoConfiguration;
use arcen_media::canvas::pixels::ZstdTileDecoder;
use arcen_media::canvas::tile::{TilePacker, ZstdTilePacker};
use arcen_media::canvas::{
    CanvasExecError, CanvasGovernor, CanvasPixels, CanvasPlanner, CanvasState, CostModel, CpuShare,
    GovernorConfig, PlannerConfig, SurfaceDecoder, SurfaceEncodeError, SurfaceEncodeRequest,
    SurfaceEncoder, SurfacePicture, TileDecoder,
};
use arcen_media::refine::tile::{RefineGrid, TileCodes};
use arcen_media::video::PresentTiming;
use arcen_protocol::canvas::{
    CanvasCommand, CanvasControlMsg, CanvasFrame, CanvasPixelFormat, CanvasRecord, CanvasRect,
    CanvasReset, CanvasResetReason, CanvasScope, CanvasTerms, CanvasTile, SURFACE_CODEC_H264,
    SurfacePaint, canvas_block_count,
};
use arcen_protocol::refine::{BlockRuns, CODEC_DELTA_ZSTD1};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

/// The Deck's terms (`arcen_deck_core::canvas`): two surfaces, the default
/// 100 MiB cache in 4 KiB slots.
fn terms() -> CanvasTerms {
    CanvasTerms {
        max_canvas_px: MAX_CANVAS_PX,
        max_surfaces_per_monitor: 2,
        cache_bytes: 100 * 1024 * 1024,
        cache_slots: 25_600,
    }
}

// ------------------------------------------------------------- surfaces

/// A surface access unit: `[width u16, height u16, seed u32]`, big endian;
/// it decodes to `Picture::synthetic(AUTO, width, height, seed)`.
fn access_unit(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let mut au = u16::try_from(width).unwrap().to_be_bytes().to_vec();
    au.extend_from_slice(&u16::try_from(height).unwrap().to_be_bytes());
    au.extend_from_slice(&seed.to_be_bytes());
    au
}

fn picture_of(access_unit: &[u8]) -> Picture {
    let width = u32::from(u16::from_be_bytes([access_unit[0], access_unit[1]]));
    let height = u32::from(u16::from_be_bytes([access_unit[2], access_unit[3]]));
    let seed = u32::from_be_bytes([
        access_unit[4],
        access_unit[5],
        access_unit[6],
        access_unit[7],
    ]);
    Picture::synthetic(AUTO, width, height, seed)
}

/// The CPU side's surface decoder: the shared reference conversion of the
/// access unit's picture, rounded to eight-bit codes.
#[derive(Default)]
struct ReferenceSurfaces {
    current: Vec<u8>,
}

impl SurfaceDecoder for ReferenceSurfaces {
    fn open(&mut self, _surface: u8, _width: u32, _height: u32) {}

    fn close(&mut self, _surface: u8) {}

    fn decode(
        &mut self,
        _surface: u8,
        access_unit: &[u8],
        _idr: bool,
    ) -> std::result::Result<Option<SurfacePicture<'_>>, CanvasExecError> {
        let picture = picture_of(access_unit);
        let (width, height) = (picture.width, picture.height);
        let uniform = VideoUniform::from_contract(AUTO, (width, height), (width / 2, height / 2));
        self.current.clear();
        for y in 0..height {
            for x in 0..width {
                let [red, green, blue] = picture
                    .reference(&uniform, x, y)
                    .map(|code| (code.clamp(0.0, 1.0) * 255.0).round() as u8);
                self.current.extend_from_slice(&[blue, green, red, 255]);
            }
        }
        Ok(Some(SurfacePicture {
            width,
            height,
            bgra: &self.current,
        }))
    }
}

/// The GPU side's input for one operation, as the media worker prepares
/// it: tile codes, or the surface's picture as the decoder hands it over.
fn gpu_input(gpu: &Gpu, op: &CanvasOp<'_>) -> CanvasOpInput<D3d11Picture> {
    match op {
        CanvasOp::Tile { rect, payload } => CanvasOpInput::Tile(
            ZstdTileDecoder
                .decode(payload, rect.width, rect.height)
                .expect("tile decodes"),
        ),
        CanvasOp::SurfacePaint { access_unit, .. } => {
            let texture = picture_of(access_unit)
                .upload(gpu, DXGI_FORMAT_NV12)
                .expect("NV12 upload");
            CanvasOpInput::Surface(Some(NativeDecodedVideoFrame {
                picture: D3d11Picture::texture(texture),
                format: NativePictureFormat::Nv12,
                video: VideoConfiguration::legacy_h264(),
            }))
        }
        _ => CanvasOpInput::None,
    }
}

// ------------------------------------------------------------------ rig

/// What one comparison covered: lossless pixels (all exact, or the
/// comparison panics) and surface pixels with their worst difference.
#[derive(Debug, Default, Clone, Copy)]
struct Difference {
    lossless_pixels: usize,
    lossy_pixels: usize,
    lossy_worst: u8,
}

struct Rig {
    gpu: Gpu,
    executor: D3d11CanvasExecutor,
    state: CanvasState,
    cpu: CanvasPixels,
    surfaces: ReferenceSurfaces,
    records: usize,
}

impl Rig {
    fn new(gpu: Gpu) -> Self {
        Self {
            gpu,
            executor: D3d11CanvasExecutor::new(),
            state: CanvasState::new(0, terms()),
            cpu: CanvasPixels::new(),
            surfaces: ReferenceSurfaces::default(),
            records: 0,
        }
    }

    /// Applies one record to both executors; with `compare`, reads the GPU
    /// canvas back and compares it with the CPU one.
    fn apply(&mut self, record: &CanvasRecord<'_>, compare: bool) -> Option<Difference> {
        self.records += 1;
        let work = self.state.apply(record).expect("the state owner admits");
        let inputs: Vec<_> = work.ops.iter().map(|op| gpu_input(&self.gpu, op)).collect();
        let picture = self
            .executor
            .execute(
                &self.gpu,
                &CanvasExecution {
                    work: &work,
                    inputs: &inputs,
                    arrival: Instant::now(),
                    timing: PresentTiming::Newest,
                    present: true,
                    readback: compare,
                },
            )
            .expect("the GPU executes");
        assert!(picture.is_whole());
        self.cpu
            .execute(&work, &mut ZstdTileDecoder, &mut self.surfaces)
            .expect("the CPU executes");
        if !compare {
            return None;
        }
        let readback = self.executor.take_readback().expect("a readback");
        assert_eq!(readback.monitor_id, work.monitor_id);
        assert_eq!(readback.generation, work.generation);
        assert_eq!(readback.fence, work.fence);
        Some(self.compare(&readback))
    }

    fn compare(&self, readback: &CanvasReadback) -> Difference {
        let (width, height) = (self.cpu.width(), self.cpu.height());
        assert_eq!(readback.stride, self.cpu.stride());
        assert_eq!(readback.bgra.len(), self.cpu.bgra().len());
        let grid = RefineGrid { width, height };
        let mut difference = Difference::default();
        for index in 0..grid.block_count() {
            let block = grid.block_rect(index).unwrap();
            let lossy = self.state.is_lossy(index);
            for y in block.y..block.y + block.height {
                for x in block.x..block.x + block.width {
                    let at = y as usize * readback.stride + x as usize * 4;
                    let gpu = &readback.bgra[at..at + 4];
                    let cpu = &self.cpu.bgra()[at..at + 4];
                    let worst = (0..4).map(|c| gpu[c].abs_diff(cpu[c])).max().unwrap();
                    if lossy {
                        difference.lossy_pixels += 1;
                        difference.lossy_worst = difference.lossy_worst.max(worst);
                    } else {
                        difference.lossless_pixels += 1;
                        assert!(
                            worst == 0,
                            "record {}: lossless pixel ({x}, {y}) of a {width}x{height} canvas: \
                             GPU {gpu:?} CPU {cpu:?}",
                            self.records
                        );
                    }
                }
            }
        }
        assert!(
            difference.lossy_worst <= 1,
            "record {}: surface pixels differ by {} codes",
            self.records,
            difference.lossy_worst
        );
        difference
    }
}

// ----------------------------------------------------------- hand-made

fn scope(generation: u32) -> CanvasScope {
    CanvasScope {
        monitor_id: 0,
        topology_generation: 0,
        stream_epoch: 0,
        generation: NonZeroU32::new(generation).unwrap(),
    }
}

fn rect(x: u32, y: u32, width: u32, height: u32) -> CanvasRect {
    let short = |value: u32| u16::try_from(value).unwrap();
    CanvasRect {
        x: short(x),
        y: short(y),
        width: short(width),
        height: short(height),
    }
}

fn reset(generation: u32, width: u32, height: u32) -> CanvasRecord<'static> {
    CanvasRecord::Reset(CanvasReset {
        scope: scope(generation),
        reason: CanvasResetReason::Start,
        format: CanvasPixelFormat::Bgra8,
        width,
        height,
    })
}

fn frame(
    generation: u32,
    fence: u64,
    (width, height): (u32, u32),
    commands: Vec<CanvasCommand<'_>>,
) -> CanvasRecord<'_> {
    CanvasRecord::Frame(CanvasFrame {
        scope: scope(generation),
        fence,
        timestamp_ms: u32::try_from(fence).unwrap(),
        block_count: canvas_block_count(width, height),
        commands,
    })
}

fn fill(x: u32, y: u32, width: u32, height: u32, rgb: [u8; 3]) -> CanvasCommand<'static> {
    CanvasCommand::Fill {
        rect: rect(x, y, width, height),
        rgb,
    }
}

/// A tile payload of noisy codes (every value 0..=255 appears).
fn tile_payload(packer: &mut ZstdTilePacker, width: u32, height: u32, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    let samples = (0..width * height * 3)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            u16::try_from(state % 256).unwrap()
        })
        .collect();
    packer
        .pack(&TileCodes::new(width, height, samples).unwrap())
        .unwrap()
}

/// Every executor path on a canvas whose size is not a multiple of the
/// block, then a resize.
fn handmade(gpu: &Gpu) -> Vec<Difference> {
    const SIZE: (u32, u32) = (200, 120);
    let (w, h) = SIZE;
    let mut rig = Rig::new(gpu.clone());
    let mut packer = ZstdTilePacker::new().unwrap();
    let mut out = Vec::new();
    rig.apply(&reset(1, w, h), false);
    // Tiles everywhere (edge tiles are narrower and shorter), so copies
    // move distinct pixels.
    let mut payloads = Vec::new();
    for ty in 0..h.div_ceil(64) {
        for tx in 0..w.div_ceil(64) {
            let (x, y) = (tx * 64, ty * 64);
            let (tw, th) = (64.min(w - x), 64.min(h - y));
            let payload = tile_payload(&mut packer, tw, th, tx * 31 + ty);
            payloads.push((rect(x, y, tw, th), payload));
        }
    }
    let tiles = payloads
        .iter()
        .map(|(rect, payload)| {
            CanvasCommand::Tile(CanvasTile {
                rect: *rect,
                codec: CODEC_DELTA_ZSTD1,
                depth: 8,
                payload,
            })
        })
        .collect();
    out.extend(rig.apply(&frame(1, 1, SIZE, tiles), true));
    // Fills, overlapping, at any position: the later one wins.
    out.extend(rig.apply(
        &frame(
            1,
            2,
            SIZE,
            vec![
                fill(3, 5, 50, 40, [255, 0, 7]),
                fill(20, 17, 9, 101, [1, 128, 254]),
                fill(199, 119, 1, 1, [17, 34, 51]),
            ],
        ),
        true,
    ));
    // Overlapping copies in every direction, then disjoint ones.
    let mut fence = 3;
    for (sx, sy, dx, dy) in [
        (0, 0, 7, 0),
        (7, 0, 0, 0),
        (0, 0, 0, 9),
        (0, 9, 0, 0),
        (10, 10, 13, 21),
        (13, 21, 10, 10),
        (30, 4, 26, 11),
        (26, 11, 30, 4),
        (0, 0, 120, 60),
        (120, 60, 0, 0),
    ] {
        let copy = CanvasCommand::Copy {
            src: rect(sx, sy, 80, 60),
            dst_x: u16::try_from(dx).unwrap(),
            dst_y: u16::try_from(dy).unwrap(),
        };
        out.extend(rig.apply(&frame(1, fence, SIZE, vec![copy]), true));
        fence += 1;
    }
    // A fill, a copy of it and another fill in one record: order holds
    // across the batched fills and the copy.
    out.extend(rig.apply(
        &frame(
            1,
            fence,
            SIZE,
            vec![
                fill(0, 0, 16, 16, [9, 9, 9]),
                CanvasCommand::Copy {
                    src: rect(0, 0, 16, 16),
                    dst_x: 16,
                    dst_y: 0,
                },
                fill(8, 0, 16, 8, [200, 100, 50]),
            ],
        ),
        true,
    ));
    fence += 1;
    // The cache: store, overwrite the source, paint it back elsewhere,
    // an edge entry, evict.
    out.extend(rig.apply(
        &frame(
            1,
            fence,
            SIZE,
            vec![
                CanvasCommand::CacheStore {
                    slot: 7,
                    rect: rect(16, 16, 32, 48),
                },
                fill(16, 16, 32, 48, [0, 0, 0]),
                CanvasCommand::CachePaint {
                    slot: 7,
                    dst_x: 160,
                    dst_y: 64,
                },
                CanvasCommand::CacheStore {
                    slot: 8,
                    rect: rect(192, 112, 8, 8),
                },
                CanvasCommand::CachePaint {
                    slot: 8,
                    dst_x: 0,
                    dst_y: 112,
                },
                CanvasCommand::CacheEvict { slot: 7 },
            ],
        ),
        true,
    ));
    fence += 1;
    // Surfaces: 1:1, scaled up, scaled down, each writing only its runs.
    let grid = RefineGrid {
        width: w,
        height: h,
    };
    let cols = grid.cols();
    let runs = |rows: &[(u32, u32, u32)]| {
        BlockRuns::new(
            rows.iter()
                .map(|&(row, col, len)| (row * cols + col, len))
                .collect(),
            grid.block_count(),
        )
        .unwrap()
    };
    let au_a = access_unit(64, 48, 3);
    let au_b = access_unit(32, 32, 5);
    let au_c = access_unit(96, 64, 9);
    let open = |surface, width: u16, height: u16| CanvasCommand::SurfaceOpen {
        surface,
        codec: SURFACE_CODEC_H264,
        width,
        height,
    };
    out.extend(rig.apply(
        &frame(
            1,
            fence,
            SIZE,
            vec![
                open(0, 64, 48),
                CanvasCommand::SurfacePaint(SurfacePaint {
                    surface: 0,
                    idr: true,
                    dest: rect(16, 16, 64, 48),
                    blocks: runs(&[(1, 1, 4), (2, 1, 4), (3, 1, 4)]),
                    access_unit: &au_a,
                }),
                open(1, 32, 32),
                CanvasCommand::SurfacePaint(SurfacePaint {
                    surface: 1,
                    idr: true,
                    dest: rect(96, 16, 104, 104),
                    blocks: runs(&[(1, 6, 7), (3, 6, 3), (5, 9, 4), (7, 6, 7)]),
                    access_unit: &au_b,
                }),
            ],
        ),
        true,
    ));
    fence += 1;
    out.extend(rig.apply(
        &frame(
            1,
            fence,
            SIZE,
            vec![
                open(0, 96, 64),
                CanvasCommand::SurfacePaint(SurfacePaint {
                    surface: 0,
                    idr: true,
                    dest: rect(0, 64, 48, 32),
                    blocks: runs(&[(4, 0, 3), (5, 1, 2)]),
                    access_unit: &au_c,
                }),
                fill(4, 70, 4, 4, [1, 2, 3]),
            ],
        ),
        true,
    ));
    fence += 1;
    // Lossless pixels copied over a painted area are exact again.
    out.extend(rig.apply(
        &frame(
            1,
            fence,
            SIZE,
            vec![CanvasCommand::Copy {
                src: rect(160, 64, 32, 48),
                dst_x: 16,
                dst_y: 16,
            }],
        ),
        true,
    ));
    // A resize: a new canvas, black, then written.
    let resized = (72, 40);
    rig.apply(&reset(2, resized.0, resized.1), false);
    out.extend(rig.apply(
        &frame(
            2,
            1,
            resized,
            vec![
                fill(64, 32, 8, 8, [10, 20, 30]),
                CanvasCommand::Copy {
                    src: rect(60, 30, 12, 10),
                    dst_x: 0,
                    dst_y: 0,
                },
            ],
        ),
        true,
    ));
    out
}

// --------------------------------------------------------------- Keel

/// A surface encoder whose access units name a synthetic picture of the
/// coded size.
#[derive(Default)]
struct SyntheticEncoder {
    pictures: u32,
}

impl SurfaceEncoder for SyntheticEncoder {
    fn open(
        &mut self,
        _surface: u8,
        _width: u32,
        _height: u32,
    ) -> std::result::Result<(), SurfaceEncodeError> {
        Ok(())
    }

    fn close(&mut self, _surface: u8) {}

    fn encode(
        &mut self,
        request: &SurfaceEncodeRequest<'_>,
    ) -> std::result::Result<Vec<u8>, SurfaceEncodeError> {
        self.pictures += 1;
        Ok(access_unit(
            request.coded_width,
            request.coded_height,
            self.pictures * 977 + u32::from(request.surface),
        ))
    }
}

/// The canvas planner over a Keel scenario, every record also applied by
/// the Deck rig, compared every eighth tick and at the end.
fn keel_scenario(
    gpu: &Gpu,
    kind: ScenarioKind,
    (width, height): (u32, u32),
    ticks: u64,
) -> Vec<Difference> {
    const TICK: Duration = Duration::from_nanos(1_000_000_000 / 60);
    let scenario = Scenario::new(width as usize, height as usize, kind, 7);
    let mut config = GovernorConfig::new(60, 8, CpuShare::default(), 20_000_000);
    config.learn = false;
    let mut governor = CanvasGovernor::new(config, CostModel::default());
    let mut planner =
        CanvasPlanner::new(0, 0, 0, width, height, terms(), PlannerConfig::default()).unwrap();
    let mut encoder = SyntheticEncoder::default();
    let mut packer = ZstdTilePacker::new().unwrap();
    let mut rig = Rig::new(gpu.clone());
    let mut pixels = Vec::new();
    let mut out = Vec::new();
    for tick in 0..ticks {
        scenario.render(tick, &mut pixels);
        let frame = || {
            BgraFrame::new(&pixels, width as usize, height as usize, width as usize * 4).unwrap()
        };
        let now = TICK * u32::try_from(tick + 1).unwrap();
        let demand = planner
            .observe(frame(), None, now, governor.costs(), governor.scale())
            .unwrap();
        let (grants, _) = governor.allocate(&[demand]);
        let tick_records = planner
            .plan(
                frame(),
                &grants[0],
                u32::try_from(tick).unwrap(),
                &mut encoder,
                &mut packer,
            )
            .unwrap();
        let count = tick_records.records.len();
        for (index, bytes) in tick_records.records.iter().enumerate() {
            let record = CanvasRecord::decode(bytes).unwrap();
            let compare = index + 1 == count && (tick % 8 == 7 || tick + 1 == ticks);
            out.extend(rig.apply(&record, compare));
        }
        let outcome = tick_records.outcome(0);
        governor.complete(std::slice::from_ref(&outcome));
        governor.on_control(&CanvasControlMsg::CanvasAck {
            monitor_id: 0,
            generation: outcome.generation,
            fence: outcome.fence,
            unprocessed_bytes: 0,
            decode_ms_p95: 2,
        });
    }
    // The oracle the GPU canvas matched is the planner's mirror.
    assert_eq!(
        arcen_media::canvas::canvas_digest(&rig.state, rig.cpu.bgra(), rig.cpu.stride()),
        planner.mirror().digest(),
        "{kind:?}: the Deck's canvas replays the Pier's mirror"
    );
    out
}

/// Runs every corpus on `gpu`; one summary line per corpus.
fn corpora(gpu: &Gpu) -> Vec<String> {
    let mut lines = Vec::new();
    let mut summarise = |name: &str, differences: &[Difference]| {
        assert!(!differences.is_empty(), "{name}: nothing compared");
        let lossless: usize = differences.iter().map(|d| d.lossless_pixels).sum();
        let lossy: usize = differences.iter().map(|d| d.lossy_pixels).sum();
        let worst = differences.iter().map(|d| d.lossy_worst).max().unwrap_or(0);
        lines.push(format!(
            "{name}: {} comparisons, {lossless} lossless pixels exact, {lossy} surface pixels \
             within {worst}",
            differences.len()
        ));
        lossy
    };
    let painted = summarise("handmade", &handmade(gpu));
    assert!(painted > 0, "the hand-made corpus paints surfaces");
    let mut painted = 0;
    for kind in [
        ScenarioKind::Typing,
        ScenarioKind::Drag,
        ScenarioKind::Scroll,
        ScenarioKind::DocumentScroll,
        ScenarioKind::Editor,
        ScenarioKind::Video,
        ScenarioKind::Burst,
    ] {
        painted += summarise(
            &format!("keel {kind:?} 328x200"),
            &keel_scenario(gpu, kind, (328, 200), 40),
        );
    }
    assert!(painted > 0, "Keel's video scenarios paint surfaces");
    summarise(
        "keel DocumentScroll 1280x720",
        &keel_scenario(gpu, ScenarioKind::DocumentScroll, (1280, 720), 24),
    );
    lines
}

#[test]
fn the_canvas_matches_the_cpu_executor_on_warp() {
    let gpu = Gpu::create(&AdapterChoice::Warp).expect("WARP device");
    for line in corpora(&gpu) {
        println!("WARP {line}");
    }
}

/// The same corpora on every hardware adapter of the machine. Needs a GPU,
/// so it is opt-in:
/// `cargo test -p arcen-deck-windows -- --ignored --nocapture canvas_matches`.
#[test]
#[ignore = "needs a hardware D3D11 adapter"]
fn the_canvas_matches_the_cpu_executor_on_every_hardware_adapter() {
    let hardware: Vec<_> = crate::present::device::adapters()
        .expect("adapters")
        .into_iter()
        .filter(|(_, facts)| !facts.software)
        .collect();
    assert!(!hardware.is_empty(), "no hardware adapter");
    for (_, facts) in hardware {
        let gpu = Gpu::create(&AdapterChoice::Name(facts.name.clone())).expect("device");
        for line in corpora(&gpu) {
            println!("{}: {line}", facts.name);
        }
    }
}

/// A record in parts: the presenter never sees the canvas between them;
/// and a failed operation is reported as an error.
#[test]
fn parts_hold_the_canvas_until_the_last_and_errors_are_reported() {
    let gpu = Gpu::create(&AdapterChoice::Warp).expect("WARP device");
    let mut rig = Rig::new(gpu.clone());
    rig.apply(&reset(1, 64, 32), false);
    let work = rig
        .state
        .apply(&frame(1, 1, (64, 32), vec![fill(0, 0, 8, 8, [1, 2, 3])]))
        .unwrap();
    let inputs = vec![CanvasOpInput::None];
    let execution = |present| CanvasExecution {
        work: &work,
        inputs: &inputs,
        arrival: Instant::now(),
        timing: PresentTiming::Newest,
        present,
        readback: false,
    };
    let part = rig.executor.execute(&gpu, &execution(false)).unwrap();
    assert!(!part.is_whole(), "a part leaves the record half applied");
    let last = rig.executor.execute(&gpu, &execution(true)).unwrap();
    assert!(part.is_whole() && last.is_whole());
    // A record that fails part-way is never shown, until a record
    // succeeds.
    let failing = arcen_media::canvas::CanvasWork {
        ops: vec![
            CanvasOp::Fill {
                rect: arcen_media::refine::tile::TileRect {
                    x: 0,
                    y: 0,
                    width: 8,
                    height: 8,
                },
                bgra: [3, 2, 1, 255],
            },
            CanvasOp::CacheEvict { slot: 3 },
        ],
        ..work.clone()
    };
    let failing_inputs = vec![CanvasOpInput::None, CanvasOpInput::None];
    let failed = CanvasExecution {
        work: &failing,
        inputs: &failing_inputs,
        ..execution(true)
    };
    assert!(rig.executor.execute(&gpu, &failed).is_err());
    assert!(!last.is_whole(), "a failed record leaves the canvas hidden");
    rig.executor.execute(&gpu, &execution(true)).unwrap();
    assert!(last.is_whole());
    // Inputs that do not match the operations, and a slot that holds
    // nothing, are errors.
    let mismatched = CanvasExecution {
        inputs: &[],
        ..execution(true)
    };
    assert!(rig.executor.execute(&gpu, &mismatched).is_err());
    let evict = arcen_media::canvas::CanvasWork {
        ops: vec![CanvasOp::CacheEvict { slot: 3 }],
        ..work.clone()
    };
    let evicting = CanvasExecution {
        work: &evict,
        ..execution(true)
    };
    assert!(rig.executor.execute(&gpu, &evicting).is_err());
    // Before any reset, nothing can be applied.
    let mut fresh = D3d11CanvasExecutor::new();
    assert!(fresh.execute(&gpu, &execution(true)).is_err());
}

/// The presenter thread shows a whole canvas and never one between the
/// parts of a record, and follows a canvas resize. No window is needed (a
/// composition swapchain), but DXGI creates one only in an interactive
/// session (over SSH: `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`), so it is
/// opt-in: `cargo test -p arcen-deck-windows -- --ignored whole_canvases`
/// from a signed-in desktop.
#[test]
#[ignore = "needs an interactive session (DirectComposition swapchain)"]
fn the_presenter_thread_shows_only_whole_canvases() {
    use crate::present::swapchain::{PresentMode, VideoSwapchain};
    use crate::present::thread::PresenterThread;
    use std::sync::atomic::Ordering;
    let gpu = Gpu::create(&AdapterChoice::Warp).expect("WARP device");
    let swapchain = VideoSwapchain::create(&gpu, 64, 32, PresentMode::Sdr8).expect("swapchain");
    let thread =
        PresenterThread::spawn(gpu.clone(), swapchain, None, "canvas test".into()).expect("thread");
    let handle = thread.handle();
    handle.set_timing(PresentTiming::Newest);
    let presented = || handle.counters().presented.load(Ordering::Relaxed);
    let wait_for = |count: u64| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while presented() < count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        presented()
    };
    let mut rig = Rig::new(gpu.clone());
    let record = reset(1, 64, 32);
    let work = rig.state.apply(&record).unwrap();
    let inputs = vec![CanvasOpInput::None];
    let execution = |work, present| CanvasExecution {
        work,
        inputs: &inputs,
        arrival: Instant::now(),
        timing: PresentTiming::Newest,
        present,
        readback: false,
    };
    let canvas = rig.executor.execute(&gpu, &execution(&work, true)).unwrap();
    handle.submit_canvas(canvas, Instant::now());
    assert_eq!(wait_for(1), 1, "a whole canvas is presented");

    let work = rig
        .state
        .apply(&frame(1, 1, (64, 32), vec![fill(0, 0, 8, 8, [1, 2, 3])]))
        .unwrap();
    let part = rig
        .executor
        .execute(&gpu, &execution(&work, false))
        .unwrap();
    handle.submit_canvas(part, Instant::now());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(presented(), 1, "a half-applied record is never presented");
    let last = rig.executor.execute(&gpu, &execution(&work, true)).unwrap();
    handle.submit_canvas(last, Instant::now());
    assert_eq!(wait_for(2), 2, "the record's last part presents it");

    let record = reset(2, 48, 40);
    let work = rig.state.apply(&record).unwrap();
    let resized = rig.executor.execute(&gpu, &execution(&work, true)).unwrap();
    handle.submit_canvas(resized, Instant::now());
    assert_eq!(wait_for(3), 3, "a resized canvas is presented");
    assert!(handle.take_events().iter().all(|event| !matches!(
        event,
        crate::present::thread::PresenterEvent::Failed(_)
            | crate::present::thread::PresenterEvent::DeviceLost(_)
    )));
}
