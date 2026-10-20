//! Host audio playout on its own worker thread (ADR 0014).
//!
//! The audio worker owns the CoreAudio player (created on this thread, since
//! its stream is not `Send`) and drains only the audio-output lane's queue.
//! It never touches the video decoder, the video inbox or the shared media
//! state: it publishes a small status snapshot of its own, which the UI and
//! the media heartbeat copy. A stalled video decode or a blocked video lane
//! therefore cannot delay audio playout.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arcen_telemetry::WireDelayEstimator;

use crate::pipeline::audio::PcmAudioPlayer;
use crate::pipeline::audio_lane::{AudioLaneItem, AudioLaneReceiver};

const STATUS_SAMPLES: usize = 120;

/// What the audio worker publishes for the UI and the heartbeat.
#[derive(Debug, Clone, Default)]
pub struct AudioPlaybackStatus {
    pub frames_seen: u64,
    pub bytes_accepted: u64,
    pub batch_high_water: usize,
    pub queued_ms: usize,
    pub playback_underruns: u64,
    pub buffer_trim_events: u64,
    pub buffer_trimmed_samples: u64,
    pub feed_gap_ms: u64,
    pub max_feed_gap_ms: u64,
    pub decode_failures: u64,
    pub concealed_frames: u64,
    pub queue_phase: String,
    pub summary: String,
    pub queue_overflow_drops: u64,
    pub malformed_frames: u64,
    pub queue_high_water: usize,
    pub wire_clock_age_samples: VecDeque<f64>,
    pub wire_delay_samples: VecDeque<f64>,
}

/// The audio worker's status cell. Its lock is held only to copy the small
/// snapshot, never across playout.
pub type AudioPlaybackCell = Arc<Mutex<AudioPlaybackStatus>>;

/// Copies the current snapshot.
pub fn audio_playback_snapshot(cell: &AudioPlaybackCell) -> AudioPlaybackStatus {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn push_sample(samples: &mut VecDeque<f64>, value: f64) {
    samples.push_back(value);
    while samples.len() > STATUS_SAMPLES {
        samples.pop_front();
    }
}

fn wire_clock_age_ms(timestamp_ms: u32) -> i32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;
    now.wrapping_sub(timestamp_ms) as i32
}

/// Starts the audio worker for one session. It ends when the queue closes.
pub fn spawn_audio_worker(
    queue: AudioLaneReceiver,
    status: AudioPlaybackCell,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("audio-worker".to_string())
        .spawn(move || {
            let mut player = PcmAudioPlayer::awaiting_stream_result();
            let mut wire_delay = WireDelayEstimator::default();
            tracing::info!(target: crate::logging::target::AUDIO, "audio worker started");
            while let Some(batch) = queue.recv_batch(Duration::from_millis(250)) {
                let frames = batch
                    .iter()
                    .filter(|item| matches!(item, AudioLaneItem::Frame { .. }))
                    .count();
                for item in batch {
                    match item {
                        AudioLaneItem::StreamResult(result) => player.set_stream_result(&result),
                        AudioLaneItem::Frame {
                            header, payload, ..
                        } => {
                            let clock_age = wire_clock_age_ms(header.timestamp_ms);
                            let feed = player.feed(header, &payload);
                            let delay = wire_delay.observe(clock_age);
                            let counters = queue.counters();
                            let mut state = status
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let previous_underruns = state.playback_underruns;
                            state.frames_seen = counters.received;
                            state.queue_overflow_drops = counters.dropped_overflow;
                            state.malformed_frames = counters.malformed;
                            state.queue_high_water = counters.high_water;
                            state.batch_high_water = state.batch_high_water.max(frames);
                            state.bytes_accepted += feed.accepted_bytes as u64;
                            state.queued_ms = feed.queued_ms;
                            state.playback_underruns = feed.playback_underruns;
                            state.buffer_trim_events = feed.buffer_trim_events;
                            state.buffer_trimmed_samples = feed.buffer_trimmed_samples;
                            state.feed_gap_ms = feed.feed_gap_ms;
                            state.max_feed_gap_ms = feed.max_feed_gap_ms;
                            state.decode_failures = feed.decode_failures;
                            state.concealed_frames = feed.concealed_frames;
                            state.queue_phase = feed.queue_phase.to_string();
                            push_sample(&mut state.wire_clock_age_samples, f64::from(clock_age));
                            push_sample(&mut state.wire_delay_samples, f64::from(delay));
                            state.summary = format!(
                                "Audio {} queued={} ms accepted={} bytes underruns={} trims={} gap={} ms phase={}{}",
                                feed.backend,
                                feed.queued_ms,
                                feed.accepted_bytes,
                                feed.playback_underruns,
                                feed.buffer_trim_events,
                                feed.feed_gap_ms,
                                feed.queue_phase,
                                feed.note
                                    .as_deref()
                                    .map(|note| format!(" · {note}"))
                                    .unwrap_or_default(),
                            );
                            drop(state);
                            if feed.playback_underruns > previous_underruns {
                                tracing::warn!(
                                    target: crate::logging::target::AUDIO,
                                    playback_underruns = feed.playback_underruns,
                                    queued_ms = feed.queued_ms,
                                    feed_gap_ms = feed.feed_gap_ms,
                                    feed_gap_max_ms = feed.max_feed_gap_ms,
                                    decode_failures = feed.decode_failures,
                                    concealed_frames = feed.concealed_frames,
                                    "CoreAudio playback queue underrun; rebuffering"
                                );
                            }
                        }
                    }
                }
            }
            tracing::info!(target: crate::logging::target::AUDIO, "audio worker ended");
        })
}
