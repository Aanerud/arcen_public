//! Host audio from its own lane to its own playout worker (ADR 0014).
//!
//! The audio-output lane reader pushes frames here and returns at once; the
//! audio worker thread drains them. Nothing here is shared with the video
//! inbox or decoder, so a slow video decode or a stalled video lane cannot
//! delay the next audio frame. The queue is bounded by audio policy (about
//! 320 ms of 20 ms frames): on overflow the oldest frame is dropped and
//! counted, and the jitter buffer conceals it.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use arcen_protocol::messages::AudioStreamResultMsg;

use crate::protocol::{decode_audio_header, AudioHeader, ProtocolError, AUDIO_HEADER_SIZE};

/// Frames the audio queue holds.
pub const AUDIO_LANE_FRAMES: usize = 16;
/// Bytes the audio queue holds.
pub const AUDIO_LANE_BYTES: usize = 128 * 1024;

/// One item for the audio worker.
#[derive(Debug)]
pub enum AudioLaneItem {
    /// A received audio frame.
    Frame {
        header: AudioHeader,
        payload: Vec<u8>,
        received_at: Instant,
    },
    /// The host's audio stream decision (codec, enabled, reason).
    StreamResult(AudioStreamResultMsg),
}

/// Counters of the audio queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioLaneCounters {
    pub received: u64,
    pub dropped_overflow: u64,
    pub malformed: u64,
    pub high_water: usize,
}

#[derive(Default)]
struct State {
    items: VecDeque<AudioLaneItem>,
    frames: usize,
    bytes: usize,
    closed: bool,
    counters: AudioLaneCounters,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Producer side: the audio lane reader and the session's control handling.
#[derive(Clone)]
pub struct AudioLaneSender {
    shared: Arc<Shared>,
}

/// Consumer side: the audio worker.
#[derive(Clone)]
pub struct AudioLaneReceiver {
    shared: Arc<Shared>,
}

/// A new, empty audio queue.
pub fn audio_lane_queue() -> (AudioLaneSender, AudioLaneReceiver) {
    let shared = Arc::new(Shared::default());
    (
        AudioLaneSender {
            shared: Arc::clone(&shared),
        },
        AudioLaneReceiver { shared },
    )
}

impl AudioLaneSender {
    /// Queues one audio frame (the bytes a host audio frame carries).
    ///
    /// # Errors
    ///
    /// The frame does not decode as an audio frame; it is counted.
    pub fn push_frame(&self, bytes: &[u8]) -> Result<(), ProtocolError> {
        let header = match decode_audio_header(bytes) {
            Ok(header) => header,
            Err(error) => {
                self.shared.lock().counters.malformed += 1;
                return Err(error);
            }
        };
        let payload = bytes.get(AUDIO_HEADER_SIZE..).unwrap_or_default().to_vec();
        let mut state = self.shared.lock();
        if state.closed {
            return Ok(());
        }
        state.counters.received += 1;
        state.frames += 1;
        state.bytes += payload.len();
        state.items.push_back(AudioLaneItem::Frame {
            header,
            payload,
            received_at: Instant::now(),
        });
        while state.frames > AUDIO_LANE_FRAMES || state.bytes > AUDIO_LANE_BYTES {
            let Some(index) = state
                .items
                .iter()
                .position(|item| matches!(item, AudioLaneItem::Frame { .. }))
            else {
                break;
            };
            if let Some(AudioLaneItem::Frame { payload, .. }) = state.items.remove(index) {
                state.frames -= 1;
                state.bytes -= payload.len();
                state.counters.dropped_overflow += 1;
            }
        }
        state.counters.high_water = state.counters.high_water.max(state.frames);
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }

    /// Queues the host's audio stream decision, in order with the frames.
    pub fn push_stream_result(&self, result: AudioStreamResultMsg) {
        let mut state = self.shared.lock();
        if state.closed {
            return;
        }
        state.items.push_back(AudioLaneItem::StreamResult(result));
        drop(state);
        self.shared.ready.notify_one();
    }

    /// Ends the queue; the worker drains and exits.
    pub fn close(&self) {
        self.shared.lock().closed = true;
        self.shared.ready.notify_all();
    }
}

impl AudioLaneReceiver {
    /// Takes everything queued, waiting up to `timeout` for the first item.
    /// `None` once the queue is closed and empty.
    pub fn recv_batch(&self, timeout: Duration) -> Option<Vec<AudioLaneItem>> {
        let mut state = self.shared.lock();
        if state.items.is_empty() && !state.closed {
            state = self
                .shared
                .ready
                .wait_timeout_while(state, timeout, |state| {
                    state.items.is_empty() && !state.closed
                })
                .map_or_else(|poisoned| poisoned.into_inner().0, |(state, _)| state);
        }
        if state.items.is_empty() && state.closed {
            return None;
        }
        state.frames = 0;
        state.bytes = 0;
        Some(state.items.drain(..).collect())
    }

    /// Counters so far.
    pub fn counters(&self) -> AudioLaneCounters {
        self.shared.lock().counters
    }

    /// Ends the queue from the consumer side.
    pub fn close(&self) {
        self.shared.lock().closed = true;
        self.shared.ready.notify_all();
    }

    /// A producer handle for the same queue.
    pub fn sender(&self) -> AudioLaneSender {
        AudioLaneSender {
            shared: Arc::clone(&self.shared),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{encode_audio_header, AudioCodec};

    fn frame(timestamp_ms: u32, payload: usize) -> Vec<u8> {
        let mut bytes = encode_audio_header(AudioHeader {
            codec: AudioCodec::Pcm,
            timestamp_ms,
        })
        .to_vec();
        bytes.extend(std::iter::repeat_n(1_u8, payload));
        bytes
    }

    #[test]
    fn overflow_drops_the_oldest_frame_and_keeps_stream_results() {
        let (sender, receiver) = audio_lane_queue();
        sender.push_stream_result(AudioStreamResultMsg::disabled(
            arcen_protocol::messages::AudioStreamReason::NotNegotiated,
        ));
        for timestamp in 0..(AUDIO_LANE_FRAMES as u32 + 3) {
            sender.push_frame(&frame(timestamp, 100)).unwrap();
        }
        let batch = receiver.recv_batch(Duration::from_millis(1)).unwrap();
        assert!(matches!(batch[0], AudioLaneItem::StreamResult(_)));
        let first_timestamp = batch.iter().find_map(|item| match item {
            AudioLaneItem::Frame { header, .. } => Some(header.timestamp_ms),
            AudioLaneItem::StreamResult(_) => None,
        });
        assert_eq!(first_timestamp, Some(3));
        assert_eq!(receiver.counters().dropped_overflow, 3);
    }

    #[test]
    fn a_closed_queue_ends_the_worker_after_draining() {
        let (sender, receiver) = audio_lane_queue();
        sender.push_frame(&frame(1, 10)).unwrap();
        sender.close();
        assert_eq!(
            receiver.recv_batch(Duration::from_millis(1)).unwrap().len(),
            1
        );
        assert!(receiver.recv_batch(Duration::from_millis(1)).is_none());
    }

    #[test]
    fn a_malformed_frame_is_counted_not_queued() {
        let (sender, receiver) = audio_lane_queue();
        assert!(sender.push_frame(&[0x10]).is_err());
        assert_eq!(receiver.counters().malformed, 1);
        assert!(receiver
            .recv_batch(Duration::from_millis(1))
            .unwrap()
            .is_empty());
    }
}
