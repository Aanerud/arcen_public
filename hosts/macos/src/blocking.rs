//! Supervision for native calls that cannot be cancelled by dropping a future.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A single native operation that must not overlap with another attempt.
#[derive(Debug)]
pub struct BlockingSlot {
    running: AtomicBool,
}

impl BlockingSlot {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
        }
    }

    fn claim(&'static self) -> Option<BlockingSlotGuard> {
        self.running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| BlockingSlotGuard { slot: self })
            .ok()
    }

    pub fn is_running(&'static self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

impl Default for BlockingSlot {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct BlockingSlotGuard {
    slot: &'static BlockingSlot,
}

impl Drop for BlockingSlotGuard {
    fn drop(&mut self) {
        self.slot.running.store(false, Ordering::Release);
    }
}

/// Why an exclusive blocking job could not deliver its result.
#[derive(Debug)]
pub enum ExclusiveFailure {
    Busy,
    Spawn(std::io::Error),
    Disconnected,
}

/// Runs one blocking native job while a reaper owns its join handle and slot.
///
/// If the caller gives up before the job finishes, the receiver is dropped; the
/// worker's late result is then discarded by the channel send, and the reaper
/// keeps the slot claimed until the native call actually returns.
///
/// # Errors
///
/// Returns the mapped [`ExclusiveFailure`] when the slot is already occupied,
/// the worker cannot be spawned, or the worker ends without sending a result.
pub async fn run_exclusive<T, E, F, M>(
    name: &str,
    slot: &'static BlockingSlot,
    map_failure: M,
    job: F,
) -> Result<T, E>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
    M: Fn(ExclusiveFailure) -> E,
{
    let Some(guard) = slot.claim() else {
        return Err(map_failure(ExclusiveFailure::Busy));
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let (reaper_sender, reaper_receiver) =
        std::sync::mpsc::sync_channel::<(std::thread::JoinHandle<()>, BlockingSlotGuard)>(1);
    std::thread::Builder::new()
        .name(format!("{name}-reaper"))
        .spawn(move || {
            if let Ok((handle, guard)) = reaper_receiver.recv() {
                let _guard = guard;
                let _ = handle.join();
            }
        })
        .map_err(|error| map_failure(ExclusiveFailure::Spawn(error)))?;
    let worker = match std::thread::Builder::new()
        .name(format!("{name}-worker"))
        .spawn(move || {
            let result = job();
            let _ = sender.send(result);
        }) {
        Ok(worker) => worker,
        Err(error) => return Err(map_failure(ExclusiveFailure::Spawn(error))),
    };
    if let Err(error) = reaper_sender.send((worker, guard)) {
        let (worker, guard) = error.0;
        let _guard = guard;
        let _ = worker.join();
        return Err(map_failure(ExclusiveFailure::Disconnected));
    }
    receive_thread_result(receiver, slot, map_failure).await
}

async fn receive_thread_result<T, E, M>(
    receiver: std::sync::mpsc::Receiver<T>,
    slot: &'static BlockingSlot,
    map_failure: M,
) -> Result<T, E>
where
    M: Fn(ExclusiveFailure) -> E,
{
    loop {
        match receiver.try_recv() {
            Ok(result) => {
                while slot.is_running() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                return Ok(result);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(map_failure(ExclusiveFailure::Disconnected));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The observed outcome of a bounded join.
#[derive(Debug)]
pub enum JoinOutcome<T> {
    Completed(std::thread::Result<T>),
    TimedOut,
    ReaperStopped,
}

/// Joins a blocking thread through a reaper, but only waits up to `timeout`.
///
/// # Errors
///
/// Returns an I/O error when the reaper thread cannot be spawned.
pub async fn join_thread_with_timeout<T>(
    name: &str,
    handle: std::thread::JoinHandle<T>,
    timeout: Duration,
) -> Result<JoinOutcome<T>, std::io::Error>
where
    T: Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = sender.send(handle.join());
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        match receiver.try_recv() {
            Ok(result) => return Ok(JoinOutcome::Completed(result)),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Ok(JoinOutcome::ReaperStopped);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Ok(JoinOutcome::TimedOut);
        }
        tokio::time::sleep(Duration::from_millis(25).min(timeout)).await;
    }
}

/// Reaps a thread in the background after giving it a short graceful window.
///
/// # Errors
///
/// Returns an I/O error when the supervising thread cannot be spawned.
pub fn reap_thread_after_grace<T>(
    name: &str,
    handle: std::thread::JoinHandle<T>,
    grace: Duration,
    poll: Duration,
) -> Result<(), std::io::Error>
where
    T: Send + 'static,
{
    let late_name = format!("{name}-late");
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if handle.is_finished() {
                    let _ = handle.join();
                    return;
                }
                std::thread::sleep(poll);
            }
            let _ = reap_thread_detached(&late_name, handle);
        })
        .map(|_| ())
}

/// Reaps a thread without making its caller wait.
///
/// # Errors
///
/// Returns an I/O error when the reaper thread cannot be spawned.
pub fn reap_thread_detached<T>(
    name: &str,
    handle: std::thread::JoinHandle<T>,
) -> Result<(), std::io::Error>
where
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = handle.join();
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct DropProbe(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    static LATE_RESULT_SLOT: BlockingSlot = BlockingSlot::new();

    #[tokio::test]
    async fn timed_out_exclusive_job_drops_late_result_and_releases_slot() {
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_dropped = std::sync::Arc::clone(&dropped);
        let result = tokio::time::timeout(
            Duration::from_millis(20),
            run_exclusive(
                "arcen-test-late-result",
                &LATE_RESULT_SLOT,
                failure_name,
                move || {
                    let _ = wait.recv();
                    DropProbe(worker_dropped)
                },
            ),
        )
        .await;
        assert!(result.is_err());

        let _ = release.send(());
        for _ in 0..40 {
            if dropped.load(Ordering::Relaxed) == 1 && !LATE_RESULT_SLOT.is_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        let later = run_exclusive(
            "arcen-test-slot-reuse",
            &LATE_RESULT_SLOT,
            failure_name,
            || 7_u8,
        )
        .await;
        assert_eq!(later, Ok(7));
    }

    fn failure_name(failure: ExclusiveFailure) -> &'static str {
        match failure {
            ExclusiveFailure::Busy => "busy",
            ExclusiveFailure::Spawn(error) => {
                drop(error);
                "spawn"
            }
            ExclusiveFailure::Disconnected => "disconnected",
        }
    }
}
