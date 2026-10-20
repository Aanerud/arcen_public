//! A persistent set of conversion threads for the ten-bit host staging.
//!
//! The RGB10 to I444P16 conversion splits each frame into row bands. Spawning
//! a scoped thread per band per frame costs tens of microseconds a thread
//! and lets the scheduler place the threads anew every frame; these threads
//! live as long as the encoder and wait on a condition variable between
//! frames.
//!
//! [`ConversionPool::run`] borrows its task for exactly one call and returns
//! only after every worker has finished with it, which is what makes handing
//! a non-`'static` closure to long-lived threads sound.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// A task borrowed for one [`ConversionPool::run`] call, with its lifetime
/// erased. `run` does not return while any worker may still call it.
#[derive(Clone, Copy)]
struct TaskRef(*const (dyn Fn(usize) + Sync));

// SAFETY: the pointee is `Sync`, and `run` keeps it alive until every worker
// has stopped using it.
unsafe impl Send for TaskRef {}

struct State {
    generation: u64,
    task: Option<TaskRef>,
    tasks: usize,
    remaining: usize,
    panicked: bool,
    shutdown: bool,
}

struct Shared {
    state: Mutex<State>,
    start: Condvar,
    done: Condvar,
}

pub(crate) struct ConversionPool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl ConversionPool {
    /// A pool that runs up to `threads` tasks at once: the caller's thread
    /// plus `threads - 1` workers.
    pub(crate) fn new(threads: usize) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                generation: 0,
                task: None,
                tasks: 0,
                remaining: 0,
                panicked: false,
                shutdown: false,
            }),
            start: Condvar::new(),
            done: Condvar::new(),
        });
        let mut pool = Self {
            shared: Arc::clone(&shared),
            workers: Vec::new(),
        };
        for index in 1..threads.max(1) {
            let shared = Arc::clone(&shared);
            let worker = std::thread::Builder::new()
                .name(format!("arcen-rgb10-{index}"))
                .spawn(move || worker(&shared, index))
                .map_err(|error| format!("could not spawn RGB10 conversion worker: {error}"))?;
            pool.workers.push(worker);
        }
        Ok(pool)
    }

    /// Tasks that run concurrently.
    pub(crate) fn threads(&self) -> usize {
        self.workers.len() + 1
    }

    /// Runs `task(0)..task(tasks - 1)`, each exactly once, task 0 on the
    /// calling thread, and returns when all have finished. `tasks` must not
    /// exceed [`Self::threads`].
    pub(crate) fn run(&self, tasks: usize, task: &(dyn Fn(usize) + Sync)) -> Result<(), String> {
        if tasks > self.threads() {
            return Err(format!(
                "{tasks} conversion tasks for a pool of {}",
                self.threads()
            ));
        }
        if tasks <= 1 {
            if tasks == 1 {
                task(0);
            }
            return Ok(());
        }
        // SAFETY: only the lifetime is erased; this function waits below until
        // `remaining` is zero, after which no worker touches the pointer again
        // (they clear nothing but read `task` only for their own generation).
        let erased: *const (dyn Fn(usize) + Sync) = task;
        let erased: *const (dyn Fn(usize) + Sync + 'static) =
            unsafe { std::mem::transmute(erased) };
        {
            let mut state = lock(&self.shared.state);
            state.generation = state.generation.wrapping_add(1);
            state.task = Some(TaskRef(erased));
            state.tasks = tasks;
            state.remaining = tasks - 1;
            state.panicked = false;
        }
        self.shared.start.notify_all();
        let own = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(0)));
        let mut state = lock(&self.shared.state);
        while state.remaining != 0 {
            state = self
                .shared
                .done
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.task = None;
        let panicked = state.panicked || own.is_err();
        drop(state);
        if panicked {
            return Err("RGB10 conversion worker panicked".to_string());
        }
        Ok(())
    }
}

impl Drop for ConversionPool {
    fn drop(&mut self) {
        lock(&self.shared.state).shutdown = true;
        self.shared.start.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn worker(shared: &Shared, index: usize) {
    let mut seen = 0u64;
    loop {
        let task = {
            let mut state = lock(&shared.state);
            loop {
                if state.shutdown {
                    return;
                }
                if state.generation != seen {
                    seen = state.generation;
                    break;
                }
                state = shared
                    .start
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            (index < state.tasks).then_some(state.task).flatten()
        };
        let Some(TaskRef(task)) = task else {
            continue;
        };
        // SAFETY: `run` is blocked until this worker decrements `remaining`
        // below, so the borrowed task is alive.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (*task)(index);
        }));
        let mut state = lock(&shared.state);
        state.panicked |= outcome.is_err();
        state.remaining -= 1;
        if state.remaining == 0 {
            shared.done.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn every_task_runs_exactly_once_per_call() {
        let pool = ConversionPool::new(4).unwrap();
        assert_eq!(pool.threads(), 4);
        for tasks in [0, 1, 2, 4] {
            let counts: Vec<AtomicUsize> = (0..4).map(|_| AtomicUsize::new(0)).collect();
            for _ in 0..50 {
                pool.run(tasks, &|index| {
                    counts[index].fetch_add(1, Ordering::Relaxed);
                })
                .unwrap();
            }
            for (index, count) in counts.iter().enumerate() {
                let expected = if index < tasks { 50 } else { 0 };
                assert_eq!(count.load(Ordering::Relaxed), expected, "{tasks} tasks");
            }
        }
    }

    #[test]
    fn borrowed_mutable_bands_are_written_in_place() {
        let pool = ConversionPool::new(3).unwrap();
        let mut data = vec![0u32; 30];
        let bands: Vec<Mutex<&mut [u32]>> = data.chunks_mut(10).map(Mutex::new).collect();
        pool.run(3, &|index| {
            for value in lock_band(&bands[index]).iter_mut() {
                *value = u32::try_from(index).unwrap() + 1;
            }
        })
        .unwrap();
        drop(bands);
        assert!(data[..10].iter().all(|&v| v == 1));
        assert!(data[20..].iter().all(|&v| v == 3));
    }

    fn lock_band<'a, 'b>(
        band: &'a Mutex<&'b mut [u32]>,
    ) -> std::sync::MutexGuard<'a, &'b mut [u32]> {
        band.lock().unwrap()
    }

    #[test]
    fn a_panicking_task_is_an_error_and_the_pool_survives() {
        let pool = ConversionPool::new(2).unwrap();
        let result = pool.run(2, &|index| assert_ne!(index, 1, "worker fails"));
        assert!(result.is_err());
        let ran = AtomicUsize::new(0);
        pool.run(2, &|_| {
            ran.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        assert_eq!(ran.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn more_tasks_than_threads_are_refused() {
        let pool = ConversionPool::new(2).unwrap();
        assert!(pool.run(3, &|_| {}).is_err());
    }
}
