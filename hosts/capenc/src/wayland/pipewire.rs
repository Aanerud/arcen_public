//! `PipeWire` stream consumer for one compositor output.
//!
//! One thread owns the `PipeWire` main loop, context, core and stream; no
//! `PipeWire` object leaves it. The process callback dequeues every ready
//! buffer, classifies and validates it, and either requeues it at once
//! (cursor-only, corrupted, refused, superseded) or leases it into the
//! bounded [`CaptureMailbox`]. It counts; it never logs, waits on another
//! thread's work, or touches the GPU.
//!
//! A lease's token goes back to the loop thread through a `PipeWire` channel
//! when the import worker drops it; only the loop thread requeues buffers.
//! [`LeaseGate`] makes buffer removal safe: removing buffers (format change,
//! renegotiation, shutdown) takes the gate's write lock and advances its
//! epoch, so it waits for a reader that is mid-copy and revokes every
//! outstanding lease. A revoked token is never dereferenced or requeued.
//!
//! **Explicit sync.** When DMA-BUF is offered and a [`SyncobjDevice`] is
//! configured, the stream asks for `SPA_META_SyncTimeline` with two extra
//! `SyncObj` data blocks per buffer (Mutter 49 grants it on a DRM device with
//! timeline syncobjs). The sync blocks never become planes: a lease carries
//! the acquire and release points ([`BufferToken::explicit_sync`]), the
//! import worker waits for the acquire point before reading, and the loop
//! thread signals the release point immediately before it queues any such
//! buffer back — leased, superseded or refused alike — which is only ever
//! after the worker stopped reading it. A buffer without the meta (shared
//! memory, or a producer that declined) is synchronised implicitly.
//!
//! **Fallback.** [`PipeWireSource::fall_back_to_host_memory`] withdraws the
//! DMA-BUF formats; the producer renegotiates to shared memory, revoking
//! every outstanding lease on the way.

use std::cell::RefCell;
use std::os::fd::{BorrowedFd, RawFd};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arcen_media::video::{
    CaptureImporter, CapturePixelFormat, CopyPath, FormatModifier, GpuIdentity, SourceColour,
    SourceFact, SourceTransfer,
};
use arcen_media::{ColorMatrix, ColorPrimaries, ColorRange};
use pipewire as pw;
use pw::spa;
use spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use spa::sys as spa_sys;
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

use super::cursor::{parse_cursor_meta, CursorState, PendingCursor};
use super::explicit_sync::{
    hand_back_release, sync_block_indices, ReleaseFailures, SyncPoints, SyncTimelineMeta,
    SyncobjDevice, SPA_DATA_SYNC_OBJ, SPA_META_SYNC_TIMELINE, SPA_META_SYNC_TIMELINE_BYTES,
    SPA_PARAM_BUFFERS_BLOCKS, SPA_PARAM_BUFFERS_META_TYPE, SYNC_OBJ_BLOCKS,
};
use super::format::{
    cursor_meta_size, FormatOffer, MemoryOffer, NegotiatedFormat, CURSOR_BITMAP_MAX_SIDE,
    DAMAGE_META_MAX_REGIONS, SPA_META_HEADER_BYTES, SPA_META_REGION_BYTES,
};
use super::handoff::WaylandHandoff;
use super::layout::{
    classify, Classified, DataKind, HeaderMeta, LayoutContext, RawBuffer, RawPlane,
};
use arcen_media::video::{
    CaptureLease, CaptureLeaseCounters, CaptureLeaseReleaser, CaptureMailbox, CaptureTakeError,
};

/// Revocation gate for every lease of one stream.
#[derive(Debug, Default)]
pub struct LeaseGate {
    epoch: RwLock<u64>,
}

impl LeaseGate {
    fn current(&self) -> u64 {
        *self
            .epoch
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Revokes every outstanding lease, waiting for any reader in progress.
    fn revoke(&self) {
        let mut epoch = self
            .epoch
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *epoch = epoch.wrapping_add(1);
    }
}

/// One plane of a leased buffer.
#[derive(Debug, Clone, Copy)]
enum PlaneAccess {
    Host { data: NonNull<u8>, len: usize },
    DmaBuf { fd: RawFd },
    Unavailable,
}

/// A borrowed view of one plane while the lease's gate is held.
#[derive(Debug, Clone, Copy)]
pub enum PlaneView<'lease> {
    /// The mapped buffer object, `spa_data.maxsize` bytes from its start.
    Host(&'lease [u8]),
    /// The DMA-BUF descriptor.
    DmaBuf(BorrowedFd<'lease>),
    /// Not accessible from this process.
    Unavailable,
}

/// The lease was revoked by buffer removal; the buffer must not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseRevoked;

/// A dequeued `PipeWire` buffer. Only the loop thread dereferences
/// `buffer`; other threads read plane memory through [`Self::read`].
#[derive(Debug)]
pub struct BufferToken {
    buffer: NonNull<pw::sys::pw_buffer>,
    epoch: u64,
    gate: Arc<LeaseGate>,
    planes: Vec<PlaneAccess>,
    sync: Option<SyncPoints>,
}

// SAFETY: the raw `pw_buffer` pointer is only dereferenced on the loop thread
// that created the token (`requeue`), and only while the gate epoch proves the
// buffer still belongs to the stream. Plane memory is read on other threads
// only inside `read`, under the gate's read lock, which buffer removal must
// acquire exclusively before `PipeWire` unmaps anything.
unsafe impl Send for BufferToken {}

impl BufferToken {
    /// Runs `reader` over the buffer's planes while removal is excluded.
    ///
    /// # Errors
    ///
    /// [`LeaseRevoked`] when the buffer was removed since it was leased.
    pub fn read<R>(&self, reader: impl FnOnce(&[PlaneView<'_>]) -> R) -> Result<R, LeaseRevoked> {
        let epoch = self
            .gate
            .epoch
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *epoch != self.epoch {
            return Err(LeaseRevoked);
        }
        let views: Vec<PlaneView<'_>> = self
            .planes
            .iter()
            .map(|plane| match *plane {
                // SAFETY: `data`/`len` describe the region `PipeWire` mapped
                // for this dequeued buffer (`spa_data.data`, `maxsize`). It
                // stays mapped until the buffer is removed, which cannot
                // happen while this read lock is held, and the producer does
                // not write a buffer the consumer holds dequeued.
                PlaneAccess::Host { data, len } => {
                    PlaneView::Host(unsafe { std::slice::from_raw_parts(data.as_ptr(), len) })
                }
                // SAFETY: the descriptor belongs to the buffer and is closed
                // by `PipeWire` only on removal, excluded as above.
                PlaneAccess::DmaBuf { fd } => {
                    PlaneView::DmaBuf(unsafe { BorrowedFd::borrow_raw(fd) })
                }
                PlaneAccess::Unavailable => PlaneView::Unavailable,
            })
            .collect();
        Ok(reader(&views))
    }

    /// The buffer's explicit-sync points, when the producer attached them.
    /// The descriptors stay valid while the lease is unrevoked; use them
    /// only inside [`Self::read`].
    #[must_use]
    pub const fn explicit_sync(&self) -> Option<SyncPoints> {
        self.sync
    }
}

enum Command {
    Release(BufferToken),
    HostMemoryOnly,
    Stop,
}

struct ChannelReleaser(pw::channel::Sender<Command>);

impl CaptureLeaseReleaser<BufferToken> for ChannelReleaser {
    fn release(&self, token: BufferToken) {
        // If the loop is gone the token is simply dropped: its stream no
        // longer exists, so there is nothing to requeue.
        let _ = self.0.send(Command::Release(token));
    }
}

/// Stream phase, as last reported by `PipeWire`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamPhase {
    /// Connecting to the node.
    Connecting,
    /// Negotiated but not running.
    Paused,
    /// Delivering buffers.
    Streaming,
    /// Disconnected.
    Unconnected,
    /// `PipeWire` reported an error.
    Error(String),
}

/// Counters the callback updates; read by the worker for its once-a-second
/// report. Relaxed ordering: these are statistics.
#[derive(Debug, Default)]
pub struct SourceStats {
    /// `process` callbacks.
    pub process_calls: AtomicU64,
    /// `process` callbacks that found no buffer.
    pub empty_process: AtomicU64,
    /// Buffers dequeued.
    pub buffers: AtomicU64,
    /// Frames leased.
    pub frames: AtomicU64,
    /// Size-0 cursor-only buffers.
    pub cursor_only: AtomicU64,
    /// Buffers the producer marked corrupted.
    pub corrupted: AtomicU64,
    /// Buffers refused by classification.
    pub refused: AtomicU64,
    /// Buffers refused by `CaptureSurface::validate`.
    pub validation_refused: AtomicU64,
    /// Frames in shared memory.
    pub host_memory: AtomicU64,
    /// Frames in DMA-BUF.
    pub dmabuf: AtomicU64,
    /// DMA-BUF frames that carried explicit-sync points.
    pub explicit_sync: AtomicU64,
    /// Release points signalled before a buffer was queued back.
    pub release_signals: AtomicU64,
    /// Release points that could not be signalled; the buffer went back
    /// with the producer's unscheduled-release flag still set.
    pub release_signal_failures: AtomicU64,
    /// DMA-BUF withdrawn after repeated release-signal failures.
    pub release_fallbacks: AtomicU64,
    /// Time spent in `process`, nanoseconds.
    pub callback_ns: AtomicU64,
    /// Longest `process` call, nanoseconds.
    pub callback_ns_max: AtomicU64,
    /// Format changes.
    pub format_changes: AtomicU64,
    /// Lease revocations (buffer removal).
    pub revocations: AtomicU64,
    /// Leases requeued after release.
    pub requeued: AtomicU64,
    /// Releases ignored because the lease was revoked.
    pub stale_releases: AtomicU64,
    /// Last refusal token.
    pub last_refusal: Mutex<Option<&'static str>>,
}

/// State shared between the loop thread and consumers.
#[derive(Debug)]
pub struct SourceShared {
    negotiated: Mutex<Option<NegotiatedFormat>>,
    changed: Condvar,
    phase: Mutex<StreamPhase>,
    cursor: Mutex<CursorState>,
    /// Statistics.
    pub stats: SourceStats,
}

impl SourceShared {
    fn set_phase(&self, phase: StreamPhase) {
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = phase;
        self.changed.notify_all();
    }
}

/// What the source needs beyond the hand-off.
#[derive(Debug, Clone)]
pub struct SourceConfig {
    /// Formats, size, cadence and memory kinds to offer.
    pub offer: FormatOffer,
    /// Frames the mailbox may hold.
    pub mailbox_capacity: usize,
    /// Compositor GPU, when known.
    pub source_gpu: Option<GpuIdentity>,
    /// Encoder GPU, when known.
    pub encoder_gpu: Option<GpuIdentity>,
    /// Copy path DMA-BUF frames take, when DMA-BUF is offered.
    pub dmabuf_copy_path: Option<CopyPath>,
    /// Device for explicit-sync release points. With DMA-BUF offered, the
    /// stream asks the producer for explicit sync only when this is set.
    pub explicit_sync: Option<Arc<SyncobjDevice>>,
}

/// Why the source could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// Creating the loop, context or stream failed.
    Setup(String),
    /// Connecting to the `PipeWire` daemon failed.
    Connect(String),
    /// The loop thread ended before reporting.
    ThreadEnded,
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Setup(detail) => write!(formatter, "PipeWire setup failed: {detail}"),
            Self::Connect(detail) => write!(formatter, "PipeWire connect failed: {detail}"),
            Self::ThreadEnded => write!(formatter, "PipeWire thread ended during startup"),
        }
    }
}

impl std::error::Error for SourceError {}

/// A running `PipeWire` capture of one output.
pub struct PipeWireSource {
    commands: pw::channel::Sender<Command>,
    mailbox: Arc<CaptureMailbox<BufferToken>>,
    shared: Arc<SourceShared>,
    thread: Option<JoinHandle<()>>,
}

impl PipeWireSource {
    /// Connects to `handoff.node_id` and starts negotiating `config.offer`.
    ///
    /// # Errors
    ///
    /// [`SourceError`] when `PipeWire` cannot be reached or the stream cannot
    /// be created.
    pub fn start(handoff: &WaylandHandoff, config: SourceConfig) -> Result<Self, SourceError> {
        let (commands, receiver) = pw::channel::channel::<Command>();
        let mailbox = CaptureMailbox::new(
            config.mailbox_capacity,
            Arc::new(ChannelReleaser(commands.clone())),
        );
        let shared = Arc::new(SourceShared {
            negotiated: Mutex::new(None),
            changed: Condvar::new(),
            phase: Mutex::new(StreamPhase::Connecting),
            cursor: Mutex::new(CursorState::default()),
            stats: SourceStats::default(),
        });
        let (started, startup) = mpsc::sync_channel(1);
        let thread = {
            let handoff = handoff.clone();
            let mailbox = Arc::clone(&mailbox);
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("arcen-pw-capture".to_string())
                .spawn(move || {
                    let result = run_loop(&handoff, config, receiver, &mailbox, &shared, &started);
                    if let Err(error) = result {
                        let _ = started.try_send(Err(error.clone()));
                        shared.set_phase(StreamPhase::Error(error.to_string()));
                    }
                    mailbox.close();
                })
                .map_err(|error| SourceError::Setup(error.to_string()))?
        };
        match startup.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                mailbox,
                shared,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(SourceError::ThreadEnded)
            }
        }
    }

    /// Waits up to `timeout` for the producer to fix a format.
    #[must_use]
    pub fn wait_negotiated(&self, timeout: Duration) -> Option<NegotiatedFormat> {
        let deadline = Instant::now() + timeout;
        let mut negotiated = self
            .shared
            .negotiated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(format) = *negotiated {
                return Some(format);
            }
            let now = Instant::now();
            if now >= deadline || self.mailbox.is_closed() {
                return None;
            }
            negotiated = self
                .shared
                .changed
                .wait_timeout(negotiated, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// The format currently fixed, if any. `PipeWire` reports a producer that
    /// stops its stream as `Paused` with the format cleared, not as an error;
    /// a pipeline treats a missing format beyond its own bound as source loss.
    #[must_use]
    pub fn current_format(&self) -> Option<NegotiatedFormat> {
        *self
            .shared
            .negotiated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The newest frame, waiting up to `timeout`.
    ///
    /// # Errors
    ///
    /// [`CaptureTakeError::Closed`] once the stream ended.
    pub fn next_frame(
        &self,
        timeout: Duration,
    ) -> Result<Option<CaptureLease<BufferToken>>, CaptureTakeError> {
        self.mailbox.take_latest(timeout)
    }

    /// Current stream phase.
    #[must_use]
    pub fn phase(&self) -> StreamPhase {
        self.shared
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Latest cursor facts.
    #[must_use]
    pub fn cursor(&self) -> CursorState {
        self.shared
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Callback statistics.
    #[must_use]
    pub fn shared(&self) -> &SourceShared {
        &self.shared
    }

    /// Lease statistics.
    #[must_use]
    pub fn lease_counters(&self) -> Arc<CaptureLeaseCounters> {
        self.mailbox.counters()
    }

    /// Withdraws the DMA-BUF formats from the offer so the producer
    /// renegotiates to shared memory: the explicit fallback after a GPU
    /// import failure. Outstanding leases are revoked by the renegotiation.
    pub fn fall_back_to_host_memory(&self) {
        let _ = self.commands.send(Command::HostMemoryOnly);
    }
}

impl Drop for PipeWireSource {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct CallbackState {
    offer: Rc<RefCell<FormatOffer>>,
    explicit_sync: Option<Arc<SyncobjDevice>>,
    handoff_output: arcen_media::OutputIdentity,
    handoff_generation: arcen_media::RegionGeneration,
    source_gpu: Option<GpuIdentity>,
    encoder_gpu: Option<GpuIdentity>,
    dmabuf_copy_path: Option<CopyPath>,
    layout: Option<LayoutContext>,
    importer: Option<CaptureImporter>,
    pending_cursor: PendingCursor,
    sequence: u64,
    base: Instant,
    release_failures: Rc<RefCell<ReleaseFailures>>,
    gate: Arc<LeaseGate>,
    mailbox: Arc<CaptureMailbox<BufferToken>>,
    shared: Arc<SourceShared>,
}

fn run_loop(
    handoff: &WaylandHandoff,
    config: SourceConfig,
    receiver: pw::channel::Receiver<Command>,
    mailbox: &Arc<CaptureMailbox<BufferToken>>,
    shared: &Arc<SourceShared>,
    started: &mpsc::SyncSender<Result<(), SourceError>>,
) -> Result<(), SourceError> {
    pw::init();
    let setup = |error: pw::Error| SourceError::Setup(error.to_string());
    let main_loop = pw::main_loop::MainLoopRc::new(None).map_err(setup)?;
    let context = pw::context::ContextRc::new(&main_loop, None).map_err(setup)?;
    let socket = handoff.pipewire_socket().to_string_lossy().into_owned();
    let core = context
        .connect_rc(Some(pw::properties::properties! {
            *pw::keys::REMOTE_NAME => socket.as_str(),
        }))
        .map_err(|error| SourceError::Connect(format!("{socket}: {error}")))?;
    let stream = pw::stream::StreamRc::new(
        core,
        "arcen-capenc-wayland",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(setup)?;

    let gate = Arc::new(LeaseGate::default());
    let offer = Rc::new(RefCell::new(config.offer.clone()));
    let release_failures = Rc::new(RefCell::new(ReleaseFailures::default()));
    let state = CallbackState {
        offer: Rc::clone(&offer),
        explicit_sync: config.explicit_sync.clone(),
        handoff_output: handoff.output().clone(),
        handoff_generation: handoff.generation(),
        source_gpu: config.source_gpu,
        encoder_gpu: config.encoder_gpu,
        dmabuf_copy_path: config.dmabuf_copy_path,
        layout: None,
        importer: None,
        pending_cursor: PendingCursor::default(),
        sequence: 0,
        base: Instant::now(),
        release_failures: Rc::clone(&release_failures),
        gate: Arc::clone(&gate),
        mailbox: Arc::clone(mailbox),
        shared: Arc::clone(shared),
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, state, _, new| {
            let phase = match new {
                pw::stream::StreamState::Error(message) => StreamPhase::Error(message),
                pw::stream::StreamState::Unconnected => StreamPhase::Unconnected,
                pw::stream::StreamState::Connecting => StreamPhase::Connecting,
                pw::stream::StreamState::Paused => StreamPhase::Paused,
                pw::stream::StreamState::Streaming => StreamPhase::Streaming,
            };
            let ended = matches!(phase, StreamPhase::Error(_) | StreamPhase::Unconnected);
            state.shared.set_phase(phase);
            if ended {
                state.mailbox.close();
            }
        })
        .param_changed(on_param_changed)
        .remove_buffer(|_, state, _| {
            state.gate.revoke();
            state
                .shared
                .stats
                .revocations
                .fetch_add(1, Ordering::Relaxed);
        })
        .process(on_process)
        .register()
        .map_err(setup)?;

    let enum_formats = enum_format_pods(&config.offer).map_err(SourceError::Setup)?;
    let mut params: Vec<&Pod> = enum_formats
        .iter()
        .map(|bytes| {
            Pod::from_bytes(bytes).ok_or_else(|| SourceError::Setup("invalid format pod".into()))
        })
        .collect::<Result<_, _>>()?;
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(handoff.node_id()),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|error| SourceError::Connect(format!("node {}: {error}", handoff.node_id())))?;

    let _receiver = receiver.attach(main_loop.loop_(), {
        let stream = stream.clone();
        let main_loop = main_loop.clone();
        let gate = Arc::clone(&gate);
        let shared = Arc::clone(shared);
        let sync = config.explicit_sync.clone();
        move |command| match command {
            Command::Release(token) => {
                let release = Release {
                    sync: sync.as_deref(),
                    failures: &release_failures,
                    shared: &shared,
                };
                if requeue(&stream, &gate, &release, token) {
                    withdraw_dmabuf(&stream, &offer);
                }
            }
            Command::HostMemoryOnly => withdraw_dmabuf(&stream, &offer),
            Command::Stop => main_loop.quit(),
        }
    });
    let _ = started.try_send(Ok(()));
    main_loop.run();

    // Shutdown: revoke every lease (waiting for a reader mid-copy), stop
    // leasing, then tear the stream down. Tokens still queued or held are
    // never dereferenced again.
    gate.revoke();
    mailbox.close();
    drop(mailbox.drain());
    let _ = stream.disconnect();
    shared.set_phase(StreamPhase::Unconnected);
    Ok(())
}

/// What returning a buffer to the producer needs (loop thread only).
struct Release<'state> {
    sync: Option<&'state SyncobjDevice>,
    failures: &'state RefCell<ReleaseFailures>,
    shared: &'state SourceShared,
}

/// Withdraws the DMA-BUF formats so the producer renegotiates to shared
/// memory (loop thread only). Idempotent.
fn withdraw_dmabuf(stream: &pw::stream::Stream, offer: &RefCell<FormatOffer>) {
    let host_only = {
        let mut offer = offer.borrow_mut();
        if !offer.memory.offers_dmabuf() {
            return;
        }
        offer.memory = MemoryOffer::HostOnly;
        offer.clone()
    };
    if let Ok(pods) = enum_format_pods(&host_only) {
        let mut params: Vec<&Pod> = pods
            .iter()
            .filter_map(|bytes| Pod::from_bytes(bytes))
            .collect();
        let _ = stream.update_params(&mut params);
    }
}

/// Requeues a released lease; returns whether DMA-BUF must be withdrawn
/// (counted in `release_fallbacks`).
fn requeue(
    stream: &pw::stream::Stream,
    gate: &LeaseGate,
    release: &Release<'_>,
    token: BufferToken,
) -> bool {
    if token.epoch != gate.current() {
        release
            .shared
            .stats
            .stale_releases
            .fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // SAFETY: the epoch is unchanged, so the buffer has not been removed and
    // was dequeued from this stream by `on_process`; it is requeued once,
    // because the token is consumed here. Its reader dropped the lease only
    // after it stopped reading, so the release point may be signalled.
    let withdraw = unsafe { queue_back(stream, token.buffer, release) };
    release
        .shared
        .stats
        .requeued
        .fetch_add(1, Ordering::Relaxed);
    withdraw
}

/// Signals the buffer's release point, when it carries one, then queues
/// it. Only a signalled release clears the producer's unscheduled-release
/// flag; after a failure the flag stays set, so a producer that honours it
/// releases the buffer itself. Returns whether repeated failures now require
/// withdrawing DMA-BUF.
///
/// # Safety
///
/// `buffer` is currently dequeued from `stream` on this (the loop) thread,
/// nothing reads it any more, and it is queued exactly once.
unsafe fn queue_back(
    stream: &pw::stream::Stream,
    buffer: NonNull<pw::sys::pw_buffer>,
    release: &Release<'_>,
) -> bool {
    let mut withdraw = false;
    if let (Some(spa_buffer), Some(device)) = ((*buffer.as_ptr()).buffer.as_ref(), release.sync) {
        let datas = buffer_datas(spa_buffer);
        if let (Some(points), Some(meta)) = (
            read_sync_points(spa_buffer, datas),
            sync_timeline_meta(spa_buffer),
        ) {
            let signalled = device
                .signal_point(points.release_fd, points.release_point)
                .is_ok();
            // SAFETY: the meta's data is `meta.size` writable bytes owned by
            // the dequeued buffer, which only this thread touches until the
            // queue below.
            let bytes = std::slice::from_raw_parts_mut(meta.data.cast::<u8>(), meta.size as usize);
            let _ = hand_back_release(bytes, signalled);
            let counter = if signalled {
                &release.shared.stats.release_signals
            } else {
                &release.shared.stats.release_signal_failures
            };
            counter.fetch_add(1, Ordering::Relaxed);
            withdraw = release.failures.borrow_mut().record(signalled);
            if withdraw {
                release
                    .shared
                    .stats
                    .release_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    stream.queue_raw_buffer(buffer.as_ptr());
    withdraw
}

fn note_refusal(shared: &SourceShared, token: &'static str) {
    if let Ok(mut last) = shared.stats.last_refusal.try_lock() {
        *last = Some(token);
    }
}

fn on_process(stream: &pw::stream::Stream, state: &mut CallbackState) {
    let started = Instant::now();
    let shared = Arc::clone(&state.shared);
    let stats = &shared.stats;
    stats.process_calls.fetch_add(1, Ordering::Relaxed);
    let mut dequeued = 0u32;
    loop {
        // SAFETY: called on the loop thread inside `process`, as PipeWire
        // requires; a null result means no buffer is ready.
        let raw = unsafe { stream.dequeue_raw_buffer() };
        let Some(buffer) = NonNull::new(raw) else {
            break;
        };
        dequeued += 1;
        stats.buffers.fetch_add(1, Ordering::Relaxed);
        handle_buffer(stream, state, buffer);
    }
    if dequeued == 0 {
        stats.empty_process.fetch_add(1, Ordering::Relaxed);
    }
    // Cursor updates of this call are coalesced (latest position, latest
    // bitmap) and applied once. Taking the lock is bounded: readers hold it
    // only to clone a small struct and an `Arc`, never across other work, so
    // no update (in particular a shape change GNOME 40 sends only once) is
    // dropped.
    if state.pending_cursor.is_pending() {
        let mut cursor_state = shared
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_cursor.flush_into(&mut cursor_state);
    }
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    stats.callback_ns.fetch_add(elapsed, Ordering::Relaxed);
    stats.callback_ns_max.fetch_max(elapsed, Ordering::Relaxed);
}

fn handle_buffer(
    stream: &pw::stream::Stream,
    state: &mut CallbackState,
    buffer: NonNull<pw::sys::pw_buffer>,
) {
    // SAFETY: `buffer` is a dequeued buffer of this stream; its spa_buffer,
    // datas, chunks and metas are valid until it is requeued or removed,
    // neither of which happens during this call.
    let (raw, planes, sync) = unsafe { read_buffer(buffer) };
    let device = state.explicit_sync.clone();
    let shared = Arc::clone(&state.shared);
    let failures = Rc::clone(&state.release_failures);
    let offer = Rc::clone(&state.offer);
    let requeue_now = |stream: &pw::stream::Stream| {
        let release = Release {
            sync: device.as_deref(),
            failures: &failures,
            shared: &shared,
        };
        // SAFETY: `buffer` was just dequeued from `stream` on this thread,
        // has not been leased and nothing read it.
        if unsafe { queue_back(stream, buffer, &release) } {
            withdraw_dmabuf(stream, &offer);
        }
    };
    // Cursor metadata does not depend on the pixels: Mutter writes a
    // sprite only once, so it is kept from every buffer that is not marked
    // corrupted, including ones whose pixels are refused or that arrive
    // before the format is known.
    let header_corrupted = raw.meta.header.is_some_and(|header| header.corrupted);
    if let (false, Some(Ok(meta))) = (header_corrupted, &raw.meta.cursor) {
        let cursor_only = raw.planes.iter().all(|plane| plane.chunk_size == 0);
        state.pending_cursor.push(meta.clone(), cursor_only);
    }
    let (Some(layout), Some(importer)) = (state.layout.as_ref(), state.importer.as_mut()) else {
        requeue_now(stream);
        return;
    };
    state.sequence += 1;
    let capture_ns = u64::try_from(state.base.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let stats = &state.shared.stats;
    match classify(&raw, layout, state.sequence, capture_ns) {
        Classified::CursorOnly(_) => {
            stats.cursor_only.fetch_add(1, Ordering::Relaxed);
            requeue_now(stream);
        }
        Classified::Corrupted => {
            stats.corrupted.fetch_add(1, Ordering::Relaxed);
            state
                .mailbox
                .carry_damage(arcen_media::video::DamageEvidence::Unknown);
            requeue_now(stream);
        }
        Classified::Refused(refusal) => {
            stats.refused.fetch_add(1, Ordering::Relaxed);
            note_refusal(&state.shared, refusal.token());
            state
                .mailbox
                .carry_damage(arcen_media::video::DamageEvidence::Unknown);
            requeue_now(stream);
        }
        Classified::Frame { surface, .. } => {
            let host = matches!(
                surface.memory,
                arcen_media::video::CaptureMemory::HostShared
            );
            match surface.validate(importer) {
                Ok(validated) => {
                    importer.last_sequence = Some(state.sequence);
                    if host {
                        stats.host_memory.fetch_add(1, Ordering::Relaxed);
                    } else {
                        stats.dmabuf.fetch_add(1, Ordering::Relaxed);
                        if sync.is_some() {
                            stats.explicit_sync.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    stats.frames.fetch_add(1, Ordering::Relaxed);
                    let token = BufferToken {
                        buffer,
                        epoch: state.gate.current(),
                        gate: Arc::clone(&state.gate),
                        planes,
                        sync,
                    };
                    // Superseded and closed-mailbox tokens come straight
                    // back on this thread unread; `requeue` skips any whose
                    // buffer was removed since it was leased.
                    let release = Release {
                        sync: state.explicit_sync.as_deref(),
                        failures: &state.release_failures,
                        shared: &state.shared,
                    };
                    let mut withdraw = false;
                    for superseded in state.mailbox.push(token, validated) {
                        withdraw |= requeue(stream, &state.gate, &release, superseded);
                    }
                    if withdraw {
                        withdraw_dmabuf(stream, &state.offer);
                    }
                }
                Err(error) => {
                    stats.validation_refused.fetch_add(1, Ordering::Relaxed);
                    note_refusal(&state.shared, error.token());
                    state
                        .mailbox
                        .carry_damage(arcen_media::video::DamageEvidence::Unknown);
                    requeue_now(stream);
                }
            }
        }
    }
}

/// Copies the facts of a dequeued buffer into plain data, separating the
/// explicit-sync blocks (never planes) from the planes.
///
/// # Safety
///
/// `buffer` must be a buffer currently dequeued from a stream on this thread.
unsafe fn read_buffer(
    buffer: NonNull<pw::sys::pw_buffer>,
) -> (RawBuffer, Vec<PlaneAccess>, Option<SyncPoints>) {
    let mut raw = RawBuffer::default();
    let mut access = Vec::new();
    let spa_buffer = (*buffer.as_ptr()).buffer;
    let Some(spa_buffer) = spa_buffer.as_ref() else {
        return (raw, access, None);
    };
    let datas = buffer_datas(spa_buffer);
    let sync = read_sync_points(spa_buffer, datas);
    let pixel_datas = if sync.is_some() {
        &datas[..datas.len() - SYNC_OBJ_BLOCKS]
    } else {
        datas
    };
    for data in pixel_datas {
        let kind = match data.type_ {
            spa_sys::SPA_DATA_MemFd => DataKind::MemFd,
            spa_sys::SPA_DATA_MemPtr => DataKind::MemPtr,
            spa_sys::SPA_DATA_DmaBuf => DataKind::DmaBuf,
            other => DataKind::Other(other),
        };
        let chunk = data.chunk.as_ref();
        let mapped = NonNull::new(data.data.cast::<u8>());
        raw.planes.push(RawPlane {
            kind,
            fd: (data.fd >= 0).then_some(data.fd),
            mapped: mapped.is_some(),
            max_size: data.maxsize,
            chunk_offset: chunk.map_or(0, |chunk| chunk.offset),
            chunk_size: chunk.map_or(0, |chunk| chunk.size),
            chunk_stride: chunk.map_or(0, |chunk| chunk.stride),
            corrupted: chunk
                .is_some_and(|chunk| chunk.flags & spa_sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0),
        });
        access.push(match (kind, mapped) {
            (DataKind::MemFd | DataKind::MemPtr, Some(data_ptr)) => PlaneAccess::Host {
                data: data_ptr,
                len: data.maxsize as usize,
            },
            (DataKind::DmaBuf, _) => match RawFd::try_from(data.fd) {
                Ok(fd) if fd >= 0 => PlaneAccess::DmaBuf { fd },
                _ => PlaneAccess::Unavailable,
            },
            _ => PlaneAccess::Unavailable,
        });
    }
    let metas: &[spa_sys::spa_meta] = if spa_buffer.metas.is_null() || spa_buffer.n_metas == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(spa_buffer.metas, spa_buffer.n_metas as usize)
    };
    for meta in metas {
        if meta.data.is_null() {
            continue;
        }
        let size = meta.size as usize;
        match meta.type_ {
            spa_sys::SPA_META_Header if size >= SPA_META_HEADER_BYTES => {
                let header = &*meta.data.cast::<spa_sys::spa_meta_header>();
                raw.meta.header = Some(HeaderMeta {
                    corrupted: header.flags & spa_sys::SPA_META_HEADER_FLAG_CORRUPTED != 0,
                    seq: header.seq,
                    pts: header.pts,
                });
            }
            spa_sys::SPA_META_VideoCrop if size >= SPA_META_REGION_BYTES => {
                let region = &(*meta.data.cast::<spa_sys::spa_meta_region>()).region;
                raw.meta.crop = Some((
                    region.position.x,
                    region.position.y,
                    region.size.width,
                    region.size.height,
                ));
            }
            spa_sys::SPA_META_VideoDamage => {
                let count = (size / SPA_META_REGION_BYTES).min(DAMAGE_META_MAX_REGIONS + 1);
                let regions =
                    std::slice::from_raw_parts(meta.data.cast::<spa_sys::spa_meta_region>(), count);
                raw.meta.damage = Some(
                    regions
                        .iter()
                        .map(|meta_region| {
                            let region = meta_region.region;
                            (
                                region.position.x,
                                region.position.y,
                                region.size.width,
                                region.size.height,
                            )
                        })
                        .collect(),
                );
            }
            spa_sys::SPA_META_Cursor => {
                let bytes = std::slice::from_raw_parts(meta.data.cast::<u8>(), size);
                raw.meta.cursor = Some(parse_cursor_meta(bytes));
            }
            _ => {}
        }
    }
    (raw, access, sync)
}

/// The buffer's data blocks.
///
/// # Safety
///
/// `spa_buffer` belongs to a buffer currently dequeued on this thread.
unsafe fn buffer_datas(spa_buffer: &spa_sys::spa_buffer) -> &[spa_sys::spa_data] {
    if spa_buffer.datas.is_null() || spa_buffer.n_datas == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(spa_buffer.datas, spa_buffer.n_datas as usize)
    }
}

/// The buffer's `SPA_META_SyncTimeline` meta, when it carries a whole one.
///
/// # Safety
///
/// As for [`buffer_datas`].
unsafe fn sync_timeline_meta(spa_buffer: &spa_sys::spa_buffer) -> Option<&spa_sys::spa_meta> {
    let metas: &[spa_sys::spa_meta] = if spa_buffer.metas.is_null() || spa_buffer.n_metas == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(spa_buffer.metas, spa_buffer.n_metas as usize)
    };
    metas.iter().find(|meta| {
        meta.type_ == SPA_META_SYNC_TIMELINE
            && !meta.data.is_null()
            && meta.size as usize >= SPA_META_SYNC_TIMELINE_BYTES
    })
}

/// The buffer's explicit-sync points: a `SPA_META_SyncTimeline` meta and
/// two trailing `SyncObj` data blocks with valid descriptors. Read only:
/// the unscheduled-release flag is answered when the buffer is queued back
/// (`queue_back`), after the release point was signalled.
///
/// # Safety
///
/// As for [`read_buffer`]; `datas` are the buffer's data blocks.
unsafe fn read_sync_points(
    spa_buffer: &spa_sys::spa_buffer,
    datas: &[spa_sys::spa_data],
) -> Option<SyncPoints> {
    let (acquire, release) = sync_block_indices(datas.len())?;
    if datas[acquire].type_ != SPA_DATA_SYNC_OBJ || datas[release].type_ != SPA_DATA_SYNC_OBJ {
        return None;
    }
    let meta = sync_timeline_meta(spa_buffer)?;
    let bytes = std::slice::from_raw_parts(meta.data.cast::<u8>(), SPA_META_SYNC_TIMELINE_BYTES);
    let timeline = SyncTimelineMeta::parse(bytes)?;
    let fd = |index: usize| i32::try_from(datas[index].fd).ok().filter(|fd| *fd >= 0);
    Some(SyncPoints {
        acquire_fd: fd(acquire)?,
        acquire_point: timeline.acquire_point,
        release_fd: fd(release)?,
        release_point: timeline.release_point,
    })
}

fn on_param_changed(
    stream: &pw::stream::Stream,
    state: &mut CallbackState,
    id: u32,
    param: Option<&Pod>,
) {
    if id != spa::param::ParamType::Format.as_raw() {
        return;
    }
    // Any format change ends every outstanding lease's validity; queued
    // leases are dropped unread (their buffers belong to the old format).
    state.gate.revoke();
    drop(state.mailbox.drain());
    // The next frame may have another geometry: it carries full damage.
    state
        .mailbox
        .carry_damage(arcen_media::video::DamageEvidence::Unknown);
    state
        .shared
        .stats
        .format_changes
        .fetch_add(1, Ordering::Relaxed);
    let negotiated = param.and_then(|pod| negotiated_from_pod(pod, &state.offer.borrow()));
    {
        let mut slot = state
            .shared
            .negotiated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = negotiated;
    }
    state.shared.changed.notify_all();
    let Some(negotiated) = negotiated else {
        state.layout = None;
        state.importer = None;
        if param.is_some() {
            note_refusal(&state.shared, "format_unsupported");
        }
        return;
    };
    let layout = LayoutContext {
        negotiated,
        output: state.handoff_output.clone(),
        generation: state.handoff_generation,
        source_gpu: state.source_gpu,
        host_copy_path: CopyPath::HostCopy,
        dmabuf_copy_path: state.dmabuf_copy_path,
    };
    state.importer = Some(CaptureImporter {
        output: layout.output.clone(),
        generation: layout.generation,
        colour: negotiated.colour,
        encoder_gpu: state.encoder_gpu,
        gpu_modifiers: match &state.offer.borrow().memory {
            MemoryOffer::HostOnly => Vec::new(),
            MemoryOffer::DmaBufThenHost { modifiers } => modifiers.clone(),
        },
        last_sequence: None,
    });
    state.layout = Some(layout);
    if let Ok(params) = buffer_param_pods(&negotiated, state.explicit_sync.is_some()) {
        let mut pods: Vec<&Pod> = params
            .iter()
            .filter_map(|bytes| Pod::from_bytes(bytes))
            .collect();
        let _ = stream.update_params(&mut pods);
    }
}

fn negotiated_from_pod(pod: &Pod, offer: &FormatOffer) -> Option<NegotiatedFormat> {
    let (media_type, media_subtype) = spa::param::format_utils::parse_format(pod).ok()?;
    if media_type != spa::param::format::MediaType::Video
        || media_subtype != spa::param::format::MediaSubtype::Raw
    {
        return None;
    }
    let mut info = spa::param::video::VideoInfoRaw::new();
    info.parse(pod).ok()?;
    let format = capture_format(info.format())?;
    let modifier = (info.flags().bits() & spa_sys::SPA_VIDEO_FLAG_MODIFIER != 0)
        .then(|| FormatModifier(info.modifier()));
    let size = info.size();
    let negotiated = NegotiatedFormat {
        format,
        width: size.width,
        height: size.height,
        modifier,
        colour: colour_of(&info),
        framerate: (info.framerate().num, info.framerate().denom),
        max_framerate: (info.max_framerate().num, info.max_framerate().denom),
    };
    negotiated.satisfies(offer).then_some(negotiated)
}

fn capture_format(format: spa::param::video::VideoFormat) -> Option<CapturePixelFormat> {
    use spa::param::video::VideoFormat as F;
    Some(match format {
        F::BGRx => CapturePixelFormat::Bgrx8,
        F::BGRA => CapturePixelFormat::Bgra8,
        F::RGBx => CapturePixelFormat::Rgbx8,
        F::RGBA => CapturePixelFormat::Rgba8,
        F::NV12 => CapturePixelFormat::Nv12,
        F::xRGB_210LE => CapturePixelFormat::Xrgb2101010,
        F::xBGR_210LE => CapturePixelFormat::Xbgr2101010,
        F::RGBA_F16 => CapturePixelFormat::RgbaF16,
        _ => return None,
    })
}

fn spa_format(format: CapturePixelFormat) -> spa::param::video::VideoFormat {
    use spa::param::video::VideoFormat as F;
    match format {
        CapturePixelFormat::Bgrx8 => F::BGRx,
        CapturePixelFormat::Bgra8 => F::BGRA,
        CapturePixelFormat::Rgbx8 => F::RGBx,
        CapturePixelFormat::Rgba8 => F::RGBA,
        CapturePixelFormat::Nv12 => F::NV12,
        CapturePixelFormat::Xrgb2101010 => F::xRGB_210LE,
        CapturePixelFormat::Xbgr2101010 => F::xBGR_210LE,
        CapturePixelFormat::RgbaF16 => F::RGBA_F16,
    }
}

/// Colour facts the producer stated in the format; unstated is unknown.
///
/// Only values the build's SPA headers name are mapped, plus the PQ and HLG
/// transfers by their ABI values: `PipeWire` 1.6 added them
/// (`SPA_VIDEO_TRANSFER_SMPTE2084` = 14, `SPA_VIDEO_TRANSFER_ARIB_STD_B67` =
/// 15, pipewire!2419), every Mutter that screencasts HDR (50 and later)
/// tags its wide formats with them, and capenc may be built against older
/// headers. Anything else stays unknown, never inferred.
fn colour_of(info: &spa::param::video::VideoInfoRaw) -> SourceColour {
    const SPA_VIDEO_TRANSFER_SMPTE2084: u32 = 14;
    const SPA_VIDEO_TRANSFER_ARIB_STD_B67: u32 = 15;
    fn fact<T>(value: Option<T>) -> SourceFact<T> {
        value.map_or(SourceFact::Unknown, SourceFact::Authoritative)
    }
    SourceColour {
        primaries: fact(match info.color_primaries() {
            spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT709 => Some(ColorPrimaries::Bt709),
            spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020 => Some(ColorPrimaries::Bt2020),
            _ => None,
        }),
        transfer: fact(match info.transfer_function() {
            spa_sys::SPA_VIDEO_TRANSFER_SRGB => Some(SourceTransfer::Srgb),
            spa_sys::SPA_VIDEO_TRANSFER_BT709 => Some(SourceTransfer::Bt709),
            SPA_VIDEO_TRANSFER_SMPTE2084 => Some(SourceTransfer::Pq),
            SPA_VIDEO_TRANSFER_ARIB_STD_B67 => Some(SourceTransfer::Hlg),
            _ => None,
        }),
        range: fact(match info.color_range() {
            spa_sys::SPA_VIDEO_COLOR_RANGE_0_255 => Some(ColorRange::Full),
            spa_sys::SPA_VIDEO_COLOR_RANGE_16_235 => Some(ColorRange::Limited),
            _ => None,
        }),
        matrix: fact(match info.color_matrix() {
            spa_sys::SPA_VIDEO_COLOR_MATRIX_RGB => Some(ColorMatrix::Identity),
            spa_sys::SPA_VIDEO_COLOR_MATRIX_BT709 => Some(ColorMatrix::Bt709),
            spa_sys::SPA_VIDEO_COLOR_MATRIX_BT601 => Some(ColorMatrix::Bt601),
            spa_sys::SPA_VIDEO_COLOR_MATRIX_BT2020 => Some(ColorMatrix::Bt2020Ncl),
            _ => None,
        }),
        sdr_white_nits: SourceFact::Unknown,
    }
}

fn serialize(object: Object) -> Result<Vec<u8>, String> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|error| format!("{error:?}"))
}

fn id_choice(values: &[u32]) -> Value {
    let ids: Vec<Id> = values.iter().map(|value| Id(*value)).collect();
    Value::Choice(ChoiceValue::Id(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Enum {
            default: ids[0],
            alternatives: ids,
        },
    )))
}

fn int_range(default: i32, min: i32, max: i32) -> Value {
    Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range { default, min, max },
    )))
}

/// `EnumFormat` pods: DMA-BUF with explicit modifiers first when offered,
/// then the same formats in shared memory.
fn enum_format_pods(offer: &FormatOffer) -> Result<Vec<Vec<u8>>, String> {
    if offer.formats.is_empty() {
        return Err("no formats offered".to_string());
    }
    let formats: Vec<u32> = offer
        .formats
        .iter()
        .map(|format| spa_format(*format).as_raw())
        .collect();
    let size = match offer.size {
        Some((width, height)) => Value::Rectangle(Rectangle { width, height }),
        None => Value::Choice(ChoiceValue::Rectangle(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Range {
                default: Rectangle {
                    width: 1920,
                    height: 1080,
                },
                min: Rectangle {
                    width: 1,
                    height: 1,
                },
                max: Rectangle {
                    width: 16384,
                    height: 16384,
                },
            },
        ))),
    };
    let max_fps = offer.max_fps.max(1);
    let base = |modifiers: Option<&[FormatModifier]>| {
        let mut properties = vec![
            Property::new(
                spa_sys::SPA_FORMAT_mediaType,
                Value::Id(Id(spa_sys::SPA_MEDIA_TYPE_video)),
            ),
            Property::new(
                spa_sys::SPA_FORMAT_mediaSubtype,
                Value::Id(Id(spa_sys::SPA_MEDIA_SUBTYPE_raw)),
            ),
            Property::new(spa_sys::SPA_FORMAT_VIDEO_format, id_choice(&formats)),
            Property::new(spa_sys::SPA_FORMAT_VIDEO_size, size.clone()),
            Property::new(
                spa_sys::SPA_FORMAT_VIDEO_framerate,
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction { num: 0, denom: 1 },
                        min: Fraction { num: 0, denom: 1 },
                        max: Fraction {
                            num: 1000,
                            denom: 1,
                        },
                    },
                ))),
            ),
            Property::new(
                spa_sys::SPA_FORMAT_VIDEO_maxFramerate,
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction {
                            num: max_fps,
                            denom: 1,
                        },
                        min: Fraction { num: 1, denom: 1 },
                        max: Fraction {
                            num: max_fps,
                            denom: 1,
                        },
                    },
                ))),
            ),
        ];
        // A wide offer asks for what a colour-managed compositor tags its
        // HDR composition with: BT.2020 primaries and PQ (SPA transfer 14,
        // `PipeWire` 1.6 `SPA_VIDEO_TRANSFER_SMPTE2084`). Mutter 50 does not
        // fixate a wide format whose colour the consumer leaves open.
        if offer
            .formats
            .iter()
            .all(|format| format.precision().is_wide())
        {
            const SPA_VIDEO_TRANSFER_SMPTE2084: u32 = 14;
            properties.push(Property::new(
                spa_sys::SPA_FORMAT_VIDEO_colorPrimaries,
                Value::Id(Id(spa_sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020)),
            ));
            properties.push(Property::new(
                spa_sys::SPA_FORMAT_VIDEO_transferFunction,
                Value::Id(Id(SPA_VIDEO_TRANSFER_SMPTE2084)),
            ));
        }
        if let Some(modifiers) = modifiers {
            let values: Vec<i64> = modifiers
                .iter()
                .map(|modifier| i64::from_ne_bytes(modifier.0.to_ne_bytes()))
                .collect();
            properties.push(Property {
                key: spa_sys::SPA_FORMAT_VIDEO_modifier,
                flags: PropertyFlags::MANDATORY
                    | PropertyFlags::from_bits_retain(spa_sys::SPA_POD_PROP_FLAG_DONT_FIXATE),
                value: Value::Choice(ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: values[0],
                        alternatives: values,
                    },
                ))),
            });
        }
        Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties,
        }
    };
    let mut pods = Vec::new();
    if let MemoryOffer::DmaBufThenHost { modifiers } = &offer.memory {
        if !modifiers.is_empty() {
            pods.push(serialize(base(Some(modifiers)))?);
        }
    }
    pods.push(serialize(base(None))?);
    Ok(pods)
}

/// `Buffers` and `Meta` pods for a fixed format. A DMA-BUF format with
/// `explicit_sync` first asks for `SPA_META_SyncTimeline` buffers (two extra
/// `SyncObj` blocks, the meta mandatory), then accepts plain buffers.
fn buffer_param_pods(
    negotiated: &NegotiatedFormat,
    explicit_sync: bool,
) -> Result<Vec<Vec<u8>>, String> {
    let dmabuf = negotiated.modifier.is_some();
    let data_types: i32 = if dmabuf {
        1 << spa_sys::SPA_DATA_DmaBuf
    } else {
        (1 << spa_sys::SPA_DATA_MemFd) | (1 << spa_sys::SPA_DATA_MemPtr)
    };
    let data_type = || {
        Value::Choice(ChoiceValue::Int(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Flags {
                default: data_types,
                flags: vec![data_types],
            },
        )))
    };
    let mut pods = Vec::new();
    if dmabuf && explicit_sync {
        let blocks = negotiated.format.plane_count() + SYNC_OBJ_BLOCKS;
        let synced = Object {
            type_: SpaTypes::ObjectParamBuffers.as_raw(),
            id: spa::param::ParamType::Buffers.as_raw(),
            properties: vec![
                Property::new(spa_sys::SPA_PARAM_BUFFERS_buffers, int_range(8, 4, 16)),
                Property::new(
                    SPA_PARAM_BUFFERS_BLOCKS,
                    Value::Int(i32::try_from(blocks).unwrap_or(i32::MAX)),
                ),
                Property::new(spa_sys::SPA_PARAM_BUFFERS_dataType, data_type()),
                Property {
                    key: SPA_PARAM_BUFFERS_META_TYPE,
                    flags: PropertyFlags::MANDATORY,
                    value: Value::Int(1 << SPA_META_SYNC_TIMELINE),
                },
            ],
        };
        pods.push(serialize(synced)?);
    }
    let buffers = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(spa_sys::SPA_PARAM_BUFFERS_buffers, int_range(8, 4, 16)),
            Property::new(spa_sys::SPA_PARAM_BUFFERS_dataType, data_type()),
        ],
    };
    let meta = |meta_type: u32, size: Value| Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(spa_sys::SPA_PARAM_META_type, Value::Id(Id(meta_type))),
            Property::new(spa_sys::SPA_PARAM_META_size, size),
        ],
    };
    let int = |value: usize| i32::try_from(value).unwrap_or(i32::MAX);
    let cursor_max = int(cursor_meta_size(
        CURSOR_BITMAP_MAX_SIDE,
        CURSOR_BITMAP_MAX_SIDE,
    ));
    let damage_max = int(SPA_META_REGION_BYTES * DAMAGE_META_MAX_REGIONS);
    pods.extend([
        serialize(buffers)?,
        serialize(meta(
            spa_sys::SPA_META_Header,
            Value::Int(int(SPA_META_HEADER_BYTES)),
        ))?,
        serialize(meta(
            spa_sys::SPA_META_VideoCrop,
            Value::Int(int(SPA_META_REGION_BYTES)),
        ))?,
        serialize(meta(
            spa_sys::SPA_META_VideoDamage,
            int_range(damage_max, int(SPA_META_REGION_BYTES), damage_max),
        ))?,
        // The range must reach 384x384: Mutter 40 offers nothing smaller.
        serialize(meta(
            spa_sys::SPA_META_Cursor,
            int_range(cursor_max, int(cursor_meta_size(1, 1)), cursor_max),
        ))?,
    ]);
    if dmabuf && explicit_sync {
        pods.push(serialize(meta(
            SPA_META_SYNC_TIMELINE,
            Value::Int(int(SPA_META_SYNC_TIMELINE_BYTES)),
        ))?);
    }
    Ok(pods)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_pods_serialize_and_parse() {
        let mut offer = FormatOffer::sdr8_host(30);
        let pods = enum_format_pods(&offer).unwrap();
        assert_eq!(pods.len(), 1);
        assert!(Pod::from_bytes(&pods[0]).is_some());
        offer.memory = MemoryOffer::DmaBufThenHost {
            modifiers: vec![FormatModifier::LINEAR],
        };
        assert_eq!(enum_format_pods(&offer).unwrap().len(), 2);
        let negotiated = NegotiatedFormat {
            format: CapturePixelFormat::Bgrx8,
            width: 1920,
            height: 1080,
            modifier: None,
            colour: SourceColour::UNKNOWN,
            framerate: (0, 1),
            max_framerate: (30, 1),
        };
        let params = buffer_param_pods(&negotiated, true).unwrap();
        assert_eq!(
            params.len(),
            5,
            "shared memory never asks for explicit sync"
        );
        assert!(params.iter().all(|bytes| Pod::from_bytes(bytes).is_some()));
        let dmabuf = NegotiatedFormat {
            modifier: Some(FormatModifier(0x0300_0000_00cd_b014)),
            ..negotiated
        };
        assert_eq!(buffer_param_pods(&dmabuf, false).unwrap().len(), 5);
        let synced = buffer_param_pods(&dmabuf, true).unwrap();
        assert_eq!(
            synced.len(),
            7,
            "sync-timeline Buffers first, then the meta"
        );
        assert!(synced.iter().all(|bytes| Pod::from_bytes(bytes).is_some()));
    }

    #[test]
    fn cursor_bitmap_format_ids_match_libspa() {
        use crate::wayland::cursor as c;
        use spa::param::video::VideoFormat;
        assert_eq!(VideoFormat::RGBx.as_raw(), c::SPA_VIDEO_FORMAT_RGBX);
        assert_eq!(VideoFormat::BGRx.as_raw(), c::SPA_VIDEO_FORMAT_BGRX);
        assert_eq!(VideoFormat::RGBA.as_raw(), c::SPA_VIDEO_FORMAT_RGBA);
        assert_eq!(VideoFormat::BGRA.as_raw(), c::SPA_VIDEO_FORMAT_BGRA);
        assert_eq!(VideoFormat::ARGB.as_raw(), c::SPA_VIDEO_FORMAT_ARGB);
        assert_eq!(VideoFormat::ABGR.as_raw(), c::SPA_VIDEO_FORMAT_ABGR);
    }

    #[test]
    fn spa_format_mapping_round_trips() {
        for format in CapturePixelFormat::ALL {
            assert_eq!(capture_format(spa_format(*format)), Some(*format));
        }
    }

    #[test]
    fn revoked_lease_is_not_readable() {
        let gate = Arc::new(LeaseGate::default());
        let mut bytes = vec![7u8; 16];
        let token = BufferToken {
            buffer: NonNull::dangling(),
            epoch: gate.current(),
            gate: Arc::clone(&gate),
            planes: vec![PlaneAccess::Host {
                data: NonNull::new(bytes.as_mut_ptr()).unwrap(),
                len: bytes.len(),
            }],
            sync: None,
        };
        let sum = token
            .read(|views| match views[0] {
                PlaneView::Host(data) => data.iter().map(|byte| u32::from(*byte)).sum::<u32>(),
                _ => 0,
            })
            .unwrap();
        assert_eq!(sum, 7 * 16);
        gate.revoke();
        assert_eq!(token.read(|_| ()), Err(LeaseRevoked));
    }
}
