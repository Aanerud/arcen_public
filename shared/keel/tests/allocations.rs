#![allow(unsafe_code, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;

use arcen_keel::{BgraFrame, DamageTracker, ExternalDamage, KernelPreference, PixelRect};

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
        black_box(external.summary());
        black_box(external.damage_map().dirty_blocks().count());
        black_box(external.damage_map().dirty_block_rows().count());
    }
    let allocations = ALLOCATIONS.with(Cell::get);

    assert_eq!(
        allocations, 0,
        "damage tracking allocated {allocations} time(s) on this thread across 128 frames"
    );
}
