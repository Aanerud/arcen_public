use std::hint::black_box;
use std::time::Duration;

use arcen_keel::ActivityHint;
use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_region_patch_benchmark::{DeliveryMode, ModelKind, RegionPatchHarness, StepOptions};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

const WIDTH: usize = 1792;
const HEIGHT: usize = 1168;

fn bench_region_patch_models(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("region_patch_1792x1168");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(250));
    group.measurement_time(Duration::from_millis(750));

    for scenario_kind in [
        ScenarioKind::Idle,
        ScenarioKind::Typing,
        ScenarioKind::Drag,
        ScenarioKind::Scroll,
        ScenarioKind::Video,
        ScenarioKind::Burst,
    ] {
        let scenario = Scenario::new(WIDTH, HEIGHT, scenario_kind, 42);
        let (previous_tick, current_tick) = if scenario_kind == ScenarioKind::Burst {
            (9, 10)
        } else {
            (4, 5)
        };
        let mut previous = Vec::new();
        let mut current = Vec::new();
        scenario.render(previous_tick, &mut previous);
        scenario.render(current_tick, &mut current);
        let activity_hint = if scenario_kind == ScenarioKind::Scroll {
            ActivityHint::Scroll
        } else {
            ActivityHint::None
        };

        for model in ModelKind::ALL {
            group.bench_with_input(
                BenchmarkId::new(model.name(), format!("{scenario_kind:?}")),
                &model,
                |bench, model| {
                    bench.iter_batched(
                        || {
                            let mut harness = RegionPatchHarness::new(*model, WIDTH, HEIGHT)
                                .unwrap_or_else(|error| panic!("benchmark harness: {error}"));
                            harness
                                .step(
                                    &previous,
                                    scenario.stride(),
                                    previous_tick,
                                    StepOptions {
                                        activity_hint,
                                        delivery: DeliveryMode::InOrder,
                                        ..StepOptions::default()
                                    },
                                )
                                .unwrap_or_else(|error| panic!("benchmark baseline: {error}"));
                            harness
                        },
                        |mut harness| {
                            black_box(
                                harness
                                    .step(
                                        black_box(&current),
                                        scenario.stride(),
                                        current_tick,
                                        StepOptions {
                                            activity_hint,
                                            delivery: DeliveryMode::InOrder,
                                            ..StepOptions::default()
                                        },
                                    )
                                    .unwrap_or_else(|error| {
                                        panic!("benchmark transition: {error}")
                                    }),
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

criterion_group!(benches, bench_region_patch_models);
criterion_main!(benches);
