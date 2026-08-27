//! Per-client outbound frame queue — the backpressure heart of the relay.
//!
//! Faithful port of `server/client_session.py` (`send_queue` + `enqueue`):
//!
//!   * bounded at [`CAPACITY`] = 8 frames;
//!   * on overflow, clear the dependent chain and request one IDR — throttled
//!     to at most once per [`KEYFRAME_REQUEST_MIN_INTERVAL`];
//!   * while awaiting that IDR, suppress every non-keyframe so the writer can
//!     never expose an undecodable continuation of the broken chain;
//!   * on a **keyframe enqueue**, clear the queue first (the new IDR supersedes
//!     any pending P-frames), leave recovery state, and resume normal flow.
//!
//! A single writer task drains via [`dequeue`](FrameQueue::dequeue) and does the
//! actual `ws.send`, exactly like the Python `_send_loop`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::logging::target;
use crate::media::capenc::IdrRequester;

/// `send_queue` maxsize. Raised from 4 to 8 to match the observed QUIC burst
/// depth (5-7 frames) at ~33ms RTT. IDR-on-drop still bounds recovery; the
/// larger queue prevents spurious IDR storms during CWND expansion.
pub const CAPACITY: usize = 8;

/// At most one IDR request per second while a drop streak persists
/// (`KEYFRAME_REQUEST_MIN_INTERVAL_S = 1.0`).
pub const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_secs(1);

struct Inner {
    deque: VecDeque<Vec<u8>>,
    generation_recovery: Option<Vec<u8>>,
    generation_chain: bool,
    require_generation_recovery: bool,
    protected_front: bool,
    awaiting_keyframe: bool,
    drops_since_keyframe: u64,
    last_keyframe_request_at: Option<Instant>,
    paused: bool,
    closed: bool,
}

/// Bounded, drop-oldest, IDR-on-drop outbound queue for one client.
pub struct FrameQueue {
    inner: Mutex<Inner>,
    notify: Notify,
    idr: IdrRequester,
    frames_sent: AtomicU64,
    frames_dropped: AtomicU64,
    /// Aggregate wire bytes handed to the writer, for bandwidth-derived
    /// health telemetry only — never logged per frame.
    bytes_sent: AtomicU64,
}

impl FrameQueue {
    pub fn new(idr: IdrRequester) -> Self {
        Self {
            inner: Mutex::new(Inner {
                deque: VecDeque::with_capacity(CAPACITY),
                generation_recovery: None,
                generation_chain: false,
                require_generation_recovery: false,
                protected_front: false,
                awaiting_keyframe: false,
                drops_since_keyframe: 0,
                last_keyframe_request_at: None,
                paused: false,
                closed: false,
            }),
            notify: Notify::new(),
            idr,
            frames_sent: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
        }
    }

    /// Enqueue one wire frame (10-byte header + payload). Returns `false` if a
    /// frame had to be dropped to make room (for drop telemetry), `true`
    /// otherwise. Non-blocking.
    pub fn enqueue(&self, data: Vec<u8>, is_keyframe: bool) -> bool {
        self.enqueue_classified_at(data, is_keyframe, is_keyframe, Instant::now())
    }

    pub(crate) fn enqueue_classified(
        &self,
        data: Vec<u8>,
        is_keyframe: bool,
        is_recovery_point: bool,
    ) -> bool {
        self.enqueue_classified_at(data, is_keyframe, is_recovery_point, Instant::now())
    }

    fn enqueue_at(&self, data: Vec<u8>, is_keyframe: bool, now: Instant) -> bool {
        self.enqueue_classified_at(data, is_keyframe, is_keyframe, now)
    }

    fn enqueue_classified_at(
        &self,
        data: Vec<u8>,
        is_keyframe: bool,
        is_recovery_point: bool,
        now: Instant,
    ) -> bool {
        let mut request_idr = false;
        let mut dropped = 0u64;
        let accepted;
        {
            let mut g = self.inner.lock().unwrap();
            if g.closed {
                return false;
            }
            if g.generation_chain && g.require_generation_recovery {
                if is_recovery_point && g.deque.len() < CAPACITY {
                    g.deque.push_back(data);
                    g.require_generation_recovery = false;
                    g.awaiting_keyframe = false;
                    g.drops_since_keyframe = 0;
                    g.last_keyframe_request_at = None;
                    if !g.paused {
                        g.generation_chain = false;
                    }
                    accepted = true;
                } else {
                    dropped = 1;
                    g.drops_since_keyframe += dropped;
                    request_idr = idr_request_due(&mut g, now);
                    accepted = false;
                }
            } else if g.paused && g.generation_chain {
                let generation_capacity =
                    CAPACITY.saturating_sub(usize::from(g.generation_recovery.is_some()));
                if g.deque.len() < generation_capacity {
                    g.deque.push_back(data);
                    accepted = true;
                } else {
                    dropped = g.deque.len() as u64 + 1;
                    g.deque.clear();
                    g.require_generation_recovery = true;
                    g.awaiting_keyframe = true;
                    g.drops_since_keyframe += dropped;
                    request_idr = idr_request_due(&mut g, now);
                    accepted = false;
                }
            } else if g.paused && g.generation_recovery.is_some() {
                dropped = 1;
                accepted = false;
            } else if g.protected_front {
                if g.deque.len() < CAPACITY {
                    g.deque.push_back(data);
                    accepted = true;
                } else {
                    let pinned = g.deque.pop_front();
                    dropped = g.deque.len() as u64 + 1;
                    g.deque.clear();
                    if let Some(pinned) = pinned {
                        g.deque.push_back(pinned);
                    }
                    g.generation_chain = true;
                    g.require_generation_recovery = !is_recovery_point;
                    g.awaiting_keyframe = !is_recovery_point;
                    if is_recovery_point {
                        g.deque.push_back(data);
                        g.drops_since_keyframe = 0;
                        g.last_keyframe_request_at = None;
                        accepted = true;
                    } else {
                        g.drops_since_keyframe += dropped;
                        request_idr = idr_request_due(&mut g, now);
                        accepted = false;
                    }
                }
            } else if is_keyframe {
                // Only leave recovery after the replacement IDR is in the
                // writer queue. Clearing first guarantees it cannot sit behind
                // frames from the old prediction chain.
                g.deque.clear();
                g.deque.push_back(data);
                g.awaiting_keyframe = false;
                g.drops_since_keyframe = 0;
                g.last_keyframe_request_at = None;
                accepted = true;
            } else if g.awaiting_keyframe {
                dropped = 1;
                g.drops_since_keyframe += dropped;
                request_idr = idr_request_due(&mut g, now);
                accepted = false;
            } else if g.deque.len() < CAPACITY {
                g.deque.push_back(data);
                accepted = true;
            } else {
                // Losing any AU invalidates every queued descendant. Drop the
                // entire chain, including the new P-frame, and expose nothing
                // until a replacement keyframe is safely queued.
                dropped = g.deque.len() as u64 + 1;
                g.deque.clear();
                g.awaiting_keyframe = true;
                g.drops_since_keyframe += dropped;
                accepted = false;
                request_idr = idr_request_due(&mut g, now);
                tracing::warn!(
                    target: target::MEDIA,
                    drops = g.drops_since_keyframe,
                    "send queue lost AU — cleared prediction chain, awaiting IDR"
                );
            }
        }
        // Do side-effects outside the lock.
        if accepted {
            self.notify.notify_one();
        }
        if dropped > 0 {
            self.frames_dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        if request_idr {
            self.idr.request();
        }
        accepted
    }

    /// Pin the exact self-contained recovery AU for a paused generation.
    ///
    /// Once pinned, later frames are dropped until activation so queue overflow
    /// cannot replace the SPS/PPS/IDR selected by the generation waiter. A
    /// second recovery point is requested for the post-activation prediction
    /// chain; P-frames remain suppressed until it is queued.
    pub fn pin_generation_recovery(&self, data: Vec<u8>) -> bool {
        let requested_at = Instant::now();
        let dropped = {
            let mut inner = self.inner.lock().unwrap();
            if inner.closed || !inner.paused || inner.generation_recovery.is_some() {
                return false;
            }
            let dropped = inner.deque.len() as u64;
            inner.deque.clear();
            inner.generation_recovery = Some(data);
            inner.generation_chain = true;
            inner.require_generation_recovery = true;
            inner.awaiting_keyframe = true;
            inner.drops_since_keyframe = 0;
            inner.last_keyframe_request_at = Some(requested_at);
            dropped
        };
        self.frames_dropped.fetch_add(dropped, Ordering::Relaxed);
        if !self.idr.request() {
            let mut inner = self.inner.lock().unwrap();
            if inner.last_keyframe_request_at == Some(requested_at) {
                inner.last_keyframe_request_at = None;
            }
        }
        true
    }

    /// Await the next frame to send. Returns `None` once the queue is closed and
    /// drained (writer task should then exit).
    pub async fn dequeue(&self) -> Option<Vec<u8>> {
        loop {
            // Register interest BEFORE checking to avoid a lost wakeup.
            let notified = self.notify.notified();
            {
                let mut g = self.inner.lock().unwrap();
                if !g.paused {
                    if let Some(item) = g.deque.pop_front() {
                        if g.protected_front {
                            g.protected_front = false;
                        }
                        self.frames_sent.fetch_add(1, Ordering::Relaxed);
                        self.bytes_sent
                            .fetch_add(item.len() as u64, Ordering::Relaxed);
                        return Some(item);
                    }
                }
                if g.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Close the queue and wake the writer so it can exit.
    ///
    /// Any already-buffered frames are still handed to the writer first —
    /// [`dequeue`](Self::dequeue) drains before it observes `closed`. That is
    /// exactly the single-monitor legacy contract (`close_drains_then_ends`
    /// below): the one writer for this one queue is allowed to finish
    /// sending what it already queued.
    pub fn close(&self) {
        {
            let mut g = self.inner.lock().unwrap();
            g.closed = true;
        }
        self.notify.notify_one();
    }

    /// Close the queue **and discard any buffered frames immediately**, so
    /// [`dequeue`](Self::dequeue) returns `None` right away rather than
    /// draining first.
    ///
    /// Used only by [`super::monitor_mux::MonitorMux::close_and_clear_all`]:
    /// a multi-monitor Carrier A session's atomic-teardown policy means that
    /// once *any* one monitor's pipeline ends, no monitor's queue may emit
    /// another video frame — including frames already buffered in a
    /// *different*, still-nominally-open sibling queue. Never call this for
    /// the single-monitor legacy path; use [`close`](Self::close) there.
    pub(crate) fn close_and_clear(&self) {
        {
            let mut g = self.inner.lock().unwrap();
            g.closed = true;
            g.deque.clear();
            g.generation_recovery = None;
        }
        self.notify.notify_one();
    }

    pub fn frames_sent(&self) -> u64 {
        self.frames_sent.load(Ordering::Relaxed)
    }

    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped.load(Ordering::Relaxed)
    }

    /// Aggregate wire bytes dequeued so far, for bandwidth-derived health
    /// telemetry.
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }

    /// Begin a new media-plan generation without exposing queued frames from
    /// the prior geometry. The sender remains paused until
    /// [`activate_generation`](Self::activate_generation).
    pub fn begin_generation(&self) {
        let dropped = {
            let mut inner = self.inner.lock().unwrap();
            let dropped =
                inner.deque.len() as u64 + u64::from(inner.generation_recovery.take().is_some());
            inner.deque.clear();
            inner.generation_chain = false;
            inner.require_generation_recovery = false;
            inner.protected_front = false;
            inner.awaiting_keyframe = true;
            inner.drops_since_keyframe = 0;
            inner.last_keyframe_request_at = None;
            inner.paused = true;
            dropped
        };
        self.frames_dropped.fetch_add(dropped, Ordering::Relaxed);
    }

    pub fn activate_generation(&self) -> bool {
        let activated = {
            let mut inner = self.inner.lock().unwrap();
            let Some(recovery) = inner.generation_recovery.take() else {
                return false;
            };
            inner.deque.push_front(recovery);
            inner.paused = false;
            inner.protected_front = true;
            if !inner.require_generation_recovery {
                inner.generation_chain = false;
            }
            true
        };
        self.notify.notify_one();
        activated
    }

    #[cfg(test)]
    pub(crate) fn awaiting_keyframe(&self) -> bool {
        self.inner.lock().unwrap().awaiting_keyframe
    }

    #[cfg(test)]
    pub(crate) fn is_paused(&self) -> bool {
        self.inner.lock().unwrap().paused
    }

    pub(crate) fn requires_generation_recovery(&self) -> bool {
        self.inner.lock().unwrap().require_generation_recovery
    }
}

fn idr_request_due(inner: &mut Inner, now: Instant) -> bool {
    let due = inner
        .last_keyframe_request_at
        .map(|last| now.duration_since(last) >= KEYFRAME_REQUEST_MIN_INTERVAL)
        .unwrap_or(true);
    if due {
        inner.last_keyframe_request_at = Some(now);
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::capenc::test_support::fake_idr;

    #[tokio::test]
    async fn dropped_p_frame_clears_descendants_and_awaits_idr() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        for i in 0..CAPACITY {
            assert!(q.enqueue(vec![i as u8], false), "frame {i} within capacity");
        }
        assert!(!q.enqueue(vec![0xFF], false));
        assert!(q.awaiting_keyframe());
        assert_eq!(q.frames_dropped(), (CAPACITY + 1) as u64);
        assert!(rx.try_recv().is_ok(), "loss requests an IDR");

        q.close();
        assert!(
            q.dequeue().await.is_none(),
            "no descendant P-frame may remain visible"
        );
    }

    #[tokio::test]
    async fn keyframe_clears_pending_and_fits() {
        let (idr, _rx) = fake_idr();
        let q = FrameQueue::new(idr);
        for i in 0..CAPACITY {
            q.enqueue(vec![i as u8], false);
        }
        // Keyframe clears the 4 pending P-frames and enqueues cleanly.
        assert!(q.enqueue(vec![0xAA], true));
        assert!(!q.awaiting_keyframe());
        assert_eq!(q.dequeue().await.unwrap(), vec![0xAA]);
    }

    #[tokio::test]
    async fn future_p_frames_are_suppressed_until_keyframe_resumes() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        for i in 0..CAPACITY {
            assert!(q.enqueue(vec![i as u8], false));
        }
        assert!(!q.enqueue(vec![10], false));
        assert!(!q.enqueue(vec![11], false));
        assert!(!q.enqueue(vec![12], false));
        assert_eq!(q.frames_dropped(), (CAPACITY + 3) as u64);
        assert!(rx.try_recv().is_ok(), "first loss requests an IDR");
        assert!(
            rx.try_recv().is_err(),
            "suppressed P-frames must not create an IDR storm"
        );

        assert!(q.enqueue(vec![0xAA], true));
        assert!(q.enqueue(vec![0xBB], false));
        assert_eq!(q.dequeue().await, Some(vec![0xAA]));
        assert_eq!(q.dequeue().await, Some(vec![0xBB]));
    }

    #[tokio::test]
    async fn each_recovered_chain_can_request_a_fresh_idr_immediately() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        for _ in 0..CAPACITY {
            q.enqueue(vec![0], false);
        }
        q.enqueue(vec![1], false);
        q.enqueue(vec![2], false);
        assert!(rx.try_recv().is_ok(), "first overflow requests an IDR");
        assert!(rx.try_recv().is_err(), "second overflow is throttled");

        assert!(q.enqueue(vec![3], true));
        for _ in 0..CAPACITY {
            q.enqueue(vec![4], false);
        }
        assert!(!q.enqueue(vec![5], false));
        assert!(
            rx.try_recv().is_ok(),
            "a loss after recovery needs a new IDR without waiting on the old guard"
        );
    }

    #[tokio::test]
    async fn awaiting_chain_retries_idr_only_after_throttle_interval() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        let start = Instant::now();
        for value in 0..CAPACITY {
            assert!(q.enqueue_at(vec![value as u8], false, start));
        }
        assert!(!q.enqueue_at(vec![10], false, start));
        assert!(!q.enqueue_at(vec![11], false, start + KEYFRAME_REQUEST_MIN_INTERVAL / 2,));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "early retry must be throttled");

        assert!(!q.enqueue_at(vec![12], false, start + KEYFRAME_REQUEST_MIN_INTERVAL,));
        assert!(
            rx.try_recv().is_ok(),
            "a missing replacement IDR must be retried after the guard"
        );
    }

    #[tokio::test]
    async fn failed_keyframe_enqueue_keeps_awaiting_state() {
        let (idr, _rx) = fake_idr();
        let q = FrameQueue::new(idr);
        for value in 0..CAPACITY {
            assert!(q.enqueue(vec![value as u8], false));
        }
        assert!(!q.enqueue(vec![10], false));
        q.close();
        assert!(!q.enqueue(vec![0xAA], true));
        assert!(q.awaiting_keyframe());
        assert_eq!(q.dequeue().await, None);
    }

    #[tokio::test]
    async fn generation_barrier_hides_old_and_new_frames_until_activation() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        assert!(q.enqueue(vec![1], false));

        q.begin_generation();
        assert!(q.is_paused());
        assert!(q.awaiting_keyframe());
        assert!(
            tokio::time::timeout(Duration::from_millis(5), q.dequeue())
                .await
                .is_err(),
            "old generation must not remain visible"
        );
        assert!(!q.enqueue(vec![2], false));
        assert!(rx.try_recv().is_ok(), "new generation requests an IDR");
        assert!(q.pin_generation_recovery(vec![3]));
        assert!(rx.try_recv().is_ok(), "pinning requests a follow-up IDR");
        assert!(!q.enqueue_classified(vec![4], false, false));
        assert!(q.enqueue_classified(vec![5], true, true));
        assert!(q.enqueue_classified(vec![6], false, false));
        assert!(
            tokio::time::timeout(Duration::from_millis(5), q.dequeue())
                .await
                .is_err(),
            "replacement IDR stays hidden until geometry control is written"
        );

        assert!(q.activate_generation());
        assert_eq!(q.dequeue().await, Some(vec![3]));
        assert_eq!(q.dequeue().await, Some(vec![5]));
        assert_eq!(q.dequeue().await, Some(vec![6]));
    }

    #[test]
    fn generation_activation_requires_a_pinned_recovery_au() {
        let (idr, _rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.begin_generation();
        assert!(!q.activate_generation());
        assert!(q.is_paused());
    }

    #[tokio::test]
    async fn pinned_recovery_survives_delayed_barrier_and_overflow() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.begin_generation();
        assert!(q.enqueue(vec![0x10], true));
        assert!(q.enqueue(vec![0x11], false));
        assert!(q.pin_generation_recovery(vec![0xAA]));
        assert!(rx.try_recv().is_ok(), "pinning requests the second IDR");

        assert!(!q.enqueue_classified(vec![0x20], false, false));
        assert!(q.enqueue_classified(vec![0xBB], true, true));
        for value in 0..CAPACITY * 3 {
            let accepted = q.enqueue_classified(vec![value as u8], false, false);
            if value < CAPACITY - 2 {
                assert!(accepted);
            }
        }
        assert!(q.requires_generation_recovery());
        assert!(q.enqueue_classified(vec![0xCC], true, true));
        assert!(q.enqueue_classified(vec![0xDD], false, false));
        assert!(tokio::time::timeout(Duration::from_millis(5), q.dequeue())
            .await
            .is_err());

        assert!(q.activate_generation());
        assert_eq!(q.dequeue().await, Some(vec![0xAA]));
        assert_eq!(q.dequeue().await, Some(vec![0xCC]));
        assert_eq!(q.dequeue().await, Some(vec![0xDD]));
    }

    #[tokio::test]
    async fn activation_before_followup_idr_suppresses_p_frames() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.begin_generation();
        assert!(q.pin_generation_recovery(vec![0xAA]));
        assert!(rx.try_recv().is_ok());
        assert!(q.activate_generation());

        assert!(!q.enqueue_classified(vec![0x10], false, false));
        assert_eq!(q.dequeue().await, Some(vec![0xAA]));
        assert!(q.enqueue_classified(vec![0xBB], true, true));
        assert!(q.enqueue_classified(vec![0x11], false, false));
        assert_eq!(q.dequeue().await, Some(vec![0xBB]));
        assert_eq!(q.dequeue().await, Some(vec![0x11]));
    }

    #[tokio::test]
    async fn activated_recovery_cannot_be_evicted_before_delivery() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.begin_generation();
        assert!(q.pin_generation_recovery(vec![0xAA]));
        assert!(rx.try_recv().is_ok());
        assert!(q.enqueue_classified(vec![0xBB], true, true));
        assert!(q.activate_generation());

        for value in 0..CAPACITY * 2 {
            q.enqueue_classified(vec![value as u8], false, false);
        }
        assert_eq!(q.dequeue().await, Some(vec![0xAA]));
        assert!(
            q.awaiting_keyframe(),
            "overflow behind the pinned AU requires a fresh recovery"
        );
        assert!(q.enqueue_classified(vec![0xCC], true, true));
        assert_eq!(q.dequeue().await, Some(vec![0xCC]));
    }

    #[tokio::test]
    async fn close_drains_then_ends() {
        let (idr, _rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.enqueue(vec![7], false);
        q.close();
        assert_eq!(q.dequeue().await.unwrap(), vec![7], "buffered frame drains");
        assert!(q.dequeue().await.is_none(), "then closed → None");
    }

    #[tokio::test]
    async fn close_and_clear_discards_buffered_frames_instead_of_draining() {
        let (idr, _rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.enqueue(vec![1], true);
        q.enqueue(vec![2], false);
        q.enqueue(vec![3], false);
        q.close_and_clear();
        // Unlike `close`, no buffered frame is ever handed to the writer —
        // this is the atomic multi-monitor teardown contract, not the
        // legacy single-queue "drain then end" contract exercised above.
        assert!(
            q.dequeue().await.is_none(),
            "close_and_clear must discard buffered frames, not drain them"
        );
    }

    #[tokio::test]
    async fn close_and_clear_also_discards_pinned_generation_recovery_state() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.begin_generation();
        assert!(q.pin_generation_recovery(vec![0xAA]));
        assert!(rx.try_recv().is_ok());
        assert!(
            q.activate_generation(),
            "unpauses; AA now sits in the deque"
        );
        q.close_and_clear();
        assert!(
            q.dequeue().await.is_none(),
            "an activated recovery frame must not survive close_and_clear either"
        );
    }
}
