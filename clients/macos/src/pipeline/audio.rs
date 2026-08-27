use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use arcen_media::audio::{AudioJitterBuffer, OpusDecoder, MAX_OPUS_PACKET_BYTES, MAX_PLC_FRAMES};

use crate::protocol::messages::{AudioStreamReason, AudioStreamResultMsg};
use crate::protocol::{AudioCodec, AudioHeader};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BYTES_PER_SAMPLE: usize = 2;
/// Steady-state playback backlog we trim back to.
const TARGET_LATENCY_MS: usize = 70;
/// Backlog to build after an underrun before resuming playback.
const RECOVERY_LATENCY_MS: usize = 110;
/// Backlog level that triggers a trim (hysteresis above target so bursty
/// WebSocket delivery doesn't cause a glitch every chunk).
const TRIM_THRESHOLD_MS: usize = 150;
const MAX_BUFFER_MS: usize = 260;
const INTERLEAVED_FRAME_SAMPLES: usize = 1_920;
const MAX_TRIM_SAMPLES_PER_PUSH: usize = INTERLEAVED_FRAME_SAMPLES * 3;
const PCM_FRAME_BYTES: usize = INTERLEAVED_FRAME_SAMPLES * BYTES_PER_SAMPLE;
const DECODER_FAILURE_THRESHOLD: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFeedStatus {
    pub backend: &'static str,
    pub accepted_bytes: usize,
    pub queued_ms: usize,
    pub playback_underruns: u64,
    pub buffer_trim_events: u64,
    pub buffer_trimmed_samples: u64,
    pub feed_gap_ms: u64,
    pub max_feed_gap_ms: u64,
    /// Total Opus decode failures since stream start.
    pub decode_failures: u64,
    /// Total frames concealed via PLC since stream start.
    pub concealed_frames: u64,
    /// Current playback queue phase: "playing", "prebuffering", or "recovering".
    pub queue_phase: &'static str,
    pub note: Option<String>,
}

pub struct PcmAudioPlayer {
    output: Option<CpalOutput>,
    startup_error: Option<String>,
    stream: AudioStream,
    decoder: Option<OpusDecoder>,
    decoded: [i16; INTERLEAVED_FRAME_SAMPLES],
    jitter: AudioJitterBuffer,
    decode_failures: u64,
    consecutive_decode_failures: u8,
    concealed_frames: u64,
    dropped_frames: u64,
    last_feed_at: Option<Instant>,
    last_feed_gap_ms: u64,
    max_feed_gap_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AudioStream {
    #[default]
    Legacy,
    Disabled,
    PcmV1,
    OpusV1,
}

impl Default for PcmAudioPlayer {
    fn default() -> Self {
        Self {
            output: None,
            startup_error: None,
            stream: AudioStream::Legacy,
            decoder: None,
            decoded: [0; INTERLEAVED_FRAME_SAMPLES],
            jitter: AudioJitterBuffer::default(),
            decode_failures: 0,
            consecutive_decode_failures: 0,
            concealed_frames: 0,
            dropped_frames: 0,
            last_feed_at: None,
            last_feed_gap_ms: 0,
            max_feed_gap_ms: 0,
        }
    }
}

impl PcmAudioPlayer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_stream_result(&mut self, result: &AudioStreamResultMsg) {
        let next_stream = match result.config.as_ref().filter(|config| {
            result.enabled
                && result.reason == AudioStreamReason::Enabled
                && config.is_valid_v1()
                && !config.fec
                && !config.dtx
        }) {
            Some(config) if config.codec == AudioCodec::Opus => AudioStream::OpusV1,
            Some(config) if config.codec == AudioCodec::Pcm => AudioStream::PcmV1,
            _ => AudioStream::Disabled,
        };
        if next_stream == self.stream
            && (!matches!(next_stream, AudioStream::OpusV1) || self.decoder.is_some())
        {
            return;
        }
        self.decoder = None;
        self.jitter.reset();
        self.consecutive_decode_failures = 0;
        if let Some(output) = &self.output {
            output.reset();
        }
        self.stream = match next_stream {
            AudioStream::OpusV1 => match OpusDecoder::new() {
                Ok(decoder) => {
                    self.decoder = Some(decoder);
                    AudioStream::OpusV1
                }
                Err(_) => {
                    self.decode_failures = self.decode_failures.saturating_add(1);
                    AudioStream::Disabled
                }
            },
            stream => stream,
        };
    }

    pub fn feed(&mut self, header: AudioHeader, payload: &[u8]) -> AudioFeedStatus {
        self.observe_feed_at(Instant::now());
        let mut note = (matches!(self.stream, AudioStream::Legacy)
            && header.codec == AudioCodec::Opus)
            .then(|| {
                "legacy Opus tag interpreted as 48 kHz stereo PCM compatibility mode".to_string()
            });
        match self.stream {
            AudioStream::Disabled => {
                return self.status(0, Some("audio stream disabled by negotiation".to_string()));
            }
            AudioStream::PcmV1
                if header.codec != AudioCodec::Pcm || payload.len() != PCM_FRAME_BYTES =>
            {
                self.dropped_frames = self.dropped_frames.saturating_add(1);
                return self.status(0, Some("audio-v1 PCM packet shape is invalid".to_string()));
            }
            AudioStream::OpusV1
                if header.codec != AudioCodec::Opus
                    || payload.is_empty()
                    || payload.len() > MAX_OPUS_PACKET_BYTES =>
            {
                self.dropped_frames = self.dropped_frames.saturating_add(1);
                return self.status(0, Some("audio-v1 Opus packet shape is invalid".to_string()));
            }
            _ => {}
        }
        let action = if matches!(self.stream, AudioStream::Legacy) {
            None
        } else {
            Some(self.jitter.observe(header.timestamp_ms))
        };
        if action.is_some_and(|action| !action.accept) {
            self.dropped_frames = self.dropped_frames.saturating_add(1);
            return self.status(
                0,
                Some("audio-v1 late or duplicate packet dropped".to_string()),
            );
        }
        if action.is_some_and(|action| action.reset) {
            if let Some(decoder) = self.decoder.as_mut() {
                if decoder.reset().is_err() {
                    record_decoder_failure(
                        &mut self.decoder,
                        &mut self.stream,
                        &mut self.decode_failures,
                        &mut self.consecutive_decode_failures,
                    );
                }
            }
            if let Some(output) = &self.output {
                output.reset();
            }
        }

        if self.output.is_none() && self.startup_error.is_none() {
            match CpalOutput::start() {
                Ok(output) => self.output = Some(output),
                Err(error) => self.startup_error = Some(error),
            }
        }

        let Some(output) = &self.output else {
            return self.status(0, note);
        };

        if let Some(action) = action {
            let plc_frames = action.plc_frames.min(MAX_PLC_FRAMES);
            if plc_frames > 0 && matches!(self.stream, AudioStream::OpusV1) {
                let Some(decoder) = self.decoder.as_mut() else {
                    return self.status(0, Some("audio-v1 decoder unavailable".to_string()));
                };
                for _ in 0..plc_frames {
                    if decoder.decode_plc(&mut self.decoded).is_err() {
                        let recovered = record_decoder_failure(
                            &mut self.decoder,
                            &mut self.stream,
                            &mut self.decode_failures,
                            &mut self.consecutive_decode_failures,
                        );
                        note = Some(if recovered {
                            "audio-v1 PLC failed; decoder recovery is bounded".to_string()
                        } else {
                            "audio-v1 PLC failed; audio disabled".to_string()
                        });
                        break;
                    }
                    self.consecutive_decode_failures = 0;
                    output.push_samples(&self.decoded);
                    self.concealed_frames = self.concealed_frames.saturating_add(1);
                }
            }
        }

        let accepted_bytes = match (self.stream, header.codec) {
            (AudioStream::Legacy, AudioCodec::Pcm) => output.push_s16le(payload),
            (AudioStream::Legacy, AudioCodec::Opus) => output.push_s16le(payload),
            (AudioStream::PcmV1, AudioCodec::Pcm) => output.push_s16le(payload),
            (AudioStream::OpusV1, AudioCodec::Opus) => {
                let Some(decoder) = self.decoder.as_mut() else {
                    return self.status(0, Some("audio-v1 decoder unavailable".to_string()));
                };
                match decoder.decode(payload, &mut self.decoded) {
                    Ok(()) => {
                        self.consecutive_decode_failures = 0;
                        output.push_samples(&self.decoded)
                    }
                    Err(_) => {
                        let recovered = record_decoder_failure(
                            &mut self.decoder,
                            &mut self.stream,
                            &mut self.decode_failures,
                            &mut self.consecutive_decode_failures,
                        );
                        let note = if recovered {
                            "audio-v1 packet decode failed; decoder recovery is bounded"
                        } else {
                            "audio-v1 packet decode failed; audio disabled"
                        };
                        return self.status(0, Some(note.to_string()));
                    }
                }
            }
            (AudioStream::Disabled, _) => unreachable!("disabled stream returned before playout"),
            _ => {
                self.dropped_frames = self.dropped_frames.saturating_add(1);
                return self.status(
                    0,
                    Some("audio codec differs from negotiated mode".to_string()),
                );
            }
        };
        self.status_with_output(output, accepted_bytes, note)
    }

    fn status(&self, accepted_bytes: usize, note: Option<String>) -> AudioFeedStatus {
        match &self.output {
            Some(output) => self.status_with_output(output, accepted_bytes, note),
            None => AudioFeedStatus {
                backend: "none",
                accepted_bytes,
                queued_ms: 0,
                playback_underruns: 0,
                buffer_trim_events: 0,
                buffer_trimmed_samples: 0,
                feed_gap_ms: self.last_feed_gap_ms,
                max_feed_gap_ms: self.max_feed_gap_ms,
                decode_failures: self.decode_failures,
                concealed_frames: self.concealed_frames,
                queue_phase: "prebuffering",
                note: combine_notes(self.startup_error.clone(), note),
            },
        }
    }

    fn status_with_output(
        &self,
        output: &CpalOutput,
        accepted_bytes: usize,
        note: Option<String>,
    ) -> AudioFeedStatus {
        let playback = output.snapshot();
        let queue_phase = if playback.prebuffering {
            if playback.recovering {
                "recovering"
            } else {
                "prebuffering"
            }
        } else {
            "playing"
        };
        AudioFeedStatus {
            backend: "cpal-coreaudio-48k-stereo",
            accepted_bytes,
            queued_ms: playback.queued_ms,
            playback_underruns: playback.underruns,
            buffer_trim_events: playback.trim_events,
            buffer_trimmed_samples: playback.trimmed_samples,
            feed_gap_ms: self.last_feed_gap_ms,
            max_feed_gap_ms: self.max_feed_gap_ms,
            decode_failures: self.decode_failures,
            concealed_frames: self.concealed_frames,
            queue_phase,
            note,
        }
    }

    fn observe_feed_at(&mut self, now: Instant) {
        self.last_feed_gap_ms = self.last_feed_at.map_or(0, |previous| {
            u64::try_from(now.duration_since(previous).as_millis()).unwrap_or(u64::MAX)
        });
        self.max_feed_gap_ms = self.max_feed_gap_ms.max(self.last_feed_gap_ms);
        self.last_feed_at = Some(now);
    }
}

fn record_decoder_failure(
    decoder: &mut Option<OpusDecoder>,
    stream: &mut AudioStream,
    total_failures: &mut u64,
    consecutive_failures: &mut u8,
) -> bool {
    *total_failures = total_failures.saturating_add(1);
    *consecutive_failures = consecutive_failures.saturating_add(1);
    if *consecutive_failures < DECODER_FAILURE_THRESHOLD {
        return true;
    }
    *consecutive_failures = 0;
    match OpusDecoder::new() {
        Ok(replacement) => {
            *decoder = Some(replacement);
            tracing::warn!(
                target: crate::logging::target::AUDIO,
                total_failures = *total_failures,
                "Opus decoder recreated after consecutive failures"
            );
            true
        }
        Err(_) => {
            *decoder = None;
            *stream = AudioStream::Disabled;
            tracing::warn!(
                target: crate::logging::target::AUDIO,
                total_failures = *total_failures,
                "Opus decoder disabled: could not recreate after consecutive failures"
            );
            false
        }
    }
}

struct CpalOutput {
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PlaybackSnapshot {
    queued_ms: usize,
    underruns: u64,
    trim_events: u64,
    trimmed_samples: u64,
    prebuffering: bool,
    recovering: bool,
}

impl CpalOutput {
    fn start() -> Result<Self, String> {
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
    if queue.samples.len() > trim_threshold_samples() {
        let trimmed = queue
            .samples
            .len()
            .saturating_sub(recovery_latency_samples())
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

fn combine_notes(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (Some(first), None) => Some(first),
        (None, Some(second)) => Some(second),
        (None, None) => None,
    }
}

const fn max_samples() -> usize {
    SAMPLE_RATE as usize * CHANNELS as usize * MAX_BUFFER_MS / 1000
}

const fn target_latency_samples() -> usize {
    SAMPLE_RATE as usize * CHANNELS as usize * TARGET_LATENCY_MS / 1000
}

const fn recovery_latency_samples() -> usize {
    SAMPLE_RATE as usize * CHANNELS as usize * RECOVERY_LATENCY_MS / 1000
}

fn prebuffer_target_samples(queue: &PlaybackQueue) -> usize {
    if queue.recovering {
        recovery_latency_samples()
    } else {
        target_latency_samples()
    }
}

const fn trim_threshold_samples() -> usize {
    SAMPLE_RATE as usize * CHANNELS as usize * TRIM_THRESHOLD_MS / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_legacy_opus_tag_as_pcm_contract() {
        let mut player = PcmAudioPlayer {
            startup_error: Some("skip device open in test".to_string()),
            ..PcmAudioPlayer::default()
        };
        let status = player.feed(
            AudioHeader {
                codec: AudioCodec::Opus,
                timestamp_ms: 1,
            },
            &[0, 0, 1, 0],
        );
        assert_eq!(status.backend, "none");
        assert_eq!(status.accepted_bytes, 0);
        assert!(status
            .note
            .expect("expected legacy tag note")
            .contains("compatibility mode"));
    }

    #[test]
    fn invalid_v1_result_disables_instead_of_guessing() {
        let mut player = PcmAudioPlayer::default();
        player.set_stream_result(&AudioStreamResultMsg::disabled(
            AudioStreamReason::InvalidCapabilities,
        ));
        assert_eq!(player.stream, AudioStream::Disabled);
    }

    #[test]
    fn negotiated_v1_decoder_accepts_a_real_synthetic_opus_packet_without_device_io() {
        let policy = arcen_media::audio::AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let result = policy
            .resolve(Some(&policy.capabilities()), true, 128)
            .result()
            .expect("audio-v1 result");
        let mut player = PcmAudioPlayer::default();
        player.set_stream_result(&result);
        assert_eq!(player.stream, AudioStream::OpusV1);

        let mut input = [0i16; INTERLEAVED_FRAME_SAMPLES];
        for (index, sample) in input.iter_mut().enumerate() {
            *sample = ((index as i32 * 37) % 16_000 - 8_000) as i16;
        }
        let mut encoder =
            arcen_media::audio::OpusEncoder::new(arcen_media::audio::AudioBitrateTier::Kbps128)
                .expect("encoder");
        let mut packet = [0u8; arcen_media::audio::MAX_OPUS_PACKET_BYTES];
        let encoded = encoder.encode(&input, &mut packet).expect("encode");
        player
            .decoder
            .as_mut()
            .expect("negotiated decoder")
            .decode(&packet[..encoded], &mut player.decoded)
            .expect("Deck decode");
        assert!(player.decoded.iter().any(|sample| *sample != 0));
    }

    #[test]
    fn negotiated_v1_rejects_wrong_codec_and_packet_bounds_before_device_io() {
        let policy = arcen_media::audio::AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let mut player = PcmAudioPlayer::default();
        player.set_stream_result(
            &policy
                .resolve(Some(&policy.capabilities()), true, 128)
                .result()
                .unwrap(),
        );

        for (codec, payload) in [
            (AudioCodec::Pcm, vec![0; PCM_FRAME_BYTES]),
            (AudioCodec::Opus, Vec::new()),
            (
                AudioCodec::Opus,
                vec![0; arcen_media::audio::MAX_OPUS_PACKET_BYTES + 1],
            ),
        ] {
            let status = player.feed(
                AudioHeader {
                    codec,
                    timestamp_ms: 1,
                },
                &payload,
            );
            assert_eq!(status.accepted_bytes, 0);
            assert!(status.note.unwrap().contains("shape is invalid"));
        }
        assert_eq!(player.dropped_frames, 3);
    }

    #[test]
    fn decoder_state_is_recreated_after_three_consecutive_failures() {
        let mut decoder = Some(OpusDecoder::new().expect("decoder"));
        let mut stream = AudioStream::OpusV1;
        let mut total = 0;
        let mut consecutive = 0;

        assert!(record_decoder_failure(
            &mut decoder,
            &mut stream,
            &mut total,
            &mut consecutive
        ));
        assert!(record_decoder_failure(
            &mut decoder,
            &mut stream,
            &mut total,
            &mut consecutive
        ));
        assert!(record_decoder_failure(
            &mut decoder,
            &mut stream,
            &mut total,
            &mut consecutive
        ));
        assert!(decoder.is_some());
        assert_eq!(stream, AudioStream::OpusV1);
        assert_eq!(total, 3);
        assert_eq!(consecutive, 0);
    }

    #[test]
    fn bitrate_only_result_preserves_decoder_and_jitter_state() {
        let policy = arcen_media::audio::AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let mut result = policy
            .resolve(Some(&policy.capabilities()), true, 128)
            .result()
            .expect("audio-v1 result");
        let mut player = PcmAudioPlayer::default();
        player.set_stream_result(&result);
        let decoder = player.decoder.as_ref().expect("decoder") as *const OpusDecoder;
        let first = player.jitter.observe(1_000);
        assert!(first.accept);
        player.consecutive_decode_failures = 2;

        result.config.as_mut().expect("config").bitrate =
            crate::protocol::messages::AudioBitrateTierMsg::Kbps64;
        player.set_stream_result(&result);

        assert_eq!(
            player.decoder.as_ref().expect("decoder") as *const OpusDecoder,
            decoder
        );
        assert_eq!(player.consecutive_decode_failures, 2);
        let next = player.jitter.observe(1_020);
        assert!(next.accept);
        assert!(!next.reset);
    }

    fn playback_queue(samples: impl IntoIterator<Item = i16>, prebuffering: bool) -> PlaybackQueue {
        PlaybackQueue {
            samples: samples.into_iter().collect(),
            prebuffering,
            recovering: false,
            underruns: 0,
            trim_events: 0,
            trimmed_samples: 0,
        }
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
        let original = trim_threshold_samples() + 2;
        let mut queue = playback_queue(std::iter::repeat_n(7, original), false);
        trim_excess(&mut queue);

        let expected_trimmed =
            (original - recovery_latency_samples()).min(MAX_TRIM_SAMPLES_PER_PUSH);
        assert_eq!(queue.samples.len(), original - expected_trimmed);
        assert_eq!(queue.trim_events, 1);
        assert_eq!(queue.trimmed_samples, expected_trimmed as u64);

        trim_excess(&mut queue);
        assert_eq!(queue.trim_events, 1);
        assert_eq!(queue.trimmed_samples, expected_trimmed as u64);
    }

    #[test]
    fn bursty_delivery_keeps_jitter_reserve_while_bounding_latency() {
        let mut queue = playback_queue(std::iter::repeat_n(7, target_latency_samples()), false);

        for _ in 0..8 {
            trim_excess(&mut queue);
            queue
                .samples
                .extend(std::iter::repeat_n(7, INTERLEAVED_FRAME_SAMPLES));
        }

        assert!(queue.samples.len() >= trim_threshold_samples());
        assert!(queue.samples.len() <= trim_threshold_samples() + INTERLEAVED_FRAME_SAMPLES);
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
            trim_threshold_samples() + INTERLEAVED_FRAME_SAMPLES,
        ));
        let before = queue.samples.len();
        trim_excess(&mut queue);
        assert_eq!(queue.samples.len(), before);
        assert_eq!(queue.trim_events, 0);
    }

    #[test]
    fn feed_gap_telemetry_uses_media_worker_delivery_time() {
        let mut player = PcmAudioPlayer::default();
        let start = Instant::now();
        player.observe_feed_at(start);
        assert_eq!(player.last_feed_gap_ms, 0);
        player.observe_feed_at(start + std::time::Duration::from_millis(37));
        assert_eq!(player.last_feed_gap_ms, 37);
        assert_eq!(player.max_feed_gap_ms, 37);
        player.observe_feed_at(start + std::time::Duration::from_millis(57));
        assert_eq!(player.last_feed_gap_ms, 20);
        assert_eq!(player.max_feed_gap_ms, 37);
    }
}
