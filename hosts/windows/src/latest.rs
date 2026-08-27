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

struct VideoState<T> {
    items: VecDeque<T>,
    awaiting_keyframe: bool,
    closed: bool,
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

/// Platform-local video queue that never exposes a prediction chain after any
/// AU loss. Audio intentionally continues to use [`LatestQueue`].
pub struct VideoQueue<T> {
    capacity: usize,
    state: Mutex<VideoState<T>>,
    notify: Notify,
}

impl<T> VideoQueue<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            state: Mutex::new(VideoState {
                items: VecDeque::with_capacity(capacity),
                awaiting_keyframe: false,
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    pub fn push(&self, item: T, keyframe: bool) -> VideoPushResult<T> {
        let result = {
            let mut state = self.state.lock().expect("video queue lock poisoned");
            if state.closed {
                return VideoPushResult::Closed(item);
            }
            if keyframe {
                let cleared = state.items.len();
                state.items.clear();
                state.items.push_back(item);
                state.awaiting_keyframe = false;
                VideoPushResult::Enqueued { cleared }
            } else if state.awaiting_keyframe {
                VideoPushResult::Dropped {
                    count: 1,
                    recovery_started: false,
                }
            } else if state.items.len() == self.capacity {
                let count = state.items.len() + 1;
                state.items.clear();
                state.awaiting_keyframe = true;
                VideoPushResult::Dropped {
                    count,
                    recovery_started: true,
                }
            } else {
                state.items.push_back(item);
                VideoPushResult::Enqueued { cleared: 0 }
            }
        };
        if matches!(&result, VideoPushResult::Enqueued { .. }) {
            self.notify.notify_one();
        }
        result
    }

    pub async fn pop(&self) -> Option<T> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().expect("video queue lock poisoned");
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

    pub fn try_pop(&self) -> Option<T> {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .items
            .pop_front()
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().expect("video queue lock poisoned").closed
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .items
            .len()
    }

    pub fn clear(&self) -> usize {
        let mut state = self.state.lock().expect("video queue lock poisoned");
        let len = state.items.len();
        state.items.clear();
        len
    }

    pub fn close(&self) {
        {
            let mut state = self.state.lock().expect("video queue lock poisoned");
            state.closed = true;
        }
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
        {
            let mut state = self.state.lock().expect("video queue lock poisoned");
            state.closed = true;
            state.items.clear();
            state.awaiting_keyframe = false;
        }
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    pub fn awaiting_keyframe(&self) -> bool {
        self.state
            .lock()
            .expect("video queue lock poisoned")
            .awaiting_keyframe
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
    async fn video_loss_clears_descendants_and_suppresses_future_p_frames() {
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
            queue.push(3, false),
            VideoPushResult::Dropped {
                count: 3,
                recovery_started: true
            }
        ));
        assert!(queue.awaiting_keyframe());
        assert_eq!(queue.len(), 0);
        assert!(matches!(
            queue.push(4, false),
            VideoPushResult::Dropped {
                count: 1,
                recovery_started: false
            }
        ));
        assert_eq!(queue.len(), 0);
    }

    #[tokio::test]
    async fn keyframe_is_first_visible_item_and_resumes_prediction_chain() {
        let queue = VideoQueue::new(2);
        queue.push(1, false);
        queue.push(2, false);
        assert!(matches!(
            queue.push(9, true),
            VideoPushResult::Enqueued { cleared: 2 }
        ));
        assert!(!queue.awaiting_keyframe());
        assert_eq!(queue.pop().await, Some(9));
        assert!(matches!(
            queue.push(10, false),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        assert_eq!(queue.pop().await, Some(10));
    }

    #[tokio::test]
    async fn failed_keyframe_enqueue_does_not_leave_recovery() {
        let queue = VideoQueue::new(1);
        queue.push(1, false);
        queue.push(2, false);
        assert!(queue.awaiting_keyframe());
        queue.close();
        assert!(matches!(queue.push(9, true), VideoPushResult::Closed(9)));
        assert!(queue.awaiting_keyframe());
        assert_eq!(queue.pop().await, None);
    }
}
