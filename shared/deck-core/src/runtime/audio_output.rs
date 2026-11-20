// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host audio on the system's default output, followed (GAP-026).
//!
//! A platform output stream (cpal on Core Audio or WASAPI) stays on the
//! device it opened. A Deck plays the host's sound on the *default* output,
//! so when the user picks another output, unplugs headphones or a USB
//! device, or the stream fails, the Deck must move with it without
//! reconnecting the desktop. Every Deck moves the same way:
//!
//! - [`OutputFollower`] makes every decision: when to open, when to let go
//!   of a stream, how long to back off after a failure, and that nothing
//!   opens once the output is closed. It is pure (the caller passes the
//!   time) and generic over the platform's device identity.
//! - [`FollowedOutput`] carries those decisions out on a thread of its own
//!   over a platform [`OutputBackend`], which only finds the default device
//!   and opens a stream on it. The audio worker that feeds the queue never
//!   waits for a device to open.
//!
//! Invariants, each tested here:
//!
//! - **One stream plays.** A replaced stream is silenced and dropped before
//!   the next one opens, so sound is never on the old and the new device at
//!   once.
//! - **Old callbacks play nothing.** A device callback carries the
//!   generation of the stream it belongs to, and only the active
//!   generation reads the queue. A late callback from a replaced stream
//!   writes silence and leaves the queue, and its underrun count, alone.
//! - **Closed stays closed.** After [`FollowedOutput::close`] (the session
//!   ended) no stream is activated again, even one whose open was in
//!   flight, and closing waits at most [`FollowPolicy::close_wait`].
//! - **The queue is the shared playout queue.** Adaptive playout, the
//!   bounded backlog and underrun accounting are
//!   [`arcen_media::audio::playback_queue`]'s; a gap while no device plays is
//!   not an underrun, and the queue keeps the newest audio within its bound.
//! - **The wire format does not change.** The queue is 48 kHz stereo
//!   (audio v1); a device in another format is adapted per stream
//!   ([`DeviceFormatAdapter`]), never renegotiated.

use std::marker::PhantomData;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arcen_media::audio::playback_queue::{DeviceFormatAdapter, PlaybackQueue, PlaybackSnapshot};

/// How the output follows the default device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowPolicy {
    /// How often the default device is read while a stream plays.
    pub device_poll: Duration,
    /// The wait after the first failure; it doubles with each failure in a
    /// row.
    pub retry_first: Duration,
    /// The longest wait between attempts.
    pub retry_max: Duration,
    /// A stream that played this long before failing starts the backoff
    /// over; one that fails sooner keeps doubling it, so a device that
    /// fails as soon as it opens cannot spin.
    pub stable_after: Duration,
    /// The longest [`FollowedOutput::close`] waits for the worker (a device
    /// open can hang); after it the worker finishes on its own, silenced.
    pub close_wait: Duration,
}

impl Default for FollowPolicy {
    fn default() -> Self {
        Self {
            device_poll: Duration::from_millis(500),
            retry_first: Duration::from_millis(250),
            retry_max: Duration::from_secs(4),
            stable_after: Duration::from_secs(5),
            close_wait: Duration::from_secs(2),
        }
    }
}

impl FollowPolicy {
    /// The wait after `failures` failures in a row (at least one).
    #[must_use]
    pub fn backoff(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        self.retry_first
            .saturating_mul(1_u32 << doublings)
            .min(self.retry_max)
    }
}

/// Why the output is being rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildReason {
    /// The system's default output is now another device.
    DefaultChanged,
    /// The stream reported an error (device removed, audio service
    /// restarted).
    StreamFailed,
    /// The default device refused to open.
    OpenFailed,
    /// The system reports no default output device.
    NoDefaultDevice,
}

impl RebuildReason {
    /// The reason as it appears in logs and Stream Statistics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DefaultChanged => "default_changed",
            Self::StreamFailed => "stream_error",
            Self::OpenFailed => "open_failed",
            Self::NoDefaultDevice => "no_default_device",
        }
    }
}

/// Where the output's lifecycle is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputPhase {
    /// Opening the first stream.
    #[default]
    Opening,
    /// A stream plays on the default device.
    Playing,
    /// Moving to another stream (or waiting for a device to come back).
    Rebuilding,
    /// The session ended; nothing plays again.
    Closed,
}

impl OutputPhase {
    /// The phase as it appears in logs and Stream Statistics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Opening => "opening",
            Self::Playing => "playing",
            Self::Rebuilding => "rebuilding",
            Self::Closed => "closed",
        }
    }
}

/// The output's state for diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputStatus {
    /// Where the lifecycle is.
    pub phase: OutputPhase,
    /// The device the current (or last) stream plays on.
    pub device: String,
    /// That device's stream format, for example `48000 Hz 2 ch F32`.
    pub format: String,
    /// The current (or last) stream's generation.
    pub generation: u64,
    /// Streams opened after the first: moves to another device or
    /// recoveries.
    pub rebuilds: u64,
    /// Errors reported by active streams.
    pub stream_errors: u64,
    /// Opens that failed after the first stream played.
    pub open_failures: u64,
    /// Failures in a row while rebuilding (0 while a stream plays).
    pub attempt: u32,
    /// Why the output is (or was last) rebuilt.
    pub reason: Option<RebuildReason>,
    /// The last stream or open error.
    pub last_error: Option<String>,
}

impl OutputStatus {
    /// What Stream Statistics shows about the device: nothing while the
    /// first stream plays untroubled, otherwise the rebuild state.
    #[must_use]
    pub fn note(&self) -> Option<String> {
        let reason = self.reason.map_or("unknown", RebuildReason::as_str);
        match self.phase {
            OutputPhase::Opening => Some("audio output opening".to_owned()),
            OutputPhase::Closed => Some("audio output closed".to_owned()),
            OutputPhase::Rebuilding => Some(format!(
                "audio output rebuilding ({reason}{}){}",
                if self.attempt > 0 {
                    format!(", attempt {}", self.attempt)
                } else {
                    String::new()
                },
                self.last_error
                    .as_deref()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            )),
            OutputPhase::Playing if self.rebuilds == 0 && self.stream_errors == 0 => None,
            OutputPhase::Playing => Some(format!(
                "audio output on {} (rebuilds={} stream_errors={} last={reason})",
                self.device, self.rebuilds, self.stream_errors
            )),
        }
    }
}

/// Why an open did not produce a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenFailure {
    /// The system reports no default output device.
    NoDefaultDevice,
    /// The default device refused; the platform's reason.
    Refused(String),
}

impl OpenFailure {
    fn reason(&self) -> RebuildReason {
        match self {
            Self::NoDefaultDevice => RebuildReason::NoDefaultDevice,
            Self::Refused(_) => RebuildReason::OpenFailed,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::NoDefaultDevice => "No default audio output device".to_owned(),
            Self::Refused(message) => message.clone(),
        }
    }
}

/// What the output worker does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowStep {
    /// Open a stream on the current default device as `generation`.
    Open {
        /// The new stream's generation.
        generation: u64,
    },
    /// Silence and drop the held stream before anything else.
    Release {
        /// The stream to let go of.
        generation: u64,
    },
    /// A stream plays: wait for a stream error until `until`, then read the
    /// default device.
    Watch {
        /// When to read the default device.
        until: Instant,
    },
    /// No stream: wait until `until` (backoff), reading the default device.
    Wait {
        /// When to look again.
        until: Instant,
    },
    /// Closed and released: the worker ends.
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowState {
    /// Ready to open (first open, or a rebuild that needs no wait).
    Ready,
    Opening {
        generation: u64,
    },
    Playing {
        since: Instant,
    },
    Backoff {
        until: Instant,
    },
    Closed,
}

/// The default-output lifecycle decisions, for any platform device
/// identity `D` (two values are equal when they name the same device).
#[derive(Debug)]
pub struct OutputFollower<D> {
    policy: FollowPolicy,
    state: FollowState,
    held: Option<(u64, D)>,
    next_generation: u64,
    failures: u32,
    /// The default device when the last failure happened: a different
    /// default ends the backoff at once.
    failed_default: Option<D>,
    opened_once: bool,
    status: OutputStatus,
}

impl<D: PartialEq + Clone> OutputFollower<D> {
    /// A follower about to open its first stream.
    #[must_use]
    pub fn new(policy: FollowPolicy) -> Self {
        Self {
            policy,
            state: FollowState::Ready,
            held: None,
            next_generation: 1,
            failures: 0,
            failed_default: None,
            opened_once: false,
            status: OutputStatus::default(),
        }
    }

    /// The state for diagnostics.
    #[must_use]
    pub fn status(&self) -> &OutputStatus {
        &self.status
    }

    /// The generation of the stream held now, if any.
    #[must_use]
    pub fn held_generation(&self) -> Option<u64> {
        self.held.as_ref().map(|(generation, _)| *generation)
    }

    /// What to do next. A held stream is always released before another
    /// opens, and nothing opens once closed.
    pub fn next_step(&mut self, now: Instant) -> FollowStep {
        let playing = matches!(self.state, FollowState::Playing { .. });
        if let Some(generation) = self.held_generation().filter(|_| !playing) {
            return FollowStep::Release { generation };
        }
        match self.state {
            FollowState::Closed => FollowStep::Stop,
            FollowState::Playing { .. } => FollowStep::Watch {
                until: now + self.policy.device_poll,
            },
            FollowState::Opening { .. } => FollowStep::Wait {
                until: now + self.policy.device_poll,
            },
            FollowState::Backoff { until } if now < until => FollowStep::Wait {
                until: until.min(now + self.policy.device_poll),
            },
            FollowState::Backoff { .. } | FollowState::Ready => {
                let generation = self.next_generation;
                self.next_generation += 1;
                self.state = FollowState::Opening { generation };
                FollowStep::Open { generation }
            }
        }
    }

    /// The stream `generation` opened on `device`. Returns whether it may
    /// play: `false` for an open that was overtaken (closed meanwhile), which
    /// the next step releases.
    pub fn opened(
        &mut self,
        generation: u64,
        device: D,
        name: String,
        format: String,
        now: Instant,
    ) -> bool {
        self.held = Some((generation, device));
        if self.state != (FollowState::Opening { generation }) {
            return false;
        }
        self.state = FollowState::Playing { since: now };
        if self.opened_once {
            self.status.rebuilds += 1;
        }
        self.opened_once = true;
        self.status.phase = OutputPhase::Playing;
        self.status.device = name;
        self.status.format = format;
        self.status.generation = generation;
        self.status.attempt = 0;
        true
    }

    /// The open of `generation` failed while the default was
    /// `failed_default`: back off, longer with each failure in a row.
    pub fn open_failed(
        &mut self,
        generation: u64,
        failure: &OpenFailure,
        failed_default: Option<D>,
        now: Instant,
    ) {
        if self.state != (FollowState::Opening { generation }) {
            return;
        }
        if self.opened_once {
            self.status.open_failures += 1;
        }
        self.back_off(failure.reason(), failure.message(), failed_default, now);
    }

    /// The stream `generation` reported `error`. Returns whether it was the
    /// playing stream; an error from a replaced stream changes nothing.
    pub fn stream_failed(&mut self, generation: u64, error: &str, now: Instant) -> bool {
        let FollowState::Playing { since } = self.state else {
            return false;
        };
        let Some((held, device)) = &self.held else {
            return false;
        };
        if *held != generation {
            return false;
        }
        if now.saturating_duration_since(since) >= self.policy.stable_after {
            self.failures = 0;
        }
        let device = device.clone();
        self.status.stream_errors += 1;
        self.back_off(
            RebuildReason::StreamFailed,
            error.to_owned(),
            Some(device),
            now,
        );
        true
    }

    /// The system's default output now. While a stream plays, another
    /// default moves the output at once and starts the backoff over; while
    /// backing off, a default other than the one that failed ends the wait.
    /// No default while playing keeps the stream (its own error ends it if
    /// the device went away).
    pub fn observe_default(&mut self, current: Option<&D>) {
        match self.state {
            FollowState::Playing { .. } => {
                let moved = matches!(
                    (current, &self.held),
                    (Some(current), Some((_, held))) if current != held
                );
                if moved {
                    // A move the user made is not a failure: the next
                    // device starts the backoff over.
                    self.failures = 0;
                    self.state = FollowState::Ready;
                    self.status.phase = OutputPhase::Rebuilding;
                    self.status.reason = Some(RebuildReason::DefaultChanged);
                }
            }
            FollowState::Backoff { .. } => {
                if current.is_some() && current != self.failed_default.as_ref() {
                    self.state = FollowState::Ready;
                }
            }
            FollowState::Ready | FollowState::Opening { .. } | FollowState::Closed => {}
        }
    }

    /// The stream `generation` was silenced and dropped.
    pub fn released(&mut self, generation: u64) {
        if self.held_generation() == Some(generation) {
            self.held = None;
        }
    }

    /// The session ended: release what is held, open nothing again.
    pub fn close(&mut self) {
        self.state = FollowState::Closed;
        self.status.phase = OutputPhase::Closed;
    }

    fn back_off(
        &mut self,
        reason: RebuildReason,
        error: String,
        failed_default: Option<D>,
        now: Instant,
    ) {
        self.failures = self.failures.saturating_add(1);
        self.failed_default = failed_default;
        self.state = FollowState::Backoff {
            until: now + self.policy.backoff(self.failures),
        };
        self.status.phase = OutputPhase::Rebuilding;
        self.status.attempt = self.failures;
        self.status.reason = Some(reason);
        self.status.last_error = Some(error);
    }
}

/// A platform's output: finds the default device and opens a stream on it.
/// Created and used only on the output worker's thread.
pub trait OutputBackend {
    /// A device identity; equal values name the same device.
    type Device: PartialEq + Clone;
    /// An open stream; dropping it stops it.
    type Stream;
    /// The backend label in Stream Statistics, for example
    /// `cpal-coreaudio-48k-stereo`.
    const BACKEND: &'static str;

    /// The system's default output device now.
    fn default_device(&mut self) -> Option<Self::Device>;

    /// Opens and starts a stream on `device` whose callback renders through
    /// [`StreamTicket::renderer`] and reports errors through
    /// [`StreamTicket::errors`].
    ///
    /// # Errors
    ///
    /// Why the device refused, shown in Stream Statistics.
    fn open(
        &mut self,
        device: &Self::Device,
        ticket: &StreamTicket,
    ) -> Result<OpenedStream<Self::Stream>, String>;
}

/// A stream an [`OutputBackend`] opened.
#[derive(Debug)]
pub struct OpenedStream<S> {
    /// The stream; dropping it stops it.
    pub stream: S,
    /// The device's name.
    pub device: String,
    /// The stream's format, for example `48000 Hz 2 ch F32`.
    pub format: String,
}

struct Gate {
    queue: PlaybackQueue,
    /// The only generation whose callbacks read the queue.
    active: Option<u64>,
    closed: bool,
    /// Counts queue resets, so a renderer forgets its interpolation.
    reset_epoch: u64,
}

#[derive(Default)]
struct Signals {
    stop: bool,
    /// The newest generation's error, kept until the worker reads it.
    stream_error: Option<(u64, String)>,
}

struct Shared {
    gate: Mutex<Gate>,
    signals: Mutex<Signals>,
    wake: Condvar,
    status: Mutex<OutputStatus>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn activate(&self, generation: u64) -> bool {
        let mut gate = lock(&self.gate);
        if gate.closed {
            return false;
        }
        gate.active = Some(generation);
        true
    }

    fn deactivate(&self, generation: u64) {
        let mut gate = lock(&self.gate);
        if gate.active == Some(generation) {
            gate.active = None;
        }
    }

    fn stop_requested(&self) -> bool {
        lock(&self.signals).stop
    }

    /// Waits until `until`, a stream error or a stop; returns the error.
    fn wait_until(&self, until: Instant) -> Option<(u64, String)> {
        let mut signals = lock(&self.signals);
        loop {
            if signals.stop {
                return None;
            }
            if let Some(error) = signals.stream_error.take() {
                return Some(error);
            }
            let now = Instant::now();
            if now >= until {
                return None;
            }
            signals = self
                .wake
                .wait_timeout(signals, until - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// What a backend needs to open one stream: its renderer and error
/// reporter, bound to the stream's generation.
pub struct StreamTicket {
    shared: Arc<Shared>,
    generation: u64,
}

impl std::fmt::Debug for StreamTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamTicket")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl StreamTicket {
    /// The stream's generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The callback state for a device that plays `rate_hz` with
    /// `channels` (48 kHz stereo passes through; others are adapted).
    ///
    /// # Errors
    ///
    /// A zero rate or channel count.
    pub fn renderer(&self, rate_hz: u32, channels: u16) -> Result<StreamRenderer, String> {
        let adapter = DeviceFormatAdapter::new(rate_hz, channels)
            .ok_or_else(|| "The output device reports no channels".to_owned())?;
        let reset_epoch = lock(&self.shared.gate).reset_epoch;
        Ok(StreamRenderer {
            shared: Arc::clone(&self.shared),
            generation: self.generation,
            adapter,
            reset_epoch,
            scratch: Vec::new(),
        })
    }

    /// The stream's error reporter, for its error callback.
    #[must_use]
    pub fn errors(&self) -> StreamErrors {
        StreamErrors {
            shared: Arc::clone(&self.shared),
            generation: self.generation,
        }
    }
}

/// A stream's error callback: wakes the worker to rebuild.
#[derive(Clone)]
pub struct StreamErrors {
    shared: Arc<Shared>,
    generation: u64,
}

impl std::fmt::Debug for StreamErrors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamErrors")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl StreamErrors {
    /// Reports that the stream failed.
    pub fn report(&self, error: impl Into<String>) {
        let mut signals = lock(&self.shared.signals);
        let newer = signals
            .stream_error
            .as_ref()
            .is_none_or(|(generation, _)| self.generation >= *generation);
        if newer {
            signals.stream_error = Some((self.generation, error.into()));
        }
        drop(signals);
        self.shared.wake.notify_all();
    }
}

/// One stream's device callback state. Only the active generation reads
/// the queue; any other writes silence. Allocates only on the first
/// callback of a non-float device.
pub struct StreamRenderer {
    shared: Arc<Shared>,
    generation: u64,
    adapter: DeviceFormatAdapter,
    reset_epoch: u64,
    scratch: Vec<f32>,
}

impl std::fmt::Debug for StreamRenderer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamRenderer")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl StreamRenderer {
    /// Fills normalised float samples.
    pub fn fill_f32(&mut self, out: &mut [f32]) {
        let mut gate = lock(&self.shared.gate);
        if gate.active != Some(self.generation) {
            out.fill(0.0);
            return;
        }
        if gate.reset_epoch != self.reset_epoch {
            self.reset_epoch = gate.reset_epoch;
            self.adapter.reset();
        }
        self.adapter.fill_f32(&mut gate.queue, out);
    }

    /// Fills signed 16-bit samples.
    pub fn fill_i16(&mut self, out: &mut [i16]) {
        if self.adapter.is_passthrough() {
            let mut gate = lock(&self.shared.gate);
            if gate.active == Some(self.generation) {
                gate.queue.fill_i16(out);
            } else {
                out.fill(0);
            }
            return;
        }
        self.fill_converted(out, |value| {
            #[allow(clippy::cast_possible_truncation, reason = "clamped to i16")]
            let sample = (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
            sample
        });
    }

    /// Fills offset-binary unsigned 16-bit samples.
    pub fn fill_u16(&mut self, out: &mut [u16]) {
        if self.adapter.is_passthrough() {
            let mut gate = lock(&self.shared.gate);
            if gate.active == Some(self.generation) {
                gate.queue.fill_u16(out);
            } else {
                out.fill(32_768);
            }
            return;
        }
        self.fill_converted(out, |value| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to the u16 range"
            )]
            let sample = ((value.clamp(-1.0, 1.0) + 1.0) * 32_767.5).round() as u16;
            sample
        });
    }

    fn fill_converted<T>(&mut self, out: &mut [T], convert: impl Fn(f32) -> T) {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.resize(out.len(), 0.0);
        self.fill_f32(&mut scratch);
        for (sample, value) in out.iter_mut().zip(&scratch) {
            *sample = convert(*value);
        }
        self.scratch = scratch;
    }
}

/// The playback queue on the default output device, followed by a worker
/// thread that owns the platform streams.
pub struct FollowedOutput<B> {
    shared: Arc<Shared>,
    worker: Option<(JoinHandle<()>, mpsc::Receiver<()>)>,
    close_wait: Duration,
    _backend: PhantomData<fn() -> B>,
}

impl<B> std::fmt::Debug for FollowedOutput<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FollowedOutput")
            .field("status", &*lock(&self.shared.status))
            .finish_non_exhaustive()
    }
}

impl<B: OutputBackend + 'static> FollowedOutput<B> {
    /// Opens the default output with the default policy.
    ///
    /// # Errors
    ///
    /// Why the first stream could not open (no device, or the device
    /// refused). A first failure is final: the player then plays nothing.
    pub fn start(make_backend: impl FnOnce() -> B + Send + 'static) -> Result<Self, String> {
        Self::start_with(FollowPolicy::default(), make_backend)
    }

    /// Opens the default output and starts following it. `make_backend`
    /// runs on the worker thread; this returns once the first stream plays
    /// or failed.
    ///
    /// # Errors
    ///
    /// Why the first stream could not open, or the thread could not start.
    pub fn start_with(
        policy: FollowPolicy,
        make_backend: impl FnOnce() -> B + Send + 'static,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            gate: Mutex::new(Gate {
                queue: PlaybackQueue::new(),
                active: None,
                closed: false,
                reset_epoch: 0,
            }),
            signals: Mutex::new(Signals::default()),
            wake: Condvar::new(),
            status: Mutex::new(OutputStatus::default()),
        });
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("audio-output".to_owned())
            .spawn(move || {
                let _done = done_tx;
                let backend = make_backend();
                run_worker(&worker_shared, policy, backend, started_tx);
            })
            .map_err(|error| format!("Could not start the audio output thread: {error}"))?;
        match started_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                worker: Some((worker, done_rx)),
                close_wait: policy.close_wait,
                _backend: PhantomData,
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err("The audio output thread ended before opening a device".to_owned())
            }
        }
    }
}

impl<B> FollowedOutput<B> {
    /// Queues little-endian PCM; returns the bytes accepted.
    #[must_use]
    pub fn push_s16le(&self, payload: &[u8]) -> usize {
        lock(&self.shared.gate).queue.push_s16le(payload)
    }

    /// Queues interleaved samples; returns the bytes accepted.
    #[must_use]
    pub fn push_samples(&self, samples: &[i16]) -> usize {
        lock(&self.shared.gate).queue.push_samples(samples)
    }

    /// Drops what is queued and prebuffers again.
    pub fn reset(&self) {
        let mut gate = lock(&self.shared.gate);
        gate.queue.reset();
        gate.reset_epoch = gate.reset_epoch.wrapping_add(1);
    }

    /// The queue's state.
    #[must_use]
    pub fn snapshot(&self) -> PlaybackSnapshot {
        lock(&self.shared.gate).queue.snapshot()
    }

    /// The device lifecycle's state.
    #[must_use]
    pub fn status(&self) -> OutputStatus {
        lock(&self.shared.status).clone()
    }

    /// Ends playback for good: silences every stream at once, drops what
    /// is queued, and waits at most [`FollowPolicy::close_wait`] for the
    /// worker to release its stream. Idempotent.
    pub fn close(&mut self) {
        {
            let mut gate = lock(&self.shared.gate);
            gate.closed = true;
            gate.active = None;
            gate.queue.reset();
        }
        lock(&self.shared.signals).stop = true;
        self.shared.wake.notify_all();
        let Some((worker, done)) = self.worker.take() else {
            return;
        };
        match done.recv_timeout(self.close_wait) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    target: crate::log_target::AUDIO,
                    wait_ms = u64::try_from(self.close_wait.as_millis()).unwrap_or(u64::MAX),
                    "audio output worker still busy at close; it finishes silenced"
                );
            }
        }
    }
}

impl<B> Drop for FollowedOutput<B> {
    fn drop(&mut self) {
        self.close();
    }
}

fn run_worker<B: OutputBackend>(
    shared: &Arc<Shared>,
    policy: FollowPolicy,
    mut backend: B,
    started: mpsc::SyncSender<Result<(), String>>,
) {
    let mut follower = OutputFollower::<B::Device>::new(policy);
    let mut stream: Option<B::Stream> = None;
    let mut started = Some(started);
    loop {
        let before = follower.status().clone();
        if shared.stop_requested() {
            follower.close();
        }
        match follower.next_step(Instant::now()) {
            FollowStep::Open { generation } => {
                let ticket = StreamTicket {
                    shared: Arc::clone(shared),
                    generation,
                };
                let outcome = match backend.default_device() {
                    None => Err((OpenFailure::NoDefaultDevice, None)),
                    Some(device) => match backend.open(&device, &ticket) {
                        Ok(opened) => Ok((device, opened)),
                        Err(error) => Err((OpenFailure::Refused(error), Some(device))),
                    },
                };
                match outcome {
                    Ok((device, opened)) => {
                        stream = Some(opened.stream);
                        if shared.stop_requested() {
                            follower.close();
                        }
                        let current = follower.opened(
                            generation,
                            device,
                            opened.device,
                            opened.format,
                            Instant::now(),
                        );
                        if current && shared.activate(generation) {
                            if let Some(started) = started.take() {
                                // The caller reads the device at once.
                                lock(&shared.status).clone_from(follower.status());
                                let _ = started.send(Ok(()));
                            }
                        } else {
                            // Closed while it opened: released next.
                            follower.close();
                        }
                    }
                    Err((failure, failed_default)) => {
                        follower.open_failed(generation, &failure, failed_default, Instant::now());
                        if let Some(started) = started.take() {
                            let _ = started.send(Err(failure.message()));
                            follower.close();
                        }
                    }
                }
            }
            FollowStep::Release { generation } => {
                shared.deactivate(generation);
                // Dropping may join the platform's render thread, which can
                // still report an error on its way out: that error carries
                // this generation and changes nothing.
                drop(stream.take());
                follower.released(generation);
            }
            FollowStep::Watch { until } | FollowStep::Wait { until } => {
                if let Some((generation, error)) = shared.wait_until(until) {
                    follower.stream_failed(generation, &error, Instant::now());
                } else if !shared.stop_requested() {
                    follower.observe_default(backend.default_device().as_ref());
                }
            }
            FollowStep::Stop => break,
        }
        let after = follower.status().clone();
        if after != before {
            log_transition(&before, &after);
            lock(&shared.status).clone_from(&after);
        }
    }
}

fn log_transition(before: &OutputStatus, after: &OutputStatus) {
    let reason = after.reason.map_or("none", RebuildReason::as_str);
    match after.phase {
        OutputPhase::Playing if before.phase != OutputPhase::Playing => tracing::info!(
            target: crate::log_target::AUDIO,
            device = %after.device,
            format = %after.format,
            generation = after.generation,
            rebuilds = after.rebuilds,
            reason,
            "audio output playing on the default device"
        ),
        OutputPhase::Rebuilding
            if before.attempt != after.attempt || before.phase != after.phase =>
        {
            tracing::warn!(
                target: crate::log_target::AUDIO,
                device = %after.device,
                reason,
                attempt = after.attempt,
                stream_errors = after.stream_errors,
                open_failures = after.open_failures,
                error = after.last_error.as_deref().unwrap_or(""),
                "audio output rebuilding"
            );
        }
        OutputPhase::Closed if before.phase != OutputPhase::Closed => tracing::info!(
            target: crate::log_target::AUDIO,
            rebuilds = after.rebuilds,
            stream_errors = after.stream_errors,
            "audio output closed"
        ),
        _ => {}
    }
}

impl<B: OutputBackend + Default + 'static> crate::runtime::audio_player::AudioOutputDevice
    for FollowedOutput<B>
{
    fn open() -> Result<Self, String> {
        Self::start(B::default)
    }

    fn backend(&self) -> &'static str {
        B::BACKEND
    }

    fn push_s16le(&self, payload: &[u8]) -> usize {
        Self::push_s16le(self, payload)
    }

    fn push_samples(&self, samples: &[i16]) -> usize {
        Self::push_samples(self, samples)
    }

    fn reset(&self) {
        Self::reset(self);
    }

    fn snapshot(&self) -> PlaybackSnapshot {
        Self::snapshot(self)
    }

    fn device_note(&self) -> Option<String> {
        self.status().note()
    }
}

#[cfg(test)]
mod tests;
