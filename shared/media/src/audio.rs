//! Pure audio format, negotiation, timestamp, and playout policy.

use arcen_protocol::AudioCodec;

mod microphone;
#[cfg(feature = "audio-opus")]
mod opus;
use arcen_protocol::messages::{
    AUDIO_PROTOCOL_VERSION, AudioBitrateTierMsg, AudioOutputCapabilitiesMsg, AudioStreamConfigMsg,
    AudioStreamReason, AudioStreamResultMsg,
};
#[cfg(feature = "audio-opus")]
pub use microphone::MicrophoneDecoder;
pub use microphone::{
    MICROPHONE_JITTER_MAX_FRAMES, MICROPHONE_JITTER_TARGET_FRAMES, MICROPHONE_PCM_BITRATE_KBPS,
    MICROPHONE_STATS_INTERVAL, MICROPHONE_V1_FRAME_SAMPLES, MicrophoneCodecAvailability,
    MicrophoneDecodeError, MicrophoneFrameDecision, MicrophoneFrameOrder, MicrophoneFrameOutput,
    MicrophoneFrameReceiver, MicrophoneIngestOutcome, MicrophoneJitterBuffer, MicrophonePolicy,
    MicrophoneStats, MicrophoneStatsTracker, ResolvedMicrophoneBitrate, ResolvedMicrophoneStream,
};
#[cfg(feature = "audio-opus")]
pub use opus::{OpusDecoder, OpusEncoder, OpusError, OpusErrorKind};

/// Fixed audio-v1 sample rate.
pub const AUDIO_V1_SAMPLE_RATE_HZ: u32 = 48_000;
/// Fixed audio-v1 channel count.
pub const AUDIO_V1_CHANNELS: u8 = 2;
/// Fixed microphone-v1 channel count.
pub const MICROPHONE_V1_CHANNELS: u8 = 1;
/// Fixed audio-v1 frame duration.
pub const AUDIO_V1_FRAME_DURATION_MS: u16 = 20;
/// Maximum encoded Opus packet accepted by audio-v1.
pub const MAX_OPUS_PACKET_BYTES: usize = 1_275;
/// Fixed Opus bitrate selected by `audio.compressed=true`.
pub const CONFIGURED_OPUS_BITRATE_KBPS: u32 = 128;
/// Maximum consecutive packet-loss-concealment frames.
pub const MAX_PLC_FRAMES: u8 = 3;
/// Target Deck playout latency.
pub const JITTER_TARGET_MS: u16 = 60;
/// Queue latency that triggers trimming.
pub const JITTER_TRIM_THRESHOLD_MS: u16 = 110;
/// Hard Deck decoded-audio bound.
pub const JITTER_MAX_MS: u16 = 200;

/// Exact interleaved signed-PCM frame shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFrameSpec {
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub frame_duration_ms: u16,
}

impl AudioFrameSpec {
    /// The fixed audio-v1 format.
    pub const V1: Self = Self {
        sample_rate_hz: AUDIO_V1_SAMPLE_RATE_HZ,
        channels: AUDIO_V1_CHANNELS,
        frame_duration_ms: AUDIO_V1_FRAME_DURATION_MS,
    };

    /// Fixed post-decode client microphone format.
    pub const MICROPHONE_V1: Self = Self {
        sample_rate_hz: AUDIO_V1_SAMPLE_RATE_HZ,
        channels: MICROPHONE_V1_CHANNELS,
        frame_duration_ms: AUDIO_V1_FRAME_DURATION_MS,
    };

    /// Interleaved samples in one frame.
    #[must_use]
    pub fn interleaved_samples(self) -> Option<usize> {
        if self.sample_rate_hz == 0
            || self.sample_rate_hz > 384_000
            || self.channels == 0
            || self.channels > 32
            || self.frame_duration_ms == 0
            || self.frame_duration_ms > 1_000
        {
            return None;
        }
        usize::try_from(self.sample_rate_hz)
            .ok()?
            .checked_mul(usize::from(self.channels))?
            .checked_mul(usize::from(self.frame_duration_ms))?
            .checked_div(1_000)
    }

    /// Signed 16-bit PCM bytes in one frame.
    #[must_use]
    pub fn pcm_bytes(self) -> Option<usize> {
        self.interleaved_samples()?.checked_mul(size_of::<i16>())
    }

    #[must_use]
    pub fn is_v1(self) -> bool {
        self == Self::V1
    }
}

/// One codec's exact fixed-format support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioCodecCapability {
    pub codec: AudioCodec,
    pub frame_spec: AudioFrameSpec,
    pub fec: bool,
    pub dtx: bool,
}

/// Runtime audio bandwidth tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum AudioBitrateTier {
    #[default]
    Off,
    Kbps32,
    Kbps64,
    Kbps128,
    Kbps256,
    Kbps510,
}

impl AudioBitrateTier {
    /// Resolve a requested bitrate as a ceiling over the supported tiers.
    #[must_use]
    pub const fn from_ceiling_kbps(kbps: u32) -> Self {
        match kbps {
            0..=31 => Self::Off,
            32..=63 => Self::Kbps32,
            64..=127 => Self::Kbps64,
            128..=255 => Self::Kbps128,
            256..=509 => Self::Kbps256,
            _ => Self::Kbps510,
        }
    }

    #[must_use]
    pub const fn kbps(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Kbps32 => Some(32),
            Self::Kbps64 => Some(64),
            Self::Kbps128 => Some(128),
            Self::Kbps256 => Some(256),
            Self::Kbps510 => Some(510),
        }
    }
}

impl From<AudioBitrateTier> for AudioBitrateTierMsg {
    fn from(value: AudioBitrateTier) -> Self {
        match value {
            AudioBitrateTier::Off => Self::Off,
            AudioBitrateTier::Kbps32 => Self::Kbps32,
            AudioBitrateTier::Kbps64 => Self::Kbps64,
            AudioBitrateTier::Kbps128 => Self::Kbps128,
            AudioBitrateTier::Kbps256 => Self::Kbps256,
            AudioBitrateTier::Kbps510 => Self::Kbps510,
        }
    }
}

/// Whether audio uses the deployed compatibility path or explicit audio-v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioProtocolMode {
    Legacy,
    V1,
}

/// Selected attachment-scoped audio behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAudioStream {
    pub mode: AudioProtocolMode,
    pub codec: Option<AudioCodec>,
    pub frame_spec: AudioFrameSpec,
    pub bitrate: AudioBitrateTier,
    pub fec: bool,
    pub dtx: bool,
    pub reason: AudioStreamReason,
}

impl ResolvedAudioStream {
    #[must_use]
    pub const fn disabled(mode: AudioProtocolMode, reason: AudioStreamReason) -> Self {
        Self {
            mode,
            codec: None,
            frame_spec: AudioFrameSpec::V1,
            bitrate: AudioBitrateTier::Off,
            fec: false,
            dtx: false,
            reason,
        }
    }

    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.codec.is_some() && !matches!(self.bitrate, AudioBitrateTier::Off)
    }

    #[must_use]
    pub fn result(self) -> Option<AudioStreamResultMsg> {
        if self.mode != AudioProtocolMode::V1 {
            return None;
        }
        let Some(codec) = self.codec else {
            return Some(AudioStreamResultMsg::disabled(self.reason));
        };
        Some(AudioStreamResultMsg::enabled(
            AudioStreamConfigMsg {
                protocol_version: AUDIO_PROTOCOL_VERSION,
                codec,
                sample_rate_hz: self.frame_spec.sample_rate_hz,
                channels: self.frame_spec.channels,
                frame_duration_ms: self.frame_spec.frame_duration_ms,
                bitrate: self.bitrate.into(),
                fec: self.fec,
                dtx: self.dtx,
            },
            self.reason,
        ))
    }
}

/// Host-side deterministic audio selection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPolicy {
    pub opus_available: bool,
    pub pcm_available: bool,
}

/// Operator-selected codec policy with a fixed Opus bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfiguredAudioPolicy {
    policy: AudioPolicy,
}

impl AudioPolicy {
    /// Construct the exact operator-selected output codec policy.
    #[must_use]
    pub const fn configured(enabled: bool, compressed: bool) -> ConfiguredAudioPolicy {
        ConfiguredAudioPolicy {
            policy: Self {
                opus_available: enabled && compressed,
                pcm_available: enabled && !compressed,
            },
        }
    }

    /// Capabilities advertised by this host.
    #[must_use]
    pub fn capabilities(self) -> AudioOutputCapabilitiesMsg {
        let mut codecs = Vec::with_capacity(2);
        if self.opus_available {
            codecs.push(AudioCodec::Opus);
        }
        if self.pcm_available {
            codecs.push(AudioCodec::Pcm);
        }
        AudioOutputCapabilitiesMsg {
            protocol_version: AUDIO_PROTOCOL_VERSION,
            codecs,
            sample_rate_hz: AUDIO_V1_SAMPLE_RATE_HZ,
            channels: AUDIO_V1_CHANNELS,
            frame_duration_ms: AUDIO_V1_FRAME_DURATION_MS,
            fec: false,
            dtx: false,
        }
    }
}

impl ConfiguredAudioPolicy {
    #[must_use]
    pub fn capabilities(self) -> AudioOutputCapabilitiesMsg {
        self.policy.capabilities()
    }

    #[must_use]
    pub fn resolve(
        self,
        peer: Option<&AudioOutputCapabilitiesMsg>,
        enable_audio: bool,
    ) -> ResolvedAudioStream {
        self.policy
            .resolve(peer, enable_audio, CONFIGURED_OPUS_BITRATE_KBPS)
    }

    #[must_use]
    pub const fn without_opus(mut self) -> Self {
        self.policy.opus_available = false;
        self
    }

    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.policy.opus_available || self.policy.pcm_available
    }
}

impl AudioPolicy {
    /// Resolve host policy, peer capabilities, and runtime quality settings.
    #[must_use]
    pub fn resolve(
        self,
        peer: Option<&AudioOutputCapabilitiesMsg>,
        enable_audio: bool,
        requested_kbps: u32,
    ) -> ResolvedAudioStream {
        let mode = if peer.is_some() {
            AudioProtocolMode::V1
        } else {
            AudioProtocolMode::Legacy
        };
        if !enable_audio || requested_kbps == 0 {
            return ResolvedAudioStream::disabled(mode, AudioStreamReason::DisabledByPolicy);
        }
        let bitrate = AudioBitrateTier::from_ceiling_kbps(requested_kbps);
        if matches!(bitrate, AudioBitrateTier::Off) {
            return ResolvedAudioStream::disabled(mode, AudioStreamReason::BelowMinimumBitrate);
        }

        let Some(peer) = peer else {
            return if self.pcm_available {
                ResolvedAudioStream {
                    mode,
                    codec: Some(AudioCodec::Pcm),
                    frame_spec: AudioFrameSpec::V1,
                    bitrate,
                    fec: false,
                    dtx: false,
                    reason: AudioStreamReason::LegacyPcm,
                }
            } else {
                ResolvedAudioStream::disabled(mode, AudioStreamReason::NoCommonCodec)
            };
        };
        if peer.protocol_version != AUDIO_PROTOCOL_VERSION {
            return ResolvedAudioStream::disabled(mode, AudioStreamReason::VersionMismatch);
        }
        if !peer.is_valid_v1() {
            return ResolvedAudioStream::disabled(mode, AudioStreamReason::InvalidCapabilities);
        }
        let codec = peer.codecs.iter().copied().find(|codec| match codec {
            AudioCodec::Opus => self.opus_available,
            AudioCodec::Pcm => self.pcm_available,
        });
        let Some(codec) = codec else {
            return ResolvedAudioStream::disabled(mode, AudioStreamReason::NoCommonCodec);
        };
        ResolvedAudioStream {
            mode,
            codec: Some(codec),
            frame_spec: AudioFrameSpec::V1,
            bitrate,
            fec: false,
            dtx: false,
            reason: AudioStreamReason::Enabled,
        }
    }
}

/// Ordering and loss classification for one audio timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioTimestampDecision {
    First,
    OnTime,
    Gap { missing_frames: u8 },
    Duplicate,
    Late,
    Discontinuity,
}

/// Wrapping-u32 timestamp tracker for fixed 20 ms audio-v1 frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioTimestampTracker {
    last_timestamp_ms: Option<u32>,
}

impl AudioTimestampTracker {
    #[must_use]
    pub fn observe(&mut self, timestamp_ms: u32) -> AudioTimestampDecision {
        let Some(previous) = self.last_timestamp_ms else {
            self.last_timestamp_ms = Some(timestamp_ms);
            return AudioTimestampDecision::First;
        };
        let delta = timestamp_ms.wrapping_sub(previous);
        if delta == 0 {
            return AudioTimestampDecision::Duplicate;
        }
        if delta > i32::MAX as u32 {
            return AudioTimestampDecision::Late;
        }

        self.last_timestamp_ms = Some(timestamp_ms);
        let cadence = u32::from(AUDIO_V1_FRAME_DURATION_MS);
        if delta == cadence {
            return AudioTimestampDecision::OnTime;
        }
        if delta % cadence != 0 {
            return AudioTimestampDecision::Discontinuity;
        }
        let missing = delta / cadence - 1;
        match u8::try_from(missing) {
            Ok(missing_frames) if (1..=MAX_PLC_FRAMES).contains(&missing_frames) => {
                AudioTimestampDecision::Gap { missing_frames }
            }
            _ => AudioTimestampDecision::Discontinuity,
        }
    }

    pub fn reset(&mut self) {
        self.last_timestamp_ms = None;
    }
}

/// Caller action produced by bounded jitter policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioJitterAction {
    pub accept: bool,
    pub plc_frames: u8,
    pub reset: bool,
    pub rebuffer: bool,
    pub trim_frames: u8,
}

/// Bounded, clock-free decoded-frame queue policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioJitterBuffer {
    timestamps: AudioTimestampTracker,
    queued_frames: u8,
    prebuffering: bool,
}

impl Default for AudioJitterBuffer {
    fn default() -> Self {
        let mut buffer = Self {
            timestamps: AudioTimestampTracker::default(),
            queued_frames: 0,
            prebuffering: false,
        };
        buffer.reset();
        buffer
    }
}

impl AudioJitterBuffer {
    pub const TARGET_FRAMES: u8 = 3;
    pub const TRIM_THRESHOLD_FRAMES: u8 = 5;
    pub const MAX_FRAMES: u8 = 10;

    #[must_use]
    pub fn observe(&mut self, timestamp_ms: u32) -> AudioJitterAction {
        let decision = self.timestamps.observe(timestamp_ms);
        match decision {
            AudioTimestampDecision::Duplicate | AudioTimestampDecision::Late => {
                return AudioJitterAction {
                    accept: false,
                    plc_frames: 0,
                    reset: false,
                    rebuffer: self.prebuffering,
                    trim_frames: 0,
                };
            }
            AudioTimestampDecision::Discontinuity => {
                self.reset();
                let _ = self.timestamps.observe(timestamp_ms);
                return AudioJitterAction {
                    accept: true,
                    plc_frames: 0,
                    reset: true,
                    rebuffer: true,
                    trim_frames: 0,
                };
            }
            _ => {}
        }

        let plc_frames = match decision {
            AudioTimestampDecision::Gap { missing_frames } => missing_frames,
            _ => 0,
        };
        self.queued_frames = self
            .queued_frames
            .saturating_add(plc_frames)
            .saturating_add(1)
            .min(Self::MAX_FRAMES);
        let trim_frames = if self.queued_frames > Self::TRIM_THRESHOLD_FRAMES {
            let trim = self.queued_frames.saturating_sub(Self::TARGET_FRAMES);
            self.queued_frames = self.queued_frames.saturating_sub(trim);
            trim
        } else {
            0
        };
        if self.prebuffering && self.queued_frames >= Self::TARGET_FRAMES {
            self.prebuffering = false;
        }
        AudioJitterAction {
            accept: true,
            plc_frames,
            reset: false,
            rebuffer: self.prebuffering,
            trim_frames,
        }
    }

    pub fn frame_played(&mut self) {
        self.queued_frames = self.queued_frames.saturating_sub(1);
        if self.queued_frames == 0 {
            self.prebuffering = true;
        }
    }

    pub fn reset(&mut self) {
        self.timestamps.reset();
        self.queued_frames = 0;
        self.prebuffering = true;
    }

    #[must_use]
    pub const fn queued_frames(self) -> u8 {
        self.queued_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_frame_arithmetic_is_exact_and_checked() {
        assert_eq!(AudioFrameSpec::V1.interleaved_samples(), Some(1_920));
        assert_eq!(AudioFrameSpec::V1.pcm_bytes(), Some(3_840));
        assert_eq!(
            AudioFrameSpec {
                sample_rate_hz: u32::MAX,
                channels: u8::MAX,
                frame_duration_ms: u16::MAX,
            }
            .pcm_bytes(),
            None
        );
    }

    #[test]
    fn bitrate_ceiling_uses_exact_tiers() {
        for (requested, expected) in [
            (0, AudioBitrateTier::Off),
            (31, AudioBitrateTier::Off),
            (32, AudioBitrateTier::Kbps32),
            (63, AudioBitrateTier::Kbps32),
            (64, AudioBitrateTier::Kbps64),
            (127, AudioBitrateTier::Kbps64),
            (128, AudioBitrateTier::Kbps128),
            (255, AudioBitrateTier::Kbps128),
            (256, AudioBitrateTier::Kbps256),
            (509, AudioBitrateTier::Kbps256),
            (510, AudioBitrateTier::Kbps510),
            (u32::MAX, AudioBitrateTier::Kbps510),
        ] {
            assert_eq!(AudioBitrateTier::from_ceiling_kbps(requested), expected);
        }
    }

    #[test]
    fn negotiation_requires_explicit_v1_for_opus() {
        let policy = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let legacy = policy.resolve(None, true, 128);
        assert_eq!(legacy.mode, AudioProtocolMode::Legacy);
        assert_eq!(legacy.codec, Some(AudioCodec::Pcm));
        assert_eq!(legacy.reason, AudioStreamReason::LegacyPcm);

        let v1 = policy.resolve(Some(&policy.capabilities()), true, 128);
        assert_eq!(v1.mode, AudioProtocolMode::V1);
        assert_eq!(v1.codec, Some(AudioCodec::Opus));
        assert!(
            v1.result()
                .expect("v1 result")
                .config
                .expect("enabled config")
                .is_valid_v1()
        );
        assert!(legacy.result().is_none());
    }

    #[test]
    fn configured_compression_is_strict_and_never_falls_back() {
        let compressed = AudioPolicy::configured(true, true);
        assert_eq!(
            compressed.resolve(None, true).reason,
            AudioStreamReason::NoCommonCodec
        );
        let compressed_stream = compressed.resolve(
            Some(
                &AudioPolicy {
                    opus_available: true,
                    pcm_available: true,
                }
                .capabilities(),
            ),
            true,
        );
        assert_eq!(compressed_stream.codec, Some(AudioCodec::Opus));
        assert_eq!(compressed_stream.bitrate, AudioBitrateTier::Kbps128);

        let uncompressed = AudioPolicy::configured(true, false);
        assert_eq!(
            uncompressed
                .resolve(Some(&compressed.capabilities()), true)
                .reason,
            AudioStreamReason::NoCommonCodec
        );
        assert_eq!(
            uncompressed.resolve(None, true).codec,
            Some(AudioCodec::Pcm)
        );
    }

    #[test]
    fn negotiation_fails_closed_for_mismatch_and_low_bitrate() {
        let policy = AudioPolicy {
            opus_available: true,
            pcm_available: false,
        };
        let mut peer = policy.capabilities();
        peer.protocol_version = 2;
        assert_eq!(
            policy.resolve(Some(&peer), true, 128).reason,
            AudioStreamReason::VersionMismatch
        );
        assert_eq!(
            policy.resolve(None, true, 31).reason,
            AudioStreamReason::BelowMinimumBitrate
        );
    }

    #[test]
    fn timestamps_handle_wrap_duplicates_gaps_and_late_packets() {
        let mut tracker = AudioTimestampTracker::default();
        assert_eq!(tracker.observe(u32::MAX - 9), AudioTimestampDecision::First);
        assert_eq!(tracker.observe(10), AudioTimestampDecision::OnTime);
        assert_eq!(tracker.observe(10), AudioTimestampDecision::Duplicate);
        assert_eq!(
            tracker.observe(70),
            AudioTimestampDecision::Gap { missing_frames: 2 }
        );
        assert_eq!(tracker.observe(50), AudioTimestampDecision::Late);
        assert_eq!(tracker.observe(170), AudioTimestampDecision::Discontinuity);
    }

    #[test]
    fn jitter_bounds_plc_trim_and_rebuffer() {
        let mut jitter = AudioJitterBuffer::default();
        jitter.reset();
        assert!(jitter.observe(0).rebuffer);
        assert!(jitter.observe(20).rebuffer);
        assert!(!jitter.observe(40).rebuffer);
        let gap = jitter.observe(100);
        assert_eq!(gap.plc_frames, 2);
        for timestamp in [120, 140, 160, 180, 200] {
            assert!(jitter.observe(timestamp).accept);
        }
        assert!(jitter.queued_frames() <= AudioJitterBuffer::TRIM_THRESHOLD_FRAMES);

        let reset = jitter.observe(500);
        assert!(reset.reset);
        assert!(reset.rebuffer);
        assert_eq!(jitter.queued_frames(), 0);
    }
}
