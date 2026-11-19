// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

#![allow(unsafe_code, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;

use std::time::Duration;

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{
    BgraFrame, BlockClassMap, BlockHistory, BlockPaint, BlockTraitsMap, ClassConfig, DamageTracker,
    ExternalDamage, KernelPreference, MotionConfig, MotionEstimator, MotionRegions, PaintKind,
    PendingExternalDamage, PixelRect, RegionConfig, TileKey, TileLedger, WorkKind, WorkMeter,
};

struct CountingAllocator;

// Counted per thread rather than process-wide.
//
// This was a global `AtomicUsize`, so the assertion was really "nothing
// anywhere in this process allocated", not "the damage hot path did not
// allocate". Anything the test harness did on another thread inside the
// measured window landed in the count. That is why it reported four
// allocations on Linux and none on macOS, while `arcen-keel` has no
// platform-specific code at all and depends only on two no_std hashing crates.
//
// A `Cell<usize>` with `const` initialisation and no destructor is safe to
// touch from inside the allocator: it needs no lazy setup, so reading it cannot
// re-enter the allocator. `try_with` keeps that true during thread teardown,
// when the slot may already be gone.
thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn record_allocation() {
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn damage_paths_and_iterators_allocate_nothing_after_warmup() {
    const WIDTH: usize = 96;
    const HEIGHT: usize = 64;
    let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
    let mut tracker = DamageTracker::new(WIDTH, HEIGHT, KernelPreference::Xxh3).expect("tracker");
    let initial = BgraFrame::new(&pixels, WIDTH, HEIGHT, WIDTH * 4).expect("initial frame");
    tracker.update(initial).expect("warm-up update");
    let mut external = ExternalDamage::new(WIDTH, HEIGHT).expect("external damage");
    let mut block_map = vec![0u8; 6 * 4];
    block_map[0] = 1;
    external
        .mark_block_map(&block_map, 6, 4, 16)
        .expect("warm-up external map");

    // Warm the paths the measured loop uses that the setup above does not, so
    // one-off lazy initialisation happens before counting starts instead of
    // being reported as a steady-state allocation.
    external.reset();
    external.mark_rect(PixelRect {
        x: 0,
        y: 0,
        width: 17,
        height: 17,
    });
    black_box(external.summary());
    black_box(external.damage_map().dirty_blocks().count());
    black_box(external.damage_map().dirty_block_rows().count());
    black_box(tracker.damage_map().dirty_blocks().count());
    black_box(tracker.damage_map().dirty_block_rows().count());
    let mut narrowed =
        DamageTracker::new(WIDTH, HEIGHT, KernelPreference::Xxh3).expect("narrowed tracker");
    narrowed.update(initial).expect("narrowed baseline");
    let mut history = BlockHistory::new(tracker.grid());
    let mut traits = BlockTraitsMap::new(tracker.grid());
    let mut pending = PendingExternalDamage::new(WIDTH, HEIGHT).expect("pending damage");
    external.mark_move_rect(0, 16, 0, 0, 32, 16);
    pending
        .observe_external(&external)
        .expect("warm-up pending");
    pending.mark_submitted();

    ALLOCATIONS.with(|count| count.set(0));
    for tick in 0..128 {
        let offset = (tick % (WIDTH * HEIGHT)) * 4;
        pixels[offset] = pixels[offset].wrapping_add(1);
        let frame = BgraFrame::new(&pixels, WIDTH, HEIGHT, WIDTH * 4).expect("frame");
        black_box(tracker.update(frame).expect("update"));
        black_box(tracker.damage_map().dirty_blocks().count());
        black_box(tracker.damage_map().dirty_block_rows().count());

        external.reset();
        external.mark_rect(PixelRect {
            x: tick % WIDTH,
            y: tick % HEIGHT,
            width: 17,
            height: 17,
        });
        external
            .mark_block_map(&block_map, 6, 4, 16)
            .expect("external map");
        external.mark_move_rect(0, 16, 0, 0, 32, 16);
        black_box(external.summary());
        black_box(external.damage_map().dirty_blocks().count());
        black_box(external.damage_map().dirty_block_rows().count());

        black_box(
            narrowed
                .update_within(frame, external.damage_map())
                .expect("narrowed update"),
        );
        history
            .observe(narrowed.damage_map(), Duration::from_millis(tick as u64))
            .expect("history");
        black_box(traits.update(frame, narrowed.damage_map()).expect("traits"));
        history.record_paint_blocks(
            narrowed.damage_map().dirty_blocks(),
            BlockPaint::new(PaintKind::Exact, false),
        );
        pending.observe_external(&external).expect("pending");
        black_box(pending.move_rects().len());
        pending.mark_submitted();
    }
    let allocations = ALLOCATIONS.with(Cell::get);

    assert_eq!(
        allocations, 0,
        "damage tracking allocated {allocations} time(s) on this thread across 128 frames"
    );
}

#[test]
fn canvas_analysis_allocates_nothing_after_warmup() {
    const WIDTH: usize = 320;
    const HEIGHT: usize = 192;
    let scenarios = [
        Scenario::new(WIDTH, HEIGHT, ScenarioKind::DocumentScroll, 42),
        Scenario::new(WIDTH, HEIGHT, ScenarioKind::Video, 42),
    ];
    let mut frames = Vec::new();
    for scenario in scenarios {
        for tick in 0..32 {
            let mut pixels = Vec::new();
            scenario.render(tick, &mut pixels);
            frames.push(pixels);
        }
    }
    let mut tracker = DamageTracker::new(WIDTH, HEIGHT, KernelPreference::Xxh3).expect("tracker");
    tracker.retain_row_hashes();
    let grid = tracker.grid();
    let mut estimator = MotionEstimator::new(grid, MotionConfig::default());
    let mut history = BlockHistory::new(grid);
    let mut classes = BlockClassMap::new(grid, ClassConfig::default());
    let mut regions = MotionRegions::new(grid, RegionConfig::default());
    let mut meter = WorkMeter::default();
    let mut ledger = TileLedger::new(64 * 1024, 64);
    let mut evicted = Vec::with_capacity(64);
    let rect = PixelRect {
        x: 0,
        y: 0,
        width: 16,
        height: 16,
    };

    let mut step = |pixels: &[u8], tick: u64| {
        let frame = BgraFrame::new(pixels, WIDTH, HEIGHT, WIDTH * 4).expect("frame");
        tracker.update(frame).expect("update");
        let moves = estimator.estimate(&tracker).expect("estimate");
        history
            .observe(tracker.damage_map(), Duration::from_millis(tick * 16))
            .expect("history");
        classes
            .update(&history, tracker.damage_map(), moves)
            .expect("classes");
        black_box(regions.update(&classes).expect("regions"));
        meter.record(WorkKind::Hash, 4096, Duration::from_micros(40));
        let key = TileKey(u128::from(tick));
        evicted.clear();
        black_box(ledger.insert(key, 4096, &mut evicted).expect("insert"));
        black_box(ledger.lookup(TileKey(u128::from(tick / 2))));
        black_box(TileKey::of(frame, rect));
    };
    for (tick, pixels) in frames.iter().enumerate() {
        step(pixels, tick as u64);
    }

    ALLOCATIONS.with(|count| count.set(0));
    for round in 1..4u64 {
        for (tick, pixels) in frames.iter().enumerate() {
            step(pixels, round * 1000 + tick as u64);
        }
    }
    let allocations = ALLOCATIONS.with(Cell::get);
    assert_eq!(
        allocations, 0,
        "canvas analysis allocated {allocations} time(s) on this thread"
    );
}
