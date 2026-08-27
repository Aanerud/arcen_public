#![allow(clippy::unwrap_used)]

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{
    BgraFrame, DamageTracker, ExternalDamage, HashKernel, KernelPreference, PixelRect,
};

fn frame(pixels: &[u8], width: usize, height: usize, stride: usize) -> BgraFrame<'_> {
    BgraFrame::new(pixels, width, height, stride).unwrap()
}

#[test]
fn single_pixel_changes_dirty_the_containing_tail_or_full_block() {
    let width = 33;
    let height = 17;
    let stride = width * 4 + 12;
    let base = vec![0u8; stride * height];

    for preference in [KernelPreference::Xxh3, KernelPreference::Crc32c] {
        for y in 0..height {
            for x in 0..width {
                let mut changed = base.clone();
                changed[y * stride + x * 4] = 1;
                let mut tracker = DamageTracker::new(width, height, preference).unwrap();
                tracker.update(frame(&base, width, height, stride)).unwrap();
                let summary = tracker
                    .update(frame(&changed, width, height, stride))
                    .unwrap();
                let expected = (y / 16) * tracker.grid().blocks_wide() + x / 16;
                assert_eq!(
                    tracker.damage_map().dirty_blocks().collect::<Vec<_>>(),
                    [expected],
                    "kernel={:?} pixel=({x},{y}) summary={summary:?}",
                    tracker.kernel()
                );
            }
        }
    }
}

#[test]
fn stride_padding_is_not_hashed() {
    let width = 17;
    let height = 18;
    let stride = width * 4 + 28;
    let base = vec![3u8; stride * height];
    let mut changed_padding = base.clone();
    for y in 0..height {
        changed_padding[y * stride + width * 4] ^= 0xff;
    }

    let mut tracker = DamageTracker::new(width, height, KernelPreference::Xxh3).unwrap();
    tracker.update(frame(&base, width, height, stride)).unwrap();
    let summary = tracker
        .update(frame(&changed_padding, width, height, stride))
        .unwrap();
    assert!(summary.is_clean());
}

#[test]
fn kernels_produce_identical_damage_decisions_for_all_scenarios() {
    for kind in [
        ScenarioKind::Idle,
        ScenarioKind::Typing,
        ScenarioKind::Drag,
        ScenarioKind::Scroll,
        ScenarioKind::Video,
        ScenarioKind::Burst,
    ] {
        let scenario = Scenario::new(80, 48, kind, 0x5eed);
        let mut previous = Vec::new();
        let mut current = Vec::new();
        scenario.render(3, &mut previous);
        scenario.render(4, &mut current);

        let mut decisions = Vec::new();
        for preference in [KernelPreference::Xxh3, KernelPreference::Crc32c] {
            let mut tracker =
                DamageTracker::new(scenario.width(), scenario.height(), preference).unwrap();
            tracker
                .update(frame(
                    &previous,
                    scenario.width(),
                    scenario.height(),
                    scenario.stride(),
                ))
                .unwrap();
            tracker
                .update(frame(
                    &current,
                    scenario.width(),
                    scenario.height(),
                    scenario.stride(),
                ))
                .unwrap();
            decisions.push(tracker.damage_map().dirty_blocks().collect::<Vec<_>>());
        }
        assert_eq!(decisions[0], decisions[1], "scenario={kind:?}");
    }
}

#[test]
fn geometry_error_does_not_mutate_the_previous_baseline() {
    let base = vec![9u8; 32 * 32 * 4];
    let other = vec![7u8; 16 * 16 * 4];
    let mut tracker = DamageTracker::new(32, 32, KernelPreference::Xxh3).unwrap();
    tracker.update(frame(&base, 32, 32, 128)).unwrap();
    assert!(tracker.update(frame(&other, 16, 16, 64)).is_err());
    let summary = tracker.update(frame(&base, 32, 32, 128)).unwrap();
    assert!(summary.is_clean());
}

#[test]
fn kernel_identity_is_reported() {
    assert_eq!(
        DamageTracker::new(16, 16, KernelPreference::Xxh3)
            .unwrap()
            .kernel(),
        HashKernel::Xxh3
    );
    assert_eq!(
        DamageTracker::new(16, 16, KernelPreference::Crc32c)
            .unwrap()
            .kernel(),
        HashKernel::Crc32c
    );
}

#[test]
fn external_single_pixel_rects_dirty_exactly_the_containing_block() {
    let width = 33;
    let height = 17;
    let mut damage = ExternalDamage::new(width, height).unwrap();

    for y in 0..height {
        for x in 0..width {
            damage.reset();
            damage.mark_rect(PixelRect {
                x,
                y,
                width: 1,
                height: 1,
            });
            let expected = (y / 16) * damage.grid().blocks_wide() + x / 16;
            assert_eq!(
                damage.damage_map().dirty_blocks().collect::<Vec<_>>(),
                [expected],
                "pixel=({x},{y})"
            );
        }
    }
}

#[test]
fn external_source_blocks_dirty_every_overlapping_keel_block() {
    for (width, height) in [(1usize, 1usize), (17, 15), (33, 17), (47, 35)] {
        for source_block_size in [1usize, 8, 16, 24, 32] {
            let blocks_wide = width.div_ceil(source_block_size);
            let blocks_tall = height.div_ceil(source_block_size);
            let mut blocks = vec![0u8; blocks_wide * blocks_tall];
            let mut damage = ExternalDamage::new(width, height).unwrap();

            for source_index in 0..blocks.len() {
                blocks.fill(0);
                blocks[source_index] = 1;
                damage.reset();
                damage
                    .mark_block_map(&blocks, blocks_wide, blocks_tall, source_block_size)
                    .unwrap();

                let source_x = (source_index % blocks_wide) * source_block_size;
                let source_y = (source_index / blocks_wide) * source_block_size;
                let source_end_x = (source_x + source_block_size).min(width);
                let source_end_y = (source_y + source_block_size).min(height);
                let expected = (0..damage.grid().block_count())
                    .filter(|index| {
                        let bounds = damage.grid().block_bounds(*index).unwrap();
                        bounds.x < source_end_x
                            && bounds.x + bounds.width > source_x
                            && bounds.y < source_end_y
                            && bounds.y + bounds.height > source_y
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    damage.damage_map().dirty_blocks().collect::<Vec<_>>(),
                    expected,
                    "frame={width}x{height} source_block={source_block_size} \
                     source_index={source_index}"
                );
            }
        }
    }
}
