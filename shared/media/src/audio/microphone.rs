//! Pure client-to-host microphone-v1 negotiation, decode, and bounded playout.

use std::time::Duration;

use arcen_protocol::messages::{
    AudioBitrateTierMsg, MICROPHONE_PROTOCOL_VERSION, MicrophoneCapabilitiesMsg,
    MicrophoneStreamConfigMsg, MicrophoneStreamReason, MicrophoneStreamResultMsg,
};
use arcen_protocol::{AudioCodec, MICROPHONE_PCM_BYTES, MicrophoneHeader};
use zeroize::Zeroize;

use super::{
    AUDIO_V1_FRAME_DURATION_MS, AUDIO_V1_SAMPLE_RATE_HZ, AudioBitrateTier, MICROPHONE_V1_CHANNELS,
};
#[cfg(feature = "audio-opus")]
use super::{AudioFrameSpec, MAX_OPUS_PACKET_BYTES, OpusDecoder, OpusError};

pub const MICROPHONE_V1_FRAME_SAMPLES: usize = 960;
pub const MICROPHONE_JITTER_TARGET_FRAMES: usize = 3;
pub const MICROPHONE_JITTER_MAX_FRAMES: usize = 10;
const MICROPHONE_JITTER_TRIM_THRESHOLD_FRAMES: usize = 5;
/// Bounded health-counter cadence shared by Deck and both Piers.
pub const MICROPHONE_STATS_INTERVAL: Duration = Duration::from_secs(10);
/// Fixed microphone-v1 mono PCM bandwidth.
pub const MICROPHONE_PCM_BITRATE_KBPS: u32 = 768;
const MAX_GAP_FILL_FRAMES: u32 = 9;

/// Locally available microphone-v1 wire codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicrophoneCodecAvailability {
    pub opus: bool,
    pub pcm: bool,
}

/// Host policy facts needed to advertise and resolve microphone-v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicrophonePolicy {
    pub operator_enabled: bool,
    pub backend_available: bool,
    pub codecs: MicrophoneCodecAvailability,
}

impl MicrophonePolicy {
    #[must_use]
    pub fn capabilities(self) -> Option<MicrophoneCapabilitiesMsg> {
        if !self.operator_enabled || !self.backend_available {
            return None;
        }
        let mut codecs = Vec::with_capacity(2);
        if self.codecs.opus {
            codecs.push(AudioCodec::Opus);
        }
        if self.codecs.pcm {
            codecs.push(AudioCodec::Pcm);
        }
        (!codecs.is_empty()).then_some(MicrophoneCapabilitiesMsg {
            protocol_version: MICROPHONE_PROTOCOL_VERSION,
            codecs,
            sample_rate_hz: AUDIO_V1_SAMPLE_RATE_HZ,
            channels: MICROPHONE_V1_CHANNELS,
            frame_duration_ms: AUDIO_V1_FRAME_DURATION_MS,
            fec: false,
            dtx: false,
        })
    }

    #[must_use]
    pub fn resolve(
        self,
        peer: Option<&MicrophoneCapabilitiesMsg>,
        client_enabled: bool,
        generation: u32,
        requested_kbps: u32,
    ) -> ResolvedMicrophoneStream {
        if !self.operator_enabled {
            return ResolvedMicrophoneStream::disabled(
                generation,
                MicrophoneStreamReason::DisabledByOperator,
            );
        }
        if !self.backend_available {
            return ResolvedMicrophoneStream::disabled(
                generation,
                MicrophoneStreamReason::BackendUnavailable,
            );
        }
        if !client_enabled {
            return ResolvedMicrophoneStream::disabled(
                generation,
                MicrophoneStreamReason::DisabledByClient,
            );
        }
        let Some(peer) = peer else {
            return ResolvedMicrophoneStream::disabled(
                generation,
                MicrophoneStreamReason::NotNegotiated,
            );
        };
        if peer.protocol_version != MICROPHONE_PROTOCOL_VERSION {
            return ResolvedMicrophoneStream::disabled(
                generation,
                MicrophoneStreamReason::VersionMismatch,
            );
        }
        if generation == 0 || !peer.is_valid_v1() {
            return ResolvedMicrophoneStream::disabled(
                generation,
                MicrophoneStreamReason::InvalidCapabilities,
            );
        }
        let codec = peer.codecs.iter().copied().find(|codec| match codec {
            AudioCodec::Opus => self.codecs.opus,
            AudioCodec::Pcm => self.codecs.pcm,
        });
        let Some(codec) = codec else {
            return ResolvedMicrophoneStream::disabled(
                generation,
                MicrophoneStreamReason::NoCommonCodec,
            );
        };
        let bitrate = match codec {
            AudioCodec::Opus => {
                let bitrate = AudioBitrateTier::from_ceiling_kbps(requested_kbps);
                if matches!(bitrate, AudioBitrateTier::Off) {
                    return ResolvedMicrophoneStream::disabled(
                        generation,
                        MicrophoneStreamReason::InvalidCapabilities,
                    );
                }
                bitrate
            }
            AudioCodec::Pcm => AudioBitrateTier::Off,
        };
        ResolvedMicrophoneStream {
            codec: Some(codec),
            bitrate,
            generation,
            reason: MicrophoneStreamReason::Enabled,
        }
    }
}

/// Codec-aware microphone bitrate without assigning an Opus tier to fixed PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedMicrophoneBitrate {
    Opus(AudioBitrateTier),
    PcmFixed,
}

/// Attachment-scoped selected microphone stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedMicrophoneStream {
    pub codec: Option<AudioCodec>,
    /// Deployed wire compatibility tier; prefer [`Self::resolved_bitrate`].
    pub bitrate: AudioBitrateTier,
    pub generation: u32,
    pub reason: MicrophoneStreamReason,
}

impl ResolvedMicrophoneStream {
    #[must_use]
    pub const fn disabled(generation: u32, reason: MicrophoneStreamReason) -> Self {
        Self {
            codec: None,
            bitrate: AudioBitrateTier::Off,
            generation,
            reason,
        }
    }

    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.generation != 0
            && match self.codec {
                Some(AudioCodec::Opus) => !matches!(self.bitrate, AudioBitrateTier::Off),
                Some(AudioCodec::Pcm) => matches!(self.bitrate, AudioBitrateTier::Off),
                None => false,
            }
    }

    #[must_use]
    pub const fn resolved_bitrate(self) -> Option<ResolvedMicrophoneBitrate> {
        match self.codec {
            Some(AudioCodec::Opus)
                if self.generation != 0 && !matches!(self.bitrate, AudioBitrateTier::Off) =>
            {
                Some(ResolvedMicrophoneBitrate::Opus(self.bitrate))
            }
            Some(AudioCodec::Pcm)
                if self.generation != 0 && matches!(self.bitrate, AudioBitrateTier::Off) =>
            {
                Some(ResolvedMicrophoneBitrate::PcmFixed)
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn result(self) -> MicrophoneStreamResultMsg {
        let Some(codec) = self.codec else {
            return MicrophoneStreamResultMsg::disabled(self.reason);
        };
        MicrophoneStreamResultMsg::enabled(
            MicrophoneStreamConfigMsg {
                protocol_version: MICROPHONE_PROTOCOL_VERSION,
                codec,
                sample_rate_hz: AUDIO_V1_SAMPLE_RATE_HZ,
                channels: MICROPHONE_V1_CHANNELS,
                frame_duration_ms: AUDIO_V1_FRAME_DURATION_MS,
                bitrate: AudioBitrateTierMsg::from(self.bitrate),
                pcm_bitrate_kbps: matches!(codec, AudioCodec::Pcm).then_some(768),
                generation: self.generation,
                fec: false,
                dtx: false,
            },
            self.reason,
        )
    }
}

/// Ordering outcome for one authenticated-generation frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneFrameDecision {
    First,
    OnTime,
    Gap { missing_frames: u8 },
    Duplicate,
    Late,
    WrongGeneration,
    Discontinuity,
}

/// Sequence and timestamp validator with explicit reconnect generation binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicrophoneFrameOrder {
    generation: u32,
    last_sequence: Option<u32>,
    last_timestamp_ms: Option<u32>,
}

impl MicrophoneFrameOrder {
    #[must_use]
    pub const fn new(generation: u32) -> Self {
        Self {
            generation,
            last_sequence: None,
            last_timestamp_ms: None,
        }
    }

    #[must_use]
    pub fn classify(&self, header: MicrophoneHeader) -> MicrophoneFrameDecision {
        if header.generation != self.generation || header.sequence == 0 {
            return MicrophoneFrameDecision::WrongGeneration;
        }
        let (Some(previous_sequence), Some(previous_timestamp)) =
            (self.last_sequence, self.last_timestamp_ms)
        else {
            return MicrophoneFrameDecision::First;
        };
        if header.sequence == previous_sequence {
            return MicrophoneFrameDecision::Duplicate;
        }
        let raw_delta = header.sequence.wrapping_sub(previous_sequence);
        if raw_delta > i32::MAX as u32 {
            return MicrophoneFrameDecision::Late;
        }
        let sequence_delta = if header.sequence < previous_sequence {
            raw_delta.saturating_sub(1)
        } else {
            raw_delta
        };
        let timestamp_delta = header.timestamp_ms.wrapping_sub(previous_timestamp);
        if timestamp_delta > i32::MAX as u32 {
            return MicrophoneFrameDecision::Late;
        }
        if sequence_delta == 1 && timestamp_delta == u32::from(AUDIO_V1_FRAME_DURATION_MS) {
            return MicrophoneFrameDecision::OnTime;
        }
        let missing = sequence_delta.saturating_sub(1);
        let expected_timestamp =
            sequence_delta.saturating_mul(u32::from(AUDIO_V1_FRAME_DURATION_MS));
        if (1..=MAX_GAP_FILL_FRAMES).contains(&missing) && timestamp_delta == expected_timestamp {
            return MicrophoneFrameDecision::Gap {
                missing_frames: u8::try_from(missing).unwrap_or(u8::MAX),
            };
        }
        MicrophoneFrameDecision::Discontinuity
    }

    /// Classify and commit one frame in a single operation.
    #[must_use]
    pub fn observe(&mut self, header: MicrophoneHeader) -> MicrophoneFrameDecision {
        let decision = self.classify(header);
        self.commit(header, decision);
        decision
    }

    fn commit(&mut self, header: MicrophoneHeader, decision: MicrophoneFrameDecision) {
        if matches!(
            decision,
            MicrophoneFrameDecision::First
                | MicrophoneFrameDecision::OnTime
                | MicrophoneFrameDecision::Gap { .. }
                | MicrophoneFrameDecision::Discontinuity
        ) {
            self.record(header);
        }
    }

    fn record(&mut self, header: MicrophoneHeader) {
        self.last_sequence = Some(header.sequence);
        self.last_timestamp_ms = Some(header.timestamp_ms);
    }

    pub fn reset(&mut self) {
        self.last_sequence = None;
        self.last_timestamp_ms = None;
    }
}

/// Result of accepting or rejecting a decoded microphone frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneIngestOutcome {
    Accepted,
    AcceptedAfterGap { silence_frames: u8 },
    Reset,
    RejectedDiscontinuity,
    DroppedDuplicate,
    DroppedLate,
    DroppedWrongGeneration,
}

/// Kind of one exact playout frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneFrameOutput {
    Audio,
    Silence,
}

/// Bounded numeric microphone diagnostics without payloads or private identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MicrophoneStats {
    pub captured_frames: u64,
    pub captured_bytes: u64,
    pub encoded_frames: u64,
    pub encoded_bytes: u64,
    pub sent_frames: u64,
    pub sent_bytes: u64,
    pub received_frames: u64,
    pub received_bytes: u64,
    pub accepted_frames: u64,
    pub accepted_bytes: u64,
    pub capture_queue_drops: u64,
    pub transport_backpressure_drops: u64,
    pub transport_timeouts: u64,
    pub duplicate_frames: u64,
    pub late_frames: u64,
    pub wrong_generation_frames: u64,
    pub discontinuities: u64,
    pub rejected_discontinuities: u64,
    pub silence_frames: u64,
    pub underflow_frames: u64,
    pub decoder_resets: u64,
    pub decoder_errors: u64,
    pub backend_underruns: u64,
    pub backend_timeouts: u64,
    pub backend_failures: u64,
    pub telemetry_drops: u64,
}

impl MicrophoneStats {
    fn increment(value: &mut u64, amount: u64) {
        *value = value.saturating_add(amount);
    }
}

/// Session totals plus a resettable interval snapshot for rate-limited logging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MicrophoneStatsTracker {
    total: MicrophoneStats,
    interval: MicrophoneStats,
}

impl MicrophoneStatsTracker {
    fn update(&mut self, update: impl Fn(&mut MicrophoneStats)) {
        update(&mut self.total);
        update(&mut self.interval);
    }

    pub fn record_captured(&mut self, bytes: usize) {
        self.record_captured_frames(1, bytes);
    }

    pub fn record_captured_frames(&mut self, frames: u64, bytes_per_frame: usize) {
        let bytes = u64::try_from(bytes_per_frame)
            .unwrap_or(u64::MAX)
            .saturating_mul(frames);
        self.update(|stats| {
            MicrophoneStats::increment(&mut stats.captured_frames, frames);
            MicrophoneStats::increment(&mut stats.captured_bytes, bytes);
        });
    }

    pub fn record_encoded(&mut self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.update(|stats| {
            MicrophoneStats::increment(&mut stats.encoded_frames, 1);
            MicrophoneStats::increment(&mut stats.encoded_bytes, bytes);
        });
    }

    pub fn record_sent(&mut self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.update(|stats| {
            MicrophoneStats::increment(&mut stats.sent_frames, 1);
            MicrophoneStats::increment(&mut stats.sent_bytes, bytes);
        });
    }

    pub fn record_received(&mut self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.update(|stats| {
            MicrophoneStats::increment(&mut stats.received_frames, 1);
            MicrophoneStats::increment(&mut stats.received_bytes, bytes);
        });
    }

    pub fn record_ingest(&mut self, outcome: MicrophoneIngestOutcome, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.update(|stats| match outcome {
            MicrophoneIngestOutcome::Accepted => {
                MicrophoneStats::increment(&mut stats.accepted_frames, 1);
                MicrophoneStats::increment(&mut stats.accepted_bytes, bytes);
            }
            MicrophoneIngestOutcome::AcceptedAfterGap { silence_frames } => {
                MicrophoneStats::increment(&mut stats.accepted_frames, 1);
                MicrophoneStats::increment(&mut stats.accepted_bytes, bytes);
                MicrophoneStats::increment(&mut stats.silence_frames, u64::from(silence_frames));
            }
            MicrophoneIngestOutcome::Reset => {
                MicrophoneStats::increment(&mut stats.accepted_frames, 1);
                MicrophoneStats::increment(&mut stats.accepted_bytes, bytes);
                MicrophoneStats::increment(&mut stats.discontinuities, 1);
                MicrophoneStats::increment(&mut stats.decoder_resets, 1);
            }
            MicrophoneIngestOutcome::RejectedDiscontinuity => {
                MicrophoneStats::increment(&mut stats.rejected_discontinuities, 1);
            }
            MicrophoneIngestOutcome::DroppedDuplicate => {
                MicrophoneStats::increment(&mut stats.duplicate_frames, 1);
            }
            MicrophoneIngestOutcome::DroppedLate => {
                MicrophoneStats::increment(&mut stats.late_frames, 1);
            }
            MicrophoneIngestOutcome::DroppedWrongGeneration => {
                MicrophoneStats::increment(&mut stats.wrong_generation_frames, 1);
            }
        });
    }

    pub fn record_output(&mut self, output: MicrophoneFrameOutput) {
        if output == MicrophoneFrameOutput::Silence {
            self.update(|stats| {
                MicrophoneStats::increment(&mut stats.silence_frames, 1);
                MicrophoneStats::increment(&mut stats.underflow_frames, 1);
            });
        }
    }

    pub fn record_decoder_error(&mut self) {
        self.update(|stats| MicrophoneStats::increment(&mut stats.decoder_errors, 1));
    }

    pub fn record_capture_queue_drop(&mut self, count: u64) {
        self.update(|stats| {
            MicrophoneStats::increment(&mut stats.capture_queue_drops, count);
        });
    }

    pub fn record_transport_backpressure_drop(&mut self) {
        self.update(|stats| {
            MicrophoneStats::increment(&mut stats.transport_backpressure_drops, 1);
        });
    }

    pub fn record_transport_timeout(&mut self) {
        self.update(|stats| MicrophoneStats::increment(&mut stats.transport_timeouts, 1));
    }

    pub fn record_backend_underrun(&mut self) {
        self.update(|stats| MicrophoneStats::increment(&mut stats.backend_underruns, 1));
    }

    pub fn record_backend_timeout(&mut self) {
        self.update(|stats| MicrophoneStats::increment(&mut stats.backend_timeouts, 1));
    }

    pub fn record_backend_failure(&mut self) {
        self.update(|stats| MicrophoneStats::increment(&mut stats.backend_failures, 1));
    }

    pub fn record_telemetry_drops(&mut self, count: u64) {
        self.update(|stats| MicrophoneStats::increment(&mut stats.telemetry_drops, count));
    }

    #[must_use]
    pub const fn total(&self) -> MicrophoneStats {
        self.total
    }

    pub fn take_interval(&mut self) -> MicrophoneStats {
        std::mem::take(&mut self.interval)
    }
}

/// Fixed-storage, clock-free microphone jitter buffer.
pub struct MicrophoneJitterBuffer {
    frames: Box<[[i16; MICROPHONE_V1_FRAME_SAMPLES]]>,
    read: usize,
    len: usize,
    prebuffering: bool,
    order: MicrophoneFrameOrder,
}

impl std::fmt::Debug for MicrophoneJitterBuffer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MicrophoneJitterBuffer")
            .field("queued_frames", &self.len)
            .field("prebuffering", &self.prebuffering)
            .finish_non_exhaustive()
    }
}

impl MicrophoneJitterBuffer {
    #[must_use]
    pub fn new(generation: u32) -> Self {
        Self {
            frames: vec![[0; MICROPHONE_V1_FRAME_SAMPLES]; MICROPHONE_JITTER_MAX_FRAMES]
                .into_boxed_slice(),
            read: 0,
            len: 0,
            prebuffering: true,
            order: MicrophoneFrameOrder::new(generation),
        }
    }

    pub fn push(
        &mut self,
        header: MicrophoneHeader,
        frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
    ) -> MicrophoneIngestOutcome {
        let decision = self.classify(header);
        self.push_classified(header, decision, frame)
    }

    /// Classify a header without changing ordering or queued audio.
    #[must_use]
    pub fn classify(&self, header: MicrophoneHeader) -> MicrophoneFrameDecision {
        self.order.classify(header)
    }

    fn push_classified(
        &mut self,
        header: MicrophoneHeader,
        decision: MicrophoneFrameDecision,
        frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
    ) -> MicrophoneIngestOutcome {
        self.order.commit(header, decision);
        match decision {
            MicrophoneFrameDecision::First | MicrophoneFrameDecision::OnTime => {
                self.push_one(frame);
                self.trim_to_target();
                MicrophoneIngestOutcome::Accepted
            }
            MicrophoneFrameDecision::Gap { missing_frames } => {
                for _ in 0..missing_frames {
                    self.push_one(&[0; MICROPHONE_V1_FRAME_SAMPLES]);
                }
                self.push_one(frame);
                self.trim_to_target();
                MicrophoneIngestOutcome::AcceptedAfterGap {
                    silence_frames: missing_frames,
                }
            }
            MicrophoneFrameDecision::Discontinuity => {
                self.clear_audio();
                self.push_one(frame);
                MicrophoneIngestOutcome::Reset
            }
            MicrophoneFrameDecision::Duplicate => MicrophoneIngestOutcome::DroppedDuplicate,
            MicrophoneFrameDecision::Late => MicrophoneIngestOutcome::DroppedLate,
            MicrophoneFrameDecision::WrongGeneration => {
                MicrophoneIngestOutcome::DroppedWrongGeneration
            }
        }
    }

    /// Writes exactly one 20 ms mono frame, filling exact silence when unavailable.
    ///
    /// # Errors
    ///
    /// Rejects output slices that are not exactly one microphone-v1 frame.
    pub fn pop_into(
        &mut self,
        output: &mut [i16],
    ) -> Result<MicrophoneFrameOutput, MicrophoneDecodeError> {
        if output.len() != MICROPHONE_V1_FRAME_SAMPLES {
            output.zeroize();
            self.clear();
            return Err(MicrophoneDecodeError::InvalidPcmLength);
        }
        if self.prebuffering {
            if self.len < MICROPHONE_JITTER_TARGET_FRAMES {
                output.zeroize();
                return Ok(MicrophoneFrameOutput::Silence);
            }
            self.prebuffering = false;
        }
        if self.len == 0 {
            self.prebuffering = true;
            output.zeroize();
            return Ok(MicrophoneFrameOutput::Silence);
        }
        output.copy_from_slice(&self.frames[self.read]);
        self.frames[self.read].zeroize();
        self.read = (self.read + 1) % MICROPHONE_JITTER_MAX_FRAMES;
        self.len -= 1;
        Ok(MicrophoneFrameOutput::Audio)
    }

    pub fn clear(&mut self) {
        self.clear_audio();
        self.order.reset();
    }

    #[must_use]
    pub const fn queued_frames(&self) -> usize {
        self.len
    }

    fn push_one(&mut self, frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES]) {
        if self.len == MICROPHONE_JITTER_MAX_FRAMES {
            self.frames[self.read].zeroize();
            self.read = (self.read + 1) % MICROPHONE_JITTER_MAX_FRAMES;
            self.len -= 1;
        }
        let write = (self.read + self.len) % MICROPHONE_JITTER_MAX_FRAMES;
        self.frames[write].copy_from_slice(frame);
        self.len += 1;
    }

    fn trim_to_target(&mut self) {
        if self.len <= MICROPHONE_JITTER_TRIM_THRESHOLD_FRAMES {
            return;
        }
        while self.len > MICROPHONE_JITTER_TARGET_FRAMES {
            self.frames[self.read].zeroize();
            self.read = (self.read + 1) % MICROPHONE_JITTER_MAX_FRAMES;
            self.len -= 1;
        }
    }

    fn clear_audio(&mut self) {
        for frame in &mut self.frames {
            frame.zeroize();
        }
        self.read = 0;
        self.len = 0;
        self.prebuffering = true;
    }
}

impl Drop for MicrophoneJitterBuffer {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Safe PCM-only receiver used by adapters and default-feature tests.
#[derive(Debug)]
pub struct MicrophoneFrameReceiver {
    stream: ResolvedMicrophoneStream,
    jitter: MicrophoneJitterBuffer,
    decoded: [i16; MICROPHONE_V1_FRAME_SAMPLES],
}

impl MicrophoneFrameReceiver {
    #[must_use]
    pub fn new(stream: ResolvedMicrophoneStream) -> Self {
        Self {
            stream,
            jitter: MicrophoneJitterBuffer::new(stream.generation),
            decoded: [0; MICROPHONE_V1_FRAME_SAMPLES],
        }
    }

    /// Decode one exact little-endian PCM frame into reusable storage.
    ///
    /// # Errors
    ///
    /// Rejects disabled streams, codec mismatch, or malformed PCM.
    pub fn ingest_pcm(
        &mut self,
        header: MicrophoneHeader,
        payload: &[u8],
    ) -> Result<MicrophoneIngestOutcome, MicrophoneDecodeError> {
        if !self.stream.is_enabled() {
            self.clear();
            return Err(MicrophoneDecodeError::Disabled);
        }
        let decision = self.jitter.classify(header);
        if let Some(outcome) = dropped_outcome(decision) {
            return Ok(outcome);
        }
        if self.stream.codec != Some(AudioCodec::Pcm) || header.codec != AudioCodec::Pcm {
            self.clear();
            return Err(MicrophoneDecodeError::CodecMismatch);
        }
        if decode_pcm(payload, &mut self.decoded).is_err() {
            self.clear();
            return Err(MicrophoneDecodeError::InvalidPcmLength);
        }
        Ok(self.jitter.push_classified(header, decision, &self.decoded))
    }

    /// Writes exactly one decoded frame or exact silence into caller storage.
    ///
    /// # Errors
    ///
    /// Rejects output slices that are not exactly one microphone-v1 frame.
    pub fn pop_into(
        &mut self,
        output: &mut [i16],
    ) -> Result<MicrophoneFrameOutput, MicrophoneDecodeError> {
        let result = self.jitter.pop_into(output);
        if result.is_err() {
            self.clear();
        }
        result
    }

    pub fn clear(&mut self) {
        self.decoded.zeroize();
        self.jitter.clear();
    }

    #[must_use]
    pub const fn queued_frames(&self) -> usize {
        self.jitter.queued_frames()
    }
}

impl Drop for MicrophoneFrameReceiver {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Typed decoder failure without packet or device data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneDecodeError {
    Disabled,
    CodecMismatch,
    InvalidPcmLength,
    InvalidPacketLength,
    CodecFailure,
}

#[cfg(feature = "audio-opus")]
impl From<OpusError> for MicrophoneDecodeError {
    fn from(_: OpusError) -> Self {
        Self::CodecFailure
    }
}

#[cfg(feature = "audio-opus")]
/// Attachment-scoped reusable Opus/PCM decoder and bounded jitter storage.
pub struct MicrophoneDecoder {
    stream: ResolvedMicrophoneStream,
    opus: Option<OpusDecoder>,
    jitter: MicrophoneJitterBuffer,
    decoded: [i16; MICROPHONE_V1_FRAME_SAMPLES],
}

#[cfg(feature = "audio-opus")]
impl std::fmt::Debug for MicrophoneDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MicrophoneDecoder")
            .field("stream", &self.stream)
            .field("jitter", &self.jitter)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "audio-opus")]
impl MicrophoneDecoder {
    /// Create reusable codec state for one negotiated attachment.
    ///
    /// # Errors
    ///
    /// Rejects a disabled stream or native codec setup failure.
    pub fn new(stream: ResolvedMicrophoneStream) -> Result<Self, MicrophoneDecodeError> {
        if !stream.is_enabled() {
            return Err(MicrophoneDecodeError::Disabled);
        }
        let opus = match stream.codec {
            Some(AudioCodec::Opus) => {
                Some(OpusDecoder::new_for_spec(AudioFrameSpec::MICROPHONE_V1)?)
            }
            Some(AudioCodec::Pcm) => None,
            None => return Err(MicrophoneDecodeError::Disabled),
        };
        Ok(Self {
            stream,
            opus,
            jitter: MicrophoneJitterBuffer::new(stream.generation),
            decoded: [0; MICROPHONE_V1_FRAME_SAMPLES],
        })
    }

    /// Decode and enqueue one validated wire payload without per-frame allocation.
    ///
    /// # Errors
    ///
    /// Rejects codec mismatch, malformed payload, or native decoder failure.
    pub fn ingest(
        &mut self,
        header: MicrophoneHeader,
        payload: &[u8],
    ) -> Result<MicrophoneIngestOutcome, MicrophoneDecodeError> {
        let decision = self.jitter.classify(header);
        if let Some(outcome) = dropped_outcome(decision) {
            return Ok(outcome);
        }
        if Some(header.codec) != self.stream.codec {
            self.clear();
            return Err(MicrophoneDecodeError::CodecMismatch);
        }
        if header.codec == AudioCodec::Opus {
            if let MicrophoneFrameDecision::Gap { missing_frames } = decision {
                let Some(opus) = &mut self.opus else {
                    self.clear();
                    return Err(MicrophoneDecodeError::CodecMismatch);
                };
                for _ in 0..missing_frames {
                    if opus.decode_plc(&mut self.decoded).is_err() {
                        self.clear();
                        return Err(MicrophoneDecodeError::CodecFailure);
                    }
                }
                self.decoded.zeroize();
            }
        }
        if matches!(decision, MicrophoneFrameDecision::Discontinuity) {
            if let Some(opus) = &mut self.opus {
                if opus.reset().is_err() {
                    self.clear();
                    return Err(MicrophoneDecodeError::CodecFailure);
                }
            }
        }
        let decode_result = match header.codec {
            AudioCodec::Pcm => decode_pcm(payload, &mut self.decoded),
            AudioCodec::Opus => {
                if payload.is_empty() || payload.len() > MAX_OPUS_PACKET_BYTES {
                    Err(MicrophoneDecodeError::InvalidPacketLength)
                } else {
                    self.opus
                        .as_mut()
                        .ok_or(MicrophoneDecodeError::CodecMismatch)
                        .and_then(|opus| {
                            opus.decode(payload, &mut self.decoded).map_err(Into::into)
                        })
                }
            }
        };
        if let Err(error) = decode_result {
            self.clear();
            return Err(error);
        }
        Ok(self.jitter.push_classified(header, decision, &self.decoded))
    }

    /// Writes exactly one decoded frame or exact silence into caller storage.
    ///
    /// # Errors
    ///
    /// Rejects output slices that are not exactly one microphone-v1 frame.
    pub fn pop_into(
        &mut self,
        output: &mut [i16],
    ) -> Result<MicrophoneFrameOutput, MicrophoneDecodeError> {
        let result = self.jitter.pop_into(output);
        if result.is_err() {
            self.clear();
        }
        result
    }

    pub fn clear(&mut self) {
        self.decoded.zeroize();
        self.jitter.clear();
        if let Some(opus) = &mut self.opus {
            let _ = opus.reset();
        }
    }

    #[must_use]
    pub const fn queued_frames(&self) -> usize {
        self.jitter.queued_frames()
    }
}

#[cfg(feature = "audio-opus")]
impl Drop for MicrophoneDecoder {
    fn drop(&mut self) {
        self.clear();
    }
}

fn dropped_outcome(decision: MicrophoneFrameDecision) -> Option<MicrophoneIngestOutcome> {
    match decision {
        MicrophoneFrameDecision::Duplicate => Some(MicrophoneIngestOutcome::DroppedDuplicate),
        MicrophoneFrameDecision::Late => Some(MicrophoneIngestOutcome::DroppedLate),
        MicrophoneFrameDecision::WrongGeneration => {
            Some(MicrophoneIngestOutcome::DroppedWrongGeneration)
        }
        MicrophoneFrameDecision::First
        | MicrophoneFrameDecision::OnTime
        | MicrophoneFrameDecision::Gap { .. }
        | MicrophoneFrameDecision::Discontinuity => None,
    }
}

fn decode_pcm(
    payload: &[u8],
    output: &mut [i16; MICROPHONE_V1_FRAME_SAMPLES],
) -> Result<(), MicrophoneDecodeError> {
    if payload.len() != MICROPHONE_PCM_BYTES {
        output.zeroize();
        return Err(MicrophoneDecodeError::InvalidPcmLength);
    }
    for (sample, bytes) in output.iter_mut().zip(payload.chunks_exact(2)) {
        *sample = i16::from_le_bytes([bytes[0], bytes[1]]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> MicrophoneCapabilitiesMsg {
        MicrophoneCapabilitiesMsg {
            protocol_version: MICROPHONE_PROTOCOL_VERSION,
            codecs: vec![AudioCodec::Opus, AudioCodec::Pcm],
            sample_rate_hz: AUDIO_V1_SAMPLE_RATE_HZ,
            channels: MICROPHONE_V1_CHANNELS,
            frame_duration_ms: AUDIO_V1_FRAME_DURATION_MS,
            fec: false,
            dtx: false,
        }
    }

    fn policy() -> MicrophonePolicy {
        MicrophonePolicy {
            operator_enabled: true,
            backend_available: true,
            codecs: MicrophoneCodecAvailability {
                opus: true,
                pcm: true,
            },
        }
    }

    fn header(sequence: u32, timestamp_ms: u32) -> MicrophoneHeader {
        MicrophoneHeader {
            codec: AudioCodec::Pcm,
            sequence,
            timestamp_ms,
            generation: 7,
        }
    }

    #[test]
    fn policy_requires_every_authorization_gate() {
        let enabled = policy().resolve(Some(&capabilities()), true, 7, 64);
        assert!(enabled.is_enabled());
        assert_eq!(enabled.codec, Some(AudioCodec::Opus));
        assert!(enabled.result().config.unwrap().is_valid_v1());

        let mut disabled = policy();
        disabled.operator_enabled = false;
        assert_eq!(
            disabled.resolve(Some(&capabilities()), true, 7, 64).reason,
            MicrophoneStreamReason::DisabledByOperator
        );
        assert_eq!(
            policy().resolve(Some(&capabilities()), false, 7, 64).reason,
            MicrophoneStreamReason::DisabledByClient
        );
        assert_eq!(
            policy().resolve(None, true, 7, 64).reason,
            MicrophoneStreamReason::NotNegotiated
        );
    }

    #[test]
    fn microphone_stats_accumulate_snapshot_and_reset_without_private_data() {
        assert!((5..=10).contains(&MICROPHONE_STATS_INTERVAL.as_secs()));
        let mut tracker = MicrophoneStatsTracker::default();
        tracker.record_captured(MICROPHONE_PCM_BYTES);
        tracker.record_encoded(120);
        tracker.record_sent(144);
        tracker.record_received(144);
        tracker.record_ingest(
            MicrophoneIngestOutcome::AcceptedAfterGap { silence_frames: 2 },
            120,
        );
        tracker.record_ingest(MicrophoneIngestOutcome::DroppedDuplicate, 0);
        tracker.record_ingest(MicrophoneIngestOutcome::DroppedLate, 0);
        tracker.record_ingest(MicrophoneIngestOutcome::DroppedWrongGeneration, 0);
        tracker.record_ingest(MicrophoneIngestOutcome::Reset, 120);
        tracker.record_ingest(MicrophoneIngestOutcome::RejectedDiscontinuity, 0);
        tracker.record_output(MicrophoneFrameOutput::Silence);
        tracker.record_decoder_error();
        tracker.record_capture_queue_drop(3);
        tracker.record_transport_backpressure_drop();
        tracker.record_transport_timeout();
        tracker.record_backend_underrun();
        tracker.record_backend_timeout();
        tracker.record_backend_failure();
        tracker.record_telemetry_drops(2);

        let interval = tracker.take_interval();
        assert_eq!(interval.captured_frames, 1);
        assert_eq!(interval.accepted_frames, 2);
        assert_eq!(interval.silence_frames, 3);
        assert_eq!(interval.underflow_frames, 1);
        assert_eq!(interval.capture_queue_drops, 3);
        assert_eq!(interval.decoder_errors, 1);
        assert_eq!(interval.rejected_discontinuities, 1);
        assert_eq!(interval.backend_failures, 1);
        assert_eq!(interval.telemetry_drops, 2);
        assert_eq!(tracker.take_interval(), MicrophoneStats::default());
        assert_eq!(tracker.total(), interval);
        assert!(!std::mem::needs_drop::<MicrophoneStats>());
        let fields = format!("{interval:?}");
        for forbidden in ["payload", "sid", "credential", "profile", "certificate"] {
            assert!(!fields.to_ascii_lowercase().contains(forbidden));
        }
    }

    #[test]
    fn microphone_stats_saturate() {
        let mut tracker = MicrophoneStatsTracker {
            total: MicrophoneStats {
                captured_frames: u64::MAX,
                captured_bytes: u64::MAX,
                ..MicrophoneStats::default()
            },
            interval: MicrophoneStats {
                captured_frames: u64::MAX,
                captured_bytes: u64::MAX,
                ..MicrophoneStats::default()
            },
        };
        tracker.record_captured(1);
        assert_eq!(tracker.total().captured_frames, u64::MAX);
        assert_eq!(tracker.total().captured_bytes, u64::MAX);
    }

    #[test]
    fn pcm_negotiation_separates_fixed_bandwidth_from_the_opus_tier() {
        let mut pcm_capabilities = capabilities();
        pcm_capabilities.codecs = vec![AudioCodec::Pcm];
        let mut pcm_policy = policy();
        pcm_policy.codecs.opus = false;

        let stream = pcm_policy.resolve(Some(&pcm_capabilities), true, 7, 64);
        assert_eq!(
            stream.resolved_bitrate(),
            Some(ResolvedMicrophoneBitrate::PcmFixed)
        );
        assert!(stream.result().config.unwrap().is_valid_v1());
    }

    #[test]
    fn order_handles_wrap_duplicate_late_gap_and_generation() {
        let mut order = MicrophoneFrameOrder::new(7);
        assert_eq!(
            order.classify(header(u32::MAX, u32::MAX - 9)),
            MicrophoneFrameDecision::First
        );
        assert_eq!(
            order.classify(header(u32::MAX, u32::MAX - 9)),
            MicrophoneFrameDecision::First
        );
        assert_eq!(
            order.observe(header(u32::MAX, u32::MAX - 9)),
            MicrophoneFrameDecision::First
        );
        assert_eq!(
            order.observe(header(1, 10)),
            MicrophoneFrameDecision::OnTime
        );
        assert_eq!(
            order.observe(header(1, 10)),
            MicrophoneFrameDecision::Duplicate
        );
        assert_eq!(
            order.observe(header(4, 70)),
            MicrophoneFrameDecision::Gap { missing_frames: 2 }
        );
        assert_eq!(order.observe(header(3, 50)), MicrophoneFrameDecision::Late);
        assert_eq!(
            order.observe(MicrophoneHeader {
                generation: 8,
                ..header(5, 90)
            }),
            MicrophoneFrameDecision::WrongGeneration
        );
    }

    #[test]
    fn jitter_is_bounded_and_fills_exact_deterministic_silence() {
        let mut jitter = MicrophoneJitterBuffer::new(7);
        let frame = [42; MICROPHONE_V1_FRAME_SAMPLES];
        assert_eq!(
            jitter.push(header(1, 0), &frame),
            MicrophoneIngestOutcome::Accepted
        );
        let mut output = [9; MICROPHONE_V1_FRAME_SAMPLES];
        assert_eq!(
            jitter.pop_into(&mut output).unwrap(),
            MicrophoneFrameOutput::Silence
        );
        assert_eq!(output, [0; MICROPHONE_V1_FRAME_SAMPLES]);
        jitter.push(header(2, 20), &frame);
        jitter.push(header(3, 40), &frame);
        assert_eq!(
            jitter.pop_into(&mut output).unwrap(),
            MicrophoneFrameOutput::Audio
        );
        assert_eq!(output, frame);

        for sequence in 4..100 {
            jitter.push(header(sequence, (sequence - 1) * 20), &frame);
        }
        assert!(jitter.queued_frames() <= MICROPHONE_JITTER_MAX_FRAMES);
    }

    #[test]
    fn jitter_trims_bursts_back_to_the_target_depth() {
        let mut jitter = MicrophoneJitterBuffer::new(7);
        let frame = [42; MICROPHONE_V1_FRAME_SAMPLES];
        for sequence in 1..=6 {
            assert_eq!(
                jitter.push(header(sequence, (sequence - 1) * 20), &frame),
                MicrophoneIngestOutcome::Accepted
            );
        }
        assert_eq!(jitter.queued_frames(), MICROPHONE_JITTER_TARGET_FRAMES);
    }

    #[test]
    fn gaps_insert_exact_silence_and_duplicates_do_not_grow() {
        let mut jitter = MicrophoneJitterBuffer::new(7);
        let frame = [7; MICROPHONE_V1_FRAME_SAMPLES];
        jitter.push(header(1, 0), &frame);
        assert_eq!(
            jitter.push(header(4, 60), &frame),
            MicrophoneIngestOutcome::AcceptedAfterGap { silence_frames: 2 }
        );
        let depth = jitter.queued_frames();
        assert_eq!(
            jitter.push(header(4, 60), &frame),
            MicrophoneIngestOutcome::DroppedDuplicate
        );
        assert_eq!(jitter.queued_frames(), depth);
        let mut output = [0; MICROPHONE_V1_FRAME_SAMPLES];
        assert_eq!(
            jitter.pop_into(&mut output).unwrap(),
            MicrophoneFrameOutput::Audio
        );
        assert_eq!(output, frame);
        assert_eq!(
            jitter.pop_into(&mut output).unwrap(),
            MicrophoneFrameOutput::Audio
        );
        assert_eq!(output, [0; MICROPHONE_V1_FRAME_SAMPLES]);
    }

    #[test]
    fn pcm_receiver_teardown_zeros_all_owned_audio() {
        let stream = ResolvedMicrophoneStream {
            codec: Some(AudioCodec::Pcm),
            bitrate: AudioBitrateTier::Off,
            generation: 7,
            reason: MicrophoneStreamReason::Enabled,
        };
        let mut receiver = MicrophoneFrameReceiver::new(stream);
        let pcm = vec![0x01; MICROPHONE_PCM_BYTES];
        for sequence in 1..=4 {
            receiver
                .ingest_pcm(header(sequence, (sequence - 1) * 20), &pcm)
                .unwrap();
        }
        receiver.clear();
        assert_eq!(receiver.decoded, [0; MICROPHONE_V1_FRAME_SAMPLES]);
        assert_eq!(receiver.jitter.queued_frames(), 0);
        assert!(
            receiver
                .jitter
                .frames
                .iter()
                .all(|frame| *frame == [0; MICROPHONE_V1_FRAME_SAMPLES])
        );
        assert_eq!(
            receiver.jitter.classify(header(5, 80)),
            MicrophoneFrameDecision::First
        );
    }

    #[test]
    fn pcm_rejection_precedes_decode_and_errors_zero_owned_audio() {
        let stream = ResolvedMicrophoneStream {
            codec: Some(AudioCodec::Pcm),
            bitrate: AudioBitrateTier::Off,
            generation: 7,
            reason: MicrophoneStreamReason::Enabled,
        };
        let mut receiver = MicrophoneFrameReceiver::new(stream);
        let pcm = [1u8; MICROPHONE_PCM_BYTES];
        assert_eq!(
            receiver.ingest_pcm(header(1, 0), &pcm).unwrap(),
            MicrophoneIngestOutcome::Accepted
        );
        assert_eq!(
            receiver.ingest_pcm(header(1, 0), &[0xff]).unwrap(),
            MicrophoneIngestOutcome::DroppedDuplicate
        );
        assert!(receiver.jitter.frames.iter().any(|frame| frame[0] != 0));

        assert_eq!(
            receiver.ingest_pcm(header(2, 20), &[0xff]),
            Err(MicrophoneDecodeError::InvalidPcmLength)
        );
        assert_eq!(receiver.decoded, [0; MICROPHONE_V1_FRAME_SAMPLES]);
        assert!(
            receiver
                .jitter
                .frames
                .iter()
                .all(|frame| *frame == [0; MICROPHONE_V1_FRAME_SAMPLES])
        );
        assert_eq!(
            receiver.jitter.classify(header(2, 20)),
            MicrophoneFrameDecision::First
        );
    }

    #[test]
    fn pcm_drops_stale_mixed_codec_frames_without_clearing_audio() {
        let stream = ResolvedMicrophoneStream {
            codec: Some(AudioCodec::Pcm),
            bitrate: AudioBitrateTier::Off,
            generation: 7,
            reason: MicrophoneStreamReason::Enabled,
        };
        let mut receiver = MicrophoneFrameReceiver::new(stream);
        let pcm = [1u8; MICROPHONE_PCM_BYTES];
        receiver.ingest_pcm(header(1, 0), &pcm).unwrap();

        let mut mixed_duplicate = header(1, 0);
        mixed_duplicate.codec = AudioCodec::Opus;
        assert_eq!(
            receiver.ingest_pcm(mixed_duplicate, &[0xff]).unwrap(),
            MicrophoneIngestOutcome::DroppedDuplicate
        );

        let mut mixed_generation = header(2, 20);
        mixed_generation.codec = AudioCodec::Opus;
        mixed_generation.generation += 1;
        assert_eq!(
            receiver.ingest_pcm(mixed_generation, &[0xff]).unwrap(),
            MicrophoneIngestOutcome::DroppedWrongGeneration
        );

        receiver.ingest_pcm(header(2, 20), &pcm).unwrap();
        let mut mixed_late = header(1, 0);
        mixed_late.codec = AudioCodec::Opus;
        assert_eq!(
            receiver.ingest_pcm(mixed_late, &[0xff]).unwrap(),
            MicrophoneIngestOutcome::DroppedLate
        );
        assert_eq!(receiver.jitter.queued_frames(), 2);
        assert!(receiver.jitter.frames.iter().any(|frame| frame[0] != 0));
    }

    #[cfg(feature = "audio-opus")]
    #[test]
    fn opus_decode_uses_the_landed_reusable_codec_api() {
        use super::super::OpusEncoder;

        let stream = policy().resolve(Some(&capabilities()), true, 7, 64);
        let mut encoder =
            OpusEncoder::new_for_spec(AudioFrameSpec::MICROPHONE_V1, stream.bitrate).unwrap();
        let mut decoder = MicrophoneDecoder::new(stream).unwrap();
        let input = [0i16; MICROPHONE_V1_FRAME_SAMPLES];
        let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
        let packet_len = encoder.encode(&input, &mut packet).unwrap();
        let mut opus_header = header(1, 0);
        opus_header.codec = AudioCodec::Opus;
        assert_eq!(
            decoder.ingest(opus_header, &packet[..packet_len]).unwrap(),
            MicrophoneIngestOutcome::Accepted
        );
    }

    #[cfg(feature = "audio-opus")]
    #[test]
    fn opus_gap_advances_decoder_state_with_bounded_plc() {
        use super::super::OpusEncoder;

        let stream = policy().resolve(Some(&capabilities()), true, 7, 64);
        let mut encoder =
            OpusEncoder::new_for_spec(AudioFrameSpec::MICROPHONE_V1, stream.bitrate).unwrap();
        let mut packets = [[0u8; MAX_OPUS_PACKET_BYTES]; 3];
        let mut lengths = [0usize; 3];
        for (index, packet) in packets.iter_mut().enumerate() {
            let mut input = [0i16; MICROPHONE_V1_FRAME_SAMPLES];
            for (sample_index, sample) in input.iter_mut().enumerate() {
                *sample = ((sample_index * (index + 5) + index * 113) as i16).wrapping_mul(29);
            }
            lengths[index] = encoder.encode(&input, packet).unwrap();
        }

        let mut actual = MicrophoneDecoder::new(stream).unwrap();
        let mut first_header = header(1, 0);
        first_header.codec = AudioCodec::Opus;
        actual
            .ingest(first_header, &packets[0][..lengths[0]])
            .unwrap();
        let mut third_header = first_header;
        third_header.sequence = 3;
        third_header.timestamp_ms = 40;
        assert_eq!(
            actual
                .ingest(third_header, &packets[2][..lengths[2]])
                .unwrap(),
            MicrophoneIngestOutcome::AcceptedAfterGap { silence_frames: 1 }
        );

        let mut expected = OpusDecoder::new_for_spec(AudioFrameSpec::MICROPHONE_V1).unwrap();
        let mut expected_output = [0i16; MICROPHONE_V1_FRAME_SAMPLES];
        expected
            .decode(&packets[0][..lengths[0]], &mut expected_output)
            .unwrap();
        expected.decode_plc(&mut expected_output).unwrap();
        expected
            .decode(&packets[2][..lengths[2]], &mut expected_output)
            .unwrap();
        assert_eq!(actual.decoded, expected_output);
    }

    #[cfg(feature = "audio-opus")]
    #[test]
    fn opus_rejects_stale_before_decode_without_changing_state() {
        use super::super::OpusEncoder;

        let stream = policy().resolve(Some(&capabilities()), true, 7, 64);
        let mut encoder =
            OpusEncoder::new_for_spec(AudioFrameSpec::MICROPHONE_V1, stream.bitrate).unwrap();
        let mut packets = [[0u8; MAX_OPUS_PACKET_BYTES]; 3];
        let mut lengths = [0usize; 3];
        for (index, packet) in packets.iter_mut().enumerate() {
            let mut input = [0i16; MICROPHONE_V1_FRAME_SAMPLES];
            for (sample_index, sample) in input.iter_mut().enumerate() {
                let value = sample_index * (index + 3) + index * 97;
                *sample = i16::try_from(value).unwrap().wrapping_mul(31);
            }
            lengths[index] = encoder.encode(&input, packet).unwrap();
        }

        let mut actual = MicrophoneDecoder::new(stream).unwrap();
        let mut expected = MicrophoneDecoder::new(stream).unwrap();
        let mut opus_header = header(1, 0);
        opus_header.codec = AudioCodec::Opus;
        let mut wrong_generation = opus_header;
        wrong_generation.generation += 1;
        wrong_generation.codec = AudioCodec::Pcm;
        assert_eq!(
            actual.ingest(wrong_generation, &[0xff]).unwrap(),
            MicrophoneIngestOutcome::DroppedWrongGeneration
        );
        assert_eq!(
            actual.jitter.classify(opus_header),
            MicrophoneFrameDecision::First
        );
        actual
            .ingest(opus_header, &packets[0][..lengths[0]])
            .unwrap();
        expected
            .ingest(opus_header, &packets[0][..lengths[0]])
            .unwrap();
        assert_eq!(
            actual
                .ingest(opus_header, &packets[0][..lengths[0]])
                .unwrap(),
            MicrophoneIngestOutcome::DroppedDuplicate
        );
        let mut mixed_duplicate = opus_header;
        mixed_duplicate.codec = AudioCodec::Pcm;
        assert_eq!(
            actual.ingest(mixed_duplicate, &[0xff]).unwrap(),
            MicrophoneIngestOutcome::DroppedDuplicate
        );

        opus_header.sequence = 2;
        opus_header.timestamp_ms = 20;
        actual
            .ingest(opus_header, &packets[1][..lengths[1]])
            .unwrap();
        expected
            .ingest(opus_header, &packets[1][..lengths[1]])
            .unwrap();
        assert_eq!(actual.decoded, expected.decoded);

        let mut late_header = opus_header;
        late_header.sequence = 1;
        late_header.timestamp_ms = 0;
        assert_eq!(
            actual
                .ingest(late_header, &packets[0][..lengths[0]])
                .unwrap(),
            MicrophoneIngestOutcome::DroppedLate
        );
        late_header.codec = AudioCodec::Pcm;
        assert_eq!(
            actual.ingest(late_header, &[0xff]).unwrap(),
            MicrophoneIngestOutcome::DroppedLate
        );

        opus_header.sequence = 3;
        opus_header.timestamp_ms = 40;
        actual
            .ingest(opus_header, &packets[2][..lengths[2]])
            .unwrap();
        expected
            .ingest(opus_header, &packets[2][..lengths[2]])
            .unwrap();
        assert_eq!(actual.decoded, expected.decoded);
    }

    #[cfg(feature = "audio-opus")]
    #[test]
    fn opus_decode_failure_zeros_state_and_allows_successful_retry() {
        use super::super::OpusEncoder;

        let stream = policy().resolve(Some(&capabilities()), true, 7, 64);
        let mut encoder =
            OpusEncoder::new_for_spec(AudioFrameSpec::MICROPHONE_V1, stream.bitrate).unwrap();
        let input = [123i16; MICROPHONE_V1_FRAME_SAMPLES];
        let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
        let packet_len = encoder.encode(&input, &mut packet).unwrap();
        let mut decoder = MicrophoneDecoder::new(stream).unwrap();
        let mut opus_header = header(1, 0);
        opus_header.codec = AudioCodec::Opus;

        assert_eq!(
            decoder.ingest(opus_header, &[0xff]),
            Err(MicrophoneDecodeError::CodecFailure)
        );
        assert_eq!(decoder.decoded, [0; MICROPHONE_V1_FRAME_SAMPLES]);
        assert!(
            decoder
                .jitter
                .frames
                .iter()
                .all(|frame| *frame == [0; MICROPHONE_V1_FRAME_SAMPLES])
        );
        assert_eq!(
            decoder.jitter.classify(opus_header),
            MicrophoneFrameDecision::First
        );
        assert_eq!(
            decoder.ingest(opus_header, &packet[..packet_len]).unwrap(),
            MicrophoneIngestOutcome::Accepted
        );
    }
}
