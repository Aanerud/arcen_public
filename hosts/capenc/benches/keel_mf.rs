use std::hint::black_box;

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{BgraFrame, DamageTracker, KernelPreference};
use arcen_media::video::{convert_bgra_to_nv12, convert_bgra_to_nv12_rows, Nv12FrameMut};
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};

const WIDTH: usize = 1792;
const HEIGHT: usize = 1168;
const SEED: u64 = 42;

#[allow(clippy::too_many_arguments)]
fn bgra_to_nv12(
    bgra: &[u8],
    bgra_stride: usize,
    y: &mut [u8],
    y_stride: usize,
    uv: &mut [u8],
    uv_stride: usize,
    width: usize,
    height: usize,
) {
    let source = BgraFrame::new(bgra, width, height, bgra_stride).expect("valid BGRA benchmark");
    let mut destination = Nv12FrameMut::new(
        u32::try_from(width).expect("benchmark width fits u32"),
        u32::try_from(height).expect("benchmark height fits u32"),
        y,
        y_stride,
        uv,
        uv_stride,
    )
    .expect("valid NV12 benchmark");
    convert_bgra_to_nv12(source, &mut destination).expect("valid full conversion");
}

#[allow(clippy::too_many_arguments)]
fn bgra_to_nv12_rows(
    bgra: &[u8],
    bgra_stride: usize,
    y: &mut [u8],
    y_stride: usize,
    uv: &mut [u8],
    uv_stride: usize,
    width: usize,
    height: usize,
    rows: std::ops::Range<usize>,
) {
    let source = BgraFrame::new(bgra, width, height, bgra_stride).expect("valid BGRA benchmark");
    let mut destination = Nv12FrameMut::new(
        u32::try_from(width).expect("benchmark width fits u32"),
        u32::try_from(height).expect("benchmark height fits u32"),
        y,
        y_stride,
        uv,
        uv_stride,
    )
    .expect("valid NV12 benchmark");
    convert_bgra_to_nv12_rows(source, &mut destination, rows).expect("valid selective conversion");
}

fn render_pair(kind: ScenarioKind) -> (Scenario, Vec<u8>, Vec<u8>) {
    let scenario = Scenario::new(WIDTH, HEIGHT, kind, SEED);
    let mut previous = Vec::new();
    let mut current = Vec::new();
    let (previous_tick, current_tick) = if kind == ScenarioKind::Burst {
        (9, 10)
    } else {
        (4, 5)
    };
    scenario.render(previous_tick, &mut previous);
    scenario.render(current_tick, &mut current);
    (scenario, previous, current)
}

fn initialized_selective_state(
    kind: ScenarioKind,
    preference: KernelPreference,
) -> (DamageTracker, Vec<u8>, Vec<u8>, Vec<u8>, usize) {
    let (scenario, previous, current) = render_pair(kind);
    let mut tracker =
        DamageTracker::new(WIDTH, HEIGHT, preference).expect("valid benchmark tracker");
    tracker
        .update(
            BgraFrame::new(&previous, WIDTH, HEIGHT, scenario.stride())
                .expect("valid previous frame"),
        )
        .expect("initialize damage baseline");
    let mut y = vec![0u8; WIDTH * HEIGHT];
    let mut uv = vec![0u8; WIDTH * HEIGHT / 2];
    bgra_to_nv12(
        &previous,
        scenario.stride(),
        &mut y,
        WIDTH,
        &mut uv,
        WIDTH,
        WIDTH,
        HEIGHT,
    );
    (tracker, y, uv, current, scenario.stride())
}

fn report_corpus_metrics() {
    for kind in [
        ScenarioKind::Idle,
        ScenarioKind::Typing,
        ScenarioKind::Drag,
        ScenarioKind::Scroll,
        ScenarioKind::Video,
        ScenarioKind::Burst,
    ] {
        let (scenario, previous, current) = render_pair(kind);
        let mut tracker =
            DamageTracker::new(WIDTH, HEIGHT, KernelPreference::Xxh3).expect("tracker");
        tracker
            .update(
                BgraFrame::new(&previous, WIDTH, HEIGHT, scenario.stride())
                    .expect("previous frame"),
            )
            .expect("baseline");
        let summary = tracker
            .update(
                BgraFrame::new(&current, WIDTH, HEIGHT, scenario.stride()).expect("current frame"),
            )
            .expect("damage");
        eprintln!(
            "keel_corpus scenario={kind:?} dirty_blocks={} total_blocks={} dirty_block_rows={} \
             total_block_rows={} damage_ratio={:.4} converted_row_ratio={:.4}",
            summary.dirty_blocks,
            summary.total_blocks,
            summary.dirty_block_rows,
            summary.total_block_rows,
            summary.damage_ratio(),
            summary.converted_row_ratio(),
        );
    }
}

fn bench_sparse_conversion(criterion: &mut Criterion) {
    report_corpus_metrics();
    let (_, _, current) = render_pair(ScenarioKind::Typing);
    let mut group = criterion.benchmark_group("mf_conversion_1792x1168");
    group.sample_size(20);

    group.bench_function("full", |bench| {
        bench.iter_batched(
            || {
                (
                    vec![0u8; WIDTH * HEIGHT],
                    vec![0u8; WIDTH * HEIGHT / 2],
                    current.clone(),
                )
            },
            |(mut y, mut uv, bgra)| {
                bgra_to_nv12(
                    black_box(&bgra),
                    WIDTH * 4,
                    &mut y,
                    WIDTH,
                    &mut uv,
                    WIDTH,
                    WIDTH,
                    HEIGHT,
                );
                black_box((y, uv));
            },
            BatchSize::LargeInput,
        );
    });

    for preference in [KernelPreference::Xxh3, KernelPreference::Crc32c] {
        group.bench_with_input(
            BenchmarkId::new("typing_hash_plus_selective", format!("{preference:?}")),
            &preference,
            |bench, preference| {
                bench.iter_batched(
                    || initialized_selective_state(ScenarioKind::Typing, *preference),
                    |(mut tracker, mut y, mut uv, bgra, stride)| {
                        let summary = tracker
                            .update(
                                BgraFrame::new(black_box(&bgra), WIDTH, HEIGHT, stride)
                                    .expect("current frame"),
                            )
                            .expect("damage");
                        for rows in tracker.damage_map().dirty_block_rows() {
                            bgra_to_nv12_rows(
                                &bgra, stride, &mut y, WIDTH, &mut uv, WIDTH, WIDTH, HEIGHT, rows,
                            );
                        }
                        black_box((summary, y, uv));
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

fn full_damage_state() -> (DamageTracker, Vec<Vec<u8>>, Vec<u8>, Vec<u8>, usize) {
    let scenario = Scenario::new(WIDTH, HEIGHT, ScenarioKind::Video, SEED);
    let mut baseline = Vec::new();
    scenario.render(0, &mut baseline);
    let mut tracker = DamageTracker::new(WIDTH, HEIGHT, KernelPreference::Xxh3).expect("tracker");
    tracker
        .update(
            BgraFrame::new(&baseline, WIDTH, HEIGHT, scenario.stride()).expect("baseline frame"),
        )
        .expect("baseline");
    let frames = (1..=17)
        .map(|tick| {
            let mut frame = Vec::new();
            scenario.render(tick, &mut frame);
            frame
        })
        .collect();
    (
        tracker,
        frames,
        vec![0u8; WIDTH * HEIGHT],
        vec![0u8; WIDTH * HEIGHT / 2],
        scenario.stride(),
    )
}

fn bench_full_damage_bypass(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("mf_full_damage_17_frames");
    group.sample_size(20);

    group.bench_function("full_baseline", |bench| {
        bench.iter_batched(
            full_damage_state,
            |(_, frames, mut y, mut uv, stride)| {
                for frame in frames {
                    bgra_to_nv12(&frame, stride, &mut y, WIDTH, &mut uv, WIDTH, WIDTH, HEIGHT);
                }
                black_box((y, uv));
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("sixteen_bypass_plus_probe", |bench| {
        bench.iter_batched(
            full_damage_state,
            |(mut tracker, frames, mut y, mut uv, stride)| {
                for (index, frame) in frames.into_iter().enumerate() {
                    if index == 16 {
                        black_box(
                            tracker
                                .update(
                                    BgraFrame::new(&frame, WIDTH, HEIGHT, stride)
                                        .expect("probe frame"),
                                )
                                .expect("probe"),
                        );
                    }
                    bgra_to_nv12(&frame, stride, &mut y, WIDTH, &mut uv, WIDTH, WIDTH, HEIGHT);
                }
                black_box((y, uv));
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_sparse_conversion, bench_full_damage_bypass);
criterion_main!(benches);
