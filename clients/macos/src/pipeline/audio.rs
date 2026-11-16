// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host audio output on the macOS Deck: cpal on CoreAudio and the adaptive
//! playback queue its callback drains. The player logic (negotiation, Opus,
//! jitter, concealment) is [`arcen_deck_core::runtime::audio_player`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub use arcen_deck_core::runtime::audio_player::PlaybackSnapshot;
use arcen_deck_core::runtime::audio_player::{AudioOutputDevice, INTERLEAVED_FRAME_SAMPLES};
pub use arcen_deck_core::runtime::audio_worker::AudioFeedStatus;

/// The macOS Deck's audio player: the shared player on cpal/CoreAudio.
pub type PcmAudioPlayer = arcen_deck_core::runtime::audio_player::PcmAudioPlayer<CpalOutput>;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BYTES_PER_SAMPLE: usize = 2;
/// How far above the playout target a trim brings the queue back to.
///
/// The target itself now follows the path (see
/// `arcen_media::audio::playout`): a fixed 70 ms, 110 ms after a rebuffer,
/// underran nine times a minute on a WAN even with a still desktop.
const TRIM_TO_ABOVE_TARGET_MS: usize = 30;
/// Hard bound: the largest adaptive target plus its trim headroom.
const MAX_BUFFER_MS: usize = 360;
/// Samples played between calm updates to the playout target (100 ms).
const CALM_UPDATE_SAMPLES: usize = SAMPLE_RATE as usize * CHANNELS as usize / 10;
const MAX_TRIM_SAMPLES_PER_PUSH: usize = INTERLEAVED_FRAME_SAMPLES * 3;

/// The default CoreAudio output through cpal, with the Deck's adaptive
/// playback queue.
pub struct CpalOutput {
    _stream: cpal::Stream,
    queue: Arc<Mutex<PlaybackQueue>>,
}

struct PlaybackQueue {
    samples: VecDeque<i16>,
    prebuffering: bool,
    recovering: bool,
    underruns: u64,
    trim_events: u64,
    trimmed_samples: u64,
    playout: arcen_media::audio::playout::PlayoutTarget,
    played_since_calm_update: usize,
}

impl AudioOutputDevice for CpalOutput {
    fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "No default audio output device".to_string())?;
        let supported = device
            .default_output_config()
            .map_err(|error| format!("Could not read default output config: {error}"))?;
        let sample_format = supported.sample_format();
        let config = cpal::StreamConfig {
            channels: CHANNELS,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let queue = Arc::new(Mutex::new(PlaybackQueue {
            samples: VecDeque::with_capacity(max_samples()),
            prebuffering: true,
            recovering: false,
            underruns: 0,
            trim_events: 0,
            trimmed_samples: 0,
            playout: arcen_media::audio::playout::PlayoutTarget::new(
                arcen_media::audio::playout::PlayoutPolicy::default(),
            ),
            played_since_calm_update: 0,
        }));
        let err_fn = |error| {
            tracing::warn!(target: crate::logging::target::AUDIO, %error, "audio stream error");
        };
        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let queue = Arc::clone(&queue);
                device.build_output_stream(
                    &config,
                    move |data: &mut [f32], _| write_f32(data, &queue),
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let queue = Arc::clone(&queue);
                device.build_output_stream(
                    &config,
                    move |data: &mut [i16], _| write_i16(data, &queue),
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let queue = Arc::clone(&queue);
                device.build_output_stream(
                    &config,
                    move |data: &mut [u16], _| write_u16(data, &queue),
                    err_fn,
                    None,
                )
            }
            other => return Err(format!("Unsupported output sample format: {other:?}")),
        }
        .map_err(|error| {
            format!("Could not open 48 kHz stereo output stream on default device: {error}")
        })?;
        stream
            .play()
            .map_err(|error| format!("Could not start audio output stream: {error}"))?;

        Ok(Self {
            _stream: stream,
            queue,
        })
    }

    fn backend(&self) -> &'static str {
        "cpal-coreaudio-48k-stereo"
    }

    fn push_s16le(&self, payload: &[u8]) -> usize {
        let mut queue = self.queue.lock().expect("audio queue poisoned");
        trim_excess(&mut queue);
        let max = max_samples();
        let free = max.saturating_sub(queue.samples.len());
        let samples_to_accept = free.min(payload.len() / BYTES_PER_SAMPLE);
        for chunk in payload
            .chunks_exact(BYTES_PER_SAMPLE)
            .take(samples_to_accept)
        {
            queue
                .samples
                .push_back(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        if queue.samples.len() >= prebuffer_target_samples(&queue) {
            queue.prebuffering = false;
            queue.recovering = false;
        }
        samples_to_accept * BYTES_PER_SAMPLE
    }

    fn push_samples(&self, samples: &[i16]) -> usize {
        let mut queue = self.queue.lock().expect("audio queue poisoned");
        trim_excess(&mut queue);
        let samples_to_accept = max_samples()
            .saturating_sub(queue.samples.len())
            .min(samples.len());
        queue
            .samples
            .extend(samples.iter().copied().take(samples_to_accept));
        if queue.samples.len() >= prebuffer_target_samples(&queue) {
            queue.prebuffering = false;
            queue.recovering = false;
        }
        samples_to_accept * BYTES_PER_SAMPLE
    }

    fn reset(&self) {
        let mut queue = self.queue.lock().expect("audio queue poisoned");
        reset_playback_queue(&mut queue);
    }

    fn snapshot(&self) -> PlaybackSnapshot {
        let queue = self.queue.lock().expect("audio queue poisoned");
        PlaybackSnapshot {
            queued_ms: queue.samples.len() * 1000 / usize::from(CHANNELS) / SAMPLE_RATE as usize,
            underruns: queue.underruns,
            trim_events: queue.trim_events,
            trimmed_samples: queue.trimmed_samples,
            prebuffering: queue.prebuffering,
            recovering: queue.recovering,
        }
    }
}

fn pop_sample(queue: &mut PlaybackQueue) -> i16 {
    if queue.prebuffering {
        return 0;
    }
    let sample = queue.samples.pop_front().unwrap_or(0);
    if queue.samples.is_empty() {
        queue.prebuffering = true;
        queue.recovering = true;
        queue.underruns = queue.underruns.saturating_add(1);
        // The reserve was too small for this path; hold more from now on.
        queue.playout.record_underrun();
        queue.played_since_calm_update = 0;
    } else {
        queue.played_since_calm_update += 1;
        if queue.played_since_calm_update >= CALM_UPDATE_SAMPLES {
            queue.played_since_calm_update = 0;
            queue
                .playout
                .record_calm(std::time::Duration::from_millis(100));
        }
    }
    sample
}

fn trim_excess(queue: &mut PlaybackQueue) {
    if queue.prebuffering || queue.recovering {
        return;
    }
    // Delivery is often bursty: collapsing backlog straight back to the
    // steady-state target can leave too little reserve for the following gap
    // and create a trim/underrun oscillation. Trim toward the recovery reserve
    // (not the steady-state floor) and bound each correction so catch-up is
    // audible-safe even during packet bursts.
    if queue.samples.len() > trim_threshold_samples(queue) {
        let keep = ms_to_samples(usize::from(queue.playout.target_ms()) + TRIM_TO_ABOVE_TARGET_MS);
        let trimmed = queue
            .samples
            .len()
            .saturating_sub(keep)
            .min(MAX_TRIM_SAMPLES_PER_PUSH);
        queue.samples.drain(0..trimmed);
        queue.trim_events = queue.trim_events.saturating_add(1);
        queue.trimmed_samples = queue
            .trimmed_samples
            .saturating_add(u64::try_from(trimmed).unwrap_or(u64::MAX));
    }
}

fn reset_playback_queue(queue: &mut PlaybackQueue) {
    queue.samples.clear();
    queue.prebuffering = true;
    queue.recovering = false;
}

fn write_f32(data: &mut [f32], queue: &Arc<Mutex<PlaybackQueue>>) {
    let mut queue = queue.lock().expect("audio queue poisoned");
    for sample in data {
        *sample = f32::from(pop_sample(&mut queue)) / f32::from(i16::MAX);
    }
}

fn write_i16(data: &mut [i16], queue: &Arc<Mutex<PlaybackQueue>>) {
    let mut queue = queue.lock().expect("audio queue poisoned");
    for sample in data {
        *sample = pop_sample(&mut queue);
    }
}

fn write_u16(data: &mut [u16], queue: &Arc<Mutex<PlaybackQueue>>) {
    let mut queue = queue.lock().expect("audio queue poisoned");
    for sample in data {
        let value = i32::from(pop_sample(&mut queue)) + 32_768;
        *sample = value.clamp(0, u16::MAX as i32) as u16;
    }
}

const fn max_samples() -> usize {
    SAMPLE_RATE as usize * CHANNELS as usize * MAX_BUFFER_MS / 1000
}

const fn ms_to_samples(ms: usize) -> usize {
    SAMPLE_RATE as usize * CHANNELS as usize * ms / 1000
}

/// How much to build before playing: the adaptive target, which an underrun
/// has already raised by the time recovery begins.
fn prebuffer_target_samples(queue: &PlaybackQueue) -> usize {
    ms_to_samples(usize::from(queue.playout.target_ms()))
}

fn trim_threshold_samples(queue: &PlaybackQueue) -> usize {
    ms_to_samples(usize::from(queue.playout.trim_threshold_ms()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playback_queue(samples: impl IntoIterator<Item = i16>, prebuffering: bool) -> PlaybackQueue {
        PlaybackQueue {
            samples: samples.into_iter().collect(),
            prebuffering,
            recovering: false,
            underruns: 0,
            trim_events: 0,
            trimmed_samples: 0,
            playout: arcen_media::audio::playout::PlayoutTarget::new(
                arcen_media::audio::playout::PlayoutPolicy::default(),
            ),
            played_since_calm_update: 0,
        }
    }

    fn target_samples(queue: &PlaybackQueue) -> usize {
        ms_to_samples(usize::from(queue.playout.target_ms()))
    }

    #[test]
    fn an_underrun_raises_the_reserve_for_the_next_rebuffer() {
        let mut queue = playback_queue([1], false);
        let before = prebuffer_target_samples(&queue);
        let _ = pop_sample(&mut queue);
        assert_eq!(queue.underruns, 1);
        assert!(prebuffer_target_samples(&queue) > before);
    }

    #[test]
    fn initial_prebuffering_and_explicit_reset_are_not_underruns() {
        let mut queue = playback_queue([], true);
        assert_eq!(pop_sample(&mut queue), 0);
        assert_eq!(queue.underruns, 0);

        queue.samples.extend([1, 2]);
        queue.prebuffering = false;
        reset_playback_queue(&mut queue);
        assert!(queue.prebuffering);
        assert!(queue.samples.is_empty());
        assert_eq!(queue.underruns, 0);
    }

    #[test]
    fn playback_depletion_counts_once_per_rebuffer_cycle() {
        let mut queue = playback_queue([1, 2], false);
        assert_eq!(pop_sample(&mut queue), 1);
        assert_eq!(queue.underruns, 0);
        assert_eq!(pop_sample(&mut queue), 2);
        assert_eq!(queue.underruns, 1);
        assert_eq!(pop_sample(&mut queue), 0);
        assert_eq!(queue.underruns, 1);

        queue.samples.extend([3, 4]);
        queue.prebuffering = false;
        assert_eq!(pop_sample(&mut queue), 3);
        assert_eq!(pop_sample(&mut queue), 4);
        assert_eq!(queue.underruns, 2);
    }

    #[test]
    fn latency_trim_is_counted_without_counting_reset() {
        let probe = playback_queue([], false);
        let original = trim_threshold_samples(&probe) + 2;
        let mut queue = playback_queue(std::iter::repeat_n(7, original), false);
        trim_excess(&mut queue);

        let keep = target_samples(&queue) + ms_to_samples(TRIM_TO_ABOVE_TARGET_MS);
        let expected_trimmed = (original - keep).min(MAX_TRIM_SAMPLES_PER_PUSH);
        assert_eq!(queue.samples.len(), original - expected_trimmed);
        assert_eq!(queue.trim_events, 1);
        assert_eq!(queue.trimmed_samples, expected_trimmed as u64);

        trim_excess(&mut queue);
        assert_eq!(queue.trim_events, 1);
        assert_eq!(queue.trimmed_samples, expected_trimmed as u64);
    }

    #[test]
    fn bursty_delivery_keeps_jitter_reserve_while_bounding_latency() {
        let probe = playback_queue([], false);
        let mut queue = playback_queue(std::iter::repeat_n(7, target_samples(&probe)), false);

        for _ in 0..8 {
            trim_excess(&mut queue);
            queue
                .samples
                .extend(std::iter::repeat_n(7, INTERLEAVED_FRAME_SAMPLES));
        }

        assert!(queue.samples.len() >= trim_threshold_samples(&queue) - MAX_TRIM_SAMPLES_PER_PUSH);
        assert!(queue.samples.len() <= trim_threshold_samples(&queue) + INTERLEAVED_FRAME_SAMPLES);
        assert!(queue.trim_events > 0);
        assert!(queue.trimmed_samples >= queue.trim_events);
        assert!(queue.trimmed_samples <= (queue.trim_events * MAX_TRIM_SAMPLES_PER_PUSH as u64));

        let hundred_ms = SAMPLE_RATE as usize * CHANNELS as usize / 10;
        for _ in 0..hundred_ms {
            let _ = pop_sample(&mut queue);
        }
        assert_eq!(queue.underruns, 0);
    }

    #[test]
    fn trim_is_suppressed_until_recovery_rebuffer_is_rebuilt() {
        let mut queue = playback_queue([], false);
        queue.recovering = true;
        queue.prebuffering = true;
        queue.samples.extend(std::iter::repeat_n(
            7,
            trim_threshold_samples(&queue) + INTERLEAVED_FRAME_SAMPLES,
        ));
        let before = queue.samples.len();
        trim_excess(&mut queue);
        assert_eq!(queue.samples.len(), before);
        assert_eq!(queue.trim_events, 0);
    }
}
