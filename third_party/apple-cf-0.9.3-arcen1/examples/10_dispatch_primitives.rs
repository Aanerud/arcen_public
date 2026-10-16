use apple_cf::dispatch_queue::{
    dispatch_apply, dispatch_async, dispatch_async_and_wait, DispatchGroup, DispatchQoS,
    DispatchQueue, DispatchSemaphore, DispatchSource,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

fn main() {
    let group = DispatchGroup::new();
    group.enter();
    group.leave();
    assert!(group.wait(Some(Duration::from_millis(10))));

    let semaphore = DispatchSemaphore::new(0);
    assert_eq!(semaphore.signal(), 0);
    assert!(semaphore.wait(Some(Duration::from_millis(10))));

    let source = DispatchSource::timer(Duration::from_millis(5), Duration::from_millis(1));
    source.resume();
    thread::sleep(Duration::from_millis(25));
    source.cancel();
    assert!(source.fire_count() > 0);

    let queue = DispatchQueue::new(
        "com.doomfish.apple-cf.dispatch-primitives-example",
        DispatchQoS::UserInitiated,
    );
    let counter = Arc::new(AtomicUsize::new(0));
    let async_group = DispatchGroup::new();
    async_group.enter();
    let async_group_done = async_group.clone();
    let async_counter = Arc::clone(&counter);
    dispatch_async(&queue, move || {
        async_counter.fetch_add(1, Ordering::SeqCst);
        async_group_done.leave();
    });
    assert!(async_group.wait(Some(Duration::from_secs(1))));

    let waited_counter = Arc::clone(&counter);
    dispatch_async_and_wait(&queue, move || {
        waited_counter.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    let total = Arc::new(AtomicUsize::new(0));
    let total_clone = Arc::clone(&total);
    dispatch_apply(4, &queue, move |index| {
        total_clone.fetch_add(index + 1, Ordering::SeqCst);
    });
    assert_eq!(total.load(Ordering::SeqCst), 10);
}
