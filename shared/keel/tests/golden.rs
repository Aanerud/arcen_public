#![allow(clippy::unwrap_used)]

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{BgraFrame, DamageTracker, ExternalDamage, KernelPreference, PixelRect};

fn snapshot(kind: ScenarioKind, previous_tick: u64, current_tick: u64) -> String {
    let scenario = Scenario::new(128, 128, kind, 42);
    let mut previous = Vec::new();
    let mut current = Vec::new();
    scenario.render(previous_tick, &mut previous);
    scenario.render(current_tick, &mut current);
    let mut tracker = DamageTracker::new(128, 128, KernelPreference::Xxh3).unwrap();
    tracker
        .update(BgraFrame::new(&previous, 128, 128, 512).unwrap())
        .unwrap();
    let summary = tracker
        .update(BgraFrame::new(&current, 128, 128, 512).unwrap())
        .unwrap();
    let blocks = tracker
        .damage_map()
        .dirty_blocks()
        .map(|index| index.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let rows = tracker
        .damage_map()
        .dirty_block_rows()
        .map(|range| format!("{}..{}", range.start, range.end))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "dirty={}/{} block_rows={}/{} blocks=[{}] rows=[{}]",
        summary.dirty_blocks,
        summary.total_blocks,
        summary.dirty_block_rows,
        summary.total_block_rows,
        blocks,
        rows
    )
}

#[test]
fn idle_golden_is_clean() {
    assert_eq!(
        snapshot(ScenarioKind::Idle, 0, 1),
        "dirty=0/64 block_rows=0/8 blocks=[] rows=[]"
    );
}

#[test]
fn typing_golden_is_sparse_and_readable() {
    assert_eq!(
        snapshot(ScenarioKind::Typing, 0, 1),
        "dirty=6/64 block_rows=2/8 blocks=[0,1,2,8,9,10] rows=[0..32]"
    );
}

#[test]
fn full_video_golden_is_fully_dirty() {
    assert_eq!(
        snapshot(ScenarioKind::Video, 0, 1),
        "dirty=64/64 block_rows=8/8 blocks=[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63] rows=[0..128]"
    );
}

#[test]
fn external_rectangles_golden_is_conservative_and_readable() {
    let mut damage = ExternalDamage::new(40, 24).unwrap();
    damage.mark_rect(PixelRect {
        x: 15,
        y: 15,
        width: 2,
        height: 2,
    });
    damage.mark_rect(PixelRect {
        x: 39,
        y: 23,
        width: 1,
        height: 1,
    });
    let summary = damage.summary();
    let blocks = damage
        .damage_map()
        .dirty_blocks()
        .map(|index| index.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let rows = damage
        .damage_map()
        .dirty_block_rows()
        .map(|range| format!("{}..{}", range.start, range.end))
        .collect::<Vec<_>>()
        .join(",");

    assert_eq!(
        format!(
            "dirty={}/{} block_rows={}/{} blocks=[{blocks}] rows=[{rows}]",
            summary.dirty_blocks,
            summary.total_blocks,
            summary.dirty_block_rows,
            summary.total_block_rows,
        ),
        "dirty=5/6 block_rows=2/2 blocks=[0,1,3,4,5] rows=[0..24]"
    );
}
