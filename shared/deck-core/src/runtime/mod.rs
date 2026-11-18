// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck runtime (feature `runtime`): tokio and the QUIC adapter.
//!
//! Extraction steps 4-5 move the session engine (today
//! `clients/macos/src/transport/websocket.rs`), the lane workers
//! (`transport/lanes.rs`), the media worker, frame queue, monitor router and
//! audio lane here, behind [`crate::DeckServices`],
//! [`crate::decode::VideoDecoderBackend`] and
//! [`crate::present::VideoPresenterSink`]. Until then this module holds the
//! runtime halves of the seams: [`MicrophoneQueue`].

pub mod audio_lane;
pub mod audio_player;
pub mod audio_worker;
pub mod clipboard_controller;
pub mod clipboard_session;
pub mod connector;
pub mod decode_lag;
pub mod frame_queue;
pub mod frame_sync;
pub mod lanes;
pub mod media_worker;
pub mod monitor_router;
pub mod multi_monitor;
pub mod raw_hid;
pub mod session;
#[cfg(test)]
mod test_support;
pub mod tls;
pub mod usb;
pub mod usb_component;

/// Services for unit tests: nothing is available, every probe is empty.
#[cfg(test)]
pub(crate) fn test_services() -> std::sync::Arc<dyn crate::DeckServices> {
    std::sync::Arc::new(crate::services::test_support::UnavailableServices)
}

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;

use crate::microphone::{CapturedMicrophoneFrame, MicrophoneCaptureError, MicrophoneFrameSink};

/// Frames a [`MicrophoneQueue`] keeps before it drops the oldest.
pub const MICROPHONE_QUEUE_FRAMES: usize = 2;

struct QueueState {
    frames: VecDeque<CapturedMicrophoneFrame>,
    terminal: Option<Result<(), MicrophoneCaptureError>>,
}

/// The engine's [`MicrophoneFrameSink`]: a bounded drop-oldest queue the
/// platform capture thread pushes into and the session task awaits.
///
/// Latency beats completeness for live speech, so a full queue drops (and
/// wipes) its oldest frame rather than blocking capture. Cheap to clone.
#[derive(Clone)]
pub struct MicrophoneQueue {
    shared: Arc<QueueShared>,
}

struct QueueShared {
    state: Mutex<QueueState>,
    notify: Notify,
    captured_frames: AtomicU64,
    dropped_frames: AtomicU64,
}

impl std::fmt::Debug for MicrophoneQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MicrophoneQueue")
            .field("queued", &self.lock().frames.len())
            .finish_non_exhaustive()
    }
}

impl Default for MicrophoneQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrophoneQueue {
    /// An empty, open queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(QueueShared {
                state: Mutex::new(QueueState {
                    frames: VecDeque::with_capacity(MICROPHONE_QUEUE_FRAMES),
                    terminal: None,
                }),
                notify: Notify::new(),
                captured_frames: AtomicU64::new(0),
                dropped_frames: AtomicU64::new(0),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The next frame; `Ok(None)` once capture ended after a requested
    /// stop.
    ///
    /// # Errors
    ///
    /// Why capture ended, once every queued frame was taken (frames queued
    /// before an error are wiped instead).
    pub async fn recv(&self) -> Result<Option<CapturedMicrophoneFrame>, MicrophoneCaptureError> {
        loop {
            let notified = self.shared.notify.notified();
            {
                let mut state = self.lock();
                if let Some(frame) = state.frames.pop_front() {
                    return Ok(Some(frame));
                }
                if let Some(terminal) = state.terminal {
                    return terminal.map(|()| None);
                }
            }
            notified.await;
        }
    }

    /// Frames captured and frames dropped since the last call.
    #[must_use]
    pub fn take_capture_counters(&self) -> (u64, u64) {
        (
            self.shared.captured_frames.swap(0, Ordering::Relaxed),
            self.shared.dropped_frames.swap(0, Ordering::Relaxed),
        )
    }
}

impl MicrophoneFrameSink for MicrophoneQueue {
    fn push(&self, mut frame: CapturedMicrophoneFrame) {
        let mut state = self.lock();
        if state.terminal.is_some() {
            frame.zeroize();
            return;
        }
        self.shared.captured_frames.fetch_add(1, Ordering::Relaxed);
        if state.frames.len() == MICROPHONE_QUEUE_FRAMES
            && let Some(mut dropped) = state.frames.pop_front()
        {
            dropped.zeroize();
            self.shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
        }
        state.frames.push_back(frame);
        drop(state);
        self.shared.notify.notify_one();
    }

    fn finish(&self, result: Result<(), MicrophoneCaptureError>) {
        let mut state = self.lock();
        if result.is_err() {
            for frame in &mut state.frames {
                frame.zeroize();
            }
            state.frames.clear();
        }
        if state.terminal.is_none() {
            state.terminal = Some(result);
        }
        drop(state);
        self.shared.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::microphone::MICROPHONE_V1_FRAME_SAMPLES;

    fn frame(sequence: u32) -> CapturedMicrophoneFrame {
        CapturedMicrophoneFrame {
            samples: [1; MICROPHONE_V1_FRAME_SAMPLES],
            sequence,
            timestamp_ms: sequence * 20,
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn a_full_queue_drops_the_oldest_frame_and_counts_it() {
        let queue = MicrophoneQueue::new();
        for sequence in 0..3 {
            queue.push(frame(sequence));
        }
        assert_eq!(queue.take_capture_counters(), (3, 1));
        let first = block_on(queue.recv()).expect("open").expect("frame");
        assert_eq!(first.sequence, 1);
    }

    #[test]
    fn a_clean_stop_drains_then_ends_and_an_error_wipes_the_backlog() {
        let queue = MicrophoneQueue::new();
        queue.push(frame(0));
        queue.finish(Ok(()));
        queue.push(frame(1));
        assert_eq!(
            block_on(queue.recv()).expect("open").map(|f| f.sequence),
            Some(0)
        );
        assert_eq!(block_on(queue.recv()), Ok(None));

        let failed = MicrophoneQueue::new();
        failed.push(frame(0));
        failed.finish(Err(MicrophoneCaptureError::InputRemoved));
        assert_eq!(
            block_on(failed.recv()),
            Err(MicrophoneCaptureError::InputRemoved)
        );
    }
}
