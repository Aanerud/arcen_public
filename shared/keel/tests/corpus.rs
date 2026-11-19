// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Corpus tests for narrowed hashing, block history and block traits.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{
    BgraFrame, BlockHistory, BlockTraitsMap, DamageTracker, ExternalDamage, HISTORY_WINDOW,
    KernelPreference, PixelRect,
};

const CORPUS: [ScenarioKind; 8] = [
    ScenarioKind::Idle,
    ScenarioKind::Typing,
    ScenarioKind::Drag,
    ScenarioKind::Scroll,
    ScenarioKind::Video,
    ScenarioKind::Burst,
    ScenarioKind::DocumentScroll,
    ScenarioKind::Editor,
];

fn frame(scenario: Scenario, pixels: &[u8]) -> BgraFrame<'_> {
    BgraFrame::new(
        pixels,
        scenario.width(),
        scenario.height(),
        scenario.stride(),
    )
    .unwrap()
}

fn os_damage(
    scenario: Scenario,
    previous_tick: u64,
    tick: u64,
    rects: &mut Vec<PixelRect>,
) -> ExternalDamage {
    let mut damage = ExternalDamage::new(scenario.width(), scenario.height()).unwrap();
    scenario.damage_rects(previous_tick, tick, rects);
    for rect in rects.iter() {
        damage.mark_rect(*rect);
    }
    damage
}

#[test]
fn narrowed_hashing_equals_the_full_hash_on_the_corpus() {
    for (width, height) in [(128, 128), (200, 120), (333, 77)] {
        for kind in CORPUS {
            let scenario = Scenario::new(width, height, kind, 42);
            let mut full = DamageTracker::new(width, height, KernelPreference::Xxh3).unwrap();
            let mut narrowed = DamageTracker::new(width, height, KernelPreference::Xxh3).unwrap();
            narrowed.set_audit_period(7);
            let mut pixels = Vec::new();
            let mut rects = Vec::new();
            for tick in 0..60u64 {
                scenario.render(tick, &mut pixels);
                let damage = os_damage(scenario, tick.saturating_sub(1), tick, &mut rects);
                let expected = full.update(frame(scenario, &pixels)).unwrap();
                let actual = narrowed
                    .update_within(frame(scenario, &pixels), damage.damage_map())
                    .unwrap();
                assert_eq!(actual, expected, "{kind:?} {width}x{height} tick {tick}");
                assert!(
                    full.damage_map()
                        .dirty_blocks()
                        .eq(narrowed.damage_map().dirty_blocks()),
                    "{kind:?} {width}x{height} tick {tick}: dirty blocks differ"
                );
                assert!(narrowed.last_hashed_blocks() <= full.last_hashed_blocks());
            }
        }
    }
}

#[test]
fn narrowed_hashing_hashes_only_reported_damage_and_the_stripe() {
    let scenario = Scenario::new(1920, 1088, ScenarioKind::Typing, 42);
    let mut tracker = DamageTracker::new(1920, 1088, KernelPreference::Xxh3).unwrap();
    let mut pixels = Vec::new();
    let mut rects = Vec::new();
    scenario.render(0, &mut pixels);
    tracker.update(frame(scenario, &pixels)).unwrap();
    scenario.render(1, &mut pixels);
    let damage = os_damage(scenario, 0, 1, &mut rects);
    let summary = tracker
        .update_within(frame(scenario, &pixels), damage.damage_map())
        .unwrap();
    let reported = damage.damage_map().dirty_blocks().count();
    let stripe_rows = tracker
        .grid()
        .blocks_tall()
        .div_ceil(tracker.audit_period());
    let stripe_blocks = stripe_rows * tracker.grid().blocks_wide();
    assert!(tracker.last_hashed_blocks() <= reported + stripe_blocks);
    assert!(tracker.last_hashed_blocks() * 10 < tracker.grid().block_count());
    assert_eq!(summary.dirty_blocks, 80);
}

#[test]
fn audit_stripe_finds_unreported_change_within_one_period() {
    let scenario = Scenario::new(256, 256, ScenarioKind::Idle, 7);
    let mut pixels = Vec::new();
    scenario.render(0, &mut pixels);
    let mut tracker = DamageTracker::new(256, 256, KernelPreference::Xxh3).unwrap();
    tracker.set_audit_period(5);
    tracker.update(frame(scenario, &pixels)).unwrap();
    let silent = ExternalDamage::new(256, 256).unwrap();

    let changed_block = 15 * tracker.grid().blocks_wide() + 3;
    let offset = (15 * 16 * 256 + 3 * 16) * 4;
    pixels[offset] ^= 0xff;
    let mut found_at = None;
    for update in 1..=5 {
        tracker
            .update_within(frame(scenario, &pixels), silent.damage_map())
            .unwrap();
        let dirty = tracker.damage_map().dirty_blocks().collect::<Vec<_>>();
        if !dirty.is_empty() {
            assert_eq!(dirty, [changed_block]);
            assert!(found_at.is_none(), "an audited change is reported once");
            found_at = Some(update);
        }
    }
    assert!(
        found_at.is_some(),
        "the audit stripe must sweep the frame within its period"
    );
}

fn history_and_traits_snapshot(kind: ScenarioKind) -> String {
    let scenario = Scenario::new(128, 128, kind, 42);
    let grid = arcen_keel::BlockGrid::new(128, 128).unwrap();
    let mut tracker = DamageTracker::new(128, 128, KernelPreference::Xxh3).unwrap();
    let mut history = BlockHistory::new(grid);
    let mut traits = BlockTraitsMap::new(grid);
    let mut pixels = Vec::new();
    let mut measured = 0usize;
    for tick in 0..24u64 {
        scenario.render(tick, &mut pixels);
        let frame = frame(scenario, &pixels);
        tracker.update(frame).unwrap();
        history
            .observe(tracker.damage_map(), Duration::from_millis(tick * 16))
            .unwrap();
        if tick > 0 {
            measured += traits.update(frame, tracker.damage_map()).unwrap();
        }
    }
    let dirty = tracker.damage_map().dirty_blocks().collect::<Vec<_>>();
    let busiest = (0..grid.block_count())
        .map(|index| history.recent_changes(index, HISTORY_WINDOW))
        .max()
        .unwrap_or(0);
    let still = (0..grid.block_count())
        .filter(|index| {
            history
                .ticks_since_change(*index)
                .is_some_and(|ticks| ticks >= 8)
        })
        .count();
    let (mut solid, mut text, mut low, mut many) = (0, 0, 0, 0);
    for &index in &dirty {
        let block = traits.get(index).unwrap();
        if block.is_solid() {
            solid += 1;
        } else if block.is_text_like() {
            text += 1;
        } else if block.is_low_colour() {
            low += 1;
        } else {
            many += 1;
        }
    }
    format!(
        "dirty={} measured={measured} busiest={busiest}/16 still={still} \
         solid={solid} text={text} low={low} many={many}",
        dirty.len()
    )
}

#[test]
fn typing_golden_history_and_traits() {
    assert_eq!(
        history_and_traits_snapshot(ScenarioKind::Typing),
        "dirty=6 measured=186 busiest=7/16 still=40 solid=3 text=1 low=2 many=0"
    );
}

#[test]
fn scroll_golden_history_and_traits() {
    assert_eq!(
        history_and_traits_snapshot(ScenarioKind::Scroll),
        "dirty=64 measured=1472 busiest=16/16 still=0 solid=0 text=0 low=64 many=0"
    );
}

#[test]
fn drag_golden_history_and_traits() {
    assert_eq!(
        history_and_traits_snapshot(ScenarioKind::Drag),
        "dirty=15 measured=295 busiest=7/16 still=27 solid=11 text=0 low=4 many=0"
    );
}

#[test]
fn video_golden_history_and_traits() {
    assert_eq!(
        history_and_traits_snapshot(ScenarioKind::Video),
        "dirty=64 measured=1472 busiest=16/16 still=0 solid=0 text=0 low=0 many=64"
    );
}
