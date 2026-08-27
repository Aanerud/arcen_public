use std::error::Error;
use std::fmt::{Display, Formatter};

use opusic_c::{
    Application, Bitrate, Channels, Decoder as NativeDecoder, Encoder as NativeEncoder, ErrorCode,
    InbandFec, SampleRate,
};
use zeroize::Zeroize;

use super::{AudioBitrateTier, AudioFrameSpec, MAX_OPUS_PACKET_BYTES};

const MAX_INTERLEAVED_SAMPLES: usize = 1_920;
const SAMPLES_PER_CHANNEL: usize = 960;

/// Privacy-safe classification of a native Opus failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusErrorKind {
    BadArgument,
    AllocationFailed,
    InvalidState,
    InvalidPacket,
    BufferTooSmall,
    Internal,
    Unsupported,
    Unknown,
}

impl From<ErrorCode> for OpusErrorKind {
    fn from(value: ErrorCode) -> Self {
        match value {
            ErrorCode::BadArg => Self::BadArgument,
            ErrorCode::AllocFail => Self::AllocationFailed,
            ErrorCode::InvalidState => Self::InvalidState,
            ErrorCode::InvalidPacket => Self::InvalidPacket,
            ErrorCode::BufferTooSmall => Self::BufferTooSmall,
            ErrorCode::Internal => Self::Internal,
            ErrorCode::Unimplemented => Self::Unsupported,
            ErrorCode::Ok | ErrorCode::Unknown => Self::Unknown,
        }
    }
}

/// Typed codec failure that never contains audio content or unbounded native text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusError {
    AudioDisabled,
    InvalidPcmLength,
    InvalidPacketLength,
    OutputBufferTooSmall,
    UnexpectedFrameSize,
    UnsupportedFrameSpec,
    Native(OpusErrorKind),
}

impl Display for OpusError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::AudioDisabled => "audio bitrate is disabled",
            Self::InvalidPcmLength => "PCM frame has an invalid length",
            Self::InvalidPacketLength => "Opus packet has an invalid length",
            Self::OutputBufferTooSmall => "caller output buffer is too small",
            Self::UnexpectedFrameSize => "Opus returned an unexpected frame size",
            Self::UnsupportedFrameSpec => "audio frame specification is unsupported",
            Self::Native(kind) => match kind {
                OpusErrorKind::BadArgument => "Opus rejected an argument",
                OpusErrorKind::AllocationFailed => "Opus allocation failed",
                OpusErrorKind::InvalidState => "Opus state is invalid",
                OpusErrorKind::InvalidPacket => "Opus packet is invalid",
                OpusErrorKind::BufferTooSmall => "Opus buffer is too small",
                OpusErrorKind::Internal => "Opus reported an internal error",
                OpusErrorKind::Unsupported => "Opus operation is unsupported",
                OpusErrorKind::Unknown => "Opus reported an unknown error",
            },
        })
    }
}

impl Error for OpusError {}

fn native(error: ErrorCode) -> OpusError {
    OpusError::Native(error.into())
}

/// Attachment-scoped fixed-format Opus encoder.
pub struct OpusEncoder {
    inner: NativeEncoder,
    scratch: [u16; MAX_INTERLEAVED_SAMPLES],
    bitrate: AudioBitrateTier,
    frame_spec: AudioFrameSpec,
}

impl std::fmt::Debug for OpusEncoder {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpusEncoder")
            .field("frame_spec", &self.frame_spec)
            .field("bitrate", &self.bitrate)
            .finish_non_exhaustive()
    }
}

impl OpusEncoder {
    /// Create and configure the fixed audio-v1 encoder.
    ///
    /// # Errors
    ///
    /// Returns a typed failure if the tier is disabled or libopus rejects setup.
    pub fn new(bitrate: AudioBitrateTier) -> Result<Self, OpusError> {
        Self::new_for_spec(AudioFrameSpec::V1, bitrate)
    }

    /// Create an encoder for an admitted fixed 48 kHz, 20 ms mono or stereo spec.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for any other format or native setup failure.
    pub fn new_for_spec(
        frame_spec: AudioFrameSpec,
        bitrate: AudioBitrateTier,
    ) -> Result<Self, OpusError> {
        if matches!(bitrate, AudioBitrateTier::Off) {
            return Err(OpusError::AudioDisabled);
        }
        let channels = native_channels(frame_spec)?;
        let mut inner = NativeEncoder::new(channels, SampleRate::Hz48000, Application::Audio)
            .map_err(native)?;
        configure_encoder(&mut inner, bitrate)?;
        Ok(Self {
            inner,
            scratch: [0; MAX_INTERLEAVED_SAMPLES],
            bitrate,
            frame_spec,
        })
    }

    #[must_use]
    pub const fn bitrate(&self) -> AudioBitrateTier {
        self.bitrate
    }

    #[must_use]
    pub const fn frame_spec(&self) -> AudioFrameSpec {
        self.frame_spec
    }

    /// Update constrained-VBR bitrate without replacing codec state.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for `Off` or a rejected native control.
    pub fn set_bitrate(&mut self, bitrate: AudioBitrateTier) -> Result<(), OpusError> {
        let Some(kbps) = bitrate.kbps() else {
            return Err(OpusError::AudioDisabled);
        };
        self.inner
            .set_bitrate(Bitrate::Value(kbps * 1_000))
            .map_err(native)?;
        self.bitrate = bitrate;
        Ok(())
    }

    /// Encode one exact 20 ms stereo signed-PCM frame into caller-owned storage.
    ///
    /// # Errors
    ///
    /// Rejects malformed PCM or undersized output before calling libopus.
    pub fn encode(&mut self, pcm: &[i16], output: &mut [u8]) -> Result<usize, OpusError> {
        let result = self.encode_inner(pcm, output);
        self.scratch.zeroize();
        result
    }

    fn encode_inner(&mut self, pcm: &[i16], output: &mut [u8]) -> Result<usize, OpusError> {
        let samples = self
            .frame_spec
            .interleaved_samples()
            .ok_or(OpusError::UnsupportedFrameSpec)?;
        if pcm.len() != samples {
            return Err(OpusError::InvalidPcmLength);
        }
        let output = output
            .get_mut(..MAX_OPUS_PACKET_BYTES)
            .ok_or(OpusError::OutputBufferTooSmall)?;
        for (destination, sample) in self.scratch[..samples].iter_mut().zip(pcm) {
            *destination = u16::from_ne_bytes(sample.to_ne_bytes());
        }
        let encoded = self
            .inner
            .encode_to_slice(&self.scratch[..samples], output)
            .map_err(native)?;
        if encoded == 0 || encoded > MAX_OPUS_PACKET_BYTES {
            return Err(OpusError::InvalidPacketLength);
        }
        Ok(encoded)
    }

    /// Reset predictor state after capture restart or timestamp discontinuity.
    ///
    /// # Errors
    ///
    /// Returns a typed native failure.
    pub fn reset(&mut self) -> Result<(), OpusError> {
        let result = self.inner.reset().map_err(native);
        self.scratch.zeroize();
        result
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        let _ = self.inner.reset();
        self.scratch.zeroize();
    }
}

fn configure_encoder(
    encoder: &mut NativeEncoder,
    bitrate: AudioBitrateTier,
) -> Result<(), OpusError> {
    let Some(kbps) = bitrate.kbps() else {
        return Err(OpusError::AudioDisabled);
    };
    encoder
        .set_bitrate(Bitrate::Value(kbps * 1_000))
        .map_err(native)?;
    encoder.set_vbr(true).map_err(native)?;
    encoder.set_vbr_constraint(true).map_err(native)?;
    encoder.set_dtx(false).map_err(native)?;
    encoder.set_inband_fec(InbandFec::Off).map_err(native)?;
    encoder.set_packet_loss(0).map_err(native)?;
    Ok(())
}

/// Attachment-scoped fixed-format Opus decoder.
pub struct OpusDecoder {
    inner: NativeDecoder,
    scratch: [u16; MAX_INTERLEAVED_SAMPLES],
    frame_spec: AudioFrameSpec,
}

impl std::fmt::Debug for OpusDecoder {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpusDecoder")
            .field("frame_spec", &self.frame_spec)
            .finish_non_exhaustive()
    }
}

impl OpusDecoder {
    /// Create the fixed audio-v1 stereo decoder.
    ///
    /// # Errors
    ///
    /// Returns a typed native setup failure.
    pub fn new() -> Result<Self, OpusError> {
        Self::new_for_spec(AudioFrameSpec::V1)
    }

    /// Create a decoder for an admitted fixed 48 kHz, 20 ms mono or stereo spec.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for any other format or native setup failure.
    pub fn new_for_spec(frame_spec: AudioFrameSpec) -> Result<Self, OpusError> {
        Ok(Self {
            inner: NativeDecoder::new(native_channels(frame_spec)?, SampleRate::Hz48000)
                .map_err(native)?,
            scratch: [0; MAX_INTERLEAVED_SAMPLES],
            frame_spec,
        })
    }

    #[must_use]
    pub const fn frame_spec(&self) -> AudioFrameSpec {
        self.frame_spec
    }

    /// Decode one bounded Opus packet into exactly one caller-owned PCM frame.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or malformed packets and wrong output sizes.
    pub fn decode(&mut self, packet: &[u8], output: &mut [i16]) -> Result<(), OpusError> {
        if packet.is_empty() || packet.len() > MAX_OPUS_PACKET_BYTES {
            self.clear_after_decode_failure(output);
            return Err(OpusError::InvalidPacketLength);
        }
        self.decode_inner(packet, output)
    }

    /// Synthesize exactly one missing 20 ms frame with Opus PLC.
    ///
    /// # Errors
    ///
    /// Rejects a wrong output size or a native decode failure.
    pub fn decode_plc(&mut self, output: &mut [i16]) -> Result<(), OpusError> {
        self.decode_inner(&[], output)
    }

    fn decode_inner(&mut self, packet: &[u8], output: &mut [i16]) -> Result<(), OpusError> {
        let result = self.decode_inner_unchecked(packet, output);
        self.scratch.zeroize();
        if result.is_err() {
            self.clear_after_decode_failure(output);
        }
        result
    }

    fn clear_after_decode_failure(&mut self, output: &mut [i16]) {
        output.zeroize();
        self.scratch.zeroize();
        let _ = self.inner.reset();
    }

    fn decode_inner_unchecked(
        &mut self,
        packet: &[u8],
        output: &mut [i16],
    ) -> Result<(), OpusError> {
        let samples = self
            .frame_spec
            .interleaved_samples()
            .ok_or(OpusError::UnsupportedFrameSpec)?;
        if output.len() != samples {
            return Err(OpusError::InvalidPcmLength);
        }
        let decoded = self
            .inner
            .decode_to_slice(packet, &mut self.scratch[..samples], false)
            .map_err(native)?;
        if decoded != SAMPLES_PER_CHANNEL {
            return Err(OpusError::UnexpectedFrameSize);
        }
        for (destination, sample) in output.iter_mut().zip(&self.scratch[..samples]) {
            *destination = i16::from_ne_bytes(sample.to_ne_bytes());
        }

        Ok(())
    }

    /// Reset predictor state after reconnect or a large timestamp gap.
    ///
    /// # Errors
    ///
    /// Returns a typed native failure.
    pub fn reset(&mut self) -> Result<(), OpusError> {
        let result = self.inner.reset().map_err(native);
        self.scratch.zeroize();
        result
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        let _ = self.inner.reset();
        self.scratch.zeroize();
    }
}

fn native_channels(frame_spec: AudioFrameSpec) -> Result<Channels, OpusError> {
    if frame_spec.sample_rate_hz != 48_000 || frame_spec.frame_duration_ms != 20 {
        return Err(OpusError::UnsupportedFrameSpec);
    }
    match frame_spec.channels {
        1 => Ok(Channels::Mono),
        2 => Ok(Channels::Stereo),
        _ => Err(OpusError::UnsupportedFrameSpec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(input: &[i16], tier: AudioBitrateTier) -> (Vec<i16>, usize) {
        let mut encoder = OpusEncoder::new(tier).expect("encoder");
        let mut decoder = OpusDecoder::new().expect("decoder");
        let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
        let encoded = encoder.encode(input, &mut packet).expect("encode");
        let mut decoded = vec![0i16; MAX_INTERLEAVED_SAMPLES];
        decoder
            .decode(&packet[..encoded], &mut decoded)
            .expect("decode");
        (decoded, encoded)
    }

    #[test]
    fn native_signed_bits_round_trip_through_scratch() {
        for sample in [i16::MIN, -1, 0, 1, i16::MAX] {
            let unsigned = u16::from_ne_bytes(sample.to_ne_bytes());
            assert_eq!(i16::from_ne_bytes(unsigned.to_ne_bytes()), sample);
        }
    }

    #[test]
    fn synthetic_frames_are_bounded_and_stereo_decode_is_complete() {
        let silence = [0i16; MAX_INTERLEAVED_SAMPLES];
        let (decoded, encoded) = roundtrip(&silence, AudioBitrateTier::Kbps128);
        assert_eq!(decoded.len(), MAX_INTERLEAVED_SAMPLES);
        assert!((1..=MAX_OPUS_PACKET_BYTES).contains(&encoded));

        let mut impulse = [0i16; MAX_INTERLEAVED_SAMPLES];
        impulse[0] = i16::MAX;
        impulse[1] = i16::MIN;
        let (decoded, encoded) = roundtrip(&impulse, AudioBitrateTier::Kbps128);
        assert_eq!(decoded.len(), MAX_INTERLEAVED_SAMPLES);
        assert!((1..=MAX_OPUS_PACKET_BYTES).contains(&encoded));

        let mut random = [0i16; MAX_INTERLEAVED_SAMPLES];
        let mut state = 0x1234_5678u32;
        for sample in &mut random {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *sample = (state >> 16) as i16;
        }
        let (_, encoded) = roundtrip(&random, AudioBitrateTier::Kbps128);
        assert!((1..=MAX_OPUS_PACKET_BYTES).contains(&encoded));
    }

    #[test]
    fn sine_roundtrip_has_bounded_error_on_both_channels() {
        let mut input = [0i16; MAX_INTERLEAVED_SAMPLES];
        for (frame, stereo) in input.chunks_exact_mut(2).enumerate() {
            let phase = 2.0 * std::f64::consts::PI * 440.0 * frame as f64 / 48_000.0;
            stereo[0] = (phase.sin() * 12_000.0) as i16;
            stereo[1] = (phase.cos() * 8_000.0) as i16;
        }
        let (decoded, _) = roundtrip(&input, AudioBitrateTier::Kbps128);
        let best_snr_db = (0..=480)
            .map(|lag_frames| {
                let lag_samples = lag_frames * 2;
                let compared = MAX_INTERLEAVED_SAMPLES - lag_samples;
                let signal = input[..compared]
                    .iter()
                    .map(|sample| f64::from(*sample).powi(2))
                    .sum::<f64>();
                let noise = input[..compared]
                    .iter()
                    .zip(&decoded[lag_samples..])
                    .map(|(expected, actual)| (f64::from(*expected) - f64::from(*actual)).powi(2))
                    .sum::<f64>();
                10.0 * (signal / noise.max(1.0)).log10()
            })
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            best_snr_db > 10.0,
            "unexpected aligned SNR: {best_snr_db:.2} dB"
        );
        assert!(decoded.chunks_exact(2).any(|frame| frame[0] != 0));
        assert!(decoded.chunks_exact(2).any(|frame| frame[1] != 0));
    }

    #[test]
    fn every_tier_updates_without_recreating_state() {
        let mut encoder = OpusEncoder::new(AudioBitrateTier::Kbps32).expect("encoder");
        let silence = [0i16; MAX_INTERLEAVED_SAMPLES];
        let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
        for tier in [
            AudioBitrateTier::Kbps32,
            AudioBitrateTier::Kbps64,
            AudioBitrateTier::Kbps128,
            AudioBitrateTier::Kbps256,
            AudioBitrateTier::Kbps510,
        ] {
            encoder.set_bitrate(tier).expect("bitrate update");
            assert_eq!(encoder.bitrate(), tier);
            let encoded = encoder.encode(&silence, &mut packet).expect("encode");
            assert!((1..=MAX_OPUS_PACKET_BYTES).contains(&encoded));
        }
        assert_eq!(
            encoder.set_bitrate(AudioBitrateTier::Off),
            Err(OpusError::AudioDisabled)
        );
    }

    #[test]
    fn packet_and_frame_bounds_fail_closed() {
        let mut encoder = OpusEncoder::new(AudioBitrateTier::Kbps128).expect("encoder");
        let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
        assert_eq!(
            encoder.encode(&[0; MAX_INTERLEAVED_SAMPLES - 1], &mut packet),
            Err(OpusError::InvalidPcmLength)
        );
        assert_eq!(
            encoder.encode(&[0; MAX_INTERLEAVED_SAMPLES], &mut packet[..100]),
            Err(OpusError::OutputBufferTooSmall)
        );

        let mut decoder = OpusDecoder::new().expect("decoder");
        let mut pcm = [0i16; MAX_INTERLEAVED_SAMPLES];
        assert_eq!(
            decoder.decode(&[], &mut pcm),
            Err(OpusError::InvalidPacketLength)
        );
        assert_eq!(
            decoder.decode(&[0; MAX_OPUS_PACKET_BYTES + 1], &mut pcm),
            Err(OpusError::InvalidPacketLength)
        );
        assert!(decoder.decode(&[0xff], &mut pcm).is_err());
    }

    #[test]
    fn scratch_and_outputs_are_zeroed_on_success_error_and_reset() {
        let input = [17i16; MAX_INTERLEAVED_SAMPLES];
        let mut encoder = OpusEncoder::new(AudioBitrateTier::Kbps128).expect("encoder");
        let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
        let packet_len = encoder.encode(&input, &mut packet).expect("encode");
        assert!(encoder.scratch.iter().all(|sample| *sample == 0));
        encoder.scratch.fill(u16::MAX);
        assert_eq!(
            encoder.encode(&input[..input.len() - 1], &mut packet),
            Err(OpusError::InvalidPcmLength)
        );
        assert!(encoder.scratch.iter().all(|sample| *sample == 0));
        encoder.scratch.fill(u16::MAX);
        encoder.reset().expect("encoder reset");
        assert!(encoder.scratch.iter().all(|sample| *sample == 0));

        let mut decoder = OpusDecoder::new().expect("decoder");
        let mut output = [23i16; MAX_INTERLEAVED_SAMPLES];
        decoder
            .decode(&packet[..packet_len], &mut output)
            .expect("decode");
        assert!(decoder.scratch.iter().all(|sample| *sample == 0));
        decoder.scratch.fill(u16::MAX);
        output.fill(23);
        assert!(decoder.decode(&[0xff], &mut output).is_err());
        assert!(decoder.scratch.iter().all(|sample| *sample == 0));
        assert!(output.iter().all(|sample| *sample == 0));
        decoder.scratch.fill(u16::MAX);
        decoder.reset().expect("decoder reset");
        assert!(decoder.scratch.iter().all(|sample| *sample == 0));
    }

    #[test]
    fn plc_is_bounded_by_caller_and_resettable() {
        let silence = [0i16; MAX_INTERLEAVED_SAMPLES];
        let mut encoder = OpusEncoder::new(AudioBitrateTier::Kbps128).expect("encoder");
        let mut decoder = OpusDecoder::new().expect("decoder");
        let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
        let encoded = encoder.encode(&silence, &mut packet).expect("encode");
        let mut pcm = [0i16; MAX_INTERLEAVED_SAMPLES];
        decoder
            .decode(&packet[..encoded], &mut pcm)
            .expect("decode");
        for _ in 0..super::super::MAX_PLC_FRAMES {
            decoder.decode_plc(&mut pcm).expect("PLC");
        }
        decoder.reset().expect("reset");
        encoder.reset().expect("reset");
    }

    #[test]
    fn invalid_packet_length_resets_predictor_before_plc() {
        let input = [12_345i16; MAX_INTERLEAVED_SAMPLES];
        let mut encoder = OpusEncoder::new(AudioBitrateTier::Kbps128).expect("encoder");
        let mut decoder = OpusDecoder::new().expect("decoder");
        let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
        let encoded = encoder.encode(&input, &mut packet).expect("encode");
        let mut pcm = [0i16; MAX_INTERLEAVED_SAMPLES];

        decoder
            .decode(&packet[..encoded], &mut pcm)
            .expect("decode");
        assert!(pcm.iter().any(|sample| *sample != 0));
        assert_eq!(
            decoder.decode(&[], &mut pcm),
            Err(OpusError::InvalidPacketLength)
        );
        pcm.fill(i16::MAX);
        decoder.decode_plc(&mut pcm).expect("PLC after reset");
        assert!(pcm.iter().all(|sample| *sample == 0));

        decoder
            .decode(&packet[..encoded], &mut pcm)
            .expect("decode after reset");
        assert_eq!(
            decoder.decode(&[0; MAX_OPUS_PACKET_BYTES + 1], &mut pcm),
            Err(OpusError::InvalidPacketLength)
        );
        pcm.fill(i16::MAX);
        decoder
            .decode_plc(&mut pcm)
            .expect("PLC after oversized packet");
        assert!(pcm.iter().all(|sample| *sample == 0));
    }

    #[test]
    fn mono_uses_the_same_reusable_codec_contract() {
        let spec = AudioFrameSpec::MICROPHONE_V1;
        let mut encoder =
            OpusEncoder::new_for_spec(spec, AudioBitrateTier::Kbps64).expect("mono encoder");
        let mut decoder = OpusDecoder::new_for_spec(spec).expect("mono decoder");
        let input = [0i16; SAMPLES_PER_CHANNEL];
        let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
        let encoded = encoder.encode(&input, &mut packet).expect("encode mono");
        let mut output = [1i16; SAMPLES_PER_CHANNEL];
        decoder
            .decode(&packet[..encoded], &mut output)
            .expect("decode mono");
        assert_eq!(encoder.frame_spec(), spec);
        assert_eq!(decoder.frame_spec(), spec);
    }
}
