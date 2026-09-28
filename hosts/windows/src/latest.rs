use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::Notify;

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
    },
    Closed(T),
}

/// Platform-local async wrapper around the shared video policy. Audio
/// intentionally continues to use [`LatestQueue`].
pub struct VideoQueue<T> {
    state: Mutex<arcen_media::video::SharedVideoQueue<T>>,
    notify: Notify,
}

impl<T> VideoQueue<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            state: Mutex::new(arcen_media::video::SharedVideoQueue::new(
                capacity,
                std::time::Duration::from_secs(1),
            )),
            notify: Notify::new(),
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
                ..
            } => VideoPushResult::Dropped {
                count,
                recovery_started,
            },
            arcen_media::video::VideoQueuePush::Closed(item) => VideoPushResult::Closed(item),
        }
    }

    pub async fn pop(&self) -> Option<T> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().expect("video queue lock poisoned");
                if let Some(item) = state.pop_front() {
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
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .pop_front()
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
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .clear()
    }

    pub fn close(&self) {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .close();
        self.notify.notify_waiters();
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
    async fn video_close_rejects_new_frames_and_wakes_waiter() {
        let queue = VideoQueue::new(1);
        queue.close();
        assert!(matches!(queue.push(9, true), VideoPushResult::Closed(9)));
        assert_eq!(queue.pop().await, None);
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
