// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Corpus gates for block classes and motion regions: typing does not flap
//! in and out of `Motion`, video becomes one steady region, and the whole
//! chain allocates nothing per frame.

#![allow(clippy::unwrap_used, clippy::cast_precision_loss)]

use std::time::Duration;

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{
    BgraFrame, BlockClassMap, BlockHistory, ClassConfig, DamageTracker, KernelPreference,
    MotionRegions, RegionConfig,
};

const FRAME: Duration = Duration::from_micros(16_667);

#[derive(Debug, Default)]
struct Run {
    seconds: f64,
    motion_flaps: usize,
    max_regions: usize,
    region_changes: usize,
    final_regions: Vec<arcen_keel::MotionRegion>,
}

fn run(kind: ScenarioKind, width: usize, height: usize, ticks: u64) -> Run {
    let scenario = Scenario::new(width, height, kind, 42);
    let mut tracker = DamageTracker::new(width, height, KernelPreference::Xxh3).unwrap();
    let grid = tracker.grid();
    let mut history = BlockHistory::new(grid);
    let mut classes = BlockClassMap::new(grid, ClassConfig::default());
    let mut regions = MotionRegions::new(grid, RegionConfig::default());
    let mut pixels = Vec::new();
    let mut result = Run::default();
    let mut now = Duration::ZERO;
    for tick in 0..ticks {
        scenario.render(tick, &mut pixels);
        tracker
            .update(BgraFrame::new(&pixels, width, height, scenario.stride()).unwrap())
            .unwrap();
        now += FRAME;
        history.observe(tracker.damage_map(), now).unwrap();
        let summary = classes.update(&history, tracker.damage_map(), &[]).unwrap();
        let current = regions.update(&classes).unwrap();
        if tick > 0 {
            result.motion_flaps += summary.motion_flaps();
        }
        result.max_regions = result.max_regions.max(current.len());
        result.region_changes += current.iter().filter(|region| region.changed).count();
        result.final_regions = current.to_vec();
    }
    result.seconds = FRAME.as_secs_f64() * ticks as f64;
    result
}

#[test]
fn typing_classes_flap_at_most_once_per_second() {
    // `Typing` is a 12-row band moving 16 px every frame, a sweep rather than
    // typing; `Editor` types eight characters a second under a blinking caret.
    for kind in [ScenarioKind::Editor, ScenarioKind::Idle] {
        let run = run(kind, 640, 360, 600);
        eprintln!("{kind:?}: {run:?}");
        assert!(
            run.motion_flaps as f64 <= run.seconds,
            "{kind:?}: {} Motion flaps in {:.1} s",
            run.motion_flaps,
            run.seconds
        );
        assert_eq!(run.max_regions, 0, "{kind:?} opens no motion surface");
    }
}

#[test]
fn video_is_one_steady_full_region() {
    let run = run(ScenarioKind::Video, 320, 192, 120);
    eprintln!("{run:?}");
    let [region] = run.final_regions.as_slice() else {
        panic!("one region expected: {run:?}");
    };
    assert_eq!((region.rect.width, region.rect.height), (320, 192));
    assert_eq!(run.region_changes, 1, "opened once, never resized");
}

#[test]
fn drag_keeps_at_most_two_regions() {
    let run = run(ScenarioKind::Drag, 320, 192, 240);
    eprintln!("{run:?}");
    assert!(run.max_regions <= 2);
}
