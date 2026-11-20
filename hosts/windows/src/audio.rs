// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
#[cfg(windows)]
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_media::audio::resample::PcmResampler;
use arcen_media::audio::AudioFrameSpec;

use crate::latest::LatestQueue;
use crate::logging::AUDIO;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const CHUNK_MS: u64 = 20;
pub const CHUNK_FRAMES: usize = SAMPLE_RATE as usize * CHUNK_MS as usize / 1000;
pub const CHUNK_BYTES: usize = CHUNK_FRAMES * CHANNELS * size_of::<i16>();
pub const QUEUE_CAPACITY: usize = 4;

const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const MIN_MIX_SAMPLE_RATE: u32 = 8_000;
const MAX_MIX_SAMPLE_RATE: u32 = 384_000;
const CLOCK_GAP_TOLERANCE_MS: u64 = CHUNK_MS * 2;
const RESTART_MIN_DELAY: Duration = Duration::from_millis(100);
const RESTART_MAX_DELAY: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    /// One audio-v1 packet, held by value so the capture loop hands it over
    /// without allocating.
    pub pcm_s16le: [u8; CHUNK_BYTES],
    pub timestamp_ms: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioTelemetrySnapshot {
    pub packets: u64,
    pub bytes: u64,
    pub sent_packets: u64,
    pub sent_bytes: u64,
    pub queue_drops: u64,
    pub capture_errors: u64,
    pub restarts: u64,
    pub discontinuities: u64,
    pub underruns: u64,
    pub silent_frames: u64,
    pub idle_periods: u64,
    pub timestamp_gap_ms: u64,
}

#[derive(Default)]
pub struct AudioTelemetry {
    packets: AtomicU64,
    bytes: AtomicU64,
    sent_packets: AtomicU64,
    sent_bytes: AtomicU64,
    queue_drops: AtomicU64,
    capture_errors: AtomicU64,
    restarts: AtomicU64,
    discontinuities: AtomicU64,
    underruns: AtomicU64,
    silent_frames: AtomicU64,
    idle_periods: AtomicU64,
    timestamp_gap_ms: AtomicU64,
}

impl AudioTelemetry {
    pub fn snapshot(&self) -> AudioTelemetrySnapshot {
        AudioTelemetrySnapshot {
            packets: self.packets.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            sent_packets: self.sent_packets.load(Ordering::Relaxed),
            sent_bytes: self.sent_bytes.load(Ordering::Relaxed),
            queue_drops: self.queue_drops.load(Ordering::Relaxed),
            capture_errors: self.capture_errors.load(Ordering::Relaxed),
            restarts: self.restarts.load(Ordering::Relaxed),
            discontinuities: self.discontinuities.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            silent_frames: self.silent_frames.load(Ordering::Relaxed),
            idle_periods: self.idle_periods.load(Ordering::Relaxed),
            timestamp_gap_ms: self.timestamp_gap_ms.load(Ordering::Relaxed),
        }
    }

    pub fn record_sent(&self, bytes: usize) {
        self.sent_packets.fetch_add(1, Ordering::Relaxed);
        self.sent_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

pub struct AudioCapture {
    stop: Arc<StopSignal>,
    join: Option<JoinHandle<()>>,
    telemetry: Arc<AudioTelemetry>,
}

impl AudioCapture {
    #[cfg(windows)]
    pub fn start(queue: Arc<LatestQueue<AudioPacket>>) -> Result<Self, String> {
        let stop = Arc::new(StopSignal::default());
        let telemetry = Arc::new(AudioTelemetry::default());
        let worker_stop = Arc::clone(&stop);
        let worker_telemetry = Arc::clone(&telemetry);
        let join = std::thread::Builder::new()
            .name("wasapi-loopback".to_string())
            .spawn(move || {
                supervise(
                    platform::WasapiRunner,
                    queue,
                    worker_stop,
                    worker_telemetry,
                    RestartBackoff::default(),
                );
            })
            .map_err(|error| format!("spawn WASAPI capture thread: {error}"))?;
        Ok(Self {
            stop,
            join: Some(join),
            telemetry,
        })
    }

    #[cfg(not(windows))]
    pub fn start(_queue: Arc<LatestQueue<AudioPacket>>) -> Result<Self, String> {
        Err("WASAPI loopback is only available on Windows".to_string())
    }

    pub fn telemetry(&self) -> AudioTelemetrySnapshot {
        self.telemetry.snapshot()
    }

    pub fn telemetry_handle(&self) -> Arc<AudioTelemetry> {
        Arc::clone(&self.telemetry)
    }

    pub async fn shutdown(&mut self) {
        self.stop.request();
        if let Some(join) = self.join.take() {
            let mut blocking_join = tokio::task::spawn_blocking(move || join.join().is_ok());
            let result = match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut blocking_join).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(
                        target: AUDIO,
                        timeout_ms = SHUTDOWN_TIMEOUT.as_millis(),
                        "WASAPI capture thread is slow to stop; waiting to prevent worker overlap"
                    );
                    blocking_join.await
                }
            };
            match result {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(target: AUDIO, "WASAPI capture thread panicked during shutdown");
                }
                Err(error) => {
                    tracing::warn!(target: AUDIO, %error, "WASAPI join task failed");
                }
            }
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.request();
        if self.join.take().is_some() {
            tracing::debug!(target: AUDIO, "WASAPI capture detached during fallback drop");
        }
    }
}

#[derive(Default)]
struct StopSignal {
    requested: AtomicBool,
    mutex: Mutex<()>,
    wake: Condvar,
}

impl StopSignal {
    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    fn wait_timeout(&self, timeout: Duration) -> bool {
        if self.is_requested() {
            return true;
        }
        let guard = self.mutex.lock().expect("audio stop lock poisoned");
        let _ = self
            .wake
            .wait_timeout_while(guard, timeout, |_| !self.is_requested())
            .expect("audio stop wait poisoned");
        self.is_requested()
    }
}

trait CaptureRunner {
    fn run_once(
        &mut self,
        stop: &StopSignal,
        clock: &mut AudioClock,
        emit: &mut dyn FnMut(AudioPacket),
        telemetry: &AudioTelemetry,
    ) -> Result<(), String>;
}

fn supervise<R: CaptureRunner>(
    mut runner: R,
    queue: Arc<LatestQueue<AudioPacket>>,
    stop: Arc<StopSignal>,
    telemetry: Arc<AudioTelemetry>,
    mut backoff: RestartBackoff,
) {
    let mut clock = AudioClock::default();
    let mut first_attempt = true;
    while !stop.is_requested() {
        let packets_before = telemetry.packets.load(Ordering::Relaxed);
        let mut emit = |packet: AudioPacket| {
            let bytes = packet.pcm_s16le.len() as u64;
            match queue.push(packet) {
                Ok(Some(_)) => {
                    telemetry.queue_drops.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) => {}
                Err(_) => return,
            }
            telemetry.packets.fetch_add(1, Ordering::Relaxed);
            telemetry.bytes.fetch_add(bytes, Ordering::Relaxed);
        };

        let result = runner.run_once(&stop, &mut clock, &mut emit, &telemetry);
        if stop.is_requested() {
            break;
        }
        telemetry.capture_errors.fetch_add(1, Ordering::Relaxed);
        if telemetry.packets.load(Ordering::Relaxed) > packets_before {
            backoff.reset();
        }
        if !first_attempt {
            tracing::warn!(
                target: AUDIO,
                error = %result.err().unwrap_or_else(|| "capture stopped unexpectedly".to_string()),
                "WASAPI capture stopped; retrying"
            );
        } else if let Err(error) = result {
            tracing::warn!(target: AUDIO, %error, "WASAPI capture unavailable; retrying");
        }
        first_attempt = false;
        telemetry.restarts.fetch_add(1, Ordering::Relaxed);
        if stop.wait_timeout(backoff.next_delay()) {
            break;
        }
    }
    tracing::info!(target: AUDIO, "WASAPI capture worker stopped");
}

#[derive(Debug, Clone)]
struct RestartBackoff {
    next: Duration,
    min: Duration,
    max: Duration,
}

impl Default for RestartBackoff {
    fn default() -> Self {
        Self {
            next: RESTART_MIN_DELAY,
            min: RESTART_MIN_DELAY,
            max: RESTART_MAX_DELAY,
        }
    }
}

impl RestartBackoff {
    #[cfg(test)]
    fn new(min: Duration, max: Duration) -> Self {
        Self {
            next: min,
            min,
            max,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let current = self.next;
        self.next = self.next.saturating_mul(2).min(self.max);
        current
    }

    fn reset(&mut self) {
        self.next = self.min;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleFormat {
    Float32,
    SignedPcm {
        container_bytes: usize,
        valid_bits: u16,
    },
}

impl SampleFormat {
    fn bytes_per_sample(self) -> usize {
        match self {
            Self::Float32 => 4,
            Self::SignedPcm {
                container_bytes, ..
            } => container_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MixFormat {
    sample_rate: u32,
    channels: usize,
    sample_format: SampleFormat,
    block_align: usize,
    channel_mask: u32,
}

impl MixFormat {
    fn validate(self) -> Result<Self, String> {
        if !(MIN_MIX_SAMPLE_RATE..=MAX_MIX_SAMPLE_RATE).contains(&self.sample_rate)
            || self.channels == 0
            || self.channels > 32
        {
            return Err("invalid WASAPI sample rate or channel count".to_string());
        }
        let expected = self
            .channels
            .checked_mul(self.sample_format.bytes_per_sample())
            .ok_or_else(|| "WASAPI block alignment overflow".to_string())?;
        if self.block_align != expected {
            return Err(format!(
                "unsupported WASAPI block alignment: got {}, expected {expected}",
                self.block_align
            ));
        }
        Ok(self)
    }
}

/// Mix frames adapted and converted at a time, so every scratch buffer the
/// capture loop uses is allocated once and bounded.
const CONVERT_CHUNK_FRAMES: usize = 1_024;

/// Turns WASAPI buffers into audio-v1 packets without allocating.
///
/// Format adaptation (sample format, channel layout) is this adapter's: it
/// is what the mix format says. The rate conversion is the shared
/// `arcen_media` resampler every Pier uses.
struct AudioPipeline {
    converter: StereoConverter,
    resampler: PcmResampler,
    /// Stereo float frames at the mix rate, at most `CONVERT_CHUNK_FRAMES`.
    stereo: Vec<f32>,
    /// The resampler's output for one chunk, sized for the lowest mix rate.
    resampled: Vec<f32>,
    /// The packet being filled, handed over by value: no allocation per
    /// packet either.
    chunk: [u8; CHUNK_BYTES],
    chunk_len: usize,
}

impl AudioPipeline {
    fn new(format: MixFormat) -> Result<Self, String> {
        let format = format.validate()?;
        let resampler = PcmResampler::new(format.sample_rate, AudioFrameSpec::V1)
            .ok_or_else(|| format!("no resampler from {} Hz to audio-v1", format.sample_rate))?;
        let resampled = PcmResampler::max_output_samples_for(
            MIN_MIX_SAMPLE_RATE,
            AudioFrameSpec::V1,
            CONVERT_CHUNK_FRAMES * CHANNELS,
        )
        .ok_or_else(|| "audio-v1 has no resampler bound".to_string())?;
        Ok(Self {
            converter: StereoConverter::new(format),
            resampler,
            stereo: Vec::with_capacity(CONVERT_CHUNK_FRAMES * CHANNELS),
            resampled: Vec::with_capacity(resampled),
            chunk: [0; CHUNK_BYTES],
            chunk_len: 0,
        })
    }

    fn push(
        &mut self,
        data: &[u8],
        frames: usize,
        now_ms: u64,
        clock: &mut AudioClock,
        emit: &mut dyn FnMut(AudioPacket),
    ) -> Result<u64, String> {
        let block_align = self.converter.format.block_align;
        let expected = frames
            .checked_mul(block_align)
            .ok_or_else(|| "WASAPI packet size overflow".to_string())?;
        if expected > MAX_PACKET_BYTES {
            return Err("WASAPI packet exceeds safety limit".to_string());
        }
        if data.len() < expected {
            return Err(format!(
                "short WASAPI packet: got {} bytes, expected {expected}",
                data.len()
            ));
        }
        let mut gap_total_ms = 0;
        for piece in data[..expected].chunks(CONVERT_CHUNK_FRAMES * block_align) {
            self.stereo.clear();
            self.converter.convert_into(piece, &mut self.stereo)?;
            gap_total_ms += self.convert_and_emit(now_ms, clock, emit)?;
        }
        Ok(gap_total_ms)
    }

    #[cfg(any(windows, test))]
    fn push_silence(
        &mut self,
        frames: usize,
        now_ms: u64,
        clock: &mut AudioClock,
        emit: &mut dyn FnMut(AudioPacket),
    ) -> Result<u64, String> {
        let bytes = frames
            .checked_mul(self.converter.format.block_align)
            .ok_or_else(|| "silent WASAPI packet size overflow".to_string())?;
        if bytes > MAX_PACKET_BYTES {
            return Err("silent WASAPI packet exceeds safety limit".to_string());
        }
        let mut gap_total_ms = 0;
        let mut remaining = frames;
        while remaining > 0 {
            let piece = remaining.min(CONVERT_CHUNK_FRAMES);
            self.stereo.clear();
            self.stereo.resize(piece * CHANNELS, 0.0);
            gap_total_ms += self.convert_and_emit(now_ms, clock, emit)?;
            remaining -= piece;
        }
        Ok(gap_total_ms)
    }

    /// Converts `stereo` to audio-v1's rate and emits every completed packet.
    fn convert_and_emit(
        &mut self,
        now_ms: u64,
        clock: &mut AudioClock,
        emit: &mut dyn FnMut(AudioPacket),
    ) -> Result<u64, String> {
        self.resampled.clear();
        self.resampler
            .process(&self.stereo, &mut self.resampled)
            .map_err(|error| format!("resample WASAPI audio: {error}"))?;
        Ok(self.emit_resampled(now_ms, clock, emit))
    }

    /// Ends this capture without losing audio: the frames the resampler
    /// holds back and the partial packet are real sound, so they go out,
    /// the last packet completed with silence. Called when the device
    /// changes or the capture stops; a restart builds a new pipeline.
    fn finish(
        &mut self,
        now_ms: u64,
        clock: &mut AudioClock,
        emit: &mut dyn FnMut(AudioPacket),
    ) -> Result<u64, String> {
        self.resampled.clear();
        self.resampler
            .flush(&mut self.resampled)
            .map_err(|error| format!("flush WASAPI audio: {error}"))?;
        let mut gap_total_ms = self.emit_resampled(now_ms, clock, emit);
        if self.chunk_len > 0 {
            self.chunk[self.chunk_len..].fill(0);
            self.chunk_len = 0;
            let (timestamp_ms, gap_ms) = clock.next_chunk(now_ms);
            gap_total_ms += gap_ms;
            emit(AudioPacket {
                pcm_s16le: self.chunk,
                timestamp_ms,
            });
        }
        Ok(gap_total_ms)
    }

    /// Frames `resampled` into packets and emits every completed one.
    fn emit_resampled(
        &mut self,
        now_ms: u64,
        clock: &mut AudioClock,
        emit: &mut dyn FnMut(AudioPacket),
    ) -> u64 {
        let mut gap_total_ms = 0;
        for &sample in &self.resampled {
            let bytes = float_to_i16(sample).to_le_bytes();
            self.chunk[self.chunk_len..self.chunk_len + 2].copy_from_slice(&bytes);
            self.chunk_len += 2;
            if self.chunk_len < CHUNK_BYTES {
                continue;
            }
            self.chunk_len = 0;
            let (timestamp_ms, gap_ms) = clock.next_chunk(now_ms);
            gap_total_ms += gap_ms;
            if gap_ms > 0 {
                tracing::warn!(
                    target: AUDIO,
                    gap_ms,
                    "audio timestamp gap after capture interruption"
                );
            }
            emit(AudioPacket {
                pcm_s16le: self.chunk,
                timestamp_ms,
            });
        }
        gap_total_ms
    }
}

/// Adapts the mix format to stereo: decodes each sample and downmixes the
/// channel layout. Rates are the shared resampler's.
struct StereoConverter {
    format: MixFormat,
    downmix_weights: Vec<[f64; CHANNELS]>,
}

impl StereoConverter {
    fn new(format: MixFormat) -> Self {
        Self {
            format,
            downmix_weights: downmix_weights(format.channels, format.channel_mask),
        }
    }

    /// Appends `data`'s frames to `stereo` as interleaved float stereo.
    /// `stereo` must have room for them; it is never grown here.
    fn convert_into(&self, data: &[u8], stereo: &mut Vec<f32>) -> Result<(), String> {
        let frames = data.len() / self.format.block_align;
        if stereo.capacity() - stereo.len() < frames * CHANNELS {
            return Err("WASAPI conversion buffer too small".to_string());
        }
        for frame in data.chunks_exact(self.format.block_align) {
            let mut mixed = [0.0; CHANNELS];
            for (channel, weights) in self.downmix_weights.iter().enumerate() {
                let sample = f64::from(decode_sample(frame, channel, self.format.sample_format)?);
                mixed[0] += sample * weights[0];
                mixed[1] += sample * weights[1];
            }
            stereo.push(f32::from(clamp_i16(mixed[0])) / 32_768.0);
            stereo.push(f32::from(clamp_i16(mixed[1])) / 32_768.0);
        }
        Ok(())
    }
}

const SPEAKER_FRONT_LEFT: u32 = 0x0000_0001;
const SPEAKER_FRONT_RIGHT: u32 = 0x0000_0002;
const SPEAKER_FRONT_CENTER: u32 = 0x0000_0004;
const SPEAKER_LOW_FREQUENCY: u32 = 0x0000_0008;
const SPEAKER_BACK_LEFT: u32 = 0x0000_0010;
const SPEAKER_BACK_RIGHT: u32 = 0x0000_0020;
const SPEAKER_FRONT_LEFT_OF_CENTER: u32 = 0x0000_0040;
const SPEAKER_FRONT_RIGHT_OF_CENTER: u32 = 0x0000_0080;
const SPEAKER_BACK_CENTER: u32 = 0x0000_0100;
const SPEAKER_SIDE_LEFT: u32 = 0x0000_0200;
const SPEAKER_SIDE_RIGHT: u32 = 0x0000_0400;
const SPEAKER_TOP_CENTER: u32 = 0x0000_0800;
const SPEAKER_TOP_FRONT_LEFT: u32 = 0x0000_1000;
const SPEAKER_TOP_FRONT_CENTER: u32 = 0x0000_2000;
const SPEAKER_TOP_FRONT_RIGHT: u32 = 0x0000_4000;
const SPEAKER_TOP_BACK_LEFT: u32 = 0x0000_8000;
const SPEAKER_TOP_BACK_CENTER: u32 = 0x0001_0000;
const SPEAKER_TOP_BACK_RIGHT: u32 = 0x0002_0000;
const SURROUND_GAIN: f64 = std::f64::consts::FRAC_1_SQRT_2;

fn default_channel_mask(channels: usize) -> u32 {
    match channels {
        1 => SPEAKER_FRONT_CENTER,
        2 => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
        _ => 0,
    }
}

fn downmix_weights(channels: usize, channel_mask: u32) -> Vec<[f64; CHANNELS]> {
    if channels == 1 {
        return vec![[1.0, 1.0]];
    }
    if channels == 2 {
        return vec![[1.0, 0.0], [0.0, 1.0]];
    }

    (0..channels)
        .map(|channel| {
            channel_speaker(channel_mask, channels, channel)
                .map(speaker_weights)
                .unwrap_or_else(|| fallback_channel_weights(channel))
        })
        .collect()
}

fn channel_speaker(mask: u32, channels: usize, channel: usize) -> Option<u32> {
    if mask.count_ones() as usize != channels {
        return None;
    }
    let mut remaining = mask;
    for _ in 0..channel {
        remaining &= remaining - 1;
    }
    (remaining != 0).then(|| 1 << remaining.trailing_zeros())
}

fn speaker_weights(speaker: u32) -> [f64; CHANNELS] {
    match speaker {
        SPEAKER_FRONT_LEFT => [1.0, 0.0],
        SPEAKER_FRONT_RIGHT => [0.0, 1.0],
        SPEAKER_LOW_FREQUENCY => [0.5, 0.5],
        SPEAKER_BACK_LEFT
        | SPEAKER_FRONT_LEFT_OF_CENTER
        | SPEAKER_SIDE_LEFT
        | SPEAKER_TOP_FRONT_LEFT
        | SPEAKER_TOP_BACK_LEFT => [SURROUND_GAIN, 0.0],
        SPEAKER_BACK_RIGHT
        | SPEAKER_FRONT_RIGHT_OF_CENTER
        | SPEAKER_SIDE_RIGHT
        | SPEAKER_TOP_FRONT_RIGHT
        | SPEAKER_TOP_BACK_RIGHT => [0.0, SURROUND_GAIN],
        SPEAKER_FRONT_CENTER
        | SPEAKER_BACK_CENTER
        | SPEAKER_TOP_CENTER
        | SPEAKER_TOP_FRONT_CENTER
        | SPEAKER_TOP_BACK_CENTER => [SURROUND_GAIN, SURROUND_GAIN],
        _ => [0.5, 0.5],
    }
}

fn fallback_channel_weights(channel: usize) -> [f64; CHANNELS] {
    match channel {
        0 => [1.0, 0.0],
        1 => [0.0, 1.0],
        2 => [SURROUND_GAIN, SURROUND_GAIN],
        3 => [0.5, 0.5],
        channel if channel % 2 == 0 => [SURROUND_GAIN, 0.0],
        _ => [0.0, SURROUND_GAIN],
    }
}

fn clamp_i16(value: f64) -> i16 {
    value
        .round()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

fn decode_sample(frame: &[u8], channel: usize, format: SampleFormat) -> Result<i16, String> {
    let bytes_per_sample = format.bytes_per_sample();
    let offset = channel
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| "WASAPI sample offset overflow".to_string())?;
    let sample = frame
        .get(offset..offset + bytes_per_sample)
        .ok_or_else(|| "short WASAPI channel sample".to_string())?;
    match format {
        SampleFormat::Float32 => {
            let value = f32::from_le_bytes(sample.try_into().expect("validated f32 sample"));
            Ok(float_to_i16(value))
        }
        SampleFormat::SignedPcm {
            container_bytes: 2, ..
        } => Ok(i16::from_le_bytes(
            sample.try_into().expect("validated i16 sample"),
        )),
        SampleFormat::SignedPcm {
            container_bytes: 3,
            valid_bits,
        } => {
            let raw = i32::from_le_bytes([
                sample[0],
                sample[1],
                sample[2],
                if sample[2] & 0x80 == 0 { 0 } else { 0xFF },
            ]);
            Ok(scale_pcm_to_i16(raw, 24, valid_bits))
        }
        SampleFormat::SignedPcm {
            container_bytes: 4,
            valid_bits,
        } => Ok(scale_pcm_to_i16(
            i32::from_le_bytes(sample.try_into().expect("validated i32 sample")),
            32,
            valid_bits,
        )),
        SampleFormat::SignedPcm {
            container_bytes, ..
        } => Err(format!(
            "unsupported signed PCM container width: {container_bytes} bytes"
        )),
    }
}

fn scale_pcm_to_i16(raw: i32, container_bits: u16, valid_bits: u16) -> i16 {
    let valid_bits = valid_bits.clamp(1, container_bits);
    let aligned = raw >> (container_bits - valid_bits);
    if valid_bits > 16 {
        (aligned >> (valid_bits - 16)) as i16
    } else {
        (aligned << (16 - valid_bits)) as i16
    }
}

fn float_to_i16(value: f32) -> i16 {
    if !value.is_finite() {
        return 0;
    }
    if value <= -1.0 {
        i16::MIN
    } else if value >= 1.0 {
        i16::MAX
    } else {
        (value * 32_768.0).round() as i16
    }
}

#[derive(Debug, Default)]
struct AudioClock {
    next_ms: Option<u64>,
}

impl AudioClock {
    fn next_chunk(&mut self, now_ms: u64) -> (u32, u64) {
        let scheduled = self.next_ms.unwrap_or(now_ms);
        let gap = now_ms.saturating_sub(scheduled);
        let reportable_gap = if gap > CLOCK_GAP_TOLERANCE_MS { gap } else { 0 };
        let start = if reportable_gap > 0 {
            now_ms
        } else {
            scheduled
        };
        self.next_ms = Some(start.saturating_add(CHUNK_MS));
        ((start & u64::from(u32::MAX)) as u32, reportable_gap)
    }
}

#[cfg(windows)]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use windows::core::PWSTR;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_E_DEVICE_INVALIDATED,
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
        WAVE_FORMAT_PCM,
    };
    use windows::Win32::Media::KernelStreaming::{
        KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE,
    };
    use windows::Win32::Media::Multimedia::{
        KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    };

    use super::{
        now_ms, AudioClock, AudioPacket, AudioPipeline, AudioTelemetry, CaptureRunner, MixFormat,
        SampleFormat, StopSignal,
    };
    use crate::logging::AUDIO;

    const BUFFER_DURATION_100NS: i64 = 1_000_000;
    const IDLE_POLL: Duration = Duration::from_millis(3);
    const ENDPOINT_POLL: Duration = Duration::from_secs(1);
    const IDLE_AFTER: Duration = Duration::from_millis(100);

    pub struct WasapiRunner;

    impl CaptureRunner for WasapiRunner {
        fn run_once(
            &mut self,
            stop: &StopSignal,
            clock: &mut AudioClock,
            emit: &mut dyn FnMut(AudioPacket),
            telemetry: &AudioTelemetry,
        ) -> Result<(), String> {
            capture_once(stop, clock, emit, telemetry)
        }
    }

    fn capture_once(
        stop: &StopSignal,
        clock: &mut AudioClock,
        emit: &mut dyn FnMut(AudioPacket),
        telemetry: &AudioTelemetry,
    ) -> Result<(), String> {
        let _com = ComApartment::initialize()?;
        let enumerator: IMMDeviceEnumerator = unsafe {
            // SAFETY: COM is initialized on this dedicated thread and the CLSID/IID pair is
            // supplied by the windows crate.
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        }
        .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let device = default_render_endpoint(&enumerator)?;
        let active_endpoint_id = endpoint_id(&device)?;
        let client: IAudioClient = unsafe {
            // SAFETY: `device` is a live IMMDevice from the current COM apartment.
            device.Activate(CLSCTX_ALL, None)
        }
        .map_err(|error| format!("activate IAudioClient: {error}"))?;
        let mix_ptr = unsafe {
            // SAFETY: `client` is active and GetMixFormat returns CoTaskMem-owned memory.
            client.GetMixFormat()
        }
        .map_err(|error| format!("get WASAPI mix format: {error}"))?;
        let mix = CoTaskMem::new(mix_ptr)?;
        let format = parse_mix_format(mix.as_ptr())?;
        unsafe {
            // SAFETY: `mix` remains alive for Initialize and describes the endpoint's shared
            // mode format exactly as returned by GetMixFormat.
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_DURATION_100NS,
                0,
                mix.as_ptr(),
                None,
            )
        }
        .map_err(|error| format!("initialize WASAPI loopback: {error}"))?;
        let max_buffer_frames = unsafe {
            // SAFETY: Initialize succeeded and GetBufferSize only reads client-owned state.
            client.GetBufferSize()
        }
        .map_err(|error| format!("get WASAPI buffer size: {error}"))?
            as usize;
        let capture: IAudioCaptureClient = unsafe {
            // SAFETY: Initialize succeeded and IAudioCaptureClient is the documented capture
            // service for a loopback IAudioClient.
            client.GetService()
        }
        .map_err(|error| format!("get IAudioCaptureClient: {error}"))?;
        unsafe {
            // SAFETY: The client is initialized and has not been started yet.
            client.Start()
        }
        .map_err(|error| format!("start WASAPI loopback: {error}"))?;
        let _started = StartedClient(&client);

        let mut pipeline = AudioPipeline::new(format)?;
        let mut endpoint_check = Instant::now();
        let mut last_packet = Instant::now();
        let mut idle_reported = false;
        tracing::info!(
            target: AUDIO,
            endpoint = %active_endpoint_id,
            sample_rate = format.sample_rate,
            channels = format.channels,
            sample_format = ?format.sample_format,
            "WASAPI loopback started"
        );

        let result = (|| -> Result<(), String> {
            while !stop.is_requested() {
                if endpoint_check.elapsed() >= ENDPOINT_POLL {
                    let current = default_render_endpoint(&enumerator)
                        .and_then(|endpoint| endpoint_id(&endpoint))?;
                    if current != active_endpoint_id {
                        return Err(format!(
                        "default render endpoint changed from {active_endpoint_id} to {current}"
                    ));
                    }
                    endpoint_check = Instant::now();
                }
                let packet_frames = unsafe {
                    // SAFETY: `capture` is valid while `_started` keeps the audio client running.
                    capture.GetNextPacketSize()
                }
                .map_err(wasapi_error("query next audio packet"))?;
                if packet_frames == 0 {
                    if !idle_reported && last_packet.elapsed() >= IDLE_AFTER {
                        telemetry.idle_periods.fetch_add(1, Ordering::Relaxed);
                        idle_reported = true;
                        tracing::debug!(target: AUDIO, "WASAPI loopback idle: no rendered audio for 100 ms");
                    }
                    if stop.wait_timeout(IDLE_POLL) {
                        break;
                    }
                    continue;
                }

                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                unsafe {
                    // SAFETY: All output pointers are valid for writes and remain in scope through
                    // ReleaseBuffer. The returned data is read only before releasing it.
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                }
                .map_err(wasapi_error("read audio packet"))?;
                let buffer = CaptureBuffer::new(&capture, frames);
                let frame_count = frames as usize;
                if frame_count == 0 || frame_count > max_buffer_frames {
                    return Err(format!(
                    "invalid WASAPI packet frame count: {frame_count} (buffer {max_buffer_frames})"
                ));
                }
                let timestamp = now_ms();
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    telemetry.discontinuities.fetch_add(1, Ordering::Relaxed);
                    telemetry.underruns.fetch_add(1, Ordering::Relaxed);
                }
                if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 {
                    telemetry.discontinuities.fetch_add(1, Ordering::Relaxed);
                }
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    telemetry
                        .silent_frames
                        .fetch_add(u64::from(frames), Ordering::Relaxed);
                    let gap = pipeline.push_silence(frame_count, timestamp, clock, emit)?;
                    telemetry.timestamp_gap_ms.fetch_add(gap, Ordering::Relaxed);
                } else {
                    let bytes = frame_count
                        .checked_mul(format.block_align)
                        .ok_or_else(|| "WASAPI buffer size overflow".to_string())?;
                    if bytes > super::MAX_PACKET_BYTES {
                        return Err("WASAPI buffer exceeds safety limit".to_string());
                    }
                    let data = NonNull::new(data)
                        .ok_or_else(|| "WASAPI returned a null non-silent buffer".to_string())?;
                    let slice = unsafe {
                        // SAFETY: GetBuffer guarantees `frames * block_align` readable bytes until
                        // ReleaseBuffer, `data` is non-null, and the checked length is bounded.
                        std::slice::from_raw_parts(data.as_ptr(), bytes)
                    };
                    let gap = pipeline.push(slice, frame_count, timestamp, clock, emit)?;
                    telemetry.timestamp_gap_ms.fetch_add(gap, Ordering::Relaxed);
                }
                buffer.release()?;
                last_packet = Instant::now();
                idle_reported = false;
            }
            Ok(())
        })();
        // A device change or loss ends this capture here: send what the
        // filter and the partial packet hold before the restart builds a
        // new pipeline, rather than dropping up to 24 ms of sound.
        match pipeline.finish(now_ms(), clock, emit) {
            Ok(gap) => {
                telemetry.timestamp_gap_ms.fetch_add(gap, Ordering::Relaxed);
            }
            Err(error) => tracing::warn!(target: AUDIO, %error, "WASAPI flush failed"),
        }
        result
    }

    fn default_render_endpoint(enumerator: &IMMDeviceEnumerator) -> Result<IMMDevice, String> {
        unsafe {
            // SAFETY: `enumerator` is a live COM interface on its owning apartment.
            enumerator.GetDefaultAudioEndpoint(eRender, eConsole)
        }
        .map_err(|error| format!("get default console render endpoint: {error}"))
    }

    fn endpoint_id(device: &IMMDevice) -> Result<String, String> {
        let id = unsafe {
            // SAFETY: `device` is live; GetId returns a CoTaskMem-owned NUL-terminated string.
            device.GetId()
        }
        .map_err(|error| format!("get endpoint id: {error}"))?;
        let id = CoTaskMem::new(id.0)?;
        unsafe {
            // SAFETY: GetId guarantees a valid NUL-terminated UTF-16 string until freed.
            PWSTR(id.as_ptr()).to_string()
        }
        .map_err(|error| format!("decode endpoint id: {error}"))
    }

    fn parse_mix_format(ptr: *mut WAVEFORMATEX) -> Result<MixFormat, String> {
        let ptr = NonNull::new(ptr).ok_or_else(|| "null WASAPI mix format".to_string())?;
        let base = unsafe {
            // SAFETY: GetMixFormat returned at least a WAVEFORMATEX allocation. The generated
            // type is packed, so an unaligned copy avoids creating references to packed fields.
            ptr.as_ptr().read_unaligned()
        };
        let tag = u32::from(base.wFormatTag);
        let channels = usize::from(base.nChannels);
        let sample_rate = base.nSamplesPerSec;
        let block_align = usize::from(base.nBlockAlign);
        let bits = base.wBitsPerSample;
        let mut channel_mask = super::default_channel_mask(channels);
        let sample_format = if tag == WAVE_FORMAT_IEEE_FLOAT {
            if bits != 32 {
                return Err(format!("unsupported WASAPI float width: {bits}"));
            }
            SampleFormat::Float32
        } else if tag == WAVE_FORMAT_PCM {
            pcm_format(bits, bits)?
        } else if tag == WAVE_FORMAT_EXTENSIBLE {
            let required_extra = size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>();
            if usize::from(base.cbSize) < required_extra {
                return Err("short WAVEFORMATEXTENSIBLE from WASAPI".to_string());
            }
            let extended = unsafe {
                // SAFETY: cbSize proves the allocation includes WAVEFORMATEXTENSIBLE, and an
                // unaligned copy is required because the generated type is packed.
                ptr.as_ptr().cast::<WAVEFORMATEXTENSIBLE>().read_unaligned()
            };
            let samples = unsafe {
                // SAFETY: `extended` is a packed copy; read the union storage without creating
                // a potentially unaligned reference.
                std::ptr::addr_of!(extended.Samples).read_unaligned()
            };
            let valid_bits = unsafe {
                // SAFETY: The active union member for PCM/IEEE_FLOAT is wValidBitsPerSample.
                samples.wValidBitsPerSample
            };
            let sub_format = unsafe {
                // SAFETY: `extended` is packed, so copy the GUID with an unaligned read before
                // comparing or formatting it.
                std::ptr::addr_of!(extended.SubFormat).read_unaligned()
            };
            channel_mask = unsafe {
                // SAFETY: `extended` is packed, so copy the channel mask with an unaligned read.
                std::ptr::addr_of!(extended.dwChannelMask).read_unaligned()
            };
            if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                if bits != 32 {
                    return Err(format!("unsupported extensible float width: {bits}"));
                }
                SampleFormat::Float32
            } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                pcm_format(bits, valid_bits)?
            } else {
                return Err(format!(
                    "unsupported WASAPI extensible subtype: {:?}",
                    sub_format
                ));
            }
        } else {
            return Err(format!("unsupported WASAPI format tag: 0x{tag:04x}"));
        };
        MixFormat {
            sample_rate,
            channels,
            sample_format,
            block_align,
            channel_mask,
        }
        .validate()
    }

    fn pcm_format(container_bits: u16, valid_bits: u16) -> Result<SampleFormat, String> {
        match container_bits {
            16 | 24 | 32 if valid_bits > 0 && valid_bits <= container_bits => {
                Ok(SampleFormat::SignedPcm {
                    container_bytes: usize::from(container_bits / 8),
                    valid_bits,
                })
            }
            _ => Err(format!(
                "unsupported WASAPI PCM width: container={container_bits}, valid={valid_bits}"
            )),
        }
    }

    fn wasapi_error(context: &'static str) -> impl FnOnce(windows::core::Error) -> String {
        move |error| {
            if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                format!("{context}: audio device invalidated")
            } else {
                format!("{context}: {error}")
            }
        }
    }

    struct ComApartment;

    impl ComApartment {
        fn initialize() -> Result<Self, String> {
            unsafe {
                // SAFETY: This is a newly spawned dedicated capture thread and every successful
                // initialization is balanced by ComApartment::drop on the same thread.
                CoInitializeEx(None, COINIT_MULTITHREADED)
            }
            .ok()
            .map_err(|error| format!("initialize COM for WASAPI: {error}"))?;
            Ok(Self)
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe {
                // SAFETY: Balances the successful CoInitializeEx on this same thread.
                CoUninitialize();
            }
        }
    }

    struct CoTaskMem<T>(NonNull<T>);

    impl<T> CoTaskMem<T> {
        fn new(ptr: *mut T) -> Result<Self, String> {
            NonNull::new(ptr)
                .map(Self)
                .ok_or_else(|| "Windows returned a null CoTaskMem allocation".to_string())
        }

        fn as_ptr(&self) -> *mut T {
            self.0.as_ptr()
        }
    }

    impl<T> Drop for CoTaskMem<T> {
        fn drop(&mut self) {
            unsafe {
                // SAFETY: The pointer came from a Windows API documented to allocate with
                // CoTaskMemAlloc and this guard owns exactly one corresponding free.
                CoTaskMemFree(Some(self.0.as_ptr().cast::<c_void>()));
            }
        }
    }

    struct StartedClient<'a>(&'a IAudioClient);

    impl Drop for StartedClient<'_> {
        fn drop(&mut self) {
            if let Err(error) = unsafe {
                // SAFETY: The referenced client outlives this guard and Start succeeded.
                self.0.Stop()
            } {
                tracing::warn!(target: AUDIO, %error, "WASAPI stop failed");
            }
        }
    }

    struct CaptureBuffer<'a> {
        client: &'a IAudioCaptureClient,
        frames: u32,
    }

    impl<'a> CaptureBuffer<'a> {
        fn new(client: &'a IAudioCaptureClient, frames: u32) -> Self {
            Self { client, frames }
        }

        fn release(mut self) -> Result<(), String> {
            let frames = std::mem::take(&mut self.frames);
            unsafe {
                // SAFETY: This releases the exact frame count returned by the matching
                // successful GetBuffer call, once.
                self.client.ReleaseBuffer(frames)
            }
            .map_err(wasapi_error("release audio packet"))
        }
    }

    impl Drop for CaptureBuffer<'_> {
        fn drop(&mut self) {
            if self.frames != 0 {
                let _ = unsafe {
                    // SAFETY: Best-effort cleanup for a successful GetBuffer not yet released.
                    self.client.ReleaseBuffer(self.frames)
                };
                self.frames = 0;
            }
        }
    }

    /// The default render endpoint's shared-mode mix format.
    #[cfg(test)]
    fn default_mix_format() -> Result<(String, MixFormat), String> {
        let _com = ComApartment::initialize()?;
        let enumerator: IMMDeviceEnumerator = unsafe {
            // SAFETY: COM is initialized on this thread; the CLSID/IID pair is
            // supplied by the windows crate.
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        }
        .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let device = default_render_endpoint(&enumerator)?;
        let id = endpoint_id(&device)?;
        let client: IAudioClient = unsafe {
            // SAFETY: `device` is a live IMMDevice from this apartment.
            device.Activate(CLSCTX_ALL, None)
        }
        .map_err(|error| format!("activate IAudioClient: {error}"))?;
        let mix = CoTaskMem::new(
            unsafe {
                // SAFETY: GetMixFormat returns CoTaskMem-owned memory, freed by the guard.
                client.GetMixFormat()
            }
            .map_err(|error| format!("get WASAPI mix format: {error}"))?,
        )?;
        Ok((id, parse_mix_format(mix.as_ptr())?))
    }

    #[cfg(test)]
    mod hardware {
        use super::*;
        use crate::audio::{AudioTelemetry, StopSignal};
        use std::sync::Arc;
        use windows::core::{w, PCWSTR};
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_LOOP};

        /// Hardware qualification: run with `--ignored` on a Windows host.
        ///
        /// Captures this machine's default render endpoint for three seconds
        /// through the real WASAPI loop and the shared resampler while a
        /// system sound plays, and reports the mix format and what arrived.
        /// It changes no device setting.
        #[test]
        #[ignore = "plays a sound and captures the default render endpoint"]
        fn wasapi_loopback_captures_through_the_shared_resampler() {
            let (endpoint, mix) = default_mix_format().expect("a default render endpoint");
            eprintln!(
                "default render endpoint {endpoint}: {} Hz, {} channels, {:?}",
                mix.sample_rate, mix.channels, mix.sample_format
            );
            let stop = Arc::new(StopSignal::default());
            let timer = {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(3));
                    stop.request();
                })
            };
            let playing = unsafe {
                // SAFETY: a NUL-terminated literal path; SND_ASYNC returns at once.
                PlaySoundW(
                    w!("C:\\Windows\\Media\\Windows Background.wav"),
                    HMODULE::default(),
                    SND_FILENAME | SND_ASYNC | SND_LOOP,
                )
            }
            .as_bool();
            let telemetry = AudioTelemetry::default();
            let mut clock = AudioClock::default();
            let (mut packets, mut peak) = (0usize, 0i32);
            let result = WasapiRunner.run_once(
                &stop,
                &mut clock,
                &mut |packet: AudioPacket| {
                    packets += 1;
                    for pair in packet.pcm_s16le.chunks_exact(2) {
                        peak = peak.max(i32::from(i16::from_le_bytes([pair[0], pair[1]])).abs());
                    }
                },
                &telemetry,
            );
            unsafe {
                // SAFETY: a null sound stops whatever this process is playing.
                let _ = PlaySoundW(PCWSTR::null(), HMODULE::default(), SND_ASYNC);
            }
            timer.join().expect("timer thread");
            let snapshot = telemetry.snapshot();
            eprintln!(
                "capture: {result:?}, sound playing {playing}, {packets} packets (audio-v1, 20 ms), \
                 peak {peak}, silent frames {}, discontinuities {}",
                snapshot.silent_frames, snapshot.discontinuities
            );
            result.expect("the WASAPI loop ran without error");
            if playing {
                assert!(
                    packets > 50,
                    "3 s of sound should be about 150 packets, got {packets}"
                );
                assert!(peak > 0, "the sound must arrive, not silence");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(
        rate: u32,
        channels: usize,
        sample_format: SampleFormat,
        block_align: usize,
    ) -> MixFormat {
        MixFormat {
            sample_rate: rate,
            channels,
            sample_format,
            block_align,
            channel_mask: default_channel_mask(channels),
        }
    }

    #[test]
    fn converts_float_stereo_to_s16le_chunks() {
        let mut pipeline =
            AudioPipeline::new(format(SAMPLE_RATE, 2, SampleFormat::Float32, 8)).unwrap();
        let mut input = Vec::new();
        for _ in 0..CHUNK_FRAMES {
            input.extend_from_slice(&0.5f32.to_le_bytes());
            input.extend_from_slice(&(-0.5f32).to_le_bytes());
        }

        let mut clock = AudioClock::default();
        let mut packets = Vec::new();
        pipeline
            .push(&input, CHUNK_FRAMES, 1_000, &mut clock, &mut |packet| {
                packets.push(packet)
            })
            .unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].pcm_s16le.len(), CHUNK_BYTES);
        assert_eq!(&packets[0].pcm_s16le[..4], &[0, 64, 0, 192]);
        assert_eq!(packets[0].timestamp_ms, 1_000);
    }

    #[test]
    fn rejects_abusive_mix_format_dimensions() {
        assert!(format(1, 2, SampleFormat::Float32, 8).validate().is_err());
        assert!(format(SAMPLE_RATE, 33, SampleFormat::Float32, 132)
            .validate()
            .is_err());
    }

    /// Every frame `pipeline` emits for `data`, and what it holds back.
    fn run_pipeline(pipeline: &mut AudioPipeline, data: &[u8], frames: usize) -> Vec<i16> {
        let mut clock = AudioClock::default();
        let mut samples = Vec::new();
        pipeline
            .push(data, frames, 1_000, &mut clock, &mut |packet| {
                samples.extend(
                    packet
                        .pcm_s16le
                        .chunks_exact(2)
                        .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
                );
            })
            .unwrap();
        samples.extend(
            pipeline.chunk[..pipeline.chunk_len]
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
        );
        samples
    }

    #[test]
    fn duplicates_mono_and_resamples_44k1_to_48k() {
        let mono: Vec<u8> = (0..4_410i16)
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let mix = format(
            44_100,
            1,
            SampleFormat::SignedPcm {
                container_bytes: 2,
                valid_bits: 16,
            },
            2,
        );
        let mut pipeline = AudioPipeline::new(mix).unwrap();
        let latency = pipeline.resampler.latency_frames();
        let converted = run_pipeline(&mut pipeline, &mono, 4_410);
        // 0.1 s at 44.1 kHz is 4800 frames at 48 kHz; the last
        // `latency_frames` of input are still in the filter.
        let output_frames = converted.len() / CHANNELS;
        let expected = (4_410 - latency) * 48_000 / 44_100;
        assert!(
            output_frames.abs_diff(expected) <= 1,
            "{output_frames} frames, expected {expected}"
        );
        assert!(converted.chunks_exact(2).all(|frame| frame[0] == frame[1]));
    }

    #[test]
    fn downsampling_is_invariant_to_wasapi_packet_boundaries() {
        let mix = format(
            96_000,
            1,
            SampleFormat::SignedPcm {
                container_bytes: 2,
                valid_bits: 16,
            },
            2,
        );
        let input: Vec<u8> = (0..9_600i32)
            .map(|frame| ((frame % 400) * 50 - 10_000) as i16)
            .flat_map(i16::to_le_bytes)
            .collect();
        let mut contiguous = AudioPipeline::new(mix).unwrap();
        let expected = run_pipeline(&mut contiguous, &input, 9_600);

        let mut fragmented = AudioPipeline::new(mix).unwrap();
        let mut clock = AudioClock::default();
        let mut actual = Vec::new();
        for frame in input.chunks_exact(2 * 37) {
            fragmented
                .push(frame, 37, 1_000, &mut clock, &mut |packet| {
                    actual.extend(
                        packet
                            .pcm_s16le
                            .chunks_exact(2)
                            .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
                    );
                })
                .unwrap();
        }
        let rest = input.len() / (2 * 37) * (2 * 37);
        actual.extend(run_pipeline(
            &mut fragmented,
            &input[rest..],
            (input.len() - rest) / 2,
        ));

        assert!(expected.len() > CHUNK_FRAMES * CHANNELS * 4);
        assert_eq!(actual, expected);
    }

    #[test]
    fn converts_24_bit_pcm_without_overflow() {
        let positive = decode_sample(
            &[0x00, 0xFF, 0x7F],
            0,
            SampleFormat::SignedPcm {
                container_bytes: 3,
                valid_bits: 24,
            },
        )
        .unwrap();
        let negative = decode_sample(
            &[0x00, 0x00, 0x80],
            0,
            SampleFormat::SignedPcm {
                container_bytes: 3,
                valid_bits: 24,
            },
        )
        .unwrap();
        assert_eq!(positive, i16::MAX);
        assert_eq!(negative, i16::MIN);
    }

    #[test]
    fn downmixes_5_1_center_and_surround_channels() {
        let mut mix = format(
            SAMPLE_RATE,
            6,
            SampleFormat::SignedPcm {
                container_bytes: 2,
                valid_bits: 16,
            },
            12,
        );
        mix.channel_mask = SPEAKER_FRONT_LEFT
            | SPEAKER_FRONT_RIGHT
            | SPEAKER_FRONT_CENTER
            | SPEAKER_LOW_FREQUENCY
            | SPEAKER_BACK_LEFT
            | SPEAKER_BACK_RIGHT;
        let converter = StereoConverter::new(mix.validate().unwrap());
        let samples = [0i16, 0, 10_000, 0, 2_000, -2_000];
        let input: Vec<u8> = samples.into_iter().flat_map(i16::to_le_bytes).collect();

        let mut converted = Vec::with_capacity(CHANNELS);
        converter.convert_into(&input, &mut converted).unwrap();
        assert_eq!(converted, vec![8_485.0 / 32_768.0, 5_657.0 / 32_768.0]);
    }

    #[test]
    fn audio_clock_is_continuous_and_reports_restart_gap() {
        let mut clock = AudioClock::default();
        assert_eq!(clock.next_chunk(1_000), (1_000, 0));
        assert_eq!(clock.next_chunk(1_021), (1_020, 0));
        assert_eq!(clock.next_chunk(1_200), (1_200, 160));
        assert_eq!(clock.next_chunk(1_220), (1_220, 0));
    }

    struct FailingRunner {
        attempts: usize,
        stop_after: usize,
    }

    impl CaptureRunner for FailingRunner {
        fn run_once(
            &mut self,
            stop: &StopSignal,
            _clock: &mut AudioClock,
            _emit: &mut dyn FnMut(AudioPacket),
            _telemetry: &AudioTelemetry,
        ) -> Result<(), String> {
            self.attempts += 1;
            if self.attempts >= self.stop_after {
                stop.request();
            }
            Err("device invalidated".to_string())
        }
    }

    #[test]
    fn device_loss_restarts_until_shutdown_without_unbounded_delay() {
        let queue = Arc::new(LatestQueue::new(QUEUE_CAPACITY));
        let stop = Arc::new(StopSignal::default());
        let telemetry = Arc::new(AudioTelemetry::default());
        supervise(
            FailingRunner {
                attempts: 0,
                stop_after: 3,
            },
            queue,
            stop,
            Arc::clone(&telemetry),
            RestartBackoff::new(Duration::ZERO, Duration::ZERO),
        );
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.capture_errors, 2);
        assert_eq!(snapshot.restarts, 2);
    }

    #[test]
    fn stop_signal_interrupts_restart_wait() {
        let stop = Arc::new(StopSignal::default());
        let worker_stop = Arc::clone(&stop);
        let started = std::time::Instant::now();
        let join = std::thread::spawn(move || worker_stop.wait_timeout(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(5));
        stop.request();
        assert!(join.join().unwrap());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Interleaved float stereo bytes of a sine, as a float mix format
    /// delivers it: left carries the tone, right its inverse.
    fn float_stereo_sine(rate: u32, frequency: f64, frames: usize, amplitude: f64) -> Vec<u8> {
        (0..frames)
            .flat_map(|frame| {
                let value = (std::f64::consts::TAU * frequency * frame as f64 / f64::from(rate))
                    .sin()
                    * amplitude;
                let mut bytes = [0u8; 8];
                bytes[..4].copy_from_slice(&(value as f32).to_le_bytes());
                bytes[4..].copy_from_slice(&(-value as f32).to_le_bytes());
                bytes
            })
            .collect()
    }

    /// Runs `bytes` of a float stereo mix through the pipeline in 10 ms
    /// WASAPI-sized buffers and returns the left channel of every packet.
    fn capture_left(rate: u32, bytes: &[u8]) -> Vec<f64> {
        let mut pipeline = AudioPipeline::new(format(rate, 2, SampleFormat::Float32, 8)).unwrap();
        let mut clock = AudioClock::default();
        let mut left = Vec::new();
        let buffer_frames = rate as usize / 100;
        for buffer in bytes.chunks(buffer_frames * 8) {
            pipeline
                .push(buffer, buffer.len() / 8, 1_000, &mut clock, &mut |packet| {
                    left.extend(packet.pcm_s16le.chunks_exact(4).map(|frame| {
                        f64::from(i16::from_le_bytes([frame[0], frame[1]])) / 32_768.0
                    }));
                })
                .unwrap();
        }
        left
    }

    /// The level of a sine at `frequency` in `samples` at 48 kHz, fitted by
    /// least squares over the middle half (away from start-up).
    fn level_at(samples: &[f64], frequency: f64) -> f64 {
        let skip = samples.len() / 4;
        let body = &samples[skip..samples.len() - skip];
        let (mut sin_sum, mut cos_sum) = (0.0, 0.0);
        for (index, value) in body.iter().enumerate() {
            let angle =
                std::f64::consts::TAU * frequency * (index + skip) as f64 / f64::from(SAMPLE_RATE);
            sin_sum += value * angle.sin();
            cos_sum += value * angle.cos();
        }
        let count = body.len() as f64;
        (2.0 * sin_sum / count).hypot(2.0 * cos_sum / count)
    }

    fn decibels_below(level: f64, reference: f64) -> f64 {
        20.0 * (level / reference).log10()
    }

    #[test]
    fn a_24_khz_mix_keeps_its_image_80_db_below_the_tone() {
        // A 7 kHz tone mixed at 24 kHz mirrors to 17 kHz at 48 kHz. The
        // former linear interpolation left that image 14 dB down.
        let left = capture_left(24_000, &float_stereo_sine(24_000, 7_000.0, 24_000, 0.5));
        let tone = decibels_below(level_at(&left, 7_000.0), 0.5);
        let image = decibels_below(level_at(&left, 17_000.0), 0.5);
        eprintln!("24 kHz mix, 7 kHz tone: tone {tone:.2} dB, image at 17 kHz {image:.1} dB");
        assert!(tone.abs() < 0.1, "the tone came out at {tone:.2} dB");
        assert!(image <= -80.0, "the image is only {image:.1} dB down");
    }

    #[test]
    fn a_96_khz_mix_does_not_alias_into_the_wire_band() {
        // 30 kHz is above audio-v1's Nyquist frequency. The former linear
        // interpolation passed it through as an 18 kHz alias at full level.
        let left = capture_left(96_000, &float_stereo_sine(96_000, 30_000.0, 48_000, 0.5));
        let alias = decibels_below(level_at(&left, 18_000.0), 0.5);
        eprintln!("96 kHz mix, 30 kHz tone: alias at 18 kHz {alias:.1} dB");
        // 16-bit packets put the floor near -100 dB; the filter itself
        // leaves about -128 dB.
        assert!(alias <= -90.0, "the alias is only {alias:.1} dB down");
    }

    #[test]
    fn ending_a_capture_sends_what_the_filter_and_partial_packet_hold() {
        // 4000 frames at 44.1 kHz are 4354 frames at 48 kHz: four whole
        // packets and 514 frames, some still inside the filter. Ending the
        // capture (a device change) must send them, not drop them.
        let mut pipeline = AudioPipeline::new(format(44_100, 2, SampleFormat::Float32, 8)).unwrap();
        let mut clock = AudioClock::default();
        let level: Vec<u8> = (0..4_000)
            .flat_map(|_| {
                let mut frame = [0u8; 8];
                frame[..4].copy_from_slice(&0.5f32.to_le_bytes());
                frame[4..].copy_from_slice(&0.5f32.to_le_bytes());
                frame
            })
            .collect();
        let mut packets = Vec::new();
        pipeline
            .push(&level, 4_000, 1_000, &mut clock, &mut |packet| {
                packets.push(packet)
            })
            .unwrap();
        assert_eq!(packets.len(), 4, "four whole packets before the end");
        pipeline
            .finish(1_000, &mut clock, &mut |packet| packets.push(packet))
            .unwrap();
        assert_eq!(packets.len(), 5, "the partial packet goes out at the end");
        let samples: Vec<i16> = packets
            .iter()
            .flat_map(|packet| {
                packet
                    .pcm_s16le
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            })
            .collect();
        let sound = samples.iter().take_while(|&&sample| sample != 0).count() / CHANNELS;
        assert_eq!(sound, 4_354, "every frame of input, then silence");
        assert!(samples[..sound * CHANNELS]
            .iter()
            .all(|&sample| (sample - 16_384).abs() <= 2));
        assert!(samples[sound * CHANNELS..]
            .iter()
            .all(|&sample| sample == 0));
        let timestamps: Vec<u32> = packets.iter().map(|packet| packet.timestamp_ms).collect();
        assert_eq!(timestamps, [1_000, 1_020, 1_040, 1_060, 1_080]);
    }

    #[test]
    fn conversion_delay_stays_within_a_quarter_of_a_packet() {
        // audio-v1 packets are 20 ms and the Deck plays 60 ms behind
        // (docs/architecture/audio-compression.md). Rate conversion may add at
        // most 5 ms: the last frame of each packet must leave within 5 ms of
        // the input that covers it, at every rate WASAPI may mix at.
        for rate in [
            8_000, 11_025, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400,
            192_000, 384_000,
        ] {
            let mut pipeline =
                AudioPipeline::new(format(rate, 2, SampleFormat::Float32, 8)).unwrap();
            let mut clock = AudioClock::default();
            let frame = [0u8; 8];
            let mut worst_ms = 0.0f64;
            let mut packets = 0usize;
            let mut pushed = 0usize;
            while packets < 5 {
                let before = packets;
                pushed += 1;
                pipeline
                    .push(&frame, 1, 1_000, &mut clock, &mut |_| packets += 1)
                    .unwrap();
                if packets > before {
                    // Input frames that cover every output frame emitted so far.
                    let covering =
                        (packets * CHUNK_FRAMES) as f64 * f64::from(rate) / f64::from(SAMPLE_RATE);
                    let late = (pushed as f64 - covering).max(0.0) / f64::from(rate) * 1_000.0;
                    worst_ms = worst_ms.max(late);
                }
            }
            eprintln!("{rate} Hz mix: conversion delay {worst_ms:.2} ms");
            assert!(worst_ms <= 5.0, "{rate} Hz: {worst_ms:.2} ms");
        }
    }

    mod allocation {
        use super::super::*;
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        struct CountingAllocator;

        thread_local! {
            static COUNTING: Cell<bool> = const { Cell::new(false) };
            static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
        }

        // SAFETY: every operation is forwarded unchanged to `System`; the
        // wrapper only counts this thread's allocations while asked to.
        unsafe impl GlobalAlloc for CountingAllocator {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                record();
                // SAFETY: the caller's layout contract is forwarded unchanged.
                unsafe { System.alloc(layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                record();
                // SAFETY: as for `alloc`.
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                // SAFETY: the pointer and layout come from this allocator.
                unsafe { System.dealloc(ptr, layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                record();
                // SAFETY: the caller's pointer, layout and size are forwarded.
                unsafe { System.realloc(ptr, layout, new_size) }
            }
        }

        #[global_allocator]
        static GLOBAL: CountingAllocator = CountingAllocator;

        fn record() {
            COUNTING.with(|counting| {
                if counting.get() {
                    ALLOCATIONS.with(|count| count.set(count.get() + 1));
                }
            });
        }

        fn count_allocations(run: impl FnOnce()) -> usize {
            ALLOCATIONS.with(|count| count.set(0));
            COUNTING.with(|counting| counting.set(true));
            run();
            COUNTING.with(|counting| counting.set(false));
            ALLOCATIONS.with(Cell::get)
        }

        #[test]
        fn steady_capture_allocates_nothing_per_buffer_or_packet() {
            // Everything the capture loop does with a WASAPI buffer: format
            // adaptation, rate conversion, framing and the packet handed to
            // the session queue. Measured at a converted rate and at 48 kHz,
            // with sound and with silence, after one buffer of warm-up.
            for rate in [44_100u32, 48_000, 24_000] {
                let mut pipeline =
                    AudioPipeline::new(super::format(rate, 2, SampleFormat::Float32, 8)).unwrap();
                let queue = LatestQueue::new(QUEUE_CAPACITY);
                let mut clock = AudioClock::default();
                let frames = rate as usize / 100;
                let sound = super::float_stereo_sine(rate, 440.0, frames, 0.5);
                let mut emit = |packet: AudioPacket| {
                    let _ = queue.push(packet);
                };
                pipeline
                    .push(&sound, frames, 1_000, &mut clock, &mut emit)
                    .unwrap();
                let allocations = count_allocations(|| {
                    for _ in 0..50 {
                        pipeline
                            .push(&sound, frames, 1_000, &mut clock, &mut emit)
                            .unwrap();
                        pipeline
                            .push_silence(frames, 1_000, &mut clock, &mut emit)
                            .unwrap();
                    }
                });
                assert_eq!(
                    allocations, 0,
                    "{rate} Hz: 1 s of capture allocated {allocations} times"
                );
            }
        }
    }

    #[test]
    fn fixed_audio_chunk_matches_shared_wire_contract() {
        assert_eq!(CHUNK_FRAMES, 960);
        assert_eq!(CHUNK_BYTES, 3_840);
        assert_eq!(arcen_protocol::AUDIO_HEADER_SIZE, 8);
    }
}
