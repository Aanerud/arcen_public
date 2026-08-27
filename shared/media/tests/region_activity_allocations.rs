#![allow(unsafe_code, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;

use arcen_keel::{BgraFrame, KernelPreference};
use arcen_media::{RegionActivityGrid, RegionActivityOwner, RegionGeneration, RegionId};

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

fn owner(generation: u64, region_id: u32) -> RegionActivityOwner {
    RegionActivityOwner::new(
        RegionGeneration::new(generation).expect("generation"),
        RegionId::new(region_id).expect("region id"),
    )
}

#[test]
fn region_activity_updates_resets_and_rebinds_allocate_nothing_after_warmup() {
    const WIDTH: usize = 96;
    const HEIGHT: usize = 64;
    let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
    let first_owner = owner(1, 1);
    let mut activity = RegionActivityGrid::new(first_owner, WIDTH, HEIGHT, KernelPreference::Xxh3)
        .expect("activity grid");
    activity
        .update(
            first_owner,
            BgraFrame::new(&pixels, WIDTH, HEIGHT, WIDTH * 4).expect("baseline"),
        )
        .expect("warm-up update");
    black_box(activity.damage_map().dirty_blocks().count());
    black_box(activity.diagnostics());

    ALLOCATIONS.with(|count| count.set(0));
    for tick in 0..128 {
        if tick == 64 {
            activity.rebind(owner(2, 2)).expect("rebind");
        }
        let current_owner = if tick < 64 { first_owner } else { owner(2, 2) };
        let offset = (tick % (WIDTH * HEIGHT)) * 4;
        pixels[offset] = pixels[offset].wrapping_add(1);
        let frame = BgraFrame::new(&pixels, WIDTH, HEIGHT, WIDTH * 4).expect("frame");
        black_box(
            activity
                .update(current_owner, frame)
                .expect("activity update"),
        );
        black_box(activity.damage_map().dirty_blocks().count());
        if tick % 31 == 0 {
            activity.reset();
        }
    }
    let allocations = ALLOCATIONS.with(Cell::get);

    assert_eq!(
        allocations, 0,
        "region activity allocated {allocations} time(s) across 128 updates"
    );
}
