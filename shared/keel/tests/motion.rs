// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Corpus gates for pixel motion search: scroll vectors are found on at
//! least 95% of scroll frames, and every vector reported on any scenario
//! holds exactly against the previous frame.

#![allow(clippy::unwrap_used)]

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{
    BgraFrame, DamageTracker, ExternalDamage, KernelPreference, MotionConfig, MotionEstimator,
    verify_move,
};

#[derive(Debug, Default)]
struct Tally {
    scroll_frames: usize,
    found: usize,
    vectors: usize,
    false_vectors: usize,
}

fn run(scenario: Scenario, ticks: u64, narrowed: bool) -> Tally {
    let (width, height, stride) = (scenario.width(), scenario.height(), scenario.stride());
    let mut tracker = DamageTracker::new(width, height, KernelPreference::Xxh3).unwrap();
    tracker.retain_row_hashes();
    let mut estimator = MotionEstimator::new(tracker.grid(), MotionConfig::default());
    let mut previous = Vec::new();
    let mut current = Vec::new();
    let mut rects = Vec::new();
    let mut tally = Tally::default();
    for tick in 0..ticks {
        scenario.render(tick, &mut current);
        let frame = BgraFrame::new(&current, width, height, stride).unwrap();
        if narrowed && tick > 0 {
            let mut damage = ExternalDamage::new(width, height).unwrap();
            scenario.damage_rects(tick - 1, tick, &mut rects);
            for rect in &rects {
                damage.mark_rect(*rect);
            }
            tracker.update_within(frame, damage.damage_map()).unwrap();
        } else {
            tracker.update(frame).unwrap();
        }
        let moves = estimator.estimate(&tracker).unwrap().to_vec();
        if tick > 0 {
            let reference = BgraFrame::new(&previous, width, height, stride).unwrap();
            for moved in &moves {
                tally.vectors += 1;
                if !verify_move(reference, frame, *moved) {
                    tally.false_vectors += 1;
                }
            }
            if let Some(truth) = scenario.scroll_truth(tick - 1, tick) {
                let reachable = truth.dy != 0
                    && truth.dy.unsigned_abs() + 16 <= truth.viewport.height as u64
                    && truth.dy.unsigned_abs() <= MotionConfig::default().max_scroll as u64;
                if reachable {
                    tally.scroll_frames += 1;
                    if moves.iter().any(|moved| moved.offset() == (0, -truth.dy)) {
                        tally.found += 1;
                    } else if std::env::var_os("KEEL_MOTION_DEBUG").is_some() {
                        eprintln!("miss tick {tick} dy {} moves {moves:?}", truth.dy);
                    }
                }
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    tally
}

fn assert_scroll_gate(tally: &Tally, label: &str) {
    eprintln!("{label}: {tally:?}");
    assert!(tally.scroll_frames > 0, "{label}: no scroll frames");
    assert!(
        tally.found * 100 >= tally.scroll_frames * 95,
        "{label}: found {} of {} scroll frames",
        tally.found,
        tally.scroll_frames
    );
    assert_eq!(tally.false_vectors, 0, "{label}: {tally:?}");
}

#[test]
fn document_scroll_vectors_are_found_and_exact() {
    for (width, height) in [(640, 400), (333, 257), (960, 540)] {
        for narrowed in [false, true] {
            let scenario = Scenario::new(width, height, ScenarioKind::DocumentScroll, 42);
            let tally = run(scenario, 96, narrowed);
            assert_scroll_gate(
                &tally,
                &format!("document {width}x{height} narrowed={narrowed}"),
            );
        }
    }
}

#[test]
fn full_frame_scroll_vectors_are_found_and_exact() {
    for (width, height) in [(128, 128), (256, 192)] {
        for narrowed in [false, true] {
            let scenario = Scenario::new(width, height, ScenarioKind::Scroll, 42);
            let tally = run(scenario, 48, narrowed);
            assert_scroll_gate(
                &tally,
                &format!("scroll {width}x{height} narrowed={narrowed}"),
            );
        }
    }
}

#[test]
fn no_false_vectors_on_the_rest_of_the_corpus() {
    for kind in [
        ScenarioKind::Idle,
        ScenarioKind::Typing,
        ScenarioKind::Drag,
        ScenarioKind::Video,
        ScenarioKind::Burst,
    ] {
        let tally = run(Scenario::new(256, 192, kind, 42), 48, false);
        assert_eq!(tally.false_vectors, 0, "{kind:?}: {tally:?}");
        if kind == ScenarioKind::Video {
            assert_eq!(tally.vectors, 0, "noise has no motion vector");
        }
    }
}
