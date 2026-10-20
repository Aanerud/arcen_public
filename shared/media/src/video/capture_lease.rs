//! Bounded capture-buffer leases between a capture callback and the import
//! worker.
//!
//! A native capture callback (a `PipeWire` `process` callback today) must not
//! wait for conversion, upload, encode or the network. It validates a
//! delivered buffer ([`super::CaptureSurface::validate`]) and pushes a lease
//! into a [`CaptureMailbox`]; the import worker takes the newest frame, reads
//! the buffer, and drops the [`CaptureLease`], which returns the buffer to the
//! producer through the adapter's [`CaptureLeaseReleaser`] only then.
//!
//! The mailbox keeps at most `capacity` frames. A newer frame supersedes the
//! oldest: the superseded token is handed straight back to the callback for
//! immediate requeue, and its damage is carried into the next frame with
//! [`DamageEvidence::union`], so nothing changed is lost before a picture is
//! submitted. [`PendingCaptureDamage`] then applies the carried damage to Keel,
//! unioning observations until the encoder actually submits.
//!
//! The token type `T` is the adapter's: a buffer handle that only the
//! adapter's own thread dereferences. Nothing here touches it.

use std::collections::VecDeque;
use std::fmt::{Debug, Formatter};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use arcen_keel::{DamageMap, ExternalDamage, KeelError, PendingExternalDamage};

use super::{DamageEvidence, MAX_CAPTURE_DAMAGE_RECTS, ValidatedCaptureSurface};

/// Returns a producer buffer to its owner. An adapter implements it by
/// handing the token to the thread that alone may requeue buffers.
pub trait CaptureLeaseReleaser<T>: Send + Sync {
    /// Gives `token` back. Must not block.
    fn release(&self, token: T);
}

/// Lease statistics, updated without locks or logging.
#[derive(Debug, Default)]
pub struct CaptureLeaseCounters {
    /// Frames pushed.
    pub pushed: AtomicU64,
    /// Frames superseded before the worker read them.
    pub superseded: AtomicU64,
    /// Frames the worker took.
    pub taken: AtomicU64,
    /// Leases ended (dropped, superseded or drained).
    pub released: AtomicU64,
    /// Leases currently queued or held.
    pub outstanding: AtomicUsize,
    /// Most leases outstanding at once.
    pub max_outstanding: AtomicUsize,
}

impl CaptureLeaseCounters {
    fn acquire(&self) {
        let now = self.outstanding.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_outstanding.fetch_max(now, Ordering::AcqRel);
    }

    fn end(&self) {
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
        self.released.fetch_add(1, Ordering::Relaxed);
    }
}

/// A queued frame: the buffer token, its validated description, and the
/// damage of every frame it superseded.
#[derive(Debug)]
pub struct QueuedCapture<T> {
    /// Adapter buffer token.
    pub token: T,
    /// Validated description.
    pub surface: ValidatedCaptureSurface,
    /// Damage of this frame unioned with any frames it superseded.
    pub damage: DamageEvidence,
    /// Frames this one superseded.
    pub superseded: u32,
    /// When the callback queued it.
    pub received: Instant,
}

/// Why the worker got no frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTakeError {
    /// The source closed (stream ended, failed, or shut down) and the queue
    /// is empty.
    Closed,
}

struct MailboxState<T> {
    queue: VecDeque<QueuedCapture<T>>,
    closed: bool,
    /// Damage of buffers released unread (refused or corrupted), carried
    /// into the next queued frame.
    carried: Option<DamageEvidence>,
}

/// Bounded latest-wins queue of capture leases.
pub struct CaptureMailbox<T> {
    state: Mutex<MailboxState<T>>,
    ready: Condvar,
    capacity: usize,
    releaser: Arc<dyn CaptureLeaseReleaser<T>>,
    counters: Arc<CaptureLeaseCounters>,
}

impl<T> Debug for CaptureMailbox<T> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureMailbox")
            .field("capacity", &self.capacity)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl<T: Send> CaptureMailbox<T> {
    /// A mailbox holding at most `capacity` (at least one) frames.
    #[must_use]
    pub fn new(capacity: usize, releaser: Arc<dyn CaptureLeaseReleaser<T>>) -> Arc<Self> {
        let capacity = capacity.max(1);
        Arc::new(Self {
            state: Mutex::new(MailboxState {
                queue: VecDeque::with_capacity(capacity + 1),
                closed: false,
                carried: None,
            }),
            ready: Condvar::new(),
            capacity,
            releaser,
            counters: Arc::new(CaptureLeaseCounters::default()),
        })
    }

    /// Shared counters.
    #[must_use]
    pub fn counters(&self) -> Arc<CaptureLeaseCounters> {
        Arc::clone(&self.counters)
    }

    fn lock(&self) -> MutexGuard<'_, MailboxState<T>> {
        // A poisoned lock only means a panic elsewhere; the queue is always
        // consistent between statements, and buffers must still go back.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a validated frame and returns the tokens the caller must
    /// requeue now: superseded frames, or `token` itself when the mailbox is
    /// closed. Holds the queue lock only for the push.
    pub fn push(&self, token: T, surface: ValidatedCaptureSurface) -> Vec<T> {
        let mut damage = surface.damage().clone();
        let mut release = Vec::new();
        {
            let mut state = self.lock();
            if state.closed {
                return vec![token];
            }
            if let Some(carried) = state.carried.take() {
                damage = carried.union(damage);
            }
            self.counters.acquire();
            self.counters.pushed.fetch_add(1, Ordering::Relaxed);
            state.queue.push_back(QueuedCapture {
                token,
                surface,
                damage,
                superseded: 0,
                received: Instant::now(),
            });
            while state.queue.len() > self.capacity {
                let Some(oldest) = state.queue.pop_front() else {
                    break;
                };
                if let Some(next) = state.queue.front_mut() {
                    let newer = std::mem::replace(&mut next.damage, DamageEvidence::Unknown);
                    next.damage = oldest.damage.union(newer);
                    next.superseded = next
                        .superseded
                        .saturating_add(oldest.superseded.saturating_add(1));
                }
                self.counters.superseded.fetch_add(1, Ordering::Relaxed);
                self.counters.end();
                release.push(oldest.token);
            }
        }
        self.ready.notify_one();
        release
    }

    /// Waits up to `timeout` for the newest frame. Older queued frames are
    /// superseded: their damage is folded into the returned frame and their
    /// buffers go back through the releaser.
    ///
    /// # Errors
    ///
    /// [`CaptureTakeError::Closed`] once the source closed and nothing is
    /// queued.
    pub fn take_latest(
        self: &Arc<Self>,
        timeout: Duration,
    ) -> Result<Option<CaptureLease<T>>, CaptureTakeError> {
        let deadline = Instant::now() + timeout;
        let mut superseded_tokens = Vec::new();
        let newest = {
            let mut state = self.lock();
            let mut newest = loop {
                if let Some(newest) = state.queue.pop_back() {
                    break newest;
                }
                if state.closed {
                    return Err(CaptureTakeError::Closed);
                }
                let now = Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                state = self
                    .ready
                    .wait_timeout(state, deadline - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            };
            while let Some(older) = state.queue.pop_back() {
                newest.damage = older.damage.union(newest.damage);
                newest.superseded = newest
                    .superseded
                    .saturating_add(older.superseded.saturating_add(1));
                self.counters.superseded.fetch_add(1, Ordering::Relaxed);
                self.counters.end();
                superseded_tokens.push(older.token);
            }
            newest
        };
        for token in superseded_tokens {
            self.releaser.release(token);
        }
        self.counters.taken.fetch_add(1, Ordering::Relaxed);
        Ok(Some(CaptureLease {
            frame: Some(newest),
            mailbox: Arc::clone(self),
        }))
    }

    /// Records the damage of a buffer released unread (refused, corrupted),
    /// so the next queued frame carries it.
    pub fn carry_damage(&self, damage: DamageEvidence) {
        let mut state = self.lock();
        state.carried = Some(match state.carried.take() {
            Some(carried) => carried.union(damage),
            None => damage,
        });
    }

    /// Closes the mailbox. The worker drains what is queued, then sees
    /// [`CaptureTakeError::Closed`]; later pushes are returned for requeue.
    pub fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    /// Removes every queued frame and returns their tokens; used when the
    /// adapter revokes buffers (renegotiation, shutdown). The drained frames
    /// were never read, so their damage is carried into the next queued
    /// frame rather than lost.
    pub fn drain(&self) -> Vec<T> {
        let mut state = self.lock();
        let mut drained: Option<DamageEvidence> = None;
        let mut tokens = Vec::with_capacity(state.queue.len());
        for frame in state.queue.drain(..) {
            drained = Some(match drained {
                Some(older) => older.union(frame.damage),
                None => frame.damage,
            });
            tokens.push(frame.token);
            self.counters.end();
        }
        if let Some(drained) = drained {
            state.carried = Some(match state.carried.take() {
                Some(carried) => carried.union(drained),
                None => drained,
            });
        }
        tokens
    }

    /// Whether the mailbox is closed.
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

/// A frame the import worker holds. Dropping it returns the buffer to the
/// producer, so hold it until every reader of the buffer has finished.
pub struct CaptureLease<T: Send> {
    frame: Option<QueuedCapture<T>>,
    mailbox: Arc<CaptureMailbox<T>>,
}

impl<T: Send> CaptureLease<T> {
    /// The leased frame.
    ///
    /// # Panics
    ///
    /// Never: the frame is present until the lease is dropped.
    #[must_use]
    pub fn frame(&self) -> &QueuedCapture<T> {
        match &self.frame {
            Some(frame) => frame,
            None => unreachable!("a lease holds its frame until drop"),
        }
    }
}

impl<T: Send> Debug for CaptureLease<T> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureLease")
            .field(
                "sequence",
                &self.frame().surface.surface().timestamps.sequence,
            )
            .finish_non_exhaustive()
    }
}

impl<T: Send> Drop for CaptureLease<T> {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            self.mailbox.counters.end();
            self.mailbox.releaser.release(frame.token);
        }
    }
}

impl DamageEvidence {
    /// The damage of two frames, `self` older. Two region lists concatenate;
    /// anything else is full damage, and a union too large to describe
    /// becomes [`Self::Unknown`] (full).
    #[must_use]
    pub fn union(self, newer: Self) -> Self {
        match (self, newer) {
            (Self::Regions(mut older), Self::Regions(newer)) => {
                if older.len() + newer.len() > MAX_CAPTURE_DAMAGE_RECTS {
                    Self::Unknown
                } else {
                    older.extend(newer);
                    Self::Regions(older)
                }
            }
            (Self::Full, _) | (_, Self::Full) => Self::Full,
            _ => Self::Unknown,
        }
    }
}

/// Keel damage of frames observed but not yet submitted to the encoder.
#[derive(Debug)]
pub struct PendingCaptureDamage {
    pending: PendingExternalDamage,
    scratch: ExternalDamage,
    /// Every observation since the last submission was a producer region
    /// list (see [`Self::is_authoritative`]).
    authoritative: bool,
}

impl PendingCaptureDamage {
    /// A tracker for `width` x `height` frames.
    ///
    /// # Errors
    ///
    /// Keel geometry errors.
    pub fn new(width: usize, height: usize) -> Result<Self, KeelError> {
        Ok(Self {
            pending: PendingExternalDamage::new(width, height)?,
            scratch: ExternalDamage::new(width, height)?,
            authoritative: false,
        })
    }

    /// Unions one frame's damage into the pending picture. Anything but a
    /// region list marks the full frame.
    ///
    /// # Errors
    ///
    /// Keel geometry errors.
    pub fn observe(&mut self, damage: &DamageEvidence) -> Result<(), KeelError> {
        let regions = matches!(damage, DamageEvidence::Regions(_));
        self.authoritative = regions && (self.authoritative || !self.pending.is_pending());
        self.scratch.reset();
        match damage {
            DamageEvidence::Regions(rects) => {
                for rect in rects {
                    self.scratch.mark_rect(*rect);
                }
            }
            DamageEvidence::Unknown | DamageEvidence::Full => self.scratch.mark_full_frame(),
        }
        self.pending.observe(self.scratch.damage_map())
    }

    /// The submitted picture consumed the pending damage.
    pub fn mark_submitted(&mut self) {
        self.pending.mark_submitted();
    }

    /// Pending damage.
    #[must_use]
    pub fn damage_map(&self) -> DamageMap<'_> {
        self.pending.damage_map()
    }

    /// Whether damage waits for a submission.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending.is_pending()
    }

    /// Whether the pending damage is the producer's own description of
    /// what changed: something is pending and every frame observed since
    /// the last submission carried a region list. Damage derived from a
    /// frame without one is the conservative full frame, which says nothing
    /// about where the change is, so a consumer that biases encoding by
    /// damage (a QP map) must treat such a picture as uniform.
    #[must_use]
    pub const fn is_authoritative(&self) -> bool {
        self.authoritative && self.pending.is_pending()
    }
}

#[cfg(test)]
mod tests {
    use arcen_keel::PixelRect;

    use super::*;
    use crate::video::{
        CaptureBackend, CaptureGeometry, CaptureImporter, CaptureMemory, CapturePixelFormat,
        CaptureSurface, CaptureTimestamps, CopyPath, CursorEvidence, PlaneLayout, SourceColour,
    };
    use crate::{OutputIdentity, RegionGeneration};

    #[derive(Default)]
    struct Collector(Mutex<Vec<u32>>);

    impl CaptureLeaseReleaser<u32> for Collector {
        fn release(&self, token: u32) {
            self.0.lock().unwrap().push(token);
        }
    }

    fn rect(x: usize, y: usize) -> PixelRect {
        PixelRect {
            x,
            y,
            width: 8,
            height: 8,
        }
    }

    fn surface(sequence: u64, damage: DamageEvidence) -> ValidatedCaptureSurface {
        let output = OutputIdentity::new("Meta-0").unwrap();
        let generation = RegionGeneration::new(1).unwrap();
        CaptureSurface {
            output: output.clone(),
            generation,
            backend: CaptureBackend::PipeWire,
            format: CapturePixelFormat::Bgrx8,
            geometry: CaptureGeometry {
                coded_width: 64,
                coded_height: 64,
                visible: PixelRect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
            },
            planes: vec![PlaneLayout {
                buffer: 0,
                offset: 0,
                stride: 256,
                size: 256 * 64,
            }],
            buffer_sizes: vec![Some(256 * 64)],
            memory: CaptureMemory::HostShared,
            colour: SourceColour::UNKNOWN,
            timestamps: CaptureTimestamps {
                sequence,
                capture_monotonic_ns: 0,
                presentation_ns: None,
            },
            damage,
            cursor: CursorEvidence::SeparateMetadata,
            gpu: None,
            copy_path: CopyPath::HostCopy,
        }
        .validate(&CaptureImporter {
            output,
            generation,
            colour: SourceColour::UNKNOWN,
            encoder_gpu: None,
            gpu_modifiers: Vec::new(),
            last_sequence: None,
        })
        .unwrap()
    }

    fn regions(rects: &[(usize, usize)]) -> DamageEvidence {
        DamageEvidence::Regions(rects.iter().map(|(x, y)| rect(*x, *y)).collect())
    }

    #[test]
    fn push_never_exceeds_capacity_and_releases_the_oldest() {
        let mailbox = CaptureMailbox::new(2, Arc::new(Collector::default()));
        assert!(
            mailbox
                .push(1, surface(1, DamageEvidence::Unknown))
                .is_empty()
        );
        assert!(
            mailbox
                .push(2, surface(2, DamageEvidence::Unknown))
                .is_empty()
        );
        assert_eq!(
            mailbox.push(3, surface(3, DamageEvidence::Unknown)),
            vec![1]
        );
        let counters = mailbox.counters();
        assert_eq!(counters.outstanding.load(Ordering::Acquire), 2);
        assert_eq!(counters.superseded.load(Ordering::Relaxed), 1);
        assert_eq!(counters.max_outstanding.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn superseded_damage_reaches_the_taken_frame_and_buffers_return() {
        let collector = Arc::new(Collector::default());
        let mailbox = CaptureMailbox::new(2, collector.clone());
        mailbox.push(1, surface(1, regions(&[(0, 0)])));
        mailbox.push(2, surface(2, regions(&[(16, 16)])));
        assert_eq!(mailbox.push(3, surface(3, regions(&[(32, 32)]))), vec![1]);
        let lease = mailbox
            .take_latest(Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(lease.frame().token, 3);
        assert_eq!(lease.frame().superseded, 2);
        assert_eq!(lease.frame().damage, regions(&[(0, 0), (16, 16), (32, 32)]));
        assert_eq!(*collector.0.lock().unwrap(), vec![2]);
        drop(lease);
        assert_eq!(*collector.0.lock().unwrap(), vec![2, 3]);
        assert_eq!(mailbox.counters().outstanding.load(Ordering::Acquire), 0);
    }

    #[test]
    fn unknown_damage_absorbs_regions_across_supersession() {
        let mailbox = CaptureMailbox::new(1, Arc::new(Collector::default()));
        mailbox.push(1, surface(1, DamageEvidence::Unknown));
        mailbox.push(2, surface(2, regions(&[(0, 0)])));
        let lease = mailbox
            .take_latest(Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(lease.frame().damage, DamageEvidence::Unknown);
    }

    #[test]
    fn carried_damage_reaches_the_next_push() {
        let mailbox = CaptureMailbox::new(1, Arc::new(Collector::default()));
        mailbox.carry_damage(regions(&[(0, 0)]));
        mailbox.push(1, surface(1, regions(&[(8, 8)])));
        let lease = mailbox
            .take_latest(Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(lease.frame().damage, regions(&[(0, 0), (8, 8)]));
    }

    #[test]
    fn take_times_out_then_reports_close_after_drain() {
        let mailbox = CaptureMailbox::new(1, Arc::new(Collector::default()));
        assert!(
            mailbox
                .take_latest(Duration::from_millis(5))
                .unwrap()
                .is_none()
        );
        mailbox.push(1, surface(1, DamageEvidence::Unknown));
        mailbox.close();
        assert_eq!(
            mailbox.push(2, surface(2, DamageEvidence::Unknown)),
            vec![2]
        );
        assert!(mailbox.take_latest(Duration::ZERO).unwrap().is_some());
        assert_eq!(
            mailbox.take_latest(Duration::ZERO).unwrap_err(),
            CaptureTakeError::Closed
        );
    }

    #[test]
    fn drain_returns_queued_tokens() {
        let mailbox = CaptureMailbox::new(2, Arc::new(Collector::default()));
        mailbox.push(1, surface(1, DamageEvidence::Unknown));
        mailbox.push(2, surface(2, DamageEvidence::Unknown));
        assert_eq!(mailbox.drain(), vec![1, 2]);
        assert_eq!(mailbox.counters().outstanding.load(Ordering::Acquire), 0);
    }

    #[test]
    fn drained_damage_is_carried_into_the_next_frame() {
        let mailbox = CaptureMailbox::new(2, Arc::new(Collector::default()));
        mailbox.carry_damage(regions(&[(0, 0)]));
        mailbox.push(1, surface(1, regions(&[(16, 16)])));
        mailbox.push(2, surface(2, regions(&[(32, 32)])));
        assert_eq!(mailbox.drain(), vec![1, 2]);
        mailbox.push(3, surface(3, regions(&[(48, 48)])));
        let lease = mailbox
            .take_latest(Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(
            lease.frame().damage,
            regions(&[(0, 0), (16, 16), (32, 32), (48, 48)])
        );
        drop(lease);
        // Draining an empty mailbox carries nothing.
        assert!(mailbox.drain().is_empty());
        mailbox.push(4, surface(4, regions(&[(8, 8)])));
        let lease = mailbox
            .take_latest(Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(lease.frame().damage, regions(&[(8, 8)]));
    }

    #[test]
    fn drained_unknown_damage_makes_the_next_frame_full() {
        let mailbox = CaptureMailbox::new(1, Arc::new(Collector::default()));
        mailbox.push(1, surface(1, DamageEvidence::Unknown));
        assert_eq!(mailbox.drain(), vec![1]);
        mailbox.push(2, surface(2, regions(&[(0, 0)])));
        let lease = mailbox
            .take_latest(Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(lease.frame().damage, DamageEvidence::Unknown);
    }

    #[test]
    fn worker_wakes_when_a_frame_arrives() {
        let mailbox = CaptureMailbox::new(1, Arc::new(Collector::default()));
        let producer = Arc::clone(&mailbox);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer.push(7, surface(1, DamageEvidence::Unknown));
        });
        let lease = mailbox
            .take_latest(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(lease.frame().token, 7);
        thread.join().unwrap();
    }

    #[test]
    fn union_rules() {
        let one = || regions(&[(0, 0)]);
        assert_eq!(DamageEvidence::Full.union(one()), DamageEvidence::Full);
        assert_eq!(
            one().union(DamageEvidence::Unknown),
            DamageEvidence::Unknown
        );
        let many = DamageEvidence::Regions(vec![rect(0, 0); MAX_CAPTURE_DAMAGE_RECTS]);
        assert_eq!(many.union(one()), DamageEvidence::Unknown);
    }

    #[test]
    fn pending_damage_unions_until_submitted() {
        let mut pending = PendingCaptureDamage::new(64, 64).unwrap();
        pending.observe(&regions(&[(0, 0)])).unwrap();
        pending.observe(&regions(&[(48, 48)])).unwrap();
        assert!(pending.is_pending());
        assert_eq!(pending.damage_map().dirty_blocks().count(), 2);
        pending.mark_submitted();
        pending.observe(&regions(&[(16, 16)])).unwrap();
        assert_eq!(pending.damage_map().dirty_blocks().count(), 1);
        pending.observe(&DamageEvidence::Unknown).unwrap();
        assert_eq!(pending.damage_map().dirty_blocks().count(), 16);
    }

    #[test]
    fn pending_damage_is_authoritative_only_while_every_frame_carried_regions() {
        let mut pending = PendingCaptureDamage::new(64, 64).unwrap();
        assert!(!pending.is_authoritative(), "nothing pending");
        pending.observe(&regions(&[(0, 0)])).unwrap();
        pending.observe(&regions(&[(48, 48)])).unwrap();
        assert!(pending.is_authoritative());
        pending.observe(&DamageEvidence::Unknown).unwrap();
        pending.observe(&regions(&[(16, 16)])).unwrap();
        assert!(
            !pending.is_authoritative(),
            "a frame without regions taints the picture"
        );
        pending.mark_submitted();
        assert!(
            !pending.is_authoritative(),
            "nothing pending after a submission"
        );
        pending.observe(&regions(&[(16, 16)])).unwrap();
        assert!(
            pending.is_authoritative(),
            "a submission starts a fresh picture"
        );
        pending.mark_submitted();
        pending.observe(&DamageEvidence::Full).unwrap();
        assert!(!pending.is_authoritative());
    }
}
