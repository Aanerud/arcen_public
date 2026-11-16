// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The decoded-audio queue between a Deck's audio worker and its output
//! device callback.
//!
//! The rule is the macOS Deck's (`clients/macos/src/pipeline/audio.rs`,
//! `PlaybackQueue`), moved here so every Deck plays a Pier's audio with the
//! same latency and buffer policy: build the adaptive playout target
//! ([`super::playout`]) before playing, count an underrun once per rebuffer
//! cycle and raise the reserve when one happens, and trim a backlog towards
//! the target in bounded steps so bursty delivery keeps its reserve. The
//! device callback only pops samples; it never allocates or blocks on I/O.
//!
//! Interleaved 48 kHz stereo signed 16-bit samples (audio v1).

use std::collections::VecDeque;
use std::time::Duration;

use super::playout::{PlayoutPolicy, PlayoutTarget};
use super::{AUDIO_V1_CHANNELS, AUDIO_V1_SAMPLE_RATE_HZ};

const SAMPLE_RATE: usize = AUDIO_V1_SAMPLE_RATE_HZ as usize;
const CHANNELS: usize = AUDIO_V1_CHANNELS as usize;
/// How far above the playout target a trim brings the queue back to.
///
/// The target itself follows the path (see [`super::playout`]): a fixed
/// 70 ms, 110 ms after a rebuffer, underran nine times a minute on a WAN even
/// with a still desktop.
pub const TRIM_TO_ABOVE_TARGET_MS: usize = 30;
/// Hard bound: the largest adaptive target plus its trim headroom.
pub const MAX_BUFFER_MS: usize = 360;
/// Interleaved samples in one 20 ms audio-v1 frame.
pub const INTERLEAVED_FRAME_SAMPLES: usize = 1_920;
/// The most one push may trim, so a catch-up is never an audible jump.
pub const MAX_TRIM_SAMPLES_PER_PUSH: usize = INTERLEAVED_FRAME_SAMPLES * 3;
/// Samples played between calm updates to the playout target (100 ms).
const CALM_UPDATE_SAMPLES: usize = SAMPLE_RATE * CHANNELS / 10;

/// Interleaved samples in `ms` milliseconds of audio-v1.
#[must_use]
pub const fn ms_to_samples(ms: usize) -> usize {
    SAMPLE_RATE * CHANNELS * ms / 1000
}

/// The hard sample bound of a [`PlaybackQueue`].
#[must_use]
pub const fn max_samples() -> usize {
    ms_to_samples(MAX_BUFFER_MS)
}

/// A point-in-time reading of a [`PlaybackQueue`] for stream statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackSnapshot {
    /// Audio queued for playout.
    pub queued_ms: usize,
    /// Underruns since the stream started (one per rebuffer cycle).
    pub underruns: u64,
    /// Latency trims since the stream started.
    pub trim_events: u64,
    /// Samples removed by those trims.
    pub trimmed_samples: u64,
    /// Building the reserve before playing.
    pub prebuffering: bool,
    /// Rebuilding the reserve after an underrun.
    pub recovering: bool,
}

impl PlaybackSnapshot {
    /// The phase Stream Statistics shows: `playing`, `prebuffering` or
    /// `recovering`.
    #[must_use]
    pub const fn phase(&self) -> &'static str {
        if self.recovering {
            "recovering"
        } else if self.prebuffering {
            "prebuffering"
        } else {
            "playing"
        }
    }
}

/// Decoded audio waiting for the output device.
#[derive(Debug)]
pub struct PlaybackQueue {
    samples: VecDeque<i16>,
    prebuffering: bool,
    recovering: bool,
    underruns: u64,
    trim_events: u64,
    trimmed_samples: u64,
    playout: PlayoutTarget,
    played_since_calm_update: usize,
}

impl Default for PlaybackQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackQueue {
    /// An empty queue that prebuffers to the default playout target.
    #[must_use]
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(max_samples()),
            prebuffering: true,
            recovering: false,
            underruns: 0,
            trim_events: 0,
            trimmed_samples: 0,
            playout: PlayoutTarget::new(PlayoutPolicy::default()),
            played_since_calm_update: 0,
        }
    }

    /// Queues interleaved little-endian signed 16-bit PCM. Returns the bytes
    /// accepted; audio beyond [`max_samples`] is dropped.
    pub fn push_s16le(&mut self, payload: &[u8]) -> usize {
        self.trim_excess();
        let free = max_samples().saturating_sub(self.samples.len());
        let accept = free.min(payload.len() / 2);
        self.samples.extend(
            payload
                .chunks_exact(2)
                .take(accept)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
        );
        self.leave_prebuffer_when_full();
        accept * 2
    }

    /// Queues interleaved samples. Returns the bytes accepted.
    pub fn push_samples(&mut self, samples: &[i16]) -> usize {
        self.trim_excess();
        let accept = max_samples()
            .saturating_sub(self.samples.len())
            .min(samples.len());
        self.samples.extend(samples.iter().copied().take(accept));
        self.leave_prebuffer_when_full();
        accept * 2
    }

    /// The next sample for the device: silence while prebuffering. Emptying
    /// the queue counts one underrun and raises the reserve.
    pub fn pop_sample(&mut self) -> i16 {
        if self.prebuffering {
            return 0;
        }
        let sample = self.samples.pop_front().unwrap_or(0);
        if self.samples.is_empty() {
            self.prebuffering = true;
            self.recovering = true;
            self.underruns = self.underruns.saturating_add(1);
            // The reserve was too small for this path; hold more from now on.
            self.playout.record_underrun();
            self.played_since_calm_update = 0;
        } else {
            self.played_since_calm_update += 1;
            if self.played_since_calm_update >= CALM_UPDATE_SAMPLES {
                self.played_since_calm_update = 0;
                self.playout.record_calm(Duration::from_millis(100));
            }
        }
        sample
    }

    /// Fills `out` from the queue (the device callback).
    pub fn fill_i16(&mut self, out: &mut [i16]) {
        for sample in out {
            *sample = self.pop_sample();
        }
    }

    /// Fills `out` from the queue as normalised floats.
    pub fn fill_f32(&mut self, out: &mut [f32]) {
        for sample in out {
            *sample = f32::from(self.pop_sample()) / f32::from(i16::MAX);
        }
    }

    /// Fills `out` from the queue as offset-binary unsigned samples.
    pub fn fill_u16(&mut self, out: &mut [u16]) {
        for sample in out {
            let value = i32::from(self.pop_sample()) + 32_768;
            *sample = u16::try_from(value.clamp(0, i32::from(u16::MAX))).unwrap_or(u16::MAX);
        }
    }

    /// Drops everything queued and prebuffers again. Not an underrun.
    pub fn reset(&mut self) {
        self.samples.clear();
        self.prebuffering = true;
        self.recovering = false;
    }

    /// Statistics for Stream Statistics.
    #[must_use]
    pub fn snapshot(&self) -> PlaybackSnapshot {
        PlaybackSnapshot {
            queued_ms: self.samples.len() * 1000 / CHANNELS / SAMPLE_RATE,
            underruns: self.underruns,
            trim_events: self.trim_events,
            trimmed_samples: self.trimmed_samples,
            prebuffering: self.prebuffering,
            recovering: self.recovering,
        }
    }

    /// Interleaved samples queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// How much to build before playing: the adaptive target, which an
    /// underrun has already raised by the time recovery begins.
    #[must_use]
    pub fn prebuffer_target_samples(&self) -> usize {
        ms_to_samples(usize::from(self.playout.target_ms()))
    }

    /// The backlog above which a push trims.
    #[must_use]
    pub fn trim_threshold_samples(&self) -> usize {
        ms_to_samples(usize::from(self.playout.trim_threshold_ms()))
    }

    fn leave_prebuffer_when_full(&mut self) {
        if self.samples.len() >= self.prebuffer_target_samples() {
            self.prebuffering = false;
            self.recovering = false;
        }
    }

    /// Delivery is often bursty: collapsing a backlog straight back to the
    /// steady-state target can leave too little reserve for the next gap and
    /// create a trim/underrun oscillation. Trim toward the recovery reserve
    /// (not the steady-state floor) and bound each correction so catch-up is
    /// audible-safe even during packet bursts.
    fn trim_excess(&mut self) {
        if self.prebuffering || self.recovering {
            return;
        }
        if self.samples.len() > self.trim_threshold_samples() {
            let keep =
                ms_to_samples(usize::from(self.playout.target_ms()) + TRIM_TO_ABOVE_TARGET_MS);
            let trimmed = self
                .samples
                .len()
                .saturating_sub(keep)
                .min(MAX_TRIM_SAMPLES_PER_PUSH);
            self.samples.drain(0..trimmed);
            self.trim_events = self.trim_events.saturating_add(1);
            self.trimmed_samples = self
                .trimmed_samples
                .saturating_add(u64::try_from(trimmed).unwrap_or(u64::MAX));
        }
    }
}

/// Adapts the queue's 48 kHz stereo to a device that runs another rate or
/// channel count.
///
/// A shared-mode device plays its own mix format: macOS converts for the
/// Deck, Windows WASAPI does not, so a 44.1 kHz or 5.1 device would refuse
/// a 48 kHz stereo stream. Linear interpolation between stereo frames;
/// mono devices get the average, devices with more channels get left and
/// right in the first two and silence in the rest.
#[derive(Debug, Clone)]
pub struct DeviceFormatAdapter {
    channels: usize,
    step: f64,
    position: f64,
    current: [f32; 2],
    next: [f32; 2],
    primed: bool,
}

impl DeviceFormatAdapter {
    /// An adapter for a device at `rate_hz` with `channels`; `None` for a
    /// zero rate or channel count.
    #[must_use]
    pub fn new(rate_hz: u32, channels: u16) -> Option<Self> {
        (rate_hz > 0 && channels > 0).then(|| Self {
            channels: usize::from(channels),
            step: f64::from(AUDIO_V1_SAMPLE_RATE_HZ) / f64::from(rate_hz),
            position: 0.0,
            current: [0.0; 2],
            next: [0.0; 2],
            primed: false,
        })
    }

    /// Whether the device already plays audio-v1's format.
    #[must_use]
    pub fn is_passthrough(&self) -> bool {
        self.channels == CHANNELS && (self.step - 1.0).abs() < f64::EPSILON
    }

    fn pop_frame(queue: &mut PlaybackQueue) -> [f32; 2] {
        let scale = f32::from(i16::MAX);
        let left = f32::from(queue.pop_sample()) / scale;
        let right = f32::from(queue.pop_sample()) / scale;
        [left, right]
    }

    /// Fills interleaved device samples `out` from `queue`.
    pub fn fill_f32(&mut self, queue: &mut PlaybackQueue, out: &mut [f32]) {
        if self.is_passthrough() {
            queue.fill_f32(out);
            return;
        }
        if !self.primed {
            self.current = Self::pop_frame(queue);
            self.next = Self::pop_frame(queue);
            self.primed = true;
        }
        for frame in out.chunks_mut(self.channels) {
            #[allow(clippy::cast_possible_truncation, reason = "a fraction in 0..1")]
            let fraction = self.position as f32;
            let left = self.current[0] + (self.next[0] - self.current[0]) * fraction;
            let right = self.current[1] + (self.next[1] - self.current[1]) * fraction;
            match frame {
                [mono] => *mono = f32::midpoint(left, right),
                [first, second, rest @ ..] => {
                    *first = left;
                    *second = right;
                    rest.fill(0.0);
                }
                [] => {}
            }
            self.position += self.step;
            while self.position >= 1.0 {
                self.position -= 1.0;
                self.current = self.next;
                self.next = Self::pop_frame(queue);
            }
        }
    }

    /// Forgets the interpolation state (after a queue reset).
    pub fn reset(&mut self) {
        self.position = 0.0;
        self.primed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_with(samples: impl IntoIterator<Item = i16>, prebuffering: bool) -> PlaybackQueue {
        let mut queue = PlaybackQueue::new();
        queue.samples.extend(samples);
        queue.prebuffering = prebuffering;
        queue
    }

    fn target_samples(queue: &PlaybackQueue) -> usize {
        ms_to_samples(usize::from(queue.playout.target_ms()))
    }

    #[test]
    fn an_underrun_raises_the_reserve_for_the_next_rebuffer() {
        let mut queue = queue_with([1], false);
        let before = queue.prebuffer_target_samples();
        let _ = queue.pop_sample();
        assert_eq!(queue.underruns, 1);
        assert!(queue.prebuffer_target_samples() > before);
    }

    #[test]
    fn initial_prebuffering_and_explicit_reset_are_not_underruns() {
        let mut queue = queue_with([], true);
        assert_eq!(queue.pop_sample(), 0);
        assert_eq!(queue.underruns, 0);

        queue.samples.extend([1, 2]);
        queue.prebuffering = false;
        queue.reset();
        assert!(queue.prebuffering);
        assert!(queue.samples.is_empty());
        assert_eq!(queue.underruns, 0);
    }

    #[test]
    fn playback_depletion_counts_once_per_rebuffer_cycle() {
        let mut queue = queue_with([1, 2], false);
        assert_eq!(queue.pop_sample(), 1);
        assert_eq!(queue.underruns, 0);
        assert_eq!(queue.pop_sample(), 2);
        assert_eq!(queue.underruns, 1);
        assert_eq!(queue.pop_sample(), 0);
        assert_eq!(queue.underruns, 1);

        queue.samples.extend([3, 4]);
        queue.prebuffering = false;
        assert_eq!(queue.pop_sample(), 3);
        assert_eq!(queue.pop_sample(), 4);
        assert_eq!(queue.underruns, 2);
    }

    #[test]
    fn latency_trim_is_counted_without_counting_reset() {
        let probe = queue_with([], false);
        let original = probe.trim_threshold_samples() + 2;
        let mut queue = queue_with(std::iter::repeat_n(7, original), false);
        queue.trim_excess();

        let keep = target_samples(&queue) + ms_to_samples(TRIM_TO_ABOVE_TARGET_MS);
        let expected_trimmed = (original - keep).min(MAX_TRIM_SAMPLES_PER_PUSH);
        assert_eq!(queue.samples.len(), original - expected_trimmed);
        assert_eq!(queue.trim_events, 1);
        assert_eq!(queue.trimmed_samples, expected_trimmed as u64);

        queue.trim_excess();
        assert_eq!(queue.trim_events, 1);
        assert_eq!(queue.trimmed_samples, expected_trimmed as u64);
    }

    #[test]
    fn bursty_delivery_keeps_jitter_reserve_while_bounding_latency() {
        let probe = queue_with([], false);
        let mut queue = queue_with(std::iter::repeat_n(7, target_samples(&probe)), false);

        for _ in 0..8 {
            queue.trim_excess();
            queue
                .samples
                .extend(std::iter::repeat_n(7, INTERLEAVED_FRAME_SAMPLES));
        }

        assert!(queue.samples.len() >= queue.trim_threshold_samples() - MAX_TRIM_SAMPLES_PER_PUSH);
        assert!(queue.samples.len() <= queue.trim_threshold_samples() + INTERLEAVED_FRAME_SAMPLES);
        assert!(queue.trim_events > 0);
        assert!(queue.trimmed_samples >= queue.trim_events);
        assert!(queue.trimmed_samples <= (queue.trim_events * MAX_TRIM_SAMPLES_PER_PUSH as u64));

        for _ in 0..ms_to_samples(100) {
            let _ = queue.pop_sample();
        }
        assert_eq!(queue.underruns, 0);
    }

    #[test]
    fn trim_is_suppressed_until_recovery_rebuffer_is_rebuilt() {
        let mut queue = queue_with([], false);
        queue.recovering = true;
        queue.prebuffering = true;
        queue.samples.extend(std::iter::repeat_n(
            7,
            queue.trim_threshold_samples() + INTERLEAVED_FRAME_SAMPLES,
        ));
        let before = queue.samples.len();
        queue.trim_excess();
        assert_eq!(queue.samples.len(), before);
        assert_eq!(queue.trim_events, 0);
    }

    #[test]
    fn pushes_are_bounded_and_leave_prebuffering_at_the_target() {
        let mut queue = PlaybackQueue::new();
        let target = queue.prebuffer_target_samples();
        let below = vec![1_i16; target - 2];
        assert_eq!(queue.push_samples(&below), below.len() * 2);
        assert!(queue.snapshot().prebuffering);
        assert_eq!(queue.push_s16le(&[2, 0, 3, 0, 9]), 4);
        assert!(!queue.snapshot().prebuffering);
        assert_eq!(queue.snapshot().phase(), "playing");

        let huge = vec![0_i16; max_samples() * 2];
        let accepted = queue.push_samples(&huge) / 2;
        assert!(queue.len() <= max_samples());
        assert!(accepted < huge.len());
    }

    #[test]
    fn device_formats_carry_the_same_samples() {
        let mut queue = queue_with([i16::MAX, i16::MIN, 0], false);
        let mut floats = [0.0_f32; 1];
        queue.fill_f32(&mut floats);
        assert!((floats[0] - 1.0).abs() < f32::EPSILON);
        let mut unsigned = [0_u16; 1];
        queue.fill_u16(&mut unsigned);
        assert_eq!(unsigned[0], 0);
        let mut signed = [7_i16; 2];
        queue.fill_i16(&mut signed);
        assert_eq!(signed, [0, 0]);
        assert_eq!(queue.snapshot().phase(), "recovering");
    }

    #[test]
    fn a_device_at_another_rate_or_layout_gets_converted_audio() {
        let passthrough = DeviceFormatAdapter::new(48_000, 2).expect("adapter");
        assert!(passthrough.is_passthrough());
        assert!(DeviceFormatAdapter::new(0, 2).is_none());

        // 44.1 kHz stereo: about 441 device frames per 480 queued frames.
        let mut queue = queue_with(std::iter::repeat_n(16_384, 960 * 2), false);
        let mut adapter = DeviceFormatAdapter::new(44_100, 2).expect("adapter");
        let mut out = vec![0.0_f32; 441 * 2];
        adapter.fill_f32(&mut queue, &mut out);
        assert!(out.iter().all(|sample| (sample - 0.5).abs() < 0.01));
        let consumed = 960 * 2 - queue.len();
        assert!((958..=964).contains(&consumed), "{consumed}");

        // A 5.1 device: left and right in front, silence elsewhere.
        let mut queue = queue_with([i16::MAX, 0].repeat(64), false);
        let mut surround = DeviceFormatAdapter::new(48_000, 6).expect("adapter");
        let mut out = vec![9.0_f32; 6 * 4];
        surround.fill_f32(&mut queue, &mut out);
        assert!((out[0] - 1.0).abs() < 1e-3);
        assert!(out[1].abs() < 1e-3);
        assert!(out[2..6].iter().all(|sample| *sample == 0.0));

        // Mono: the average.
        let mut queue = queue_with([i16::MAX, 0].repeat(64), false);
        let mut mono = DeviceFormatAdapter::new(48_000, 1).expect("adapter");
        let mut out = vec![0.0_f32; 4];
        mono.fill_f32(&mut queue, &mut out);
        assert!((out[0] - 0.5).abs() < 1e-3);
    }
}
