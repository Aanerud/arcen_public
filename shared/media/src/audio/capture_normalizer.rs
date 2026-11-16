// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Turns a Deck's microphone callbacks into microphone-v1 frames.
//!
//! The rule is the macOS Deck's (`clients/macos/src/microphone.rs`,
//! `CaptureNormalizer`), moved here so every Deck sends the same frames:
//! channels are averaged to mono, the device rate is brought to 48 kHz by a
//! phase accumulator, samples are scaled by `i16::MAX` after clamping, and
//! every [`MICROPHONE_V1_FRAME_SAMPLES`] samples leave as one frame with a
//! sequence that starts at 1 and skips 0 when it wraps, and a timestamp that
//! advances 20 ms per frame.
//!
//! Samples may be speech: the partial frame and every emitted frame are
//! wiped when dropped.

use std::ops::ControlFlow;

use zeroize::Zeroize;

use super::MICROPHONE_V1_FRAME_SAMPLES;

const OUTPUT_RATE_HZ: u64 = 48_000;
const FRAME_DURATION_MS: u32 = 20;

/// One normalised microphone-v1 frame: 48 kHz mono signed 16-bit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedMicrophoneFrame {
    /// The samples.
    pub samples: [i16; MICROPHONE_V1_FRAME_SAMPLES],
    /// Capture order: 1, 2, … wrapping past `u32::MAX` back to 1.
    pub sequence: u32,
    /// Milliseconds of audio before this frame, wrapping.
    pub timestamp_ms: u32,
}

impl NormalizedMicrophoneFrame {
    /// Moves the samples out, leaving zeros behind (the frame still wipes
    /// itself when dropped).
    #[must_use]
    pub fn take_samples(&mut self) -> [i16; MICROPHONE_V1_FRAME_SAMPLES] {
        std::mem::replace(&mut self.samples, [0; MICROPHONE_V1_FRAME_SAMPLES])
    }
}

impl Drop for NormalizedMicrophoneFrame {
    fn drop(&mut self) {
        self.samples.zeroize();
    }
}

/// Normalises one capture's callbacks into frames.
pub struct MicrophoneCaptureNormalizer {
    source_rate_hz: u32,
    phase: u64,
    frame: [i16; MICROPHONE_V1_FRAME_SAMPLES],
    frame_len: usize,
    sequence: u32,
    timestamp_ms: u32,
}

impl std::fmt::Debug for MicrophoneCaptureNormalizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MicrophoneCaptureNormalizer")
            .field("source_rate_hz", &self.source_rate_hz)
            .field("frame_len", &self.frame_len)
            .field("sequence", &self.sequence)
            .finish_non_exhaustive()
    }
}

impl Drop for MicrophoneCaptureNormalizer {
    fn drop(&mut self) {
        self.clear();
    }
}

impl MicrophoneCaptureNormalizer {
    /// A normaliser for a device delivering `source_rate_hz`; `None` for 0.
    #[must_use]
    pub fn new(source_rate_hz: u32) -> Option<Self> {
        (source_rate_hz > 0).then_some(Self {
            source_rate_hz,
            phase: 0,
            frame: [0; MICROPHONE_V1_FRAME_SAMPLES],
            frame_len: 0,
            sequence: 1,
            timestamp_ms: 0,
        })
    }

    /// The device rate this normaliser was built for.
    #[must_use]
    pub const fn source_rate_hz(&self) -> u32 {
        self.source_rate_hz
    }

    /// Feeds one mono sample in `-1.0..=1.0`; `emit` receives each completed
    /// frame and may break to stop (a cancelled capture).
    pub fn push(
        &mut self,
        sample: f32,
        emit: &mut impl FnMut(NormalizedMicrophoneFrame) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.phase = self.phase.saturating_add(OUTPUT_RATE_HZ);
        let source = u64::from(self.source_rate_hz);
        while self.phase >= source {
            self.phase -= source;
            let clamped = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            #[allow(
                clippy::cast_possible_truncation,
                reason = "clamped to -1..=1 and scaled by i16::MAX, so it fits"
            )]
            let scaled = (clamped * f32::from(i16::MAX)).round() as i16;
            self.frame[self.frame_len] = scaled;
            self.frame_len += 1;
            if self.frame_len == MICROPHONE_V1_FRAME_SAMPLES {
                let frame = NormalizedMicrophoneFrame {
                    samples: std::mem::replace(&mut self.frame, [0; MICROPHONE_V1_FRAME_SAMPLES]),
                    sequence: self.sequence,
                    timestamp_ms: self.timestamp_ms,
                };
                self.frame_len = 0;
                self.sequence = self.sequence.wrapping_add(1);
                if self.sequence == 0 {
                    self.sequence = 1;
                }
                self.timestamp_ms = self.timestamp_ms.wrapping_add(FRAME_DURATION_MS);
                emit(frame)?;
            }
        }
        ControlFlow::Continue(())
    }

    /// Feeds interleaved float samples of `channels` channels, averaging
    /// each sample frame to mono. A trailing partial sample frame is ignored.
    pub fn push_interleaved_f32(
        &mut self,
        data: &[f32],
        channels: usize,
        emit: &mut impl FnMut(NormalizedMicrophoneFrame) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        if channels == 0 {
            return ControlFlow::Continue(());
        }
        #[allow(clippy::cast_precision_loss, reason = "a channel count is small")]
        let divisor = channels as f32;
        for frame in data.chunks_exact(channels) {
            let mono = frame.iter().sum::<f32>() / divisor;
            self.push(mono, emit)?;
        }
        ControlFlow::Continue(())
    }

    /// Feeds interleaved signed 16-bit samples of `channels` channels.
    pub fn push_interleaved_i16(
        &mut self,
        data: &[i16],
        channels: usize,
        emit: &mut impl FnMut(NormalizedMicrophoneFrame) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        if channels == 0 {
            return ControlFlow::Continue(());
        }
        #[allow(clippy::cast_precision_loss, reason = "a channel count is small")]
        let divisor = channels as f32;
        for frame in data.chunks_exact(channels) {
            let mono = frame
                .iter()
                .map(|sample| f32::from(*sample) / f32::from(i16::MAX))
                .sum::<f32>()
                / divisor;
            self.push(mono, emit)?;
        }
        ControlFlow::Continue(())
    }

    /// Wipes the partial frame and the resampler phase (a stopped capture).
    pub fn clear(&mut self) {
        self.phase = 0;
        self.frame.zeroize();
        self.frame_len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(
        normalizer: &mut MicrophoneCaptureNormalizer,
        samples: impl IntoIterator<Item = f32>,
    ) -> Vec<NormalizedMicrophoneFrame> {
        let mut frames = Vec::new();
        for sample in samples {
            let _ = normalizer.push(sample, &mut |frame| {
                frames.push(frame);
                ControlFlow::Continue(())
            });
        }
        frames
    }

    #[test]
    fn a_zero_rate_is_refused() {
        assert!(MicrophoneCaptureNormalizer::new(0).is_none());
    }

    #[test]
    fn forty_eight_khz_passes_through_in_twenty_millisecond_frames() {
        let mut normalizer = MicrophoneCaptureNormalizer::new(48_000).expect("rate");
        let frames = collect(
            &mut normalizer,
            (0..MICROPHONE_V1_FRAME_SAMPLES * 2).map(|_| 0.5),
        );
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].sequence, 1);
        assert_eq!(frames[0].timestamp_ms, 0);
        assert_eq!(frames[1].sequence, 2);
        assert_eq!(frames[1].timestamp_ms, 20);
        assert!(frames[0].samples.iter().all(|sample| *sample == 16_384));
    }

    #[test]
    fn other_rates_are_brought_to_forty_eight_khz() {
        let mut low = MicrophoneCaptureNormalizer::new(16_000).expect("rate");
        assert_eq!(collect(&mut low, (0..320).map(|_| 0.0)).len(), 1);
        let mut high = MicrophoneCaptureNormalizer::new(96_000).expect("rate");
        assert_eq!(collect(&mut high, (0..1_920).map(|_| 0.0)).len(), 1);
        let mut odd = MicrophoneCaptureNormalizer::new(44_100).expect("rate");
        assert_eq!(collect(&mut odd, (0..44_100).map(|_| 0.0)).len(), 50);
    }

    #[test]
    fn samples_are_clamped_and_non_finite_input_is_silence() {
        let mut normalizer = MicrophoneCaptureNormalizer::new(48_000).expect("rate");
        let mut input = vec![0.0_f32; MICROPHONE_V1_FRAME_SAMPLES];
        input[0] = 4.0;
        input[1] = -4.0;
        input[2] = f32::NAN;
        let frames = collect(&mut normalizer, input);
        assert_eq!(frames[0].samples[0], i16::MAX);
        assert_eq!(frames[0].samples[1], -i16::MAX);
        assert_eq!(frames[0].samples[2], 0);
    }

    #[test]
    fn channels_are_averaged_to_mono() {
        let mut normalizer = MicrophoneCaptureNormalizer::new(48_000).expect("rate");
        let stereo: Vec<f32> = (0..MICROPHONE_V1_FRAME_SAMPLES)
            .flat_map(|_| [1.0, 0.0])
            .collect();
        let mut frames = Vec::new();
        let _ = normalizer.push_interleaved_f32(&stereo, 2, &mut |frame| {
            frames.push(frame);
            ControlFlow::Continue(())
        });
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].samples[0], 16_384);

        let ints: Vec<i16> = (0..MICROPHONE_V1_FRAME_SAMPLES)
            .flat_map(|_| [i16::MAX, i16::MAX])
            .collect();
        let _ = normalizer.push_interleaved_i16(&ints, 2, &mut |frame| {
            frames.push(frame);
            ControlFlow::Continue(())
        });
        assert_eq!(frames[1].samples[0], i16::MAX);
    }

    #[test]
    fn the_sequence_skips_zero_when_it_wraps_and_a_break_stops_emission() {
        let mut normalizer = MicrophoneCaptureNormalizer::new(48_000).expect("rate");
        normalizer.sequence = u32::MAX;
        let frames = collect(
            &mut normalizer,
            (0..MICROPHONE_V1_FRAME_SAMPLES * 2).map(|_| 0.0),
        );
        assert_eq!(frames[0].sequence, u32::MAX);
        assert_eq!(frames[1].sequence, 1);

        let mut emitted = 0;
        let flow = normalizer.push_interleaved_f32(
            &vec![0.0; MICROPHONE_V1_FRAME_SAMPLES * 3],
            1,
            &mut |_| {
                emitted += 1;
                ControlFlow::Break(())
            },
        );
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(emitted, 1);
    }

    #[test]
    fn clearing_wipes_the_partial_frame() {
        let mut normalizer = MicrophoneCaptureNormalizer::new(48_000).expect("rate");
        let _ = collect(&mut normalizer, [0.25; 10]);
        assert_eq!(normalizer.frame_len, 10);
        normalizer.clear();
        assert_eq!(normalizer.frame_len, 0);
        assert!(normalizer.frame.iter().all(|sample| *sample == 0));
    }
}
