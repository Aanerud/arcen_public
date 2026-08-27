#![allow(unsafe_code, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_region_patch_benchmark::{DeliveryMode, ModelKind, RegionPatchHarness, StepOptions};

struct CountingAllocator;

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
fn all_models_allocate_nothing_after_construction_and_baseline() {
    const WIDTH: usize = 321;
    const HEIGHT: usize = 197;
    let scenarios = [
        Scenario::new(WIDTH, HEIGHT, ScenarioKind::Idle, 42),
        Scenario::new(WIDTH, HEIGHT, ScenarioKind::Typing, 42),
        Scenario::new(WIDTH, HEIGHT, ScenarioKind::Drag, 42),
        Scenario::new(WIDTH, HEIGHT, ScenarioKind::Scroll, 42),
        Scenario::new(WIDTH, HEIGHT, ScenarioKind::Video, 42),
        Scenario::new(WIDTH, HEIGHT, ScenarioKind::Burst, 42),
    ];
    let frame_bytes = scenarios[0].stride() * HEIGHT;
    let mut pixels = Vec::with_capacity(frame_bytes);
    scenarios[0].render(0, &mut pixels);
    let mut counts = [0usize; 4];

    for (index, model) in ModelKind::ALL.into_iter().enumerate() {
        let mut harness = RegionPatchHarness::new(model, WIDTH, HEIGHT).expect("harness");
        harness
            .step(&pixels, scenarios[0].stride(), 0, StepOptions::default())
            .expect("baseline");

        ALLOCATIONS.with(|count| count.set(0));
        let mut capture_tick = 1u64;
        for scenario in scenarios {
            for scenario_tick in 0..32 {
                scenario.render(scenario_tick, &mut pixels);
                black_box(
                    harness
                        .step(
                            &pixels,
                            scenario.stride(),
                            capture_tick,
                            StepOptions {
                                delivery: DeliveryMode::InOrder,
                                ..StepOptions::default()
                            },
                        )
                        .expect("steady-state frame"),
                );
                capture_tick = capture_tick.saturating_add(1);
            }
        }
        counts[index] = ALLOCATIONS.with(Cell::get);
    }

    assert_eq!(
        counts, [0; 4],
        "steady-state allocations by model: {counts:?}"
    );
}
