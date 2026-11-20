// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Brings interleaved float audio from a device's rate to a wire rate.
//!
//! A capture device does not always run at the wire's 48 kHz: Bluetooth
//! headphones in call mode run at 16 or 24 kHz, and many DACs at 44.1 kHz.
//! Framing such audio as 48 kHz without converting it plays it at the wrong
//! speed, so a Pier either converts or refuses.
//!
//! The conversion is a windowed-sinc interpolator: a Blackman-Harris window
//! over the sinc at the lower of the two Nyquist frequencies, tabulated at
//! [`PHASES`] fractional positions and interpolated between them, with every
//! phase normalised to unity gain. It is widened for downsampling so the
//! anti-alias filter keeps its transition width at the output rate.
//!
//! [`PcmResampler::process`] is bounded and allocation-free: everything it
//! needs is allocated by [`PcmResampler::new`], and it refuses an output
//! buffer whose spare capacity is below [`PcmResampler::max_output_samples`]
//! rather than grow it. Callers may therefore run it on a thread where an
//! allocation stall matters. [`PcmResampler::accepts`] answers whether a rate
//! is carried without building a filter.
//!
//! Output `o` sits at input frame `o × source / target`: there is no index
//! offset. It is emitted once the input [`PcmResampler::latency_frames`] past
//! that position has arrived, which is the processing latency: 32 input
//! frames when upsampling (4 ms at 8 kHz, 1.3 ms at 24 kHz), and when
//! downsampling widened with the filter to 32 × source / target input frames
//! (64 at 96 kHz, 256 at 384 kHz), which is about 32 output frames, 0.67 ms.
//!
//! A stream starts from its first frame rather than from silence, and
//! [`PcmResampler::change_source_rate`] flushes the frames still held back
//! before following a device to another rate, so neither fades in nor loses
//! audio.

use super::AudioFrameSpec;

/// The lowest device rate this resampler accepts.
pub const MIN_SOURCE_RATE_HZ: u32 = 8_000;
/// The highest device rate this resampler accepts.
pub const MAX_SOURCE_RATE_HZ: u32 = 384_000;

/// Input frames either side of an output sample when upsampling.
const BASE_HALF_TAPS: usize = 32;
/// Fractional positions tabulated between two input frames.
const PHASES: usize = 256;
/// The passband edge as a fraction of the lower Nyquist frequency.
const CUTOFF: f64 = 0.92;

/// Why [`PcmResampler::process`] refused its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResampleError {
    /// The input is not a whole number of interleaved frames.
    PartialFrame,
    /// The output's spare capacity is below what this input may produce.
    OutputTooSmall {
        /// Samples this input may produce.
        needed: usize,
        /// Samples the output can take without reallocating.
        spare: usize,
    },
    /// A source rate [`PcmResampler::accepts`] refuses.
    UnsupportedRate(u32),
}

impl std::fmt::Display for ResampleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PartialFrame => write!(formatter, "input is not a whole number of frames"),
            Self::OutputTooSmall { needed, spare } => write!(
                formatter,
                "output has room for {spare} samples, this input may produce {needed}"
            ),
            Self::UnsupportedRate(rate) => {
                write!(
                    formatter,
                    "{rate} Hz is outside what this resampler accepts"
                )
            }
        }
    }
}

impl std::error::Error for ResampleError {}

/// Converts one stream of interleaved float audio to a target rate.
///
/// The channel count is the target's; this changes rate, not layout.
#[derive(Clone)]
pub struct PcmResampler {
    source_rate_hz: u32,
    target: AudioFrameSpec,
    channels: usize,
    /// Output step in input frames, as `step_num / step_den`.
    step_num: u64,
    step_den: u64,
    /// The next output sits at input frame `center + phase / step_den`.
    center: u64,
    phase: u64,
    /// Input frames accepted so far.
    received: u64,
    half: usize,
    taps: usize,
    /// `PHASES + 1` rows of `taps` coefficients.
    table: Vec<f32>,
    /// The last `taps` frames, written twice so a window is always one
    /// contiguous slice: frame `i` of the ring is at `i` and `i + taps`.
    history: Vec<f32>,
    write: usize,
    /// One output's coefficients, interpolated between two table rows.
    coefficients: Vec<f32>,
}

impl std::fmt::Debug for PcmResampler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PcmResampler")
            .field("source_rate_hz", &self.source_rate_hz)
            .field("target", &self.target)
            .field("taps", &self.taps)
            .finish_non_exhaustive()
    }
}

impl PcmResampler {
    /// Whether [`Self::new`] would build a resampler from `source_rate_hz`
    /// to `target`. Cheap: it builds nothing.
    #[must_use]
    pub const fn accepts(source_rate_hz: u32, target: AudioFrameSpec) -> bool {
        source_rate_hz >= MIN_SOURCE_RATE_HZ
            && source_rate_hz <= MAX_SOURCE_RATE_HZ
            && target.sample_rate_hz >= MIN_SOURCE_RATE_HZ
            && target.sample_rate_hz <= MAX_SOURCE_RATE_HZ
            && target.channels > 0
            && target.channels <= 32
            && target.frame_duration_ms > 0
            && target.frame_duration_ms <= 1_000
    }

    /// A resampler from `source_rate_hz` to `target`.
    ///
    /// Returns `None` where [`Self::accepts`] refuses: a rate outside
    /// [`MIN_SOURCE_RATE_HZ`]`..=`[`MAX_SOURCE_RATE_HZ`] or a target that
    /// does not describe whole frames. Builds the filter table, so it
    /// allocates and computes; call it when the rate actually changes.
    #[must_use]
    pub fn new(source_rate_hz: u32, target: AudioFrameSpec) -> Option<Self> {
        if !Self::accepts(source_rate_hz, target) || target.interleaved_samples().is_none() {
            return None;
        }
        let channels = usize::from(target.channels);
        let divisor = gcd(u64::from(source_rate_hz), u64::from(target.sample_rate_hz));
        let step_num = u64::from(source_rate_hz) / divisor;
        let step_den = u64::from(target.sample_rate_hz) / divisor;

        let (half, table) = if step_num == step_den {
            (0, Vec::new())
        } else {
            // Below 1 when downsampling: the filter must stop at the output's
            // Nyquist frequency, and is widened by the same factor to keep
            // its transition band.
            let scale = (f64::from(target.sample_rate_hz) / f64::from(source_rate_hz)).min(1.0);
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_precision_loss,
                clippy::cast_sign_loss,
                reason = "a small constant over a scale of at least 8000/384000: at most 1536"
            )]
            let half = (BASE_HALF_TAPS as f64 / scale).ceil() as usize;
            (half, kernel_table(half, CUTOFF * scale))
        };
        let taps = half * 2;
        Some(Self {
            source_rate_hz,
            target,
            channels,
            step_num,
            step_den,
            center: 0,
            phase: 0,
            received: 0,
            half,
            taps,
            table,
            history: vec![0.0; taps * 2 * channels],
            write: 0,
            coefficients: vec![0.0; taps],
        })
    }

    /// The device rate this resampler converts from.
    #[must_use]
    pub const fn source_rate_hz(&self) -> u32 {
        self.source_rate_hz
    }

    /// The format it converts to.
    #[must_use]
    pub const fn target(&self) -> AudioFrameSpec {
        self.target
    }

    /// Whether the rates match, so samples are copied unchanged.
    #[must_use]
    pub const fn is_passthrough(&self) -> bool {
        self.step_num == self.step_den
    }

    /// Input frames an output waits for past its own position before it is
    /// emitted: 32 when upsampling, 32 × source / target when downsampling
    /// (64 at 96 kHz, 256 at 384 kHz), none when the rates match.
    #[must_use]
    pub const fn latency_frames(&self) -> usize {
        self.half
    }

    /// The most samples [`Self::process`] can append for `input_samples`.
    #[must_use]
    pub fn max_output_samples(&self, input_samples: usize) -> usize {
        Self::output_bound(self.step_num, self.step_den, self.channels, input_samples)
    }

    /// The most samples a resampler from `source_rate_hz` to `target` can
    /// append for `input_samples`, for sizing a buffer before the rate is
    /// known. `None` where [`Self::new`] would refuse.
    #[must_use]
    pub fn max_output_samples_for(
        source_rate_hz: u32,
        target: AudioFrameSpec,
        input_samples: usize,
    ) -> Option<usize> {
        if !Self::accepts(source_rate_hz, target) {
            return None;
        }
        let divisor = gcd(u64::from(source_rate_hz), u64::from(target.sample_rate_hz));
        Some(Self::output_bound(
            u64::from(source_rate_hz) / divisor,
            u64::from(target.sample_rate_hz) / divisor,
            usize::from(target.channels),
            input_samples,
        ))
    }

    fn output_bound(step_num: u64, step_den: u64, channels: usize, input_samples: usize) -> usize {
        let frames = (input_samples / channels.max(1)) as u128;
        // Outputs whose window ends on one of `frames` consecutive input
        // frames: at most ceil(frames / step) + 1.
        let outputs = (frames * u128::from(step_den)).div_ceil(u128::from(step_num)) + 1;
        usize::try_from(outputs.saturating_mul(channels as u128)).unwrap_or(usize::MAX)
    }

    /// Converts `input` and appends the result to `output`.
    ///
    /// Returns the samples appended. Never allocates: an `output` whose
    /// spare capacity is below [`Self::max_output_samples`] is refused and
    /// left unchanged. Non-finite samples are taken as silence, so one bad
    /// sample cannot poison the filter's history.
    ///
    /// # Errors
    ///
    /// [`ResampleError::PartialFrame`] for input that is not whole frames,
    /// [`ResampleError::OutputTooSmall`] for an output without room.
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
    ) -> Result<usize, ResampleError> {
        if input.len() % self.channels != 0 {
            return Err(ResampleError::PartialFrame);
        }
        let needed = self.max_output_samples(input.len());
        let spare = output.capacity() - output.len();
        if spare < needed {
            return Err(ResampleError::OutputTooSmall { needed, spare });
        }
        let before = output.len();
        if self.is_passthrough() {
            output.extend(input.iter().map(|&sample| finite(sample)));
            return Ok(input.len());
        }
        for frame in input.chunks_exact(self.channels) {
            self.push_frame(frame);
            self.emit_ready(output);
        }
        Ok(output.len() - before)
    }

    /// Emits the outputs still held back by [`Self::latency_frames`], as if
    /// the last frame held steady, then starts a new stream.
    ///
    /// Returns the samples appended: exactly the outputs whose positions
    /// fall within the input received, so a stream that ends here loses
    /// nothing. Never allocates.
    ///
    /// # Errors
    ///
    /// [`ResampleError::OutputTooSmall`] for an output without room for
    /// [`Self::max_output_samples`] of [`Self::latency_frames`] frames.
    pub fn flush(&mut self, output: &mut Vec<f32>) -> Result<usize, ResampleError> {
        if self.received == 0 || self.half == 0 {
            self.reset();
            return Ok(0);
        }
        let needed = self.max_output_samples(self.half * self.channels);
        let spare = output.capacity() - output.len();
        if spare < needed {
            return Err(ResampleError::OutputTooSmall { needed, spare });
        }
        let mut last = [0.0f32; 32];
        let newest = (self.write + self.taps - 1) % self.taps * self.channels;
        last[..self.channels].copy_from_slice(&self.history[newest..newest + self.channels]);
        let before = output.len();
        for _ in 0..self.half {
            self.push_frame(&last[..self.channels]);
            self.emit_ready(output);
        }
        self.reset();
        Ok(output.len() - before)
    }

    /// Follows the device to `source_rate_hz`: flushes what the current rate
    /// holds back into `output`, then converts from the new rate, starting
    /// from its first frame. Returns the samples flushed.
    ///
    /// Builds a new filter table only when the rate differs.
    ///
    /// # Errors
    ///
    /// [`ResampleError::UnsupportedRate`] when [`Self::accepts`] refuses the
    /// rate (nothing is flushed or changed), [`ResampleError::OutputTooSmall`]
    /// as for [`Self::flush`].
    pub fn change_source_rate(
        &mut self,
        source_rate_hz: u32,
        output: &mut Vec<f32>,
    ) -> Result<usize, ResampleError> {
        if source_rate_hz == self.source_rate_hz {
            return Ok(0);
        }
        let Some(next) = Self::new(source_rate_hz, self.target) else {
            return Err(ResampleError::UnsupportedRate(source_rate_hz));
        };
        let flushed = self.flush(output)?;
        *self = next;
        Ok(flushed)
    }

    /// Forgets the stream so far, as if newly built.
    pub fn reset(&mut self) {
        self.center = 0;
        self.phase = 0;
        self.received = 0;
        self.write = 0;
        self.history.fill(0.0);
    }

    fn push_frame(&mut self, frame: &[f32]) {
        let width = self.taps * self.channels;
        if self.received == 0 {
            // A stream starts from its first frame, not from silence: a
            // history of zeros fades the first outputs in from half level.
            for slot in self.history.chunks_exact_mut(self.channels) {
                for (target, &sample) in slot.iter_mut().zip(frame) {
                    *target = finite(sample);
                }
            }
        }
        let at = self.write * self.channels;
        for (channel, &sample) in frame.iter().enumerate() {
            let sample = finite(sample);
            self.history[at + channel] = sample;
            self.history[at + width + channel] = sample;
        }
        self.write = (self.write + 1) % self.taps;
        self.received += 1;
    }

    fn emit_ready(&mut self, output: &mut Vec<f32>) {
        // An output needs input up to `center + half`; after each frame is
        // pushed, the window of the last `taps` frames is exactly that.
        while self.center + (self.half as u64) < self.received {
            self.interpolate_coefficients();
            let start = self.write * self.channels;
            let window = &self.history[start..start + self.taps * self.channels];
            for channel in 0..self.channels {
                let mut sum = 0.0f32;
                for (tap, coefficient) in self.coefficients.iter().enumerate() {
                    sum += window[tap * self.channels + channel] * coefficient;
                }
                output.push(sum);
            }
            self.phase += self.step_num;
            self.center += self.phase / self.step_den;
            self.phase %= self.step_den;
        }
    }

    fn interpolate_coefficients(&mut self) {
        #[allow(clippy::cast_precision_loss, reason = "both are below 2^20")]
        let position = self.phase as f64 * PHASES as f64 / self.step_den as f64;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "position is in 0..PHASES"
        )]
        let row = (position.floor() as usize).min(PHASES - 1);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "row is below PHASES and the result is a fraction in 0..1"
        )]
        let blend = (position - row as f64) as f32;
        let lower = &self.table[row * self.taps..(row + 1) * self.taps];
        let upper = &self.table[(row + 1) * self.taps..(row + 2) * self.taps];
        for ((coefficient, &low), &high) in self.coefficients.iter_mut().zip(lower).zip(upper) {
            *coefficient = low + (high - low) * blend;
        }
    }
}

const fn finite(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

const fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let rest = left % right;
        left = right;
        right = rest;
    }
    left
}

/// `PHASES + 1` rows of `2 * half` taps, each row summing to one.
///
/// Row `p` holds the weights of input frames `center - half + 1 ..= center +
/// half` for an output `p / PHASES` of a frame after `center`.
fn kernel_table(half: usize, cutoff: f64) -> Vec<f32> {
    let taps = half * 2;
    let mut table = Vec::with_capacity((PHASES + 1) * taps);
    #[allow(clippy::cast_precision_loss, reason = "tap counts are small")]
    let half_width = half as f64;
    let mut row = vec![0.0f64; taps];
    for phase in 0..=PHASES {
        #[allow(clippy::cast_precision_loss, reason = "PHASES is small")]
        let offset = phase as f64 / PHASES as f64;
        for (tap, weight) in row.iter_mut().enumerate() {
            #[allow(clippy::cast_precision_loss, reason = "tap counts are small")]
            let distance = tap as f64 - (half_width - 1.0) - offset;
            *weight = cutoff * sinc(cutoff * distance) * blackman_harris(distance / half_width);
        }
        let sum: f64 = row.iter().sum();
        #[allow(clippy::cast_possible_truncation, reason = "coefficients are below 1")]
        table.extend(row.iter().map(|weight| (weight / sum) as f32));
    }
    table
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let angle = std::f64::consts::PI * x;
        angle.sin() / angle
    }
}

/// A four-term Blackman-Harris window over `-1..=1`, zero outside it.
fn blackman_harris(x: f64) -> f64 {
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let angle = std::f64::consts::PI * x;
    0.358_75
        + 0.488_29 * angle.cos()
        + 0.141_28 * (2.0 * angle).cos()
        + 0.011_68 * (3.0 * angle).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO_48K: AudioFrameSpec = AudioFrameSpec::V1;

    fn sine(rate_hz: u32, frequency_hz: f64, frames: usize, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|frame| {
                #[allow(clippy::cast_precision_loss)]
                let time = frame as f64 / f64::from(rate_hz);
                #[allow(clippy::cast_possible_truncation)]
                let value = (std::f64::consts::TAU * frequency_hz * time).sin() as f32 * amplitude;
                [value, -value]
            })
            .collect()
    }

    fn run(resampler: &mut PcmResampler, input: &[f32]) -> Vec<f32> {
        let mut output = Vec::with_capacity(resampler.max_output_samples(input.len()));
        resampler
            .process(input, &mut output)
            .expect("room was reserved");
        output
    }

    /// The level of a sine at `frequency_hz` in the left channel, and the
    /// level of everything else, measured away from the start-up transient.
    fn tone_and_residue(left_right: &[f32], rate_hz: u32, frequency_hz: f64) -> (f64, f64) {
        let left: Vec<f64> = left_right
            .iter()
            .step_by(2)
            .map(|&v| f64::from(v))
            .collect();
        let skip = left.len() / 4;
        let body = &left[skip..left.len() - skip];
        let (mut sin_sum, mut cos_sum) = (0.0, 0.0);
        for (index, value) in body.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let angle =
                std::f64::consts::TAU * frequency_hz * (index + skip) as f64 / f64::from(rate_hz);
            sin_sum += value * angle.sin();
            cos_sum += value * angle.cos();
        }
        #[allow(clippy::cast_precision_loss)]
        let count = body.len() as f64;
        let (a, b) = (2.0 * sin_sum / count, 2.0 * cos_sum / count);
        let mut residue = 0.0;
        for (index, value) in body.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let angle =
                std::f64::consts::TAU * frequency_hz * (index + skip) as f64 / f64::from(rate_hz);
            let fitted = a * angle.sin() + b * angle.cos();
            residue += (value - fitted).powi(2);
        }
        (
            a.hypot(b),
            (residue / count).sqrt() * std::f64::consts::SQRT_2,
        )
    }

    #[test]
    fn rates_a_device_can_run_at_are_accepted_and_others_refused() {
        for rate in [
            8_000, 16_000, 24_000, 44_100, 48_000, 96_000, 192_000, 384_000,
        ] {
            assert!(PcmResampler::new(rate, STEREO_48K).is_some(), "{rate} Hz");
        }
        for rate in [0, 1, 7_999, 384_001] {
            assert!(PcmResampler::new(rate, STEREO_48K).is_none(), "{rate} Hz");
        }
        // The cheap check answers as `new` does, without building a filter.
        for rate in [0, 7_999, 8_000, 24_000, 44_100, 384_000, 384_001, u32::MAX] {
            assert_eq!(
                PcmResampler::accepts(rate, STEREO_48K),
                PcmResampler::new(rate, STEREO_48K).is_some(),
                "{rate} Hz"
            );
            assert_eq!(
                PcmResampler::max_output_samples_for(rate, STEREO_48K, 2_048),
                PcmResampler::new(rate, STEREO_48K).map(|r| r.max_output_samples(2_048)),
                "{rate} Hz"
            );
        }
    }

    #[test]
    fn a_matching_rate_passes_samples_through_unchanged() {
        let mut resampler = PcmResampler::new(48_000, STEREO_48K).expect("supported");
        assert!(resampler.is_passthrough());
        let input = sine(48_000, 1_000.0, 480, 0.5);
        assert_eq!(run(&mut resampler, &input), input);
    }

    #[test]
    fn a_24_khz_stream_becomes_twice_as_many_48_khz_frames() {
        // Bluetooth headphones in call mode clock capture at 24 kHz.
        let mut resampler = PcmResampler::new(24_000, STEREO_48K).expect("supported");
        let output = run(&mut resampler, &sine(24_000, 1_000.0, 24_000, 0.5));
        let frames = output.len() / 2;
        let expected = 48_000 - 2 * resampler.latency_frames();
        assert!(
            frames.abs_diff(expected) <= 2,
            "one second at 24 kHz gave {frames} frames at 48 kHz, expected {expected}"
        );
    }

    #[test]
    fn a_tone_keeps_its_pitch_and_level_through_every_supported_ratio() {
        for (rate, frequency) in [
            (16_000, 1_000.0),
            (24_000, 1_000.0),
            (24_000, 7_000.0),
            (44_100, 5_000.0),
            (96_000, 12_000.0),
            (192_000, 3_000.0),
        ] {
            let mut resampler = PcmResampler::new(rate, STEREO_48K).expect("supported");
            let output = run(
                &mut resampler,
                &sine(rate, frequency, rate as usize / 2, 0.5),
            );
            let (level, residue) = tone_and_residue(&output, 48_000, frequency);
            assert!(
                (level - 0.5).abs() < 0.01,
                "{rate} Hz -> 48 kHz: a {frequency} Hz tone came out at {level:.4}, not 0.5"
            );
            // Imaging, aliasing and interpolation error together.
            assert!(
                residue < 0.5 * 10f64.powf(-65.0 / 20.0),
                "{rate} Hz -> 48 kHz: {frequency} Hz left residue {residue:.6}"
            );
        }
    }

    #[test]
    fn upsampling_keeps_the_image_80_db_down() {
        // A 7 kHz tone at 24 kHz mirrors to 17 kHz at 48 kHz. Linear
        // interpolation (the Windows Pier's former rule) leaves it 14 dB down.
        let mut resampler = PcmResampler::new(24_000, STEREO_48K).expect("supported");
        let output = run(&mut resampler, &sine(24_000, 7_000.0, 24_000, 0.5));
        let (image, _) = tone_and_residue(&output, 48_000, 17_000.0);
        let decibels = 20.0 * (image / 0.5).log10();
        assert!(decibels <= -80.0, "image at {decibels:.1} dB");
    }

    #[test]
    fn downsampling_removes_what_the_output_cannot_carry() {
        // 30 kHz is above 48 kHz's Nyquist frequency; passed through, it
        // would alias to 18 kHz.
        let mut resampler = PcmResampler::new(96_000, STEREO_48K).expect("supported");
        let output = run(&mut resampler, &sine(96_000, 30_000.0, 48_000, 0.5));
        let (alias, _) = tone_and_residue(&output, 48_000, 18_000.0);
        let decibels = 20.0 * (alias / 0.5).log10();
        assert!(decibels <= -120.0, "alias at {decibels:.1} dB");
    }

    #[test]
    fn an_impulse_lands_at_its_scaled_position_after_the_stated_latency() {
        // Output `o` sits at input frame `o × source / target`, and is
        // emitted once input `latency_frames` beyond that has arrived.
        for (rate, impulse_at, expected_output) in [
            (24_000, 1_000, 2_000),
            (96_000, 2_000, 1_000),
            (16_000, 600, 1_800),
        ] {
            let mut resampler = PcmResampler::new(rate, STEREO_48K).expect("supported");
            let latency = resampler.latency_frames();
            let mut output = Vec::with_capacity(resampler.max_output_samples(2 * 4_000));
            let mut emitted_after = None;
            for frame in 0..4_000usize {
                let value = if frame == impulse_at { 1.0 } else { 0.0 };
                let mut piece = Vec::with_capacity(resampler.max_output_samples(2));
                resampler
                    .process(&[value, value], &mut piece)
                    .expect("room was reserved");
                output.extend(piece);
                if emitted_after.is_none() && output.len() / 2 > expected_output {
                    emitted_after = Some(frame);
                }
            }
            let left: Vec<f32> = output.iter().step_by(2).copied().collect();
            let peak = left
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(index, _)| index);
            assert_eq!(
                peak,
                Some(expected_output),
                "{rate} Hz: impulse at {impulse_at}"
            );
            assert_eq!(
                emitted_after,
                Some(impulse_at + latency),
                "{rate} Hz: output {expected_output} must wait for input {impulse_at} + {latency}"
            );
        }
        assert_eq!(
            PcmResampler::new(96_000, STEREO_48K).map(|r| r.latency_frames()),
            Some(64)
        );
        assert_eq!(
            PcmResampler::new(384_000, STEREO_48K).map(|r| r.latency_frames()),
            Some(256)
        );
    }

    #[test]
    fn a_stream_starts_at_its_first_level_and_a_flush_loses_nothing() {
        // A constant level must come out constant from the first output to
        // the last: no fade-in from a silent history, no tail held back.
        let mut resampler = PcmResampler::new(44_100, STEREO_48K).expect("supported");
        let input = vec![0.5f32; 2 * 4_410];
        let mut output = run(&mut resampler, &input);
        let mut tail =
            Vec::with_capacity(resampler.max_output_samples(2 * resampler.latency_frames()));
        resampler.flush(&mut tail).expect("room was reserved");
        output.extend(tail);
        assert_eq!(
            output.len() / 2,
            4_800,
            "0.1 s at 44.1 kHz is 4800 frames at 48 kHz"
        );
        let worst = output
            .iter()
            .map(|v| (v - 0.5).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-4, "a constant level wandered by {worst}");
    }

    #[test]
    fn following_a_rate_change_mid_stream_loses_no_frames_and_keeps_its_level() {
        // A headset switching into call mode: 0.25 s at 48 kHz, 0.5 s at
        // 24 kHz, 0.25 s at 44.1 kHz. A constant level stays constant across
        // both joins, and every frame of input comes out.
        let mut resampler = PcmResampler::new(48_000, STEREO_48K).expect("supported");
        let mut output = Vec::new();
        for (rate, frames) in [(48_000u32, 12_000usize), (24_000, 12_000), (44_100, 11_025)] {
            let mut flushed = Vec::with_capacity(resampler.max_output_samples(2 * 1_024));
            resampler
                .change_source_rate(rate, &mut flushed)
                .expect("supported, with room");
            output.extend(flushed);
            output.extend(run(&mut resampler, &vec![0.5f32; 2 * frames]));
        }
        let mut tail = Vec::with_capacity(resampler.max_output_samples(2 * 1_024));
        resampler.flush(&mut tail).expect("room was reserved");
        output.extend(tail);
        assert_eq!(output.len() / 2, 48_000, "one second in, one second out");
        let worst = output
            .iter()
            .map(|v| (v - 0.5).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-4, "the level dipped by {worst} at a join");

        // A tone keeps its phase across a join: a lost or repeated frame
        // would put every later output a sample off, 0.065 away at 1 kHz.
        let mut resampler = PcmResampler::new(24_000, STEREO_48K).expect("supported");
        let mut output = run(&mut resampler, &sine(24_000, 1_000.0, 12_000, 0.5));
        let mut flushed = Vec::with_capacity(resampler.max_output_samples(2 * 1_024));
        resampler
            .change_source_rate(16_000, &mut flushed)
            .expect("supported, with room");
        output.extend(flushed);
        let second: Vec<f32> = sine(16_000, 1_000.0, 16_000, 0.5).split_off(2 * 8_000);
        output.extend(run(&mut resampler, &second));
        let left: Vec<f32> = output.iter().step_by(2).copied().collect();
        assert_eq!(
            left.len(),
            24_000 + (8_000 - resampler.latency_frames()) * 3
        );
        // Within a filter's reach of the join, the outputs that needed input
        // past it were made from the last frame held, or the first frame
        // repeated: the device gave nothing there to make them from.
        let join = 24_000usize;
        let reach = 2 * 32 * 3;
        let (mut far, mut near) = (0.0f32, 0.0f32);
        for (index, &value) in left.iter().enumerate().skip(reach) {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let ideal =
                (std::f64::consts::TAU * 1_000.0 * index as f64 / 48_000.0).sin() as f32 * 0.5;
            let error = (value - ideal).abs();
            if index.abs_diff(join) <= reach {
                near = near.max(error);
            } else {
                far = far.max(error);
            }
        }
        eprintln!("tone error: {far:.6} away from the join, {near:.6} at it");
        assert!(
            far < 1e-3,
            "a continuous 1 kHz tone is off by {far} away from the join"
        );
        assert!(near < 0.075, "the join is off by {near}");
    }

    #[test]
    fn chunk_boundaries_do_not_change_the_output() {
        let input = sine(44_100, 440.0, 4_410, 0.7);
        let mut whole = PcmResampler::new(44_100, STEREO_48K).expect("supported");
        let expected = run(&mut whole, &input);

        let mut chunked = PcmResampler::new(44_100, STEREO_48K).expect("supported");
        let mut output = Vec::with_capacity(expected.len() + 64);
        for chunk in input.chunks(2 * 37) {
            let mut piece = Vec::with_capacity(chunked.max_output_samples(chunk.len()));
            chunked
                .process(chunk, &mut piece)
                .expect("room was reserved");
            output.extend(piece);
        }
        assert_eq!(output, expected);
    }

    #[test]
    fn the_bound_holds_for_every_chunk_size_and_ratio() {
        for rate in [8_000, 22_050, 24_000, 44_100, 47_999, 96_000, 384_000] {
            let mut resampler = PcmResampler::new(rate, STEREO_48K).expect("supported");
            let input = sine(rate, 300.0, 3_000, 0.3);
            let mut offset = 0;
            for frames in (1..).cycle().take(200) {
                let end = (offset + frames * 2).min(input.len());
                let chunk = &input[offset..end];
                let bound = resampler.max_output_samples(chunk.len());
                let mut output = Vec::with_capacity(bound);
                let written = resampler
                    .process(chunk, &mut output)
                    .expect("room was reserved");
                assert!(written <= bound, "{rate} Hz: {written} > {bound}");
                offset = if end == input.len() { 0 } else { end };
            }
        }
    }

    #[test]
    fn processing_never_grows_the_output() {
        let mut resampler = PcmResampler::new(24_000, STEREO_48K).expect("supported");
        let input = sine(24_000, 1_000.0, 480, 0.5);
        let bound = resampler.max_output_samples(input.len());
        let mut output = Vec::with_capacity(bound);
        let (pointer, capacity) = (output.as_ptr(), output.capacity());
        resampler
            .process(&input, &mut output)
            .expect("room was reserved");
        assert_eq!((output.as_ptr(), output.capacity()), (pointer, capacity));

        let mut cramped = Vec::with_capacity(bound - 1);
        cramped.push(1.0);
        let error = resampler
            .process(&input, &mut cramped)
            .expect_err("no room");
        assert!(
            matches!(error, ResampleError::OutputTooSmall { .. }),
            "{error}"
        );
        assert_eq!(cramped, [1.0], "a refused call leaves the output as it was");
    }

    #[test]
    fn a_partial_frame_is_refused() {
        let mut resampler = PcmResampler::new(24_000, STEREO_48K).expect("supported");
        let mut output = Vec::with_capacity(64);
        assert_eq!(
            resampler.process(&[0.1, 0.2, 0.3], &mut output),
            Err(ResampleError::PartialFrame)
        );
    }

    #[test]
    fn a_non_finite_sample_is_silence_and_does_not_poison_the_stream() {
        let mut resampler = PcmResampler::new(24_000, STEREO_48K).expect("supported");
        let mut input = sine(24_000, 1_000.0, 2_400, 0.5);
        input[100] = f32::NAN;
        input[101] = f32::INFINITY;
        let output = run(&mut resampler, &input);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(
            output[output.len() - 200..]
                .iter()
                .any(|&sample| sample.abs() > 0.1)
        );
    }

    #[test]
    fn reset_starts_a_new_stream() {
        let input = sine(24_000, 1_000.0, 480, 0.5);
        let mut fresh = PcmResampler::new(24_000, STEREO_48K).expect("supported");
        let expected = run(&mut fresh, &input);
        let mut reused = PcmResampler::new(24_000, STEREO_48K).expect("supported");
        let _ = run(&mut reused, &sine(24_000, 3_000.0, 333, 0.9));
        reused.reset();
        assert_eq!(run(&mut reused, &input), expected);
    }
}
