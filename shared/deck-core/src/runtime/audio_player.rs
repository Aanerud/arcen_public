// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host audio playout logic: the negotiated stream (legacy PCM, audio-v1
//! PCM or Opus), Opus decode with bounded decoder recreation, the jitter
//! buffer and loss concealment, and the feed statistics (ADR 0014).
//!
//! Moved from the macOS Deck (ADR 0024 extraction step 5). The platform
//! supplies only the output device ([`AudioOutputDevice`]): it opens the
//! default output at 48 kHz stereo and keeps the playback queue its
//! callback drains, so every Deck negotiates, decodes and conceals the
//! same way.
#![allow(
    clippy::collapsible_if,
    clippy::match_same_arms,
    clippy::single_match_else,
    clippy::too_many_lines,
    reason = "the audio player moved verbatim from the macOS Deck (ADR 0024 step 5); \
              these lints are tightened in follow-ups, never by changing behaviour in the move"
)]

use std::time::Instant;

use arcen_media::audio::{AudioJitterBuffer, MAX_OPUS_PACKET_BYTES, MAX_PLC_FRAMES, OpusDecoder};

use arcen_protocol::messages::{AudioStreamReason, AudioStreamResultMsg};
use arcen_protocol::{AudioCodec, AudioHeader};

use crate::runtime::audio_worker::{AudioFeedStatus, AudioPlayback};

const BYTES_PER_SAMPLE: usize = 2;
/// Interleaved samples in one 20 ms stereo frame at 48 kHz.
pub const INTERLEAVED_FRAME_SAMPLES: usize = 1_920;
const PCM_FRAME_BYTES: usize = INTERLEAVED_FRAME_SAMPLES * BYTES_PER_SAMPLE;
const DECODER_FAILURE_THRESHOLD: u8 = 3;

/// The playback queue's state, for the stream statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackSnapshot {
    /// Audio waiting to play.
    pub queued_ms: usize,
    /// Times the queue ran dry while playing.
    pub underruns: u64,
    /// Latency trims.
    pub trim_events: u64,
    /// Samples removed by those trims.
    pub trimmed_samples: u64,
    /// Building the reserve before playing.
    pub prebuffering: bool,
    /// Rebuilding the reserve after an underrun.
    pub recovering: bool,
}

/// The platform's audio output: the default output device at 48 kHz
/// stereo, playing from its own playback queue (interleaved signed 16-bit
/// samples in, the device's format out). Opened on the audio worker's
/// thread when the first frame plays.
pub trait AudioOutputDevice: Sized {
    /// Opens and starts the default output.
    ///
    /// # Errors
    ///
    /// A human-readable reason, shown in the stream statistics; the player
    /// then plays nothing and does not retry.
    fn open() -> Result<Self, String>;

    /// The backend label in the stream statistics, for example
    /// `cpal-coreaudio-48k-stereo`.
    fn backend(&self) -> &'static str;

    /// Queues little-endian signed 16-bit interleaved PCM; returns the
    /// bytes accepted.
    fn push_s16le(&self, payload: &[u8]) -> usize;

    /// Queues decoded interleaved samples; returns the bytes accepted.
    fn push_samples(&self, samples: &[i16]) -> usize;

    /// Drops everything queued and prebuffers again (a stream change or a
    /// jitter reset, never an underrun).
    fn reset(&self);

    /// The queue's state now.
    fn snapshot(&self) -> PlaybackSnapshot;
}

/// Plays the host's audio stream through a platform [`AudioOutputDevice`].
pub struct PcmAudioPlayer<O: AudioOutputDevice> {
    output: Option<O>,
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
    /// Protocol v5: frames may arrive on the audio lane before
    /// `audio_stream_result` arrives on the control lane. They are dropped
    /// until it does, never guessed at (Opus played as PCM is noise).
    AwaitingResult,
    Disabled,
    PcmV1,
    OpusV1,
}

impl<O: AudioOutputDevice> std::fmt::Debug for PcmAudioPlayer<O> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PcmAudioPlayer")
            .field("stream", &self.stream)
            .field("output_open", &self.output.is_some())
            .finish_non_exhaustive()
    }
}

impl<O: AudioOutputDevice> Default for PcmAudioPlayer<O> {
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

impl<O: AudioOutputDevice> PcmAudioPlayer<O> {
    /// A player for the legacy (pre-negotiation) stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A player for a lane session (protocol v5): it plays nothing until
    /// the Pier's `audio_stream_result` says what the frames are.
    #[must_use]
    pub fn awaiting_stream_result() -> Self {
        Self {
            stream: AudioStream::AwaitingResult,
            ..Self::default()
        }
    }

    /// Applies the Pier's answer to the audio stream request.
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

    /// Plays one frame and returns the playback state.
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
            AudioStream::AwaitingResult => {
                self.dropped_frames = self.dropped_frames.saturating_add(1);
                return self.status(
                    0,
                    Some("audio frame before audio_stream_result dropped".to_string()),
                );
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
            match O::open() {
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
            (AudioStream::Disabled | AudioStream::AwaitingResult, _) => {
                unreachable!("disabled or unresolved stream returned before playout")
            }
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
        output: &O,
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
            backend: output.backend(),
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
                target: crate::log_target::AUDIO,
                total_failures = *total_failures,
                "Opus decoder recreated after consecutive failures"
            );
            true
        }
        Err(_) => {
            *decoder = None;
            *stream = AudioStream::Disabled;
            tracing::warn!(
                target: crate::log_target::AUDIO,
                total_failures = *total_failures,
                "Opus decoder disabled: could not recreate after consecutive failures"
            );
            false
        }
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

impl<O: AudioOutputDevice> AudioPlayback for PcmAudioPlayer<O> {
    fn set_stream_result(&mut self, result: &AudioStreamResultMsg) {
        Self::set_stream_result(self, result);
    }

    fn feed(&mut self, header: AudioHeader, payload: &[u8]) -> AudioFeedStatus {
        Self::feed(self, header, payload)
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::ref_as_ptr,
    clippy::similar_names,
    reason = "test fixtures moved verbatim with the player"
)]
mod tests {
    use super::*;

    /// No audio device in unit tests: opening fails, so nothing plays.
    struct NoDevice;

    impl AudioOutputDevice for NoDevice {
        fn open() -> Result<Self, String> {
            Err("no audio device in unit tests".to_owned())
        }

        fn backend(&self) -> &'static str {
            "test"
        }

        fn push_s16le(&self, _payload: &[u8]) -> usize {
            0
        }

        fn push_samples(&self, _samples: &[i16]) -> usize {
            0
        }

        fn reset(&self) {}

        fn snapshot(&self) -> PlaybackSnapshot {
            PlaybackSnapshot::default()
        }
    }

    type PcmAudioPlayer = super::PcmAudioPlayer<NoDevice>;

    #[test]
    fn a_lane_player_drops_audio_until_the_stream_result_arrives() {
        let mut player = PcmAudioPlayer::awaiting_stream_result();
        let opus = [0x55_u8; 40];
        let status = player.feed(
            AudioHeader {
                codec: AudioCodec::Opus,
                timestamp_ms: 0,
            },
            &opus,
        );
        assert_eq!(status.accepted_bytes, 0);
        assert_eq!(player.dropped_frames, 1);
        assert_eq!(player.stream, AudioStream::AwaitingResult);
        player.set_stream_result(&AudioStreamResultMsg::disabled(
            AudioStreamReason::CaptureUnavailable,
        ));
        assert_eq!(player.stream, AudioStream::Disabled);
    }

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
        assert!(
            status
                .note
                .expect("expected legacy tag note")
                .contains("compatibility mode")
        );
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
            arcen_protocol::messages::AudioBitrateTierMsg::Kbps64;
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
