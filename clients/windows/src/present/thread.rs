// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The presenter thread of one pipeline: the Windows twin of the Mac's
//! CVDisplayLink-driven presenter (`video_metal_layer.rs`
//! `DedicatedDisplayLink`).
//!
//! The media worker hands frames to a **latest-frame slot** (a newer frame
//! replaces one not yet presented; replacements are counted, never queued);
//! the thread waits on the swapchain's frame-latency waitable object
//! (latency 1) holding no lock, then takes the newest frame, converts it and
//! presents it. The wait lasts until the display's next refresh or
//! composition; the decoder and every other window's presenter share the
//! device's immediate context and must never wait behind it. A still screen
//! re-presents the last frame when Exact still image tiles changed
//! ([`PresenterHandle::represent`]). Contract changes (Grading ↔ HDR, a new
//! source size) are applied to the swapchain on this thread; the video
//! visual is pointed at this pipeline's swapchain when it presents.

use super::Result;
use super::compose::{DecodedFrame, FrameComposer};
use super::composition::SharedTree;
use super::device::Gpu;
use super::swapchain::{PresentMode, VideoSwapchain};
use arcen_media::video::PresentTiming;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What the presenter reports asynchronously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenterEvent {
    /// The swapchain now presents in `mode`.
    ModeChanged(PresentMode),
    /// A frame failed to present; the session requests a full frame (the
    /// Mac's "request a full frame after a presentation failure").
    Failed(String),
    /// `DXGI_ERROR_DEVICE_REMOVED`/`RESET`: recreate device, decoder and
    /// swapchains, then request a keyframe (same path as reconnect).
    DeviceLost(String),
}

/// Counters for Stream Statistics (monotonic; the sink takes deltas).
#[derive(Debug, Default)]
pub struct PresenterCounters {
    pub received: AtomicU64,
    pub presented: AtomicU64,
    pub confirmed: AtomicU64,
    pub unconfirmed: AtomicU64,
    pub replaced_before_present: AtomicU64,
    pub represented: AtomicU64,
    pub failed: AtomicU64,
    pub skipped_hidden: AtomicU64,
    /// Sum and count of arrival → present latencies, in microseconds.
    pub latency_us_total: AtomicU64,
    pub latency_samples: AtomicU64,
}

struct Pending {
    frame: DecodedFrame,
    arrival: Instant,
}

#[derive(Default)]
struct Slot {
    frame: Option<Pending>,
    represent: bool,
    stop: bool,
}

struct Shared {
    slot: Mutex<Slot>,
    wake: Condvar,
    counters: PresenterCounters,
    /// The served pipeline's presentation timing, as [`timing_code`].
    timing: AtomicU8,
    scrgb_fallback: AtomicBool,
    visible: AtomicBool,
    presented_any: AtomicBool,
    mode: Mutex<Option<PresentMode>>,
    events: Mutex<Vec<PresenterEvent>>,
}

/// The media worker / session side of a running presenter thread.
#[derive(Clone)]
pub struct PresenterHandle {
    shared: Arc<Shared>,
}

/// Owns the presenter thread; stops and joins it on drop.
pub struct PresenterThread {
    handle: PresenterHandle,
    join: Option<JoinHandle<()>>,
}

/// What the thread owns once started. `windows` COM interfaces are not
/// `Send`; the device is multithread-protected ([`Gpu::create`]) and after
/// the hand-over only the presenter thread touches the swapchain and the
/// composer.
struct Owned {
    gpu: Gpu,
    swapchain: VideoSwapchain,
    composer: FrameComposer,
    tree: Option<(SharedTree, usize)>,
    path: PathWatch,
    /// The frame-latency wait returned and no present has used it yet: the
    /// waitable object counts frames, so a wait that is not followed by a
    /// present must not be repeated (the next would block for its timeout).
    ready: bool,
    timing: PresentTiming5s,
    /// The last present's draw, `Present` call and composition update.
    phases: [Duration; 3],
}

/// Where a present's time goes, logged every five seconds: the swapchain's
/// frame-latency wait (no lock held), the shared context lock, and the draw
/// and `Present` call under it.
struct PresentTiming5s {
    since: Instant,
    presents: u32,
    wait: Duration,
    wait_max: Duration,
    lock: Duration,
    lock_max: Duration,
    work: Duration,
    work_max: Duration,
    phases: [Duration; 3],
    phases_max: [Duration; 3],
}

impl Default for PresentTiming5s {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            presents: 0,
            wait: Duration::ZERO,
            wait_max: Duration::ZERO,
            lock: Duration::ZERO,
            lock_max: Duration::ZERO,
            work: Duration::ZERO,
            work_max: Duration::ZERO,
            phases: [Duration::ZERO; 3],
            phases_max: [Duration::ZERO; 3],
        }
    }
}

impl PresentTiming5s {
    fn note(
        &mut self,
        wait: Duration,
        lock: Duration,
        work: Duration,
        phases: [Duration; 3],
        size: (u32, u32),
    ) {
        self.presents += 1;
        for (index, phase) in phases.into_iter().enumerate() {
            self.phases[index] += phase;
            self.phases_max[index] = self.phases_max[index].max(phase);
        }
        self.wait += wait;
        self.wait_max = self.wait_max.max(wait);
        self.lock += lock;
        self.lock_max = self.lock_max.max(lock);
        self.work += work;
        self.work_max = self.work_max.max(work);
        if self.since.elapsed() < Duration::from_secs(5) {
            return;
        }
        let avg = |sum: Duration| {
            u64::try_from(sum.as_micros() / u128::from(self.presents.max(1))).unwrap_or(u64::MAX)
        };
        let us = |value: Duration| u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        tracing::info!(
            target: arcen_deck_core::log_target::VIDEO,
            size = ?size,
            presents = self.presents,
            latency_wait_avg_us = avg(self.wait),
            latency_wait_max_us = us(self.wait_max),
            lock_wait_avg_us = avg(self.lock),
            lock_wait_max_us = us(self.lock_max),
            draw_present_avg_us = avg(self.work),
            draw_present_max_us = us(self.work_max),
            draw_avg_us = avg(self.phases[0]),
            present_call_avg_us = avg(self.phases[1]),
            present_call_max_us = us(self.phases_max[1]),
            composition_avg_us = avg(self.phases[2]),
            "video presenter timing"
        );
        *self = Self::default();
    }
}

/// How DWM shows this swapchain (`GetFrameStatisticsMedia`), read every
/// [`PathWatch::EVERY`] presents and logged when it changes: "composed",
/// "none (independent flip)" or "overlay".
#[derive(Default)]
struct PathWatch {
    presents: u32,
    last: Option<String>,
}

impl PathWatch {
    const EVERY: u32 = 60;

    fn after_present(&mut self, swapchain: &VideoSwapchain, timing: PresentTiming) {
        self.presents = self.presents.wrapping_add(1);
        if self.presents % Self::EVERY != 1 {
            return;
        }
        let (path, reading) = swapchain.composition();
        if path == super::depth::CompositionPath::Unknown {
            // Statistics disjoint (the first presents, a mode change) or
            // unsupported: no reading.
            return;
        }
        let name = reading.split(" (").next().unwrap_or_default().to_owned();
        if self.last.as_deref() != Some(name.as_str()) {
            tracing::info!(
                target: arcen_deck_core::log_target::VIDEO,
                mode = swapchain.mode.name(),
                size = ?(swapchain.width, swapchain.height),
                timing = timing.token(),
                tearing = timing.allows_tearing() && swapchain.tearing_supported,
                "video presentation path: {reading}"
            );
            self.last = Some(name);
        }
    }
}

// SAFETY: see `Owned`.
unsafe impl Send for Owned {}

const fn timing_code(timing: PresentTiming) -> u8 {
    match timing {
        PresentTiming::Vsync => 0,
        PresentTiming::Newest => 1,
        PresentTiming::Tearing => 2,
    }
}

impl Shared {
    fn timing(&self) -> PresentTiming {
        match self.timing.load(Ordering::Relaxed) {
            1 => PresentTiming::Newest,
            2 => PresentTiming::Tearing,
            _ => PresentTiming::Vsync,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PresenterHandle {
    /// Offers the newest decoded frame; replaces one not yet presented.
    pub fn submit(&self, frame: DecodedFrame) {
        self.submit_at(frame, Instant::now());
    }

    /// [`Self::submit`] with the media worker's arrival time (latency).
    pub fn submit_at(&self, frame: DecodedFrame, arrival: Instant) {
        self.shared
            .counters
            .received
            .fetch_add(1, Ordering::Relaxed);
        let mut slot = lock(&self.shared.slot);
        if slot.frame.replace(Pending { frame, arrival }).is_some() {
            self.shared
                .counters
                .replaced_before_present
                .fetch_add(1, Ordering::Relaxed);
        }
        drop(slot);
        self.shared.wake.notify_one();
    }

    /// Re-presents the last frame (its Exact still image overlay changed).
    pub fn represent(&self) {
        lock(&self.shared.slot).represent = true;
        self.shared.wake.notify_one();
    }

    /// When each frame is presented ([`PresentTiming`], already resolved for
    /// this presenter's depth).
    pub fn set_timing(&self, timing: PresentTiming) {
        self.shared
            .timing
            .store(timing_code(timing), Ordering::Relaxed);
    }

    /// Present PQ through FP16 scRGB instead of HDR10.
    pub fn set_scrgb_fallback(&self, fallback: bool) {
        self.shared
            .scrgb_fallback
            .store(fallback, Ordering::Relaxed);
    }

    /// A hidden or occluded window skips drawing (frames still replace).
    pub fn set_visible(&self, visible: bool) {
        self.shared.visible.store(visible, Ordering::Relaxed);
    }

    #[must_use]
    pub fn counters(&self) -> &PresenterCounters {
        &self.shared.counters
    }

    #[must_use]
    pub fn has_presented(&self) -> bool {
        self.shared.presented_any.load(Ordering::Relaxed)
    }

    /// The mode last applied to the swapchain.
    #[must_use]
    pub fn mode(&self) -> Option<PresentMode> {
        *lock(&self.shared.mode)
    }

    /// Events since the last call.
    #[must_use]
    pub fn take_events(&self) -> Vec<PresenterEvent> {
        std::mem::take(&mut *lock(&self.shared.events))
    }
}

fn is_device_lost(error: &str) -> bool {
    error.contains("0x887A0005") || error.contains("0x887A0007")
}

impl PresenterThread {
    /// Starts the thread over `swapchain`. With `tree`, the video visual is
    /// pointed at this swapchain (`owner` identifies the pipeline) on the
    /// first present and refitted when the source size changes.
    pub fn spawn(
        gpu: Gpu,
        swapchain: VideoSwapchain,
        tree: Option<(SharedTree, usize)>,
    ) -> Result<Self> {
        let composer = FrameComposer::new(&gpu)?;
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            wake: Condvar::new(),
            counters: PresenterCounters::default(),
            timing: AtomicU8::new(timing_code(PresentTiming::Vsync)),
            scrgb_fallback: AtomicBool::new(false),
            visible: AtomicBool::new(true),
            presented_any: AtomicBool::new(false),
            mode: Mutex::new(None),
            events: Mutex::new(Vec::new()),
        });
        let owned = Owned {
            gpu,
            swapchain,
            composer,
            tree,
            path: PathWatch::default(),
            ready: false,
            timing: PresentTiming5s::default(),
            phases: [Duration::ZERO; 3],
        };
        let thread_shared = Arc::clone(&shared);
        let join = std::thread::Builder::new()
            .name("video-presenter".into())
            .spawn(move || run(owned, &thread_shared))
            .map_err(|error| format!("spawn presenter: {error}"))?;
        Ok(Self {
            handle: PresenterHandle { shared },
            join: Some(join),
        })
    }

    #[must_use]
    pub fn handle(&self) -> PresenterHandle {
        self.handle.clone()
    }
}

impl Drop for PresenterThread {
    fn drop(&mut self) {
        lock(&self.handle.shared.slot).stop = true;
        self.handle.shared.wake.notify_one();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn poll(owned: &mut Owned, shared: &Shared) {
    let _context = owned.gpu.lock_context();
    let swapchain = &owned.swapchain;
    let (confirmed, unconfirmed) = owned.composer.poll(&owned.gpu, |id| swapchain.confirm(id));
    shared
        .counters
        .confirmed
        .fetch_add(confirmed, Ordering::Relaxed);
    shared
        .counters
        .unconfirmed
        .fetch_add(unconfirmed, Ordering::Relaxed);
}

/// Teardown: queued frames are discarded, never shown late, and
/// unconfirmed fences are never reported.
fn stop(owned: &mut Owned, shared: &Shared) {
    lock(&shared.slot).frame = None;
    owned.composer.discard();
    if let Some((tree, owner)) = &owned.tree {
        let _ = lock(tree).hide_swapchain(*owner);
    }
}

/// Blocks until a frame or a re-present waits in the slot (`true`) or the
/// thread is stopped (`false`, after [`stop`]). Presented fences are
/// reported meanwhile.
fn wait_for_work(owned: &mut Owned, shared: &Shared) -> bool {
    let mut slot = lock(&shared.slot);
    loop {
        if slot.stop {
            drop(slot);
            stop(owned, shared);
            return false;
        }
        if slot.frame.is_some() || slot.represent {
            return true;
        }
        let (guard, _) = shared
            .wake
            .wait_timeout(slot, Duration::from_millis(8))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(guard);
        poll(owned, shared);
        slot = lock(&shared.slot);
    }
}

fn run(mut owned: Owned, shared: &Shared) {
    let mut last: Option<DecodedFrame> = None;
    loop {
        // Wait for something to show, then for the swapchain to take it, and
        // only then take the newest frame: one that arrived during the wait
        // replaces the older one instead of queueing behind it.
        if !wait_for_work(&mut owned, shared) {
            return;
        }
        let waited = Instant::now();
        if shared.visible.load(Ordering::Relaxed) && !owned.ready {
            // Holding no lock: the swapchain accepts a frame at the display's
            // next refresh or composition, and the decoder and every other
            // window's presenter share this device's immediate context.
            owned.swapchain.wait(100);
            owned.ready = true;
        }
        let waited = waited.elapsed();
        let next = {
            let mut slot = lock(&shared.slot);
            if slot.stop {
                drop(slot);
                stop(&mut owned, shared);
                return;
            }
            if let Some(pending) = slot.frame.take() {
                slot.represent = false;
                Some(pending)
            } else if slot.represent {
                slot.represent = false;
                None
            } else {
                continue;
            }
        };
        let represent = next.is_none();
        let (frame, arrival) = match next {
            Some(pending) => (pending.frame, Some(pending.arrival)),
            None => match last.clone() {
                Some(frame) => (frame, None),
                None => continue,
            },
        };
        if !shared.visible.load(Ordering::Relaxed) {
            shared
                .counters
                .skipped_hidden
                .fetch_add(1, Ordering::Relaxed);
            last = Some(frame);
            continue;
        }
        let result = {
            let work = Instant::now();
            let (result, locked) = present_one(&mut owned, shared, &frame);
            let size = (owned.swapchain.width, owned.swapchain.height);
            let phases = owned.phases;
            owned
                .timing
                .note(waited, locked, work.elapsed(), phases, size);
            result
        };
        match result {
            Ok(()) => {
                shared.presented_any.store(true, Ordering::Relaxed);
                owned.path.after_present(&owned.swapchain, shared.timing());
                let counter = if represent {
                    &shared.counters.represented
                } else {
                    &shared.counters.presented
                };
                counter.fetch_add(1, Ordering::Relaxed);
                if let Some(arrival) = arrival {
                    let micros = u64::try_from(arrival.elapsed().as_micros()).unwrap_or(u64::MAX);
                    shared
                        .counters
                        .latency_us_total
                        .fetch_add(micros, Ordering::Relaxed);
                    shared
                        .counters
                        .latency_samples
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(error) => {
                shared.counters.failed.fetch_add(1, Ordering::Relaxed);
                let device_lost = is_device_lost(&error);
                lock(&shared.events).push(if device_lost {
                    PresenterEvent::DeviceLost(error)
                } else {
                    PresenterEvent::Failed(error)
                });
                if device_lost {
                    return;
                }
            }
        }
        poll(&mut owned, shared);
        last = Some(frame);
    }
}

/// Draws `frame` and presents it. Only the draw and the bookkeeping after
/// `Present` hold the device's shared context lock; `Present` itself does
/// not: a composed present can block until the next composition (lab: 16
/// ms per present with two windows), and the decoder and every other
/// window's presenter must not wait behind it. The device is
/// multithread-protected, and `Present` is one call. Returns the time spent
/// waiting for the lock.
fn present_one(owned: &mut Owned, shared: &Shared, frame: &DecodedFrame) -> (Result<()>, Duration) {
    let context_lock = std::sync::Arc::clone(&owned.gpu.context_lock);
    let asked = Instant::now();
    let context = context_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut locked = asked.elapsed();
    let drawn = draw_one(owned, shared, frame);
    let (first, resized, changed) = match drawn {
        Ok(facts) => facts,
        Err(error) => {
            owned.composer.abandon_unsubmitted();
            return (Err(error), locked);
        }
    };
    drop(context);
    let presenting = Instant::now();
    let presented = owned.swapchain.present(shared.timing());
    let present = presenting.elapsed();
    owned.ready = false;
    let asked = Instant::now();
    let _context = context_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locked += asked.elapsed();
    let composing = Instant::now();
    let finished = presented.and_then(|()| {
        let present_id = owned.swapchain.last_present_id();
        owned.composer.submitted(&owned.gpu, present_id)?;
        if let Some((tree, owner)) = &owned.tree {
            // Every present: the other pipeline may have taken the visual
            // since, or torn down and cleared it.
            let mut tree = lock(tree);
            if first || resized || changed || !tree.shows(*owner) {
                tree.show_swapchain(*owner, &owned.swapchain, frame.picture.size())?;
            }
        }
        Ok(())
    });
    if finished.is_err() {
        owned.composer.abandon_unsubmitted();
    }
    owned.phases[1] = present;
    owned.phases[2] = composing.elapsed();
    (finished, locked)
}

/// The draw half of [`present_one`], under the context lock: the contract
/// applied to the swapchain and the frame drawn into its back buffer.
/// Returns whether this was the first present, a resize, a mode change.
fn draw_one(
    owned: &mut Owned,
    shared: &Shared,
    frame: &DecodedFrame,
) -> Result<(bool, bool, bool)> {
    let mode = PresentMode::for_contract(
        frame.contract,
        shared.scrgb_fallback.load(Ordering::Relaxed),
    );
    let size = frame.picture.size();
    let resized = size != (owned.swapchain.width, owned.swapchain.height);
    let changed = mode != owned.swapchain.mode;
    if changed || resized {
        owned.swapchain.set_mode(mode, size.0, size.1)?;
    }
    let first = lock(&shared.mode).is_none();
    if changed || first {
        *lock(&shared.mode) = Some(mode);
        lock(&shared.events).push(PresenterEvent::ModeChanged(mode));
    }
    let drawing = Instant::now();
    let back = owned.swapchain.back_buffer()?;
    let rtv = owned.gpu.rtv(&back)?;
    owned
        .composer
        .draw(&owned.gpu, frame, &rtv, mode.output())?;
    drop(rtv);
    drop(back);
    owned.phases[0] = drawing.elapsed();
    Ok((first, resized, changed))
}

super::opaque_debug!(PresenterHandle, PresenterThread);
