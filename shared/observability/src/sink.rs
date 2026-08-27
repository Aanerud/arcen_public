//! Generic fixed-capacity, nonblocking sink workers.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arcen_telemetry::CanonicalRecord;

const CONTROL_POLL: Duration = Duration::from_millis(1);
const WORKER_POLL: Duration = Duration::from_millis(10);
const MAX_SINK_NAME_BYTES: usize = 64;

/// Error returned by an app-owned or writer-backed sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkError {
    message: String,
}

impl SinkError {
    /// Creates an adapter error with caller-owned context.
    #[must_use]
    pub fn adapter(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: &std::io::Error) -> Self {
        Self::adapter(format!("writer failed with {:?}: {error}", error.kind()))
    }
}

impl Display for SinkError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for SinkError {}

/// App-owned delivery adapter used by a [`BoundedSink`].
///
/// Implementations may call journald, Event Log, managed files, or another
/// platform facility. The worker guarantees these calls do not occur on the
/// producer thread.
pub trait Sink<T>: Send + 'static {
    /// Delivers one queued item.
    ///
    /// # Errors
    ///
    /// Returns a concrete delivery failure. The worker counts the failure and
    /// continues draining later records.
    fn deliver(&mut self, item: T) -> Result<(), SinkError>;

    /// Flushes buffered adapter state.
    ///
    /// # Errors
    ///
    /// Returns a concrete flush failure.
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// Canonical JSON Lines adapter over a generic standard-library writer.
#[derive(Debug)]
pub struct WriterRecordSink<W> {
    writer: W,
}

impl<W> WriterRecordSink<W> {
    /// Wraps a writer.
    #[must_use]
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: Write + Send + 'static> Sink<CanonicalRecord> for WriterRecordSink<W> {
    fn deliver(&mut self, item: CanonicalRecord) -> Result<(), SinkError> {
        let line = item.to_json_line().map_err(|error| {
            SinkError::adapter(format!("canonical serialization failed: {error}"))
        })?;
        self.writer
            .write_all(line.as_bytes())
            .map_err(|error| SinkError::io(&error))
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.writer.flush().map_err(|error| SinkError::io(&error))
    }
}

/// Human text adapter over a generic standard-library writer.
#[derive(Debug)]
pub struct WriterTextSink<W> {
    writer: W,
}

impl<W> WriterTextSink<W> {
    /// Wraps a writer.
    #[must_use]
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: Write + Send + 'static> Sink<String> for WriterTextSink<W> {
    fn deliver(&mut self, item: String) -> Result<(), SinkError> {
        self.writer
            .write_all(item.as_bytes())
            .map_err(|error| SinkError::io(&error))
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.writer.flush().map_err(|error| SinkError::io(&error))
    }
}

enum Command<T> {
    Deliver(T),
    Flush(SyncSender<Result<(), SinkError>>),
}

#[derive(Debug)]
struct LossCounter {
    total: AtomicU64,
    reported: AtomicU64,
}

impl LossCounter {
    const fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            reported: AtomicU64::new(0),
        }
    }

    fn increment(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    fn take_delta(&self) -> u64 {
        loop {
            let reported = self.reported.load(Ordering::Acquire);
            let total = self.total.load(Ordering::Acquire);
            if total <= reported {
                return 0;
            }
            if self
                .reported
                .compare_exchange_weak(reported, total, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return total - reported;
            }
        }
    }
}

#[derive(Debug)]
struct SinkState {
    name: String,
    dropped: AtomicU64,
    delivered: AtomicU64,
    failures: AtomicU64,
    last_reported_dropped: AtomicU64,
    queue_full: LossCounter,
    queue_closed: LossCounter,
    delivery_failure: LossCounter,
    flush_failure: LossCounter,
    delivery_healthy: AtomicBool,
    flush_healthy: AtomicBool,
    shutdown_requested: AtomicBool,
    worker_finished: AtomicBool,
    worker_panicked: AtomicBool,
    shutdown_error: Mutex<Option<SinkError>>,
}

impl SinkState {
    fn stats(&self) -> SinkStats {
        SinkStats {
            name: self.name.clone(),
            dropped: self.dropped.load(Ordering::Relaxed),
            delivered: self.delivered.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            healthy: self.is_healthy(),
            worker_finished: self.worker_finished.load(Ordering::Acquire),
        }
    }

    fn is_alive(&self) -> bool {
        !self.worker_finished.load(Ordering::Acquire)
            && !self.worker_panicked.load(Ordering::Acquire)
    }

    fn is_healthy(&self) -> bool {
        self.is_alive()
            && !self.shutdown_requested.load(Ordering::Acquire)
            && self.delivery_healthy.load(Ordering::Acquire)
            && self.flush_healthy.load(Ordering::Acquire)
    }

    fn mark_worker_panicked(&self) {
        self.worker_panicked.store(true, Ordering::Release);
        self.delivery_healthy.store(false, Ordering::Release);
        self.flush_healthy.store(false, Ordering::Release);
    }

    fn record_queue_full(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        self.queue_full.increment();
    }

    fn record_queue_closed(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        self.queue_closed.increment();
    }

    fn record_delivery_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        self.delivery_failure.increment();
        self.delivery_healthy.store(false, Ordering::Release);
    }

    fn record_flush_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        self.flush_failure.increment();
        self.flush_healthy.store(false, Ordering::Release);
    }

    fn take_loss_deltas(&self) -> Vec<SinkLossDelta> {
        [
            (SinkLossClass::QueueFull, self.queue_full.take_delta()),
            (SinkLossClass::QueueClosed, self.queue_closed.take_delta()),
            (
                SinkLossClass::DeliveryFailure,
                self.delivery_failure.take_delta(),
            ),
            (SinkLossClass::FlushFailure, self.flush_failure.take_delta()),
        ]
        .into_iter()
        .filter(|(_, count)| *count != 0)
        .map(|(class, count)| SinkLossDelta {
            sink: self.name.clone(),
            class,
            count,
        })
        .collect()
    }
}

struct BoundedSinkInner<T> {
    sender: SyncSender<Command<T>>,
    state: Arc<SinkState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// Cloneable producer for one fixed-capacity dedicated sink worker.
pub struct BoundedSink<T> {
    inner: Arc<BoundedSinkInner<T>>,
}

impl<T> Clone for BoundedSink<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Send + 'static> std::fmt::Debug for BoundedSink<T> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundedSink")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static> BoundedSink<T> {
    /// Starts a named worker with an exact bounded queue capacity.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid name, zero capacity, or worker spawn
    /// failure.
    pub fn new(
        name: impl Into<String>,
        capacity: usize,
        sink: impl Sink<T>,
    ) -> Result<Self, SinkBuildError> {
        let name = name.into();
        Self::new_boxed(&name, capacity, Box::new(sink))
    }

    pub(crate) fn new_boxed(
        name: &str,
        capacity: usize,
        sink: Box<dyn Sink<T>>,
    ) -> Result<Self, SinkBuildError> {
        if name.is_empty() || name.len() > MAX_SINK_NAME_BYTES || name.chars().any(char::is_control)
        {
            return Err(SinkBuildError::InvalidName);
        }
        if capacity == 0 {
            return Err(SinkBuildError::ZeroCapacity);
        }

        let (sender, receiver) = mpsc::sync_channel(capacity);
        let state = Arc::new(SinkState {
            name: name.to_owned(),
            dropped: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            last_reported_dropped: AtomicU64::new(0),
            queue_full: LossCounter::new(),
            queue_closed: LossCounter::new(),
            delivery_failure: LossCounter::new(),
            flush_failure: LossCounter::new(),
            delivery_healthy: AtomicBool::new(true),
            flush_healthy: AtomicBool::new(true),
            shutdown_requested: AtomicBool::new(false),
            worker_finished: AtomicBool::new(false),
            worker_panicked: AtomicBool::new(false),
            shutdown_error: Mutex::new(None),
        });
        let worker_state = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name(format!("arcen-observability-{name}"))
            .spawn(move || run_worker(&receiver, sink, &worker_state))
            .map_err(SinkBuildError::Spawn)?;

        Ok(Self {
            inner: Arc::new(BoundedSinkInner {
                sender,
                state,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }

    /// Attempts to enqueue without waiting for capacity.
    #[must_use]
    pub fn try_send(&self, item: T) -> DeliveryOutcome {
        if self.inner.state.shutdown_requested.load(Ordering::Acquire)
            || !self.inner.state.is_alive()
        {
            self.inner.state.record_queue_closed();
            return DeliveryOutcome::Closed;
        }
        match self.inner.sender.try_send(Command::Deliver(item)) {
            Ok(()) => DeliveryOutcome::Enqueued,
            Err(TrySendError::Full(_)) => {
                self.inner.state.record_queue_full();
                DeliveryOutcome::QueueFull
            }
            Err(TrySendError::Disconnected(_)) => {
                self.inner.state.record_queue_closed();
                DeliveryOutcome::Closed
            }
        }
    }

    /// Returns the current monotonic counters.
    #[must_use]
    pub fn stats(&self) -> SinkStats {
        self.inner.state.stats()
    }

    /// Returns the configured sink name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.state.name
    }

    /// Returns whether the worker is open and its most recent adapter delivery
    /// and flush outcomes were successful.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.inner.state.is_healthy()
    }

    /// Returns whether the dedicated worker can still accept routed records.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.inner.state.is_alive()
    }

    /// Returns the drop delta since the previous call.
    #[must_use]
    pub fn take_unreported_drops(&self) -> u64 {
        take_monotonic_delta(
            &self.inner.state.dropped,
            &self.inner.state.last_reported_dropped,
        )
    }

    /// Atomically drains complete loss deltas since the previous complete drain.
    ///
    /// Queue-full, queue-closed, delivery, and flush failures have independent
    /// monotonic cursors. Concurrent drains cannot duplicate or regress counts.
    /// This cursor is independent of the legacy [`Self::take_unreported_drops`]
    /// cursor; applications should use only one reporting API.
    #[must_use]
    pub fn take_loss_deltas(&self) -> Vec<SinkLossDelta> {
        self.inner.state.take_loss_deltas()
    }

    /// Flushes all records queued before this call, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// Returns timeout, worker closure, or adapter flush failure.
    pub fn flush(&self, timeout: Duration) -> Result<(), WaitError> {
        if self.inner.state.worker_panicked.load(Ordering::Acquire) {
            return Err(WaitError::WorkerPanicked);
        }
        if self.inner.state.shutdown_requested.load(Ordering::Acquire) {
            return Err(WaitError::Closed);
        }
        let deadline = Instant::now() + timeout;
        let (sender, receiver) = mpsc::sync_channel(1);
        send_control(&self.inner.sender, Command::Flush(sender), deadline).map_err(|error| {
            if self.inner.state.worker_panicked.load(Ordering::Acquire) {
                WaitError::WorkerPanicked
            } else {
                error
            }
        })?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        receiver
            .recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => WaitError::Timeout,
                mpsc::RecvTimeoutError::Disconnected
                    if self.inner.state.worker_panicked.load(Ordering::Acquire) =>
                {
                    WaitError::WorkerPanicked
                }
                mpsc::RecvTimeoutError::Disconnected => WaitError::Closed,
            })?
            .map_err(WaitError::Sink)
    }

    /// Requests drain-and-flush shutdown and waits at most `timeout`.
    ///
    /// # Errors
    ///
    /// Returns a timeout when a sink call remains blocked past the bound, or a
    /// synchronization failure if the join handle is poisoned.
    pub fn shutdown(&self, timeout: Duration) -> Result<(), WaitError> {
        self.inner
            .state
            .shutdown_requested
            .store(true, Ordering::Release);
        let deadline = Instant::now() + timeout;
        while !self.inner.state.worker_finished.load(Ordering::Acquire) {
            let worker_finished = self
                .inner
                .worker
                .lock()
                .map_err(|_| WaitError::Poisoned)?
                .as_ref()
                .is_some_and(JoinHandle::is_finished);
            if worker_finished {
                break;
            }
            if Instant::now() >= deadline {
                return Err(WaitError::Timeout);
            }
            thread::sleep(CONTROL_POLL);
        }
        let worker = self
            .inner
            .worker
            .lock()
            .map_err(|_| WaitError::Poisoned)?
            .take();
        if let Some(worker) = worker {
            worker.join().map_err(|_| WaitError::WorkerPanicked)?;
        }
        if self.inner.state.worker_panicked.load(Ordering::Acquire) {
            return Err(WaitError::WorkerPanicked);
        }
        self.inner
            .state
            .shutdown_error
            .lock()
            .map_err(|_| WaitError::Poisoned)?
            .take()
            .map_or(Ok(()), |error| Err(WaitError::Sink(error)))
    }
}

fn take_monotonic_delta(total: &AtomicU64, reported: &AtomicU64) -> u64 {
    loop {
        let previous = reported.load(Ordering::Acquire);
        let current = total.load(Ordering::Acquire);
        if current <= previous {
            return 0;
        }
        if reported
            .compare_exchange_weak(previous, current, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return current - previous;
        }
    }
}

fn send_control<T>(
    sender: &SyncSender<Command<T>>,
    mut command: Command<T>,
    deadline: Instant,
) -> Result<(), WaitError> {
    loop {
        match sender.try_send(command) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                if Instant::now() >= deadline {
                    return Err(WaitError::Timeout);
                }
                command = returned;
                thread::sleep(CONTROL_POLL);
            }
            Err(TrySendError::Disconnected(_)) => return Err(WaitError::Closed),
        }
    }
}

fn run_worker<T: 'static>(
    receiver: &Receiver<Command<T>>,
    mut sink: Box<dyn Sink<T>>,
    state: &SinkState,
) {
    let _completion = WorkerCompletionGuard { state };
    loop {
        match receiver.recv_timeout(WORKER_POLL) {
            Ok(Command::Deliver(item)) => deliver_item(sink.as_mut(), item, state),
            Ok(Command::Flush(reply)) => {
                let result = flush_sink(sink.as_mut(), state);
                let _reply_result = reply.try_send(result);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if state.shutdown_requested.load(Ordering::Acquire) {
            match receiver.try_recv() {
                Ok(Command::Deliver(item)) => deliver_item(sink.as_mut(), item, state),
                Ok(Command::Flush(reply)) => {
                    let result = flush_sink(sink.as_mut(), state);
                    let _reply_result = reply.try_send(result);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                    if let Err(error) = flush_sink(sink.as_mut(), state) {
                        if let Ok(mut shutdown_error) = state.shutdown_error.lock() {
                            *shutdown_error = Some(error);
                        }
                    }
                    break;
                }
            }
        }
    }
}

struct WorkerCompletionGuard<'a> {
    state: &'a SinkState,
}

impl Drop for WorkerCompletionGuard<'_> {
    fn drop(&mut self) {
        self.state.worker_finished.store(true, Ordering::Release);
    }
}

fn deliver_item<T: 'static>(sink: &mut dyn Sink<T>, item: T, state: &SinkState) {
    match catch_unwind(AssertUnwindSafe(|| sink.deliver(item))) {
        Ok(Ok(())) => {
            state.delivered.fetch_add(1, Ordering::Relaxed);
            state.delivery_healthy.store(true, Ordering::Release);
        }
        Ok(Err(_)) => state.record_delivery_failure(),
        Err(payload) => {
            state.record_delivery_failure();
            state.mark_worker_panicked();
            resume_unwind(payload);
        }
    }
}

fn flush_sink<T: 'static>(sink: &mut dyn Sink<T>, state: &SinkState) -> Result<(), SinkError> {
    match catch_unwind(AssertUnwindSafe(|| sink.flush())) {
        Ok(Ok(())) => {
            state.flush_healthy.store(true, Ordering::Release);
            Ok(())
        }
        Ok(Err(error)) => {
            state.record_flush_failure();
            Err(error)
        }
        Err(payload) => {
            state.record_flush_failure();
            state.mark_worker_panicked();
            resume_unwind(payload);
        }
    }
}

/// Independently counted class of bounded sink loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SinkLossClass {
    /// A producer found the fixed-capacity queue full.
    QueueFull,
    /// A producer submitted after shutdown or worker closure.
    QueueClosed,
    /// The adapter rejected a dequeued item.
    DeliveryFailure,
    /// The adapter rejected an explicit or shutdown flush.
    FlushFailure,
}

impl SinkLossClass {
    /// Returns the stable canonical loss-class name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QueueFull => "queue_full",
            Self::QueueClosed => "queue_closed",
            Self::DeliveryFailure => "delivery_failure",
            Self::FlushFailure => "flush_failure",
        }
    }
}

/// Atomic loss delta for one originating sink and loss class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkLossDelta {
    /// Configured originating sink name.
    sink: String,
    /// Loss mechanism.
    class: SinkLossClass,
    /// Events since the previous complete drain.
    count: u64,
}

impl SinkLossDelta {
    /// Returns the configured originating sink name.
    #[must_use]
    pub fn sink(&self) -> &str {
        &self.sink
    }

    /// Returns the independently counted loss class.
    #[must_use]
    pub const fn class(&self) -> SinkLossClass {
        self.class
    }

    /// Returns events since the previous complete drain.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }
}

/// Result of one nonblocking producer submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Record entered the bounded queue.
    Enqueued,
    /// Queue was at its exact capacity.
    QueueFull,
    /// Shutdown was requested or the worker closed.
    Closed,
}

/// Monotonic counters for one sink worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkStats {
    /// Configured sink name.
    pub name: String,
    /// Records rejected before delivery.
    pub dropped: u64,
    /// Records delivered successfully.
    pub delivered: u64,
    /// Delivery or flush calls that failed.
    pub failures: u64,
    /// Whether the worker is alive and its most recent adapter operations succeeded.
    pub healthy: bool,
    /// Whether the dedicated worker has exited.
    pub worker_finished: bool,
}

/// Sink construction failure.
#[derive(Debug)]
pub enum SinkBuildError {
    /// Name is empty, oversized, or control-bearing.
    InvalidName,
    /// A zero-capacity rendezvous queue is not supported.
    ZeroCapacity,
    /// Dedicated worker creation failed.
    Spawn(std::io::Error),
}

impl Display for SinkBuildError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName => formatter.write_str("sink name is invalid"),
            Self::ZeroCapacity => formatter.write_str("sink queue capacity must be positive"),
            Self::Spawn(error) => write!(formatter, "sink worker spawn failed: {error}"),
        }
    }
}

impl Error for SinkBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::InvalidName | Self::ZeroCapacity => None,
        }
    }
}

/// Bounded flush or shutdown failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitError {
    /// The supplied wait bound elapsed.
    Timeout,
    /// Worker closed before acknowledging the operation.
    Closed,
    /// Adapter flush failed.
    Sink(SinkError),
    /// Worker join state was poisoned.
    Poisoned,
    /// Worker panicked.
    WorkerPanicked,
}

impl Display for WaitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("sink operation exceeded its wait bound"),
            Self::Closed => formatter.write_str("sink worker is closed"),
            Self::Sink(error) => write!(formatter, "sink flush failed: {error}"),
            Self::Poisoned => formatter.write_str("sink worker state is poisoned"),
            Self::WorkerPanicked => formatter.write_str("sink worker panicked"),
        }
    }
}

impl Error for WaitError {}
