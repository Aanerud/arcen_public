//! Carrier A (`multi_monitor_v1` "`muxed_reliable_stream`") fair scheduler.
//!
//! Carrier A keeps every monitor's encoded video on the session's one
//! existing reliable transport stream (the same WebSocket/QUIC connection
//! single-monitor sessions already use) instead of opening a separate
//! transport stream per monitor (Carrier B,
//! `MultiMonitorCarrierMsg::PerMonitorReliableStream`, which this Linux
//! tranche does not select — see `session::multi_monitor::OFFERED_CARRIERS`).
//!
//! [`MonitorMux`] owns one [`FrameQueue`] per applied monitor (each already
//! bounded and IDR-on-drop per `session::client`) and fairly interleaves
//! whichever queues currently have a frame ready, in rotating round-robin
//! order, so a bursty/full-motion monitor's queue can never starve a
//! sparser/idle monitor's queue: every call to [`MonitorMux::dequeue`] tries
//! every queue starting from the position right after whichever queue won
//! last time. Control and audio messages never flow through this mux at
//! all — `net::server::sender_loop`'s existing `biased` `select!` already
//! keeps them ahead of every video source, muxed or not.
//!
//! Every wire frame a monitor's queue holds is already tagged with that
//! monitor's `session_monitor_id` in its `VideoHeader.monitor_id` field
//! (`media::build_video_frame`), so this module performs no reframing: it
//! only decides delivery *order*, never rewrites payload bytes.
//!
//! The roster validation (non-empty, no duplicate monitor id, bounded by
//! [`arcen_media::MAX_MULTI_MONITOR_COUNT`]), the rotating round-robin
//! service order, and the atomic close-and-clear-all teardown are the exact
//! same policy `arcen_outputs::fairness` gives the Windows host's
//! `OutboundVideoMux` — this module is only the Linux-native queue
//! (`FrameQueue`) and `futures_util::select_all` wakeup model wrapped around
//! [`arcen_outputs::FairRoster`], never a second implementation of that
//! policy.
//!
//! Ending: whichever half of the session (`net::server`) first learns that
//! any one monitor's pipeline has ended — a pump crash, or a deliberate
//! whole-session shutdown — must call [`MonitorMux::close_and_clear_all`],
//! never a plain `FrameQueue::close` on just that one queue. That is the
//! atomic-teardown mechanism: it closes *and clears any buffered frames in*
//! every queue this mux routes, so every one of them reports "closed and
//! empty" at once and [`MonitorMux::dequeue`] returns `None` immediately —
//! not "eventually, once a sibling monitor's already-buffered frames have
//! drained and the round-robin happens to reach the closed queue's turn"
//! (see that method's doc comment for the exact ordering hazard this
//! avoids). This matches this tranche's atomic policy that a multi-monitor
//! session never continues serving a subset of its planned monitors, even
//! for one more already-encoded frame.

use std::sync::Arc;

use arcen_media::SessionMonitorId;
use arcen_outputs::FairRoster;

use super::client::FrameQueue;

/// Typed rejection constructing a [`MonitorMux`] — the exact same validated
/// roster rejection both hosts share; see [`arcen_outputs::fairness`].
pub type MonitorMuxError = arcen_outputs::RosterError<SessionMonitorId>;

/// Fairly multiplexes 1-4 per-monitor [`FrameQueue`]s onto one logical video
/// source for Carrier A.
pub struct MonitorMux {
    roster: FairRoster<SessionMonitorId, Arc<FrameQueue>>,
}

impl std::fmt::Debug for MonitorMux {
    // `FrameQueue` intentionally carries no `Debug` impl (its contents are
    // encoded video bytes), so only the routed monitor ids are shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorMux")
            .field("monitor_ids", &self.monitor_ids().collect::<Vec<_>>())
            .finish()
    }
}

impl MonitorMux {
    /// Builds a mux over `queues` (one entry per applied monitor, in any
    /// order — delivery fairness does not depend on roster order).
    ///
    /// # Errors
    ///
    /// Returns [`RosterError::Empty`](arcen_outputs::RosterError::Empty)
    /// when `queues` is empty,
    /// [`RosterError::Duplicate`](arcen_outputs::RosterError::Duplicate)
    /// when the same [`SessionMonitorId`] appears more than once, or
    /// [`RosterError::TooMany`](arcen_outputs::RosterError::TooMany) when
    /// `queues` holds more entries than
    /// [`arcen_media::MAX_MULTI_MONITOR_COUNT`].
    pub fn new(queues: Vec<(SessionMonitorId, Arc<FrameQueue>)>) -> Result<Self, MonitorMuxError> {
        let roster = FairRoster::new(queues)?;
        Ok(Self { roster })
    }

    /// Returns this monitor's queue, or `None` when `id` is not part of this
    /// mux (e.g. a stale id from a prior/rejected topology).
    #[must_use]
    pub fn queue_for(&self, id: SessionMonitorId) -> Option<&Arc<FrameQueue>> {
        self.roster.get(id)
    }

    /// Every session monitor id this mux currently routes, in construction
    /// order.
    pub fn monitor_ids(&self) -> impl Iterator<Item = SessionMonitorId> + '_ {
        self.roster.keys()
    }

    /// Awaits the next wire-encoded video frame in fair round-robin order.
    ///
    /// Returns `None` once any one monitor's queue closes (see the module
    /// documentation): this mux never continues delivering a subset of its
    /// monitors.
    pub async fn dequeue(&self) -> Option<Vec<u8>> {
        // Common, allocation-free single-monitor-plan path (still routed
        // through the mux so a one-monitor `multi_monitor_v1` session's wire
        // framing/tagging never differs from the 2-4 monitor case).
        if self.roster.len() == 1 {
            let (_, queue) = self.roster.entry(0).expect("non-empty roster");
            return queue.dequeue().await;
        }
        let futures = self
            .roster
            .entries_in_service_order()
            .map(|(index, _, queue)| {
                let queue = Arc::clone(queue);
                Box::pin(async move { (index, queue.dequeue().await) })
            });
        let ((winner_index, item), _ready_index, _rest) =
            futures_util::future::select_all(futures).await;
        self.roster.record_served(winner_index);
        item
    }

    /// Atomically tears down every monitor's queue: closes and discards any
    /// buffered frames in **every** queue this mux routes, not only the one
    /// whose pipeline actually ended.
    ///
    /// This is the fix for a real ordering hazard in [`Self::dequeue`]: it
    /// races every queue's own `dequeue` future starting from whichever
    /// queue is "up next" in the round-robin (the roster's own cursor), so a
    /// queue that closed via the plain [`FrameQueue::close`] can lose that
    /// race to a *different*, still-open sibling queue that happens to have
    /// a buffered frame ready — `select_all` returns the first future it
    /// finds `Ready`, in the roster's rotation order, not the one that
    /// closed. A caller relying on "the next `dequeue` will surely notice"
    /// could therefore keep emitting an unbounded number of a sibling
    /// monitor's buffered frames after another monitor's pipeline has
    /// already ended — violating the atomic whole-session teardown policy
    /// documented on this type.
    ///
    /// Calling this instead, the moment any one monitor's pump/pipeline
    /// ends, closes and clears every queue up front so there is nothing
    /// left to race: every queue reports `closed` with an empty deque
    /// simultaneously, so the very next (or an already in-flight)
    /// [`Self::dequeue`] call returns `None` regardless of which queue
    /// `select_all` happens to check first.
    ///
    /// Never used for the legacy single-monitor path — that queue keeps
    /// using [`FrameQueue::close`] directly, which still drains buffered
    /// frames before ending, unchanged.
    pub fn close_and_clear_all(&self) {
        self.roster
            .close_and_clear_all(|queue| queue.close_and_clear());
    }
}

/// The video delivery path `net::server::sender_loop` drains: either the
/// existing single-queue legacy path, or a Carrier A [`MonitorMux`].
///
/// Kept as a thin enum (rather than generalizing `sender_loop`'s video
/// parameter to a trait object) so the legacy single-monitor call site stays
/// a trivial `VideoSource::Single(queue)` wrap with no behavior change.
pub enum VideoSource {
    Single(Arc<FrameQueue>),
    Muxed(Arc<MonitorMux>),
}

impl VideoSource {
    /// Awaits the next wire-encoded video frame from whichever path is
    /// active.
    pub async fn dequeue(&self) -> Option<Vec<u8>> {
        match self {
            Self::Single(queue) => queue.dequeue().await,
            Self::Muxed(mux) => mux.dequeue().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::capenc::test_support::fake_idr;

    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("nonzero session monitor id")
    }

    fn queue() -> Arc<FrameQueue> {
        let (idr, _rx) = fake_idr();
        Arc::new(FrameQueue::new(idr))
    }

    fn tagged_frame(tag: u8) -> Vec<u8> {
        vec![tag]
    }

    #[test]
    fn new_rejects_an_empty_queue_set() {
        assert_eq!(
            MonitorMux::new(Vec::new()).unwrap_err(),
            MonitorMuxError::Empty
        );
    }

    #[test]
    fn new_rejects_a_duplicate_session_monitor_id() {
        let error = MonitorMux::new(vec![(sid(1), queue()), (sid(1), queue())]).unwrap_err();
        assert_eq!(error, MonitorMuxError::Duplicate(sid(1)));
    }

    #[test]
    fn new_rejects_more_monitors_than_the_shared_maximum() {
        // The exact same bound `arcen_outputs::FairRoster` enforces for
        // Windows' `OutboundVideoMux` — one shared validated-roster policy,
        // not a Linux-only cap.
        let count =
            u16::try_from(arcen_media::MAX_MULTI_MONITOR_COUNT + 1).expect("bounded monitor count");
        let error =
            MonitorMux::new((1..=count).map(|value| (sid(value), queue())).collect()).unwrap_err();
        assert_eq!(
            error,
            MonitorMuxError::TooMany {
                count: usize::from(count),
                limit: arcen_media::MAX_MULTI_MONITOR_COUNT,
            }
        );
    }

    #[test]
    fn queue_for_routes_known_ids_and_rejects_a_stale_id() {
        let mux = MonitorMux::new(vec![(sid(1), queue()), (sid(2), queue())]).unwrap();
        assert!(mux.queue_for(sid(1)).is_some());
        assert!(mux.queue_for(sid(2)).is_some());
        // sid(3) never appeared in this mux's roster: e.g. a stale id from a
        // prior/rejected topology must never route anywhere.
        assert!(mux.queue_for(sid(3)).is_none());
        assert_eq!(mux.monitor_ids().collect::<Vec<_>>(), vec![sid(1), sid(2)]);
    }

    #[tokio::test]
    async fn single_queue_mux_dequeues_directly_with_no_reordering() {
        let queue_a = queue();
        queue_a.enqueue(tagged_frame(1), true);
        queue_a.enqueue(tagged_frame(2), false);
        let mux = MonitorMux::new(vec![(sid(1), queue_a)]).unwrap();
        assert_eq!(mux.dequeue().await, Some(tagged_frame(1)));
        assert_eq!(mux.dequeue().await, Some(tagged_frame(2)));
    }

    #[tokio::test]
    async fn round_robins_fairly_when_every_queue_has_pending_frames() {
        let queue_a = queue();
        let queue_b = queue();
        for tag in 0..4u8 {
            queue_a.enqueue(tagged_frame(tag), tag == 0);
            queue_b.enqueue(tagged_frame(100 + tag), tag == 0);
        }
        let mux = MonitorMux::new(vec![
            (sid(1), Arc::clone(&queue_a)),
            (sid(2), Arc::clone(&queue_b)),
        ])
        .unwrap();
        let mut order = Vec::new();
        for _ in 0..8 {
            order.push(mux.dequeue().await.expect("frame")[0]);
        }
        // Both queues stay continuously non-empty across all 8 dequeues, so
        // strict alternation is the only fair outcome: neither monitor may
        // ever get two frames in a row while the other still has one queued.
        assert_eq!(order, vec![0, 100, 1, 101, 2, 102, 3, 103]);
    }

    #[tokio::test]
    async fn carrier_a_three_head_baseline_is_fair_and_clear_all_is_atomic() {
        let queue_a = queue();
        let queue_b = queue();
        let queue_c = queue();
        for (queue, first, second) in [(&queue_a, 10, 11), (&queue_b, 20, 21), (&queue_c, 30, 31)] {
            queue.enqueue(tagged_frame(first), true);
            queue.enqueue(tagged_frame(second), false);
        }
        let mux = MonitorMux::new(vec![
            (sid(1), Arc::clone(&queue_a)),
            (sid(2), Arc::clone(&queue_b)),
            (sid(3), Arc::clone(&queue_c)),
        ])
        .unwrap();
        let mut order = Vec::new();
        for _ in 0..6 {
            order.push(mux.dequeue().await.unwrap()[0]);
        }
        assert_eq!(order, [10, 20, 30, 11, 21, 31]);

        queue_a.enqueue(tagged_frame(40), true);
        queue_b.enqueue(tagged_frame(50), true);
        queue_c.enqueue(tagged_frame(60), true);
        mux.close_and_clear_all();
        assert_eq!(mux.dequeue().await, None);
        assert_eq!(queue_a.dequeue().await, None);
        assert_eq!(queue_b.dequeue().await, None);
        assert_eq!(queue_c.dequeue().await, None);
    }

    #[tokio::test]
    async fn a_bursty_monitor_never_starves_a_sparse_monitor() {
        let queue_a = queue();
        let queue_b = queue();
        // A bursts three frames; B has exactly one pending frame the whole
        // time. B's single frame must still be delivered promptly, not stuck
        // behind the entirety of A's burst.
        queue_a.enqueue(tagged_frame(1), true);
        queue_a.enqueue(tagged_frame(2), false);
        queue_a.enqueue(tagged_frame(3), false);
        queue_b.enqueue(tagged_frame(200), true);
        let mux = MonitorMux::new(vec![
            (sid(1), Arc::clone(&queue_a)),
            (sid(2), Arc::clone(&queue_b)),
        ])
        .unwrap();
        let mut order = Vec::new();
        for _ in 0..4 {
            order.push(mux.dequeue().await.expect("frame")[0]);
        }
        assert_eq!(order, vec![1, 200, 2, 3]);
        assert!(
            order.iter().position(|&tag| tag == 200).unwrap() < order.len() - 1,
            "the sparse monitor's frame must not be starved to the very end of the burst"
        );
    }

    #[tokio::test]
    async fn mux_ends_when_any_one_monitor_queue_closes() {
        let queue_a = queue();
        let queue_b = queue();
        queue_b.enqueue(tagged_frame(9), true);
        queue_a.close();
        let mux = MonitorMux::new(vec![(sid(1), queue_a), (sid(2), queue_b)]).unwrap();
        // Monitor 1's queue is already closed (its pipeline ended); the
        // atomic whole-session policy means the muxed source must end too,
        // even though monitor 2 still has a pending frame.
        assert_eq!(mux.dequeue().await, None);
    }

    #[tokio::test]
    async fn regression_plain_close_can_leak_a_buffered_sibling_frame_after_closure() {
        // This documents the exact ordering hazard `close_and_clear_all`
        // exists to close: a plain `FrameQueue::close()` on one monitor,
        // while a *different*, still-open sibling has a buffered frame
        // ready to go, can let that sibling's stale frame slip out through
        // the mux instead of the mux ending on the very next call —
        // `select_all` returns the first ready future in rotation order, not
        // necessarily the one that just closed.
        let queue_a = queue();
        let queue_b = queue();
        queue_a.enqueue(tagged_frame(1), true);
        let mux = MonitorMux::new(vec![
            (sid(1), Arc::clone(&queue_a)),
            (sid(2), Arc::clone(&queue_b)),
        ])
        .unwrap();
        // Drain monitor 1's frame so the round-robin rotates to check
        // monitor 2 first on the next call.
        assert_eq!(mux.dequeue().await, Some(tagged_frame(1)));
        // Monitor 2 still has a frame buffered right when monitor 1's
        // pipeline ends.
        queue_b.enqueue(tagged_frame(2), true);
        queue_a.close();
        assert_eq!(
            mux.dequeue().await,
            Some(tagged_frame(2)),
            "a plain close() alone does not guarantee the very next dequeue \
             ends the mux — this is why close_and_clear_all exists"
        );
    }

    #[tokio::test]
    async fn close_and_clear_all_prevents_a_buffered_sibling_frame_from_leaking() {
        // Same setup as the regression above, but using the atomic teardown
        // path instead of a plain close() on only the failed monitor.
        let queue_a = queue();
        let queue_b = queue();
        queue_a.enqueue(tagged_frame(1), true);
        let mux = MonitorMux::new(vec![
            (sid(1), Arc::clone(&queue_a)),
            (sid(2), Arc::clone(&queue_b)),
        ])
        .unwrap();
        assert_eq!(mux.dequeue().await, Some(tagged_frame(1)));
        queue_b.enqueue(tagged_frame(2), true);
        mux.close_and_clear_all();
        assert_eq!(
            mux.dequeue().await,
            None,
            "close_and_clear_all must end the mux immediately, discarding a \
             sibling's already-buffered frame rather than letting it leak"
        );
        // And it stays ended: no lingering buffered state anywhere.
        assert_eq!(mux.dequeue().await, None);
    }

    #[tokio::test]
    async fn close_and_clear_all_closes_and_empties_every_queue_in_a_three_head_topology() {
        let queue_a = queue();
        let queue_b = queue();
        let queue_c = queue();
        // Monitor 1's pipeline is about to fail; monitors 2 and 3 still have
        // frames buffered from before the failure is observed.
        queue_b.enqueue(tagged_frame(1), true);
        queue_c.enqueue(tagged_frame(2), true);
        let mux = MonitorMux::new(vec![
            (sid(1), Arc::clone(&queue_a)),
            (sid(2), Arc::clone(&queue_b)),
            (sid(3), Arc::clone(&queue_c)),
        ])
        .unwrap();
        mux.close_and_clear_all();
        assert_eq!(mux.dequeue().await, None);
        // Confirm every individual queue — not only the mux's own dequeue —
        // is closed and empty: this is whole-roster teardown, not merely
        // "the mux's next call happens to return None".
        assert!(queue_a.dequeue().await.is_none());
        assert!(queue_b.dequeue().await.is_none());
        assert!(queue_c.dequeue().await.is_none());
    }
}
