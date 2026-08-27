use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{BgraFrame, DamageTracker, ExternalDamage, KernelPreference};
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

criterion_group!(benches, bench_damage_corpus, bench_external_damage);
criterion_main!(benches);
