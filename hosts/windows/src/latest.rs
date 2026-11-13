// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tokio::sync::Notify;

static VIDEO_BACKLOG_LEGACY: AtomicBool = AtomicBool::new(false);

/// Records the service's `ARCEN_VIDEO_BACKLOG=legacy` choice in a session
/// agent, which runs with the signed-in user's environment, not the
/// service's (`AgentStart::video_backlog_legacy`).
pub fn set_video_backlog_legacy(legacy: bool) {
    VIDEO_BACKLOG_LEGACY.store(legacy, Ordering::Relaxed);
}

/// Whether this process serves video the pre-budget way: the service's
/// environment, or the flag its broker handed this agent.
pub fn video_backlog_legacy() -> bool {
    VIDEO_BACKLOG_LEGACY.load(Ordering::Relaxed)
        || arcen_media::video::video_backlog_is_legacy_from_env()
}

struct State<T> {
    items: VecDeque<T>,
    closed: bool,
}

pub struct LatestQueue<T> {
    capacity: usize,
    state: Mutex<State<T>>,
    notify: Notify,
}

impl<T> LatestQueue<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            state: Mutex::new(State {
                items: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    /// Retain the new item and evict the oldest item when full.
    pub fn push(&self, item: T) -> Result<Option<T>, T> {
        let dropped = {
            let mut state = self.state.lock().expect("latest queue lock poisoned");
            if state.closed {
                return Err(item);
            }
            let dropped = if state.items.len() == self.capacity {
                state.items.pop_front()
            } else {
                None
            };
            state.items.push_back(item);
            dropped
        };
        self.notify.notify_one();
        Ok(dropped)
    }

    pub fn clear(&self) -> usize {
        let mut state = self.state.lock().expect("latest queue lock poisoned");
        let len = state.items.len();
        state.items.clear();
        len
    }

    pub async fn pop(&self) -> Option<T> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().expect("latest queue lock poisoned");
                if let Some(item) = state.items.pop_front() {
                    return Some(item);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    pub fn len(&self) -> usize {
        self.state
            .lock()
            .expect("latest queue lock poisoned")
            .items
            .len()
    }

    pub fn close(&self) {
        {
            let mut state = self.state.lock().expect("latest queue lock poisoned");
            state.closed = true;
        }
        self.notify.notify_waiters();
    }
}

pub enum VideoPushResult<T> {
    Enqueued {
        cleared: usize,
    },
    Dropped {
        count: usize,
        recovery_started: bool,
        idr_request: bool,
    },
    Closed(T),
}

impl<T> VideoPushResult<T> {
    pub const fn enqueued(&self) -> bool {
        matches!(self, Self::Enqueued { .. })
    }
}

/// Platform-local async wrapper around the shared video policy. Audio
/// intentionally continues to use [`LatestQueue`].
pub struct VideoQueue<T> {
    state: Mutex<arcen_media::video::SharedVideoQueue<T>>,
    notify: Notify,
    /// Signalled whenever the consumer takes, clears or closes, for
    /// [`Self::wait_for_room`].
    room: Notify,
}

impl<T> VideoQueue<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            state: Mutex::new(arcen_media::video::SharedVideoQueue::new(
                capacity,
                arcen_media::video::pipeline_contract(arcen_media::video::PipelineId::Auto)
                    .queue
                    .encoded_overflow
                    .keyframe_request_min_interval,
            )),
            notify: Notify::new(),
            room: Notify::new(),
        }
    }

    /// Bounds how long frames may wait for the consumer before
    /// [`Self::wait_for_room`] holds the producer back
    /// (`arcen_media::video::video_backlog_budget` of the contract rate).
    pub fn set_backlog_budget(&self, budget: Option<std::time::Duration>) {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .set_backlog_budget(budget);
    }

    /// Waits until an ordinary enqueue would neither overflow nor exceed the
    /// backlog budget, for at most `max_wait` when one is given. Returns at
    /// once when the queue is recovering, paused or closed: those states
    /// have their own rules. Returns whether there is room.
    pub async fn wait_for_room(&self, max_wait: Option<std::time::Duration>) -> bool {
        let deadline = max_wait.map(|wait| tokio::time::Instant::now() + wait);
        loop {
            let room = self.room.notified();
            match self
                .state
                .lock()
                .expect("video queue lock poisoned")
                .room_state()
            {
                arcen_media::video::RoomState::NotOrdinary => return false,
                arcen_media::video::RoomState::HasRoom => return true,
                arcen_media::video::RoomState::Full => {}
            }
            match deadline {
                Some(deadline) => {
                    if tokio::time::timeout_at(deadline, room).await.is_err() {
                        return false;
                    }
                }
                None => room.await,
            }
        }
    }

    pub fn push(&self, item: T, keyframe: bool) -> VideoPushResult<T> {
        let result = {
            let mut state = self.state.lock().expect("video queue lock poisoned");
            state.push(
                item,
                arcen_media::video::FrameClassification::keyframe_is_recovery(keyframe),
                std::time::Instant::now(),
            )
        };
        match result {
            arcen_media::video::VideoQueuePush::Enqueued { cleared } => {
                self.notify.notify_one();
                VideoPushResult::Enqueued { cleared }
            }
            arcen_media::video::VideoQueuePush::Dropped {
                count,
                recovery_started,
                idr_request,
            } => VideoPushResult::Dropped {
                count,
                recovery_started,
                idr_request,
            },
            arcen_media::video::VideoQueuePush::Closed(item) => VideoPushResult::Closed(item),
        }
    }

    pub fn note_keyframe_request_handoff(&self, success: bool) {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .note_keyframe_request_handoff(success, std::time::Instant::now());
    }

    pub fn keyframe_request_due(&self) -> bool {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .keyframe_request_retry(std::time::Instant::now())
            .due
    }

    pub async fn pop(&self) -> Option<T> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().expect("video queue lock poisoned");
                if let Some(item) = state.pop_front() {
                    drop(state);
                    self.room.notify_waiters();
                    return Some(item);
                }
                if state.is_closed() {
                    return None;
                }
            }
            notified.await;
        }
    }

    pub fn try_pop(&self) -> Option<T> {
        let item = self
            .state
            .lock()
            .expect("video queue lock poisoned")
            .pop_front();
        if item.is_some() {
            self.room.notify_waiters();
        }
        item
    }

    pub fn is_closed(&self) -> bool {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .is_closed()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state.lock().expect("video queue lock poisoned").len()
    }

    pub fn take_wait_stats(&self) -> arcen_media::video::VideoQueueWaitStats {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .take_wait_stats()
    }

    pub fn clear(&self) -> usize {
        let cleared = self
            .state
            .lock()
            .expect("video queue lock poisoned")
            .clear();
        self.room.notify_waiters();
        cleared
    }

    pub fn close(&self) {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .close();
        self.notify.notify_waiters();
        self.room.notify_waiters();
    }

    /// Close the queue **and discard any buffered items immediately**, so
    /// [`pop`](Self::pop) returns `None` right away rather than draining
    /// first.
    ///
    /// Used only by `OutboundVideoMux::close_and_clear_all`: a multi-monitor
    /// Carrier A session's atomic-teardown policy means that once *any* one
    /// monitor's pipeline ends, no monitor's queue may emit another video
    /// frame — including frames already buffered in a *different*,
    /// still-nominally-open sibling queue. Never call this for a queue that
    /// should keep draining; use [`close`](Self::close) there.
    pub fn close_and_clear(&self) {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .close_and_clear();
        self.notify.notify_waiters();
        self.room.notify_waiters();
    }

    #[cfg(test)]
    pub fn awaiting_keyframe(&self) -> bool {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .awaiting_keyframe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retains_latest_and_drops_oldest_at_capacity() {
        let queue = LatestQueue::new(2);
        assert_eq!(queue.push(1), Ok(None));
        assert_eq!(queue.push(2), Ok(None));
        assert_eq!(queue.push(3), Ok(Some(1)));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.pop().await, Some(2));
        assert_eq!(queue.pop().await, Some(3));
    }

    #[tokio::test]
    async fn close_drains_then_wakes_waiter() {
        let queue = LatestQueue::new(1);
        queue.push(7).unwrap();
        queue.close();
        assert_eq!(queue.pop().await, Some(7));
        assert_eq!(queue.pop().await, None);
    }

    #[tokio::test]
    async fn video_push_wakes_a_waiting_pop() {
        let queue = std::sync::Arc::new(VideoQueue::new(2));
        let waiter = {
            let queue = std::sync::Arc::clone(&queue);
            tokio::spawn(async move { queue.pop().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(matches!(
            queue.push(9, true),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        assert_eq!(waiter.await.unwrap(), Some(9));
    }

    #[tokio::test]
    async fn video_try_pop_uses_shared_policy_output() {
        let queue = VideoQueue::new(2);
        assert!(matches!(
            queue.push(1, false),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        assert!(matches!(
            queue.push(2, false),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        assert!(matches!(
            queue.push(9, true),
            VideoPushResult::Enqueued { cleared: 2 }
        ));
        assert_eq!(queue.try_pop(), Some(9));
    }

    #[tokio::test]
    async fn encoded_drop_exposes_rate_limited_idr_request() {
        let queue = VideoQueue::new(1);
        assert!(matches!(
            queue.push(1, false),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        assert!(matches!(
            queue.push(2, false),
            VideoPushResult::Dropped {
                recovery_started: true,
                idr_request: true,
                ..
            }
        ));
        queue.note_keyframe_request_handoff(true);
        assert!(matches!(
            queue.push(3, false),
            VideoPushResult::Dropped {
                recovery_started: false,
                idr_request: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn video_close_rejects_new_frames_and_wakes_waiter() {
        let queue = VideoQueue::new(1);
        queue.close();
        assert!(matches!(queue.push(9, true), VideoPushResult::Closed(9)));
        assert_eq!(queue.pop().await, None);
    }

    #[tokio::test]
    async fn a_full_budgeted_queue_holds_the_producer_until_the_consumer_takes() {
        let queue = std::sync::Arc::new(VideoQueue::new(8));
        queue.set_backlog_budget(Some(std::time::Duration::from_millis(20)));
        assert!(queue.push(1, true).enqueued());
        assert!(queue.push(2, false).enqueued());
        assert!(queue.wait_for_room(None).await, "fresh frames leave room");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            !queue
                .wait_for_room(Some(std::time::Duration::from_millis(10)))
                .await,
            "two frames older than the budget refuse room"
        );
        let waiter = {
            let queue = std::sync::Arc::clone(&queue);
            tokio::spawn(async move { queue.wait_for_room(None).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!waiter.is_finished());
        assert_eq!(queue.try_pop(), Some(1));
        assert!(waiter.await.unwrap(), "a take frees room");
    }

    #[tokio::test]
    async fn closing_releases_a_producer_waiting_for_room() {
        let queue = std::sync::Arc::new(VideoQueue::new(1));
        assert!(queue.push(1, false).enqueued());
        let waiter = {
            let queue = std::sync::Arc::clone(&queue);
            tokio::spawn(async move { queue.wait_for_room(None).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        queue.close();
        assert!(!waiter.await.unwrap());
    }

    #[tokio::test]
    async fn video_close_and_clear_wakes_without_draining() {
        let queue = VideoQueue::new(2);
        assert!(matches!(
            queue.push(1, true),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        queue.close_and_clear();
        assert_eq!(queue.pop().await, None);
    }
}
