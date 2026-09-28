//! Pure audio format, negotiation, timestamp, and playout policy.

use arcen_protocol::AudioCodec;

mod microphone;
#[cfg(feature = "audio-opus")]
mod opus;
pub mod playout;
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

/// How many encoded audio packets a host may hold for one Deck before the
/// oldest stop being worth sending.
///
/// Eight packets is 160 ms at [`AUDIO_V1_FRAME_DURATION_MS`], which sits under
/// the [`JITTER_MAX_MS`] a Deck will hold. Audio older than that cannot be
/// played in time whatever the host does with it, so sending it costs the
/// writer without buying the listener anything.
pub const AUDIO_SEND_BACKLOG_PACKETS: usize = 8;

/// Audio packets a host may buffer when audio has its own priority transport.
///
/// This is a memory and burst bound, not a staleness policy: unlike audio sent
/// on the video/session stream, priority audio does not make pictures wait
/// behind it. A calm path can still produce short scheduling bursts from the
/// OS audio callback, so the bound must be large enough not to turn those into
/// audible dropouts.
pub const AUDIO_PRIORITY_SEND_BACKLOG_PACKETS: usize = 64;

/// Drops the oldest packets from a backlog that has outgrown what is still
/// live, and reports how many were dropped.
///
/// A host draining a backlog writes each packet to the same connection its
/// video uses. Draining without a bound therefore lets a burst of stale audio
/// hold the writer while frames wait behind it — the listener gets sound that
/// is already too late to play, and the person watching gets a visible stall
/// in exchange.
///
/// Dropping the oldest rather than refusing the newest is the same choice the
/// capture side makes: what a listener wants is the sound happening now.
///
/// # Examples
///
/// ```
/// # use arcen_media::audio::trim_audio_backlog;
/// let mut packets = vec![1, 2, 3, 4, 5];
/// assert_eq!(trim_audio_backlog(&mut packets, 2), 3);
/// assert_eq!(packets, vec![4, 5]);
/// ```
pub fn trim_audio_backlog<T>(packets: &mut Vec<T>, capacity: usize) -> usize {
    let Some(excess) = packets.len().checked_sub(capacity) else {
        return 0;
    };
    if excess == 0 {
        return 0;
    }
    packets.drain(..excess);
    excess
}

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

/// Turns a stream of float samples into fixed-size PCM packets.
///
/// Capture hardware delivers whatever buffer size its clock happens to
/// produce — 512 frames here, 480 there, varying with the device. The wire
/// format is fixed at one 20 ms frame, so something has to hold the remainder
/// between callbacks. Doing that in the platform adapter means writing it
/// again for every platform and getting the boundary arithmetic wrong in a
/// different way each time.
///
/// Samples are clamped before conversion. Float audio is nominally
/// -1.0..=1.0, but a device can exceed it, and letting that wrap an `i16`
/// turns a loud passage into a burst of noise at the opposite polarity.
#[derive(Debug, Clone)]
pub struct PcmPacketizer {
    spec: AudioFrameSpec,
    samples_per_packet: usize,
    pending: Vec<i16>,
}

impl PcmPacketizer {
    /// Creates a packetizer for `spec`.
    ///
    /// Returns `None` for a specification that does not describe a whole
    /// number of samples.
    #[must_use]
    pub fn new(spec: AudioFrameSpec) -> Option<Self> {
        let samples_per_packet = spec.interleaved_samples()?;
        // A specification that rounds down to no samples — a 1 ms frame at
        // 1 Hz, say — would make `push` emit empty packets forever without
        // consuming input. Refusing it here is the only place that is cheap.
        if samples_per_packet == 0 {
            return None;
        }
        Some(Self {
            spec,
            samples_per_packet,
            pending: Vec::with_capacity(samples_per_packet * 2),
        })
    }

    /// Returns the specification being produced.
    #[must_use]
    pub const fn spec(&self) -> AudioFrameSpec {
        self.spec
    }

    /// Returns how many samples are held back, waiting to complete a packet.
    #[must_use]
    pub fn pending_samples(&self) -> usize {
        self.pending.len()
    }

    /// Accepts interleaved float samples and appends any completed packets.
    ///
    /// Each completed packet is exactly [`AudioFrameSpec::interleaved_samples`]
    /// long, so a caller can hand it straight to an encoder or to the PCM wire
    /// format without checking again.
    pub fn push(&mut self, samples: &[f32], packets: &mut Vec<Vec<i16>>) {
        self.pending
            .extend(samples.iter().map(|&sample| to_i16(sample)));
        while self.pending.len() >= self.samples_per_packet {
            let rest = self.pending.split_off(self.samples_per_packet);
            packets.push(std::mem::replace(&mut self.pending, rest));
        }
    }

    /// Discards anything held back.
    ///
    /// Used when a stream is replaced: carrying samples across a generation
    /// boundary would splice one session's audio onto another's.
    pub fn reset(&mut self) {
        self.pending.clear();
    }
}

/// Converts one float sample to the wire's 16-bit integer form.
///
/// Clamped rather than wrapped, and NaN becomes silence rather than an
/// arbitrary value.
#[must_use]
fn to_i16(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    // `i16::MAX` rather than 32768: scaling by the larger value lets a sample
    // of exactly 1.0 overflow, which is the classic way this conversion
    // produces a click at full scale.
    let scaled = sample.clamp(-1.0, 1.0) * f32::from(i16::MAX);
    // The clamp above bounds this to +/-32767, well inside `i16`.
    #[allow(clippy::cast_possible_truncation)]
    {
        scaled as i16
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn packets_are_exactly_one_frame_and_the_remainder_is_kept() {
        // Capture hardware delivers whatever its clock produces, never
        // conveniently aligned to the wire's frame. Losing the remainder would
        // drop a few milliseconds of audio on every callback.
        let spec = AudioFrameSpec::V1;
        let wanted = spec.interleaved_samples().expect("v1 is valid");
        let mut packetizer = PcmPacketizer::new(spec).expect("v1 is valid");
        let mut packets = Vec::new();

        packetizer.push(&vec![0.5; wanted + 100], &mut packets);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].len(), wanted);
        assert_eq!(packetizer.pending_samples(), 100);

        // The held-back samples complete the next packet rather than being
        // dropped or duplicated.
        packetizer.push(&vec![0.5; wanted - 100], &mut packets);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[1].len(), wanted);
        assert_eq!(packetizer.pending_samples(), 0);
    }

    #[test]
    fn several_packets_can_complete_in_one_push() {
        let spec = AudioFrameSpec::V1;
        let wanted = spec.interleaved_samples().expect("valid");
        let mut packetizer = PcmPacketizer::new(spec).expect("valid");
        let mut packets = Vec::new();
        packetizer.push(&vec![0.0; wanted * 3], &mut packets);
        assert_eq!(packets.len(), 3);
        assert!(packets.iter().all(|packet| packet.len() == wanted));
        assert_eq!(packetizer.pending_samples(), 0);
    }

    #[test]
    fn full_scale_does_not_wrap_to_the_opposite_polarity() {
        // Scaling by 32768 lets exactly 1.0 overflow to -32768, which is heard
        // as a click at the loudest moment of the audio.
        assert_eq!(to_i16(1.0), i16::MAX);
        assert_eq!(to_i16(-1.0), -i16::MAX);
        assert_eq!(to_i16(2.0), i16::MAX, "over-range clamps, never wraps");
        assert_eq!(to_i16(-2.0), -i16::MAX);
        assert_eq!(to_i16(0.0), 0);
        assert_eq!(to_i16(f32::NAN), 0, "a bad sample becomes silence");
        assert_eq!(to_i16(f32::INFINITY), 0);
    }

    #[test]
    fn a_reset_does_not_splice_one_session_onto_another() {
        let spec = AudioFrameSpec::V1;
        let wanted = spec.interleaved_samples().expect("valid");
        let mut packetizer = PcmPacketizer::new(spec).expect("valid");
        let mut packets = Vec::new();
        packetizer.push(&vec![1.0; 100], &mut packets);
        assert_eq!(packetizer.pending_samples(), 100);

        packetizer.reset();
        assert_eq!(packetizer.pending_samples(), 0);

        // The next packet is made entirely of new samples.
        packetizer.push(&vec![0.0; wanted], &mut packets);
        assert_eq!(packets.len(), 1);
        assert!(packets[0].iter().all(|&sample| sample == 0));
    }

    #[test]
    fn a_specification_with_no_samples_is_refused() {
        // This rounds down to zero samples per packet. Accepting it makes
        // `push` emit empty packets forever without consuming input, which
        // hangs whichever thread called it — in the macOS host, the Core Audio
        // real-time thread.
        let degenerate = AudioFrameSpec {
            sample_rate_hz: 1,
            channels: 1,
            frame_duration_ms: 1,
        };
        assert_eq!(degenerate.interleaved_samples(), Some(0));
        assert!(PcmPacketizer::new(degenerate).is_none());
    }

    #[test]
    fn a_v1_packet_matches_the_documented_wire_size() {
        let spec = AudioFrameSpec::V1;
        assert_eq!(spec.interleaved_samples(), Some(1_920));
        assert_eq!(spec.pcm_bytes(), Some(3_840));
    }
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

#[cfg(test)]
mod backlog_tests {
    use super::{
        AUDIO_PRIORITY_SEND_BACKLOG_PACKETS, AUDIO_SEND_BACKLOG_PACKETS, trim_audio_backlog,
    };

    #[test]
    fn a_backlog_within_the_bound_is_left_alone() {
        let mut packets = vec![1, 2, 3];
        assert_eq!(trim_audio_backlog(&mut packets, 8), 0);
        assert_eq!(packets, vec![1, 2, 3]);
    }

    #[test]
    fn a_backlog_exactly_at_the_bound_is_left_alone() {
        let mut packets: Vec<u8> = (0..8).collect();
        assert_eq!(trim_audio_backlog(&mut packets, 8), 0);
        assert_eq!(packets.len(), 8);
    }

    #[test]
    fn an_overlong_backlog_keeps_the_newest_packets() {
        // The listener wants the sound happening now, so the oldest go.
        let mut packets: Vec<u8> = (0..20).collect();
        assert_eq!(trim_audio_backlog(&mut packets, 8), 12);
        assert_eq!(packets, (12..20).collect::<Vec<u8>>());
    }

    #[test]
    fn an_empty_backlog_drops_nothing() {
        let mut packets: Vec<u8> = Vec::new();
        assert_eq!(trim_audio_backlog(&mut packets, 8), 0);
        assert!(packets.is_empty());
    }

    #[test]
    fn a_zero_bound_drops_everything() {
        let mut packets = vec![1, 2, 3];
        assert_eq!(trim_audio_backlog(&mut packets, 0), 3);
        assert!(packets.is_empty());
    }

    #[test]
    fn the_bound_stays_under_what_a_deck_will_hold() {
        // Eight 20 ms packets is 160 ms, which must stay under JITTER_MAX_MS or
        // the host would be sending audio the Deck has already given up on.
        let held_ms = u32::try_from(AUDIO_SEND_BACKLOG_PACKETS)
            .expect("audio backlog packet bound fits u32")
            * u32::from(super::AUDIO_V1_FRAME_DURATION_MS);
        assert!(
            held_ms < u32::from(super::JITTER_MAX_MS),
            "a {held_ms} ms backlog is not worth sending to a Deck holding {} ms",
            super::JITTER_MAX_MS
        );
    }

    #[test]
    fn priority_audio_backlog_covers_callback_bursts() {
        assert!(AUDIO_PRIORITY_SEND_BACKLOG_PACKETS > AUDIO_SEND_BACKLOG_PACKETS);
    }
}
