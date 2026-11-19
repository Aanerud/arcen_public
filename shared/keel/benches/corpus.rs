// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{
    BgraFrame, BlockClassMap, BlockHistory, BlockTraitsMap, ClassConfig, DamageTracker,
    ExternalDamage, KernelPreference, MotionConfig, MotionEstimator, MotionRegions, PixelRect,
    RegionConfig, TileKey, TileLedger,
};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn bench_damage_corpus(criterion: &mut Criterion) {
    let cases = [
        (1792, 1168, ScenarioKind::Idle),
        (1792, 1168, ScenarioKind::Typing),
        (1792, 1168, ScenarioKind::Drag),
        (1792, 1168, ScenarioKind::Scroll),
        (1792, 1168, ScenarioKind::Video),
        (1792, 1168, ScenarioKind::Burst),
        (1920, 1088, ScenarioKind::Typing),
        (1919, 1081, ScenarioKind::Typing),
    ];
    let mut group = criterion.benchmark_group("damage_corpus");
    group.sample_size(20);
    for (width, height, kind) in cases {
        let scenario = Scenario::new(width, height, kind, 42);
        let mut previous = Vec::new();
        let mut current = Vec::new();
        let (previous_tick, current_tick) = if kind == ScenarioKind::Burst {
            (9, 10)
        } else {
            (4, 5)
        };
        scenario.render(previous_tick, &mut previous);
        scenario.render(current_tick, &mut current);
        for preference in [KernelPreference::Xxh3, KernelPreference::Crc32c] {
            let id = format!("{width}x{height}/{kind:?}/{preference:?}");
            group.bench_with_input(
                BenchmarkId::new("scan", id),
                &preference,
                |bench, kernel| {
                    bench.iter_batched(
                        || {
                            let mut tracker = DamageTracker::new(width, height, *kernel)
                                .unwrap_or_else(|error| panic!("valid corpus tracker: {error}"));
                            tracker
                                .update(
                                    BgraFrame::new(&previous, width, height, scenario.stride())
                                        .unwrap_or_else(|error| {
                                            panic!("valid previous corpus frame: {error}")
                                        }),
                                )
                                .unwrap_or_else(|error| panic!("previous update: {error}"));
                            tracker
                        },
                        |mut tracker| {
                            black_box(
                                tracker
                                    .update(
                                        BgraFrame::new(
                                            black_box(&current),
                                            width,
                                            height,
                                            scenario.stride(),
                                        )
                                        .unwrap_or_else(
                                            |error| panic!("valid current corpus frame: {error}"),
                                        ),
                                    )
                                    .unwrap_or_else(|error| panic!("current update: {error}")),
                            )
                        },
                        BatchSize::LargeInput,
                    );
                },
            );
        }
    }
    group.finish();
}

fn bench_external_damage(criterion: &mut Criterion) {
    const SOURCE_BLOCK_SIZE: usize = 16;
    let mut group = criterion.benchmark_group("external_damage");
    group.sample_size(20);

    for (width, height) in [(1792usize, 1168usize), (3840, 2160)] {
        let blocks_wide = width.div_ceil(SOURCE_BLOCK_SIZE);
        let blocks_tall = height.div_ceil(SOURCE_BLOCK_SIZE);
        let block_count = blocks_wide * blocks_tall;
        for pattern in ["idle", "sparse", "full"] {
            let mut block_map = vec![0u8; block_count];
            match pattern {
                "idle" => {}
                "sparse" => {
                    for index in (0..block_count).step_by(97) {
                        block_map[index] = 1;
                    }
                }
                "full" => block_map.fill(1),
                _ => unreachable!(),
            }
            let mut damage = ExternalDamage::new(width, height)
                .unwrap_or_else(|error| panic!("valid external damage grid: {error}"));
            group.bench_with_input(
                BenchmarkId::new("block_map", format!("{width}x{height}/{pattern}")),
                &block_map,
                |bench, map| {
                    bench.iter(|| {
                        damage.reset();
                        damage
                            .mark_block_map(
                                black_box(map),
                                blocks_wide,
                                blocks_tall,
                                SOURCE_BLOCK_SIZE,
                            )
                            .unwrap_or_else(|error| panic!("valid external map: {error}"));
                        black_box(damage.summary())
                    });
                },
            );
        }
    }
    group.finish();
}

/// Full hashing against hashing narrowed to the OS-reported damage, on the
/// typing corpus where the OS names a small band; plus the per-tick cost of
/// block history and traits on the damage that results.
fn bench_narrowed_hashing(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("narrowed_hashing");
    group.sample_size(30);
    for (width, height, kind) in [
        (3840usize, 2160usize, ScenarioKind::Typing),
        (1920, 1080, ScenarioKind::Typing),
        (3840, 2160, ScenarioKind::Drag),
    ] {
        let scenario = Scenario::new(width, height, kind, 42);
        let mut previous = Vec::new();
        let mut current = Vec::new();
        scenario.render(4, &mut previous);
        scenario.render(5, &mut current);
        let mut rects = Vec::new();
        scenario.damage_rects(4, 5, &mut rects);
        let mut os_damage = ExternalDamage::new(width, height)
            .unwrap_or_else(|error| panic!("valid damage grid: {error}"));
        for rect in &rects {
            os_damage.mark_rect(*rect);
        }
        let frame = |pixels| {
            BgraFrame::new(pixels, width, height, scenario.stride())
                .unwrap_or_else(|error| panic!("valid corpus frame: {error}"))
        };
        let mut tracker = DamageTracker::new(width, height, KernelPreference::Xxh3)
            .unwrap_or_else(|error| panic!("valid tracker: {error}"));
        tracker
            .update(frame(&previous))
            .unwrap_or_else(|error| panic!("baseline: {error}"));
        let id = format!("{width}x{height}/{kind:?}");
        group.bench_function(BenchmarkId::new("full", &id), |bench| {
            bench.iter(|| {
                black_box(
                    tracker
                        .update(frame(black_box(&current)))
                        .unwrap_or_else(|error| panic!("update: {error}")),
                )
            });
        });
        group.bench_function(BenchmarkId::new("within", &id), |bench| {
            bench.iter(|| {
                black_box(
                    tracker
                        .update_within(frame(black_box(&current)), os_damage.damage_map())
                        .unwrap_or_else(|error| panic!("update_within: {error}")),
                )
            });
        });
        let mut history = BlockHistory::new(tracker.grid());
        let mut traits = BlockTraitsMap::new(tracker.grid());
        let mut changed = DamageTracker::new(width, height, KernelPreference::Xxh3)
            .unwrap_or_else(|error| panic!("valid tracker: {error}"));
        changed
            .update(frame(&previous))
            .unwrap_or_else(|error| panic!("baseline: {error}"));
        changed
            .update(frame(&current))
            .unwrap_or_else(|error| panic!("changed: {error}"));
        let mut now = std::time::Duration::ZERO;
        group.bench_function(BenchmarkId::new("history_and_traits", &id), |bench| {
            bench.iter(|| {
                now += std::time::Duration::from_millis(16);
                history
                    .observe(changed.damage_map(), now)
                    .unwrap_or_else(|error| panic!("history: {error}"));
                black_box(
                    traits
                        .update(frame(black_box(&current)), changed.damage_map())
                        .unwrap_or_else(|error| panic!("traits: {error}")),
                )
            });
        });
    }
    group.finish();
}

const WIDTH_4K: usize = 3840;
const HEIGHT_4K: usize = 2160;

fn frame(pixels: &[u8]) -> BgraFrame<'_> {
    BgraFrame::new(pixels, WIDTH_4K, HEIGHT_4K, WIDTH_4K * 4)
        .unwrap_or_else(|error| panic!("valid corpus frame: {error}"))
}

/// Row-hash retention and motion search on a 4K scroll.
fn bench_motion(criterion: &mut Criterion) {
    const WIDTH: usize = WIDTH_4K;
    const HEIGHT: usize = HEIGHT_4K;
    let mut group = criterion.benchmark_group("canvas_analysis");
    group.sample_size(20);
    let scroll = Scenario::new(WIDTH, HEIGHT, ScenarioKind::DocumentScroll, 42);
    let mut previous = Vec::new();
    let mut current = Vec::new();
    // Tick 7 -> 8 scrolls by 64 rows.
    scroll.render(7, &mut previous);
    scroll.render(8, &mut current);
    for retain in [false, true] {
        let mut tracker = DamageTracker::new(WIDTH, HEIGHT, KernelPreference::Xxh3)
            .unwrap_or_else(|error| panic!("valid tracker: {error}"));
        if retain {
            tracker.retain_row_hashes();
        }
        tracker
            .update(frame(&previous))
            .unwrap_or_else(|error| panic!("baseline: {error}"));
        let id = if retain { "rows_retained" } else { "plain" };
        group.bench_function(BenchmarkId::new("scroll_full_hash", id), |bench| {
            bench.iter(|| {
                black_box(
                    tracker
                        .update(frame(black_box(&current)))
                        .unwrap_or_else(|error| panic!("update: {error}")),
                )
            });
        });
    }
    let mut tracker = DamageTracker::new(WIDTH, HEIGHT, KernelPreference::Xxh3)
        .unwrap_or_else(|error| panic!("valid tracker: {error}"));
    tracker.retain_row_hashes();
    tracker
        .update(frame(&previous))
        .unwrap_or_else(|error| panic!("baseline: {error}"));
    tracker
        .update(frame(&current))
        .unwrap_or_else(|error| panic!("scroll: {error}"));
    let mut estimator = MotionEstimator::new(tracker.grid(), MotionConfig::default());
    group.bench_function("motion_estimate/3840x2160/DocumentScroll", |bench| {
        bench.iter(|| {
            black_box(
                estimator
                    .estimate(&tracker)
                    .unwrap_or_else(|error| panic!("estimate: {error}"))
                    .len(),
            )
        });
    });
    group.finish();
}

/// Classes and regions on 4K video, and the tile ledger.
fn bench_classes_and_ledger(criterion: &mut Criterion) {
    const WIDTH: usize = WIDTH_4K;
    const HEIGHT: usize = HEIGHT_4K;
    let mut group = criterion.benchmark_group("canvas_analysis");
    group.sample_size(20);
    let video = Scenario::new(WIDTH, HEIGHT, ScenarioKind::Video, 42);
    let mut pixels = Vec::new();
    let mut tracker = DamageTracker::new(WIDTH, HEIGHT, KernelPreference::Xxh3)
        .unwrap_or_else(|error| panic!("valid tracker: {error}"));
    let grid = tracker.grid();
    let mut history = BlockHistory::new(grid);
    let mut classes = BlockClassMap::new(grid, ClassConfig::default());
    let mut regions = MotionRegions::new(grid, RegionConfig::default());
    let mut now = std::time::Duration::ZERO;
    for tick in 0..2 {
        video.render(tick, &mut pixels);
        tracker
            .update(frame(&pixels))
            .unwrap_or_else(|error| panic!("video: {error}"));
    }
    group.bench_function("classes_and_regions/3840x2160/Video", |bench| {
        bench.iter(|| {
            now += std::time::Duration::from_millis(16);
            history
                .observe(tracker.damage_map(), now)
                .unwrap_or_else(|error| panic!("history: {error}"));
            classes
                .update(&history, tracker.damage_map(), &[])
                .unwrap_or_else(|error| panic!("classes: {error}"));
            black_box(
                regions
                    .update(&classes)
                    .unwrap_or_else(|error| panic!("regions: {error}"))
                    .len(),
            )
        });
    });

    let mut ledger = TileLedger::new(100 * 1024 * 1024, 65_536);
    let mut evicted = Vec::with_capacity(1024);
    let mut next = 0u128;
    group.bench_function("tile_ledger/insert_full_budget", |bench| {
        bench.iter(|| {
            next += 1;
            evicted.clear();
            black_box(
                ledger
                    .insert(
                        TileKey(next.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
                        4096,
                        &mut evicted,
                    )
                    .unwrap_or_else(|error| panic!("insert: {error}")),
            )
        });
    });
    let tile = PixelRect {
        x: 64,
        y: 64,
        width: 64,
        height: 64,
    };
    group.bench_function("tile_key/64x64", |bench| {
        bench.iter(|| black_box(TileKey::of(frame(black_box(&pixels)), tile)));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_damage_corpus,
    bench_external_damage,
    bench_narrowed_hashing,
    bench_motion,
    bench_classes_and_ledger
);
criterion_main!(benches);
