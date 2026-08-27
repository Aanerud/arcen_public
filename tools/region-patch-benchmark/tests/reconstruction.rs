#![allow(clippy::expect_used)]

use arcen_keel::scenario::ScenarioKind;
use arcen_region_patch_benchmark::{
    DeliveryMode, DeliveryStatus, ModelKind, RegionPatchHarness, ScenarioConfig, StepOptions,
    run_scenario,
};

#[test]
fn every_model_reconstructs_every_deterministic_scenario_with_tail_blocks() {
    for kind in [
        ScenarioKind::Idle,
        ScenarioKind::Typing,
        ScenarioKind::Drag,
        ScenarioKind::Scroll,
        ScenarioKind::Video,
        ScenarioKind::Burst,
    ] {
        let config = ScenarioConfig {
            width: 321,
            height: 197,
            ticks: 132,
            kind,
            seed: 42,
        };
        for model in ModelKind::ALL {
            let report = run_scenario(config, model).expect("scenario report");
            assert_eq!(
                report.reconstruction_mismatches,
                0,
                "{kind:?}/{} reconstruction mismatch",
                model.name()
            );
            assert_eq!(
                report.metrics.allocation_growths,
                0,
                "{kind:?}/{} grew a preallocated buffer",
                model.name()
            );
            assert!(
                report.metrics.peak_patch_count <= 64,
                "{kind:?}/{} exceeded patch bound",
                model.name()
            );
        }
    }
}

#[test]
fn framing_models_do_not_change_activity_or_emission_cadence() {
    for kind in [
        ScenarioKind::Idle,
        ScenarioKind::Typing,
        ScenarioKind::Drag,
        ScenarioKind::Scroll,
        ScenarioKind::Video,
        ScenarioKind::Burst,
    ] {
        let config = ScenarioConfig {
            width: 160,
            height: 96,
            ticks: 132,
            kind,
            seed: 42,
        };
        let baseline = run_scenario(config, ModelKind::FullPicture).expect("full-picture report");
        for model in [
            ModelKind::DirtyRows,
            ModelKind::DirtyRects,
            ModelKind::BoundedPatches,
        ] {
            let candidate = run_scenario(config, model).expect("candidate report");
            assert_eq!(
                candidate.metrics.capture_ticks,
                baseline.metrics.capture_ticks
            );
            assert_eq!(
                candidate.metrics.emitted_frames,
                baseline.metrics.emitted_frames
            );
            assert_eq!(candidate.metrics.cadence, baseline.metrics.cadence);
        }
    }
}

#[test]
fn patch_rectangles_are_composable_in_reverse_order() {
    const WIDTH: usize = 96;
    const HEIGHT: usize = 64;
    let stride = WIDTH * 4;
    let mut pixels = vec![0u8; stride * HEIGHT];
    let mut harness =
        RegionPatchHarness::new(ModelKind::BoundedPatches, WIDTH, HEIGHT).expect("harness");
    harness
        .step(&pixels, stride, 0, StepOptions::default())
        .expect("baseline");

    fill_block(&mut pixels, WIDTH, 0, 0, [1, 2, 3, 4]);
    fill_block(&mut pixels, WIDTH, 4, 2, [5, 6, 7, 8]);
    let outcome = harness
        .step(
            &pixels,
            stride,
            1,
            StepOptions {
                delivery: DeliveryMode::ReversePatches,
                ..StepOptions::default()
            },
        )
        .expect("reverse delivery");

    assert_eq!(outcome.delivery, DeliveryStatus::ReorderedApplied);
    assert!(outcome.patch_count >= 2);
    assert!(harness.reconstruction_matches(&pixels, stride));
}

#[test]
fn lost_delta_rejects_following_delta_until_a_keyframe_recovers() {
    const WIDTH: usize = 64;
    const HEIGHT: usize = 64;
    let stride = WIDTH * 4;
    let mut pixels = vec![0u8; stride * HEIGHT];
    let mut harness =
        RegionPatchHarness::new(ModelKind::BoundedPatches, WIDTH, HEIGHT).expect("harness");
    harness
        .step(&pixels, stride, 0, StepOptions::default())
        .expect("baseline");

    fill_block(&mut pixels, WIDTH, 0, 0, [9, 8, 7, 6]);
    let dropped = harness
        .step(
            &pixels,
            stride,
            1,
            StepOptions {
                delivery: DeliveryMode::DropFrame,
                ..StepOptions::default()
            },
        )
        .expect("dropped delta");
    assert_eq!(dropped.delivery, DeliveryStatus::Dropped);
    assert!(!harness.reconstruction_matches(&pixels, stride));

    fill_block(&mut pixels, WIDTH, 2, 2, [1, 3, 5, 7]);
    let rejected = harness
        .step(&pixels, stride, 2, StepOptions::default())
        .expect("gap detection");
    assert_eq!(rejected.delivery, DeliveryStatus::RejectedSequenceGap);
    assert!(!harness.reconstruction_matches(&pixels, stride));

    let recovered = harness
        .step(
            &pixels,
            stride,
            3,
            StepOptions {
                force_keyframe: true,
                ..StepOptions::default()
            },
        )
        .expect("recovery keyframe");
    assert_eq!(recovered.delivery, DeliveryStatus::Applied);
    assert!(harness.reconstruction_matches(&pixels, stride));
}

fn fill_block(pixels: &mut [u8], width: usize, block_x: usize, block_y: usize, color: [u8; 4]) {
    let start_x = block_x * 16;
    let start_y = block_y * 16;
    for y in start_y..(start_y + 16).min(pixels.len() / (width * 4)) {
        for x in start_x..(start_x + 16).min(width) {
            pixels[(y * width + x) * 4..(y * width + x + 1) * 4].copy_from_slice(&color);
        }
    }
}
