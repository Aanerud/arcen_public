// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The presenter thread of one pipeline: the Windows twin of the Mac's
//! CVDisplayLink-driven presenter (`video_metal_layer.rs`
//! `DedicatedDisplayLink`).
//!
//! The media worker hands frames to a **latest-frame slot** (a newer frame
//! replaces one not yet presented; replacements are counted, never queued);
//! the thread waits on the swapchain's frame-latency waitable object
//! (latency 1), converts the newest frame and presents it. A still screen
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

fn run(mut owned: Owned, shared: &Shared) {
    let mut last: Option<DecodedFrame> = None;
    loop {
        let next = {
            let mut slot = lock(&shared.slot);
            loop {
                if slot.stop {
                    // Teardown: queued frames are discarded, never shown
                    // late, and unconfirmed fences are never reported.
                    slot.frame = None;
                    owned.composer.discard();
                    if let Some((tree, owner)) = &owned.tree {
                        let _ = lock(tree).hide_swapchain(*owner);
                    }
                    return;
                }
                if let Some(pending) = slot.frame.take() {
                    slot.represent = false;
                    break Some(pending);
                }
                if slot.represent {
                    slot.represent = false;
                    break None;
                }
                // Wake periodically so presented fences are reported.
                let (guard, _) = shared
                    .wake
                    .wait_timeout(slot, Duration::from_millis(8))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                slot = guard;
                poll(&mut owned, shared);
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
            let context_lock = std::sync::Arc::clone(&owned.gpu.context_lock);
            let _context = context_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = present_one(&mut owned, shared, &frame);
            if result.is_err() {
                owned.composer.abandon_unsubmitted();
            }
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

fn present_one(owned: &mut Owned, shared: &Shared, frame: &DecodedFrame) -> Result<()> {
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
    owned.swapchain.wait(100);
    let back = owned.swapchain.back_buffer()?;
    let rtv = owned.gpu.rtv(&back)?;
    owned
        .composer
        .draw(&owned.gpu, frame, &rtv, mode.output())?;
    drop(rtv);
    drop(back);
    owned.swapchain.present(shared.timing())?;
    let present_id = owned.swapchain.last_present_id();
    owned.composer.submitted(&owned.gpu, present_id)?;
    if let Some((tree, owner)) = &owned.tree {
        // Every present: the other pipeline may have taken the visual
        // since, or torn down and cleared it.
        let mut tree = lock(tree);
        if first || resized || changed || !tree.shows(*owner) {
            tree.show_swapchain(*owner, &owned.swapchain, size)?;
        }
    }
    Ok(())
}

super::opaque_debug!(PresenterHandle, PresenterThread);
