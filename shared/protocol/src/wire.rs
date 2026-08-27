pub const PROTOCOL_VERSION: u16 = 4;
pub const VIDEO_HEADER_SIZE: usize = 10;
pub const REGION_VIDEO_HEADER_SIZE: usize = 26;
/// Bit in [`VideoHeader::flags`] marking a keyframe/full-frame packet.
pub const VIDEO_KEYFRAME_FLAG: u8 = 0x01;
/// Mask selecting the bit-depth field of [`VideoHeader::flags`] (bits 1-2).
///
/// The header stays ten bytes. Colour truth rides in the previously unused
/// flag bits rather than in a longer header, so every frame states its own
/// depth, range and matrix and a decoder never has to infer them from
/// handshake state that may have been renegotiated.
pub const VIDEO_BIT_DEPTH_MASK: u8 = 0x06;
/// Shift for the bit-depth field of [`VideoHeader::flags`].
pub const VIDEO_BIT_DEPTH_SHIFT: u32 = 1;
/// Bit in [`VideoHeader::flags`] marking full-range (rather than limited)
/// coded samples.
pub const VIDEO_FULL_RANGE_FLAG: u8 = 0x08;
/// Mask selecting the matrix-coefficients field of [`VideoHeader::flags`]
/// (bits 4-6).
///
/// Values 4-7 are reserved and must be rejected rather than silently
/// interpreted as BT.709.
pub const VIDEO_MATRIX_MASK: u8 = 0x70;
/// Shift for the matrix-coefficients field of [`VideoHeader::flags`].
pub const VIDEO_MATRIX_SHIFT: u32 = 4;
pub const AUDIO_HEADER_SIZE: usize = 8;
pub const MICROPHONE_HEADER_SIZE: usize = 16;
pub const MICROPHONE_PROTOCOL_VERSION: u8 = 1;
pub const MAX_MICROPHONE_OPUS_BYTES: usize = 1_275;
pub const MICROPHONE_PCM_BYTES: usize = 960 * size_of::<i16>();
pub const JPEG_HEADER_SIZE: usize = 9;
pub const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
pub const PNG_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 1024 * 1024;
pub const CLIPBOARD_HEADER_SIZE: usize = 20;
pub const HARD_MAX_CLIPBOARD_BYTES: usize = 20 * 1024 * 1024;

// HID passthrough (HoIP): client → host binary frames.
// Layout: HidDeviceAdded  = [type(1), device_id(1), vid(2le), pid(2le), desc_len(2le), desc...]
//         HidDeviceRemoved = [type(1), device_id(1)]
//         HidReport        = [type(1), device_id(1), report_bytes...]
pub const HID_DEVICE_ADDED_HEADER_SIZE: usize = 8;
pub const HID_MINIMAL_FRAME_SIZE: usize = 2; // type + device_id
                                             // SEC-raw-hid. The experimental-raw-hid path is quarantined behind an
                                             // off-by-default Cargo feature, explicit runtime opt-in, and a negotiated
                                             // `experimental_raw_hid` capability (see `messages::ClientHelloMsg` /
                                             // `ServerHelloMsg`). These bounds apply unconditionally at decode time so a
                                             // hostile or buggy peer can never hand an oversize descriptor or report to a
                                             // host's kernel-facing `/dev/uhid` parser, even if every other gate is open.
pub const MAX_HID_DESCRIPTOR_LEN: usize = 4096;
pub const MAX_HID_REPORT_LEN: usize = 4096;

// Hard USB bridge v1. These are normalized URB envelopes, not USB/IP frames.
pub const USB_URB_SUBMIT_HEADER_SIZE: usize = 33;
pub const USB_URB_CANCEL_SIZE: usize = 13;
pub const USB_URB_COMPLETE_HEADER_SIZE: usize = 20;
pub const MAX_USB_URB_TIMEOUT_MS: u32 = 1_000;

use crate::messages::ClipboardContentKind;
use arcen_usb_bridge::{
    AttachmentGeneration, EndpointAddress, SetupPacket, TransferKind, UrbId, UrbStatus,
    MAX_TRANSFER_BYTES,
};
use serde::{Deserialize, Serialize};
use std::num::{NonZeroU32, NonZeroU64};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    VideoFull = 0x01,
    VideoPartial = 0x02,
    VideoH264 = 0x03,
    VideoH265 = 0x04,
    VideoAv1 = 0x05,
    RegionVideoFull = 0x06,
    RegionVideoH264 = 0x07,
    RegionVideoH265 = 0x08,
    RegionVideoAv1 = 0x09,
    Audio = 0x10,
    AudioUpstream = 0x11,
    Clipboard = 0x20,
    HidDeviceAdded = 0x30,
    HidDeviceRemoved = 0x31,
    HidReport = 0x32,
    UsbBridgeUrbSubmit = 0x40,
    UsbBridgeUrbCancel = 0x41,
    UsbBridgeUrbComplete = 0x42,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VideoCodec {
    Jpeg = 0x00,
    H264 = 0x01,
    H265 = 0x02,
    Vp9 = 0x03,
    Av1 = 0x04,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChromaSubsampling {
    Yuv420 = 0x00,
    Yuv422 = 0x01,
    Yuv444 = 0x02,
}

/// Coded component depth carried in [`VideoHeader::flags`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum BitDepth {
    #[default]
    Eight = 0x00,
    Ten = 0x01,
    Twelve = 0x02,
}

/// Coded sample range carried in [`VideoHeader::flags`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ColorRange {
    #[default]
    Limited = 0x00,
    Full = 0x01,
}

/// Matrix coefficients carried in [`VideoHeader::flags`].
///
/// [`ColorMatrix::Identity`] means the coded planes carry G, B and R directly
/// with no conversion, so a decoder must not apply any YCbCr matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ColorMatrix {
    #[default]
    Bt709 = 0x00,
    Identity = 0x01,
    Bt601 = 0x02,
    Bt2020Ncl = 0x03,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "lowercase")]
pub enum AudioCodec {
    Opus = 0x00,
    Pcm = 0x01,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    pub frame_type: FrameType,
    pub codec: VideoCodec,
    pub chroma: ChromaSubsampling,
    pub flags: u8,
    pub timestamp_ms: u32,
    pub monitor_id: u16,
    /// Nonzero for region video frames; zero for legacy single-monitor frames.
    pub topology_generation: u64,
    /// Nonzero for region video frames; zero for legacy single-monitor frames.
    pub stream_epoch: u64,
}

impl VideoHeader {
    #[must_use]
    pub const fn encoded_len(self) -> usize {
        if self.frame_type.is_region_video() {
            REGION_VIDEO_HEADER_SIZE
        } else {
            VIDEO_HEADER_SIZE
        }
    }

    /// Returns whether this packet starts or carries a complete decoder
    /// reference frame.
    #[must_use]
    pub const fn is_keyframe(self) -> bool {
        self.flags & VIDEO_KEYFRAME_FLAG != 0
            || matches!(
                self.frame_type,
                FrameType::VideoFull | FrameType::RegionVideoFull
            )
    }

    /// Coded component depth for this packet.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::UnknownBitDepth`] for a reserved encoding, so
    /// a newer peer's deeper format is never silently decoded as eight-bit.
    pub const fn bit_depth(self) -> Result<BitDepth, ProtocolError> {
        match (self.flags & VIDEO_BIT_DEPTH_MASK) >> VIDEO_BIT_DEPTH_SHIFT {
            0x00 => Ok(BitDepth::Eight),
            0x01 => Ok(BitDepth::Ten),
            0x02 => Ok(BitDepth::Twelve),
            other => Err(ProtocolError::UnknownBitDepth(other)),
        }
    }

    /// Coded sample range for this packet.
    #[must_use]
    pub const fn color_range(self) -> ColorRange {
        if self.flags & VIDEO_FULL_RANGE_FLAG != 0 {
            ColorRange::Full
        } else {
            ColorRange::Limited
        }
    }

    /// Matrix coefficients for this packet.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::UnknownColorMatrix`] for a reserved encoding.
    pub const fn color_matrix(self) -> Result<ColorMatrix, ProtocolError> {
        match (self.flags & VIDEO_MATRIX_MASK) >> VIDEO_MATRIX_SHIFT {
            0x00 => Ok(ColorMatrix::Bt709),
            0x01 => Ok(ColorMatrix::Identity),
            0x02 => Ok(ColorMatrix::Bt601),
            0x03 => Ok(ColorMatrix::Bt2020Ncl),
            other => Err(ProtocolError::UnknownColorMatrix(other)),
        }
    }

    /// Builds the flags byte for one packet's colour truth.
    ///
    /// Centralised so encoders cannot disagree about bit positions.
    #[must_use]
    pub const fn encode_flags(
        keyframe: bool,
        bit_depth: BitDepth,
        range: ColorRange,
        matrix: ColorMatrix,
    ) -> u8 {
        let mut flags = 0u8;
        if keyframe {
            flags |= VIDEO_KEYFRAME_FLAG;
        }
        flags |= ((bit_depth as u8) << VIDEO_BIT_DEPTH_SHIFT) & VIDEO_BIT_DEPTH_MASK;
        if matches!(range, ColorRange::Full) {
            flags |= VIDEO_FULL_RANGE_FLAG;
        }
        flags |= ((matrix as u8) << VIDEO_MATRIX_SHIFT) & VIDEO_MATRIX_MASK;
        flags
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub codec: AudioCodec,
    pub timestamp_ms: u32,
}

/// Sequenced client-to-host microphone-v1 frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicrophoneHeader {
    pub codec: AudioCodec,
    pub sequence: u32,
    pub timestamp_ms: u32,
    pub generation: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardChunkHeader {
    pub kind: ClipboardContentKind,
    pub sequence: u64,
    pub total_size: u32,
    pub offset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    ShortHeader,
    UnknownFrameType(u8),
    UnknownVideoCodec(u8),
    UnknownChroma(u8),
    UnknownBitDepth(u8),
    UnknownColorMatrix(u8),
    UnknownAudioCodec(u8),
    LegacyVideoMonitorId,
    RegionVideoMonitorId,
    RegionTopologyGeneration,
    RegionStreamEpoch,
    MicrophoneReserved,
    MicrophoneVersion(u8),
    MicrophoneSequence,
    MicrophoneGeneration,
    MicrophonePayloadSize,
    UnknownClipboardKind(u8),
    ClipboardReserved,
    HidShortFrame,
    HidDescriptorTooLarge,
    HidReportTooLarge,
    UsbUrbShortFrame,
    UsbUrbGeneration,
    UsbUrbId,
    UsbUrbTransferKind(u8),
    UsbUrbStatus(u8),
    UsbUrbReserved,
    UsbUrbTimeout,
    UsbUrbPayloadSize,
    UsbUrbEndpoint,
    UsbUrbSetup,
    ClipboardSequence,
    ClipboardTotalSize,
    ClipboardPayloadSize,
    ClipboardOffset,
    AllocationFailed,
}

impl TryFrom<u8> for FrameType {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::VideoFull),
            0x02 => Ok(Self::VideoPartial),
            0x03 => Ok(Self::VideoH264),
            0x04 => Ok(Self::VideoH265),
            0x05 => Ok(Self::VideoAv1),
            0x06 => Ok(Self::RegionVideoFull),
            0x07 => Ok(Self::RegionVideoH264),
            0x08 => Ok(Self::RegionVideoH265),
            0x09 => Ok(Self::RegionVideoAv1),
            0x10 => Ok(Self::Audio),
            0x11 => Ok(Self::AudioUpstream),
            0x20 => Ok(Self::Clipboard),
            0x30 => Ok(Self::HidDeviceAdded),
            0x31 => Ok(Self::HidDeviceRemoved),
            0x32 => Ok(Self::HidReport),
            0x40 => Ok(Self::UsbBridgeUrbSubmit),
            0x41 => Ok(Self::UsbBridgeUrbCancel),
            0x42 => Ok(Self::UsbBridgeUrbComplete),
            other => Err(ProtocolError::UnknownFrameType(other)),
        }
    }
}

impl FrameType {
    #[must_use]
    pub const fn is_region_video(self) -> bool {
        matches!(
            self,
            Self::RegionVideoFull
                | Self::RegionVideoH264
                | Self::RegionVideoH265
                | Self::RegionVideoAv1
        )
    }

    #[must_use]
    pub const fn is_legacy_video(self) -> bool {
        matches!(
            self,
            Self::VideoFull
                | Self::VideoPartial
                | Self::VideoH264
                | Self::VideoH265
                | Self::VideoAv1
        )
    }
}

/// Encodes one bounded clipboard chunk.
///
/// # Errors
///
/// Rejects zero/stale-shaped metadata, oversize payloads, and checked offset
/// overflow before allocation.
pub fn encode_clipboard_chunk(
    header: ClipboardChunkHeader,
    payload: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    validate_clipboard_chunk(header, payload.len())?;
    let frame_len = CLIPBOARD_HEADER_SIZE
        .checked_add(payload.len())
        .ok_or(ProtocolError::AllocationFailed)?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(frame_len)
        .map_err(|_| ProtocolError::AllocationFailed)?;
    encoded.resize(CLIPBOARD_HEADER_SIZE, 0);
    encoded[0] = FrameType::Clipboard as u8;
    encoded[1] = match header.kind {
        ClipboardContentKind::TextUtf8 => 0,
        ClipboardContentKind::ImagePng => 1,
    };
    encoded[4..12].copy_from_slice(&header.sequence.to_be_bytes());
    encoded[12..16].copy_from_slice(&header.total_size.to_be_bytes());
    encoded[16..20].copy_from_slice(&header.offset.to_be_bytes());
    encoded.extend_from_slice(payload);
    Ok(encoded)
}

/// Decodes and validates one bounded clipboard chunk.
///
/// # Errors
///
/// Rejects non-clipboard frames, nonzero reserved bytes, invalid kinds,
/// zero/oversize metadata, empty/oversize payloads, and invalid offsets.
pub fn decode_clipboard_chunk(
    encoded: &[u8],
) -> Result<(ClipboardChunkHeader, &[u8]), ProtocolError> {
    if encoded.len() < CLIPBOARD_HEADER_SIZE {
        return Err(ProtocolError::ShortHeader);
    }
    if FrameType::try_from(encoded[0])? != FrameType::Clipboard {
        return Err(ProtocolError::UnknownFrameType(encoded[0]));
    }
    let kind = match encoded[1] {
        0 => ClipboardContentKind::TextUtf8,
        1 => ClipboardContentKind::ImagePng,
        other => return Err(ProtocolError::UnknownClipboardKind(other)),
    };
    if encoded[2] != 0 || encoded[3] != 0 {
        return Err(ProtocolError::ClipboardReserved);
    }
    let header = ClipboardChunkHeader {
        kind,
        sequence: u64::from_be_bytes(
            encoded[4..12]
                .try_into()
                .map_err(|_| ProtocolError::ShortHeader)?,
        ),
        total_size: u32::from_be_bytes(
            encoded[12..16]
                .try_into()
                .map_err(|_| ProtocolError::ShortHeader)?,
        ),
        offset: u32::from_be_bytes(
            encoded[16..20]
                .try_into()
                .map_err(|_| ProtocolError::ShortHeader)?,
        ),
    };
    let payload = &encoded[CLIPBOARD_HEADER_SIZE..];
    validate_clipboard_chunk(header, payload.len())?;
    Ok((header, payload))
}

fn validate_clipboard_chunk(
    header: ClipboardChunkHeader,
    payload_len: usize,
) -> Result<(), ProtocolError> {
    if header.sequence == 0 {
        return Err(ProtocolError::ClipboardSequence);
    }
    let total_size =
        usize::try_from(header.total_size).map_err(|_| ProtocolError::ClipboardTotalSize)?;
    if total_size == 0 || total_size > HARD_MAX_CLIPBOARD_BYTES {
        return Err(ProtocolError::ClipboardTotalSize);
    }
    if payload_len == 0 || payload_len > CHUNK_BYTES {
        return Err(ProtocolError::ClipboardPayloadSize);
    }
    let offset = usize::try_from(header.offset).map_err(|_| ProtocolError::ClipboardOffset)?;
    let end = offset
        .checked_add(payload_len)
        .ok_or(ProtocolError::ClipboardOffset)?;
    if end > total_size {
        return Err(ProtocolError::ClipboardOffset);
    }
    Ok(())
}

impl TryFrom<u8> for VideoCodec {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Jpeg),
            0x01 => Ok(Self::H264),
            0x02 => Ok(Self::H265),
            0x03 => Ok(Self::Vp9),
            0x04 => Ok(Self::Av1),
            other => Err(ProtocolError::UnknownVideoCodec(other)),
        }
    }
}

impl TryFrom<u8> for ChromaSubsampling {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Yuv420),
            0x01 => Ok(Self::Yuv422),
            0x02 => Ok(Self::Yuv444),
            other => Err(ProtocolError::UnknownChroma(other)),
        }
    }
}

impl TryFrom<u8> for AudioCodec {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Opus),
            0x01 => Ok(Self::Pcm),
            other => Err(ProtocolError::UnknownAudioCodec(other)),
        }
    }
}

pub fn encode_video_header(header: VideoHeader) -> Vec<u8> {
    let mut out = vec![0u8; header.encoded_len()];
    out[0] = header.frame_type as u8;
    out[1] = header.codec as u8;
    out[2] = header.chroma as u8;
    out[3] = header.flags;
    out[4..8].copy_from_slice(&header.timestamp_ms.to_be_bytes());
    out[8..10].copy_from_slice(&header.monitor_id.to_be_bytes());
    if header.frame_type.is_region_video() {
        out[10..18].copy_from_slice(&header.topology_generation.to_be_bytes());
        out[18..26].copy_from_slice(&header.stream_epoch.to_be_bytes());
    }
    out
}

pub fn decode_video_header(data: &[u8]) -> Result<VideoHeader, ProtocolError> {
    if data.len() < VIDEO_HEADER_SIZE {
        return Err(ProtocolError::ShortHeader);
    }
    let frame_type = FrameType::try_from(data[0])?;
    let monitor_id = u16::from_be_bytes([data[8], data[9]]);
    let (topology_generation, stream_epoch) = if frame_type.is_region_video() {
        if data.len() < REGION_VIDEO_HEADER_SIZE {
            return Err(ProtocolError::ShortHeader);
        }
        if monitor_id == 0 {
            return Err(ProtocolError::RegionVideoMonitorId);
        }
        let topology_generation = u64::from_be_bytes(
            data[10..18]
                .try_into()
                .map_err(|_| ProtocolError::ShortHeader)?,
        );
        if topology_generation == 0 {
            return Err(ProtocolError::RegionTopologyGeneration);
        }
        let stream_epoch = u64::from_be_bytes(
            data[18..26]
                .try_into()
                .map_err(|_| ProtocolError::ShortHeader)?,
        );
        if stream_epoch == 0 {
            return Err(ProtocolError::RegionStreamEpoch);
        }
        (topology_generation, stream_epoch)
    } else {
        if frame_type.is_legacy_video() && monitor_id != 0 {
            return Err(ProtocolError::LegacyVideoMonitorId);
        }
        (0, 0)
    };
    let header = VideoHeader {
        frame_type,
        codec: VideoCodec::try_from(data[1])?,
        chroma: ChromaSubsampling::try_from(data[2])?,
        flags: data[3],
        timestamp_ms: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        monitor_id,
        topology_generation,
        stream_epoch,
    };
    // Validate both wire-enum fields at the trust boundary. A caller that
    // only inspects a subset of a header must not accidentally treat a newer
    // reserved colour value as the legacy default.
    header.bit_depth()?;
    header.color_matrix()?;
    Ok(header)
}

pub fn encode_audio_header(header: AudioHeader) -> [u8; AUDIO_HEADER_SIZE] {
    let mut out = [0u8; AUDIO_HEADER_SIZE];
    out[0] = FrameType::Audio as u8;
    out[1] = header.codec as u8;
    out[4..8].copy_from_slice(&header.timestamp_ms.to_be_bytes());
    out
}

pub fn decode_audio_header(data: &[u8]) -> Result<AudioHeader, ProtocolError> {
    if data.len() < AUDIO_HEADER_SIZE {
        return Err(ProtocolError::ShortHeader);
    }
    if FrameType::try_from(data[0])? != FrameType::Audio {
        return Err(ProtocolError::UnknownFrameType(data[0]));
    }
    Ok(AudioHeader {
        codec: AudioCodec::try_from(data[1])?,
        timestamp_ms: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
    })
}

/// Encodes a validated microphone-v1 header without allocating.
///
/// # Errors
///
/// Rejects zero sequence or generation identifiers.
pub fn encode_microphone_header(
    header: MicrophoneHeader,
) -> Result<[u8; MICROPHONE_HEADER_SIZE], ProtocolError> {
    if header.sequence == 0 {
        return Err(ProtocolError::MicrophoneSequence);
    }
    if header.generation == 0 {
        return Err(ProtocolError::MicrophoneGeneration);
    }
    let mut out = [0u8; MICROPHONE_HEADER_SIZE];
    out[0] = FrameType::AudioUpstream as u8;
    out[1] = header.codec as u8;
    out[2] = MICROPHONE_PROTOCOL_VERSION;
    out[4..8].copy_from_slice(&header.sequence.to_be_bytes());
    out[8..12].copy_from_slice(&header.timestamp_ms.to_be_bytes());
    out[12..16].copy_from_slice(&header.generation.to_be_bytes());
    Ok(out)
}

/// Decodes and validates one complete microphone-v1 frame.
///
/// # Errors
///
/// Rejects short, wrong-kind, wrong-version, reserved, unbound, or malformed
/// fixed-format payloads before a host adapter consumes any audio.
pub fn decode_microphone_frame(data: &[u8]) -> Result<(MicrophoneHeader, &[u8]), ProtocolError> {
    if data.len() < MICROPHONE_HEADER_SIZE {
        return Err(ProtocolError::ShortHeader);
    }
    if FrameType::try_from(data[0])? != FrameType::AudioUpstream {
        return Err(ProtocolError::UnknownFrameType(data[0]));
    }
    if data[2] != MICROPHONE_PROTOCOL_VERSION {
        return Err(ProtocolError::MicrophoneVersion(data[2]));
    }
    if data[3] != 0 {
        return Err(ProtocolError::MicrophoneReserved);
    }
    let header = MicrophoneHeader {
        codec: AudioCodec::try_from(data[1])?,
        sequence: u32::from_be_bytes(
            data[4..8]
                .try_into()
                .map_err(|_| ProtocolError::ShortHeader)?,
        ),
        timestamp_ms: u32::from_be_bytes(
            data[8..12]
                .try_into()
                .map_err(|_| ProtocolError::ShortHeader)?,
        ),
        generation: u32::from_be_bytes(
            data[12..16]
                .try_into()
                .map_err(|_| ProtocolError::ShortHeader)?,
        ),
    };
    if header.sequence == 0 {
        return Err(ProtocolError::MicrophoneSequence);
    }
    if header.generation == 0 {
        return Err(ProtocolError::MicrophoneGeneration);
    }
    let payload = &data[MICROPHONE_HEADER_SIZE..];
    let valid = match header.codec {
        AudioCodec::Pcm => payload.len() == MICROPHONE_PCM_BYTES,
        AudioCodec::Opus => (1..=MAX_MICROPHONE_OPUS_BYTES).contains(&payload.len()),
    };
    if !valid {
        return Err(ProtocolError::MicrophonePayloadSize);
    }
    Ok((header, payload))
}

/// Header parsed from a `HidDeviceAdded` frame (excludes the descriptor bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidDeviceAddedHeader {
    pub device_id: u8,
    pub vendor_id: u16,
    pub product_id: u16,
}

/// Encode a `HidDeviceAdded` frame: 8-byte header followed by `descriptor`.
pub fn encode_hid_device_added(header: HidDeviceAddedHeader, descriptor: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HID_DEVICE_ADDED_HEADER_SIZE + descriptor.len());
    out.push(FrameType::HidDeviceAdded as u8);
    out.push(header.device_id);
    out.extend_from_slice(&header.vendor_id.to_le_bytes());
    out.extend_from_slice(&header.product_id.to_le_bytes());
    out.extend_from_slice(&(descriptor.len() as u16).to_le_bytes());
    out.extend_from_slice(descriptor);
    out
}

/// Decode a `HidDeviceAdded` frame.  Returns the header and descriptor slice.
///
/// # Errors
///
/// Rejects short frames and, before any byte reaches a kernel-facing HID
/// parser, rejects a claimed descriptor length above `MAX_HID_DESCRIPTOR_LEN`.
pub fn decode_hid_device_added(
    data: &[u8],
) -> Result<(HidDeviceAddedHeader, &[u8]), ProtocolError> {
    if data.len() < HID_DEVICE_ADDED_HEADER_SIZE {
        return Err(ProtocolError::HidShortFrame);
    }
    let device_id = data[1];
    let vendor_id = u16::from_le_bytes([data[2], data[3]]);
    let product_id = u16::from_le_bytes([data[4], data[5]]);
    let desc_len = u16::from_le_bytes([data[6], data[7]]) as usize;
    if desc_len > MAX_HID_DESCRIPTOR_LEN {
        return Err(ProtocolError::HidDescriptorTooLarge);
    }
    let end = HID_DEVICE_ADDED_HEADER_SIZE + desc_len;
    if data.len() < end {
        return Err(ProtocolError::HidShortFrame);
    }
    let header = HidDeviceAddedHeader {
        device_id,
        vendor_id,
        product_id,
    };
    Ok((header, &data[HID_DEVICE_ADDED_HEADER_SIZE..end]))
}

/// Encode a `HidDeviceRemoved` frame (2 bytes: type + device_id).
pub fn encode_hid_device_removed(device_id: u8) -> [u8; HID_MINIMAL_FRAME_SIZE] {
    [FrameType::HidDeviceRemoved as u8, device_id]
}

/// Encode a `HidReport` frame: 2-byte header followed by the raw HID report bytes.
pub fn encode_hid_report(device_id: u8, report: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HID_MINIMAL_FRAME_SIZE + report.len());
    out.push(FrameType::HidReport as u8);
    out.push(device_id);
    out.extend_from_slice(report);
    out
}

/// Decode a `HidReport` frame.  Returns (device_id, report_bytes).
///
/// # Errors
///
/// Rejects short frames and, before any byte reaches a kernel-facing HID
/// parser, rejects a report payload above `MAX_HID_REPORT_LEN`.
pub fn decode_hid_report(data: &[u8]) -> Result<(u8, &[u8]), ProtocolError> {
    if data.len() < HID_MINIMAL_FRAME_SIZE {
        return Err(ProtocolError::HidShortFrame);
    }
    let payload = &data[HID_MINIMAL_FRAME_SIZE..];
    if payload.len() > MAX_HID_REPORT_LEN {
        return Err(ProtocolError::HidReportTooLarge);
    }
    Ok((data[1], payload))
}

/// Decode a `HidDeviceRemoved` frame.  Returns device_id.
pub fn decode_hid_device_removed(data: &[u8]) -> Result<u8, ProtocolError> {
    if data.len() < HID_MINIMAL_FRAME_SIZE {
        return Err(ProtocolError::HidShortFrame);
    }
    Ok(data[1])
}

/// Metadata carried before one URB submit payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbUrbSubmitHeader {
    pub generation: AttachmentGeneration,
    pub urb_id: UrbId,
    pub endpoint: EndpointAddress,
    pub transfer_kind: TransferKind,
    pub timeout_ms: u32,
    pub declared_length: u32,
    pub setup: Option<SetupPacket>,
}

/// Metadata carried before one URB completion payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbUrbCompletionHeader {
    pub generation: AttachmentGeneration,
    pub urb_id: UrbId,
    pub status: UrbStatus,
    pub actual_length: u32,
}

/// Encodes one bounded normalized URB request.
///
/// # Errors
///
/// Returns [`ProtocolError`] when the header/payload violates v1 endpoint,
/// setup, timeout, or exact-length invariants.
pub fn encode_usb_urb_submit(
    header: UsbUrbSubmitHeader,
    payload: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    validate_usb_submit(header, payload)?;
    let capacity = USB_URB_SUBMIT_HEADER_SIZE
        .checked_add(payload.len())
        .ok_or(ProtocolError::AllocationFailed)?;
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)
        .map_err(|_| ProtocolError::AllocationFailed)?;
    out.push(FrameType::UsbBridgeUrbSubmit as u8);
    out.extend_from_slice(&header.generation.get().to_le_bytes());
    out.extend_from_slice(&header.urb_id.get().to_le_bytes());
    out.push(header.endpoint.0);
    out.push(transfer_kind_byte(header.transfer_kind));
    out.push(u8::from(header.setup.is_some()));
    out.push(0);
    out.extend_from_slice(&header.timeout_ms.to_le_bytes());
    out.extend_from_slice(&header.declared_length.to_le_bytes());
    let setup = header.setup.unwrap_or(SetupPacket {
        request_type: 0,
        request: 0,
        value: 0,
        index: 0,
        length: 0,
    });
    encode_setup(&mut out, setup);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Decodes one bounded normalized URB request.
///
/// # Errors
///
/// Returns [`ProtocolError`] before allocation or adapter dispatch when any
/// field or payload violates the v1 contract.
pub fn decode_usb_urb_submit(data: &[u8]) -> Result<(UsbUrbSubmitHeader, &[u8]), ProtocolError> {
    if data.len() < USB_URB_SUBMIT_HEADER_SIZE {
        return Err(ProtocolError::UsbUrbShortFrame);
    }
    let generation = nonzero_generation(&data[1..9])?;
    let urb_id = nonzero_urb_id(&data[9..13])?;
    let endpoint = EndpointAddress(data[13]);
    let transfer_kind = transfer_kind_from_byte(data[14])?;
    let setup_present = match data[15] {
        0 => false,
        1 => true,
        _ => return Err(ProtocolError::UsbUrbSetup),
    };
    if data[16] != 0 {
        return Err(ProtocolError::UsbUrbReserved);
    }
    let timeout_ms = u32::from_le_bytes(
        data[17..21]
            .try_into()
            .map_err(|_| ProtocolError::UsbUrbShortFrame)?,
    );
    let declared_length = u32::from_le_bytes(
        data[21..25]
            .try_into()
            .map_err(|_| ProtocolError::UsbUrbShortFrame)?,
    );
    let setup = setup_present.then(|| decode_setup(&data[25..33]));
    let header = UsbUrbSubmitHeader {
        generation,
        urb_id,
        endpoint,
        transfer_kind,
        timeout_ms,
        declared_length,
        setup,
    };
    let payload = &data[USB_URB_SUBMIT_HEADER_SIZE..];
    validate_usb_submit(header, payload)?;
    Ok((header, payload))
}

/// Encodes one URB cancellation.
#[must_use]
pub fn encode_usb_urb_cancel(
    generation: AttachmentGeneration,
    urb_id: UrbId,
) -> [u8; USB_URB_CANCEL_SIZE] {
    let mut out = [0_u8; USB_URB_CANCEL_SIZE];
    out[0] = FrameType::UsbBridgeUrbCancel as u8;
    out[1..9].copy_from_slice(&generation.get().to_le_bytes());
    out[9..13].copy_from_slice(&urb_id.get().to_le_bytes());
    out
}

/// Decodes one URB cancellation.
///
/// # Errors
///
/// Returns [`ProtocolError`] unless the frame is exact-length with nonzero
/// generation and request identifiers.
pub fn decode_usb_urb_cancel(data: &[u8]) -> Result<(AttachmentGeneration, UrbId), ProtocolError> {
    if data.len() != USB_URB_CANCEL_SIZE {
        return Err(ProtocolError::UsbUrbShortFrame);
    }
    Ok((
        nonzero_generation(&data[1..9])?,
        nonzero_urb_id(&data[9..13])?,
    ))
}

/// Encodes one bounded URB completion.
///
/// # Errors
///
/// Returns [`ProtocolError`] when the actual length and payload disagree or
/// exceed the bridge limit.
pub fn encode_usb_urb_complete(
    header: UsbUrbCompletionHeader,
    payload: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    validate_usb_completion(header, payload)?;
    let capacity = USB_URB_COMPLETE_HEADER_SIZE
        .checked_add(payload.len())
        .ok_or(ProtocolError::AllocationFailed)?;
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)
        .map_err(|_| ProtocolError::AllocationFailed)?;
    out.push(FrameType::UsbBridgeUrbComplete as u8);
    out.extend_from_slice(&header.generation.get().to_le_bytes());
    out.extend_from_slice(&header.urb_id.get().to_le_bytes());
    out.push(urb_status_byte(header.status));
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&header.actual_length.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Decodes one bounded URB completion.
///
/// # Errors
///
/// Returns [`ProtocolError`] before adapter dispatch when the frame is
/// malformed, reserved bits are nonzero, or payload length is inconsistent.
pub fn decode_usb_urb_complete(
    data: &[u8],
) -> Result<(UsbUrbCompletionHeader, &[u8]), ProtocolError> {
    if data.len() < USB_URB_COMPLETE_HEADER_SIZE {
        return Err(ProtocolError::UsbUrbShortFrame);
    }
    if data[14] != 0 || data[15] != 0 {
        return Err(ProtocolError::UsbUrbReserved);
    }
    let header = UsbUrbCompletionHeader {
        generation: nonzero_generation(&data[1..9])?,
        urb_id: nonzero_urb_id(&data[9..13])?,
        status: urb_status_from_byte(data[13])?,
        actual_length: u32::from_le_bytes(
            data[16..20]
                .try_into()
                .map_err(|_| ProtocolError::UsbUrbShortFrame)?,
        ),
    };
    let payload = &data[USB_URB_COMPLETE_HEADER_SIZE..];
    validate_usb_completion(header, payload)?;
    Ok((header, payload))
}

fn validate_usb_submit(header: UsbUrbSubmitHeader, payload: &[u8]) -> Result<(), ProtocolError> {
    if header.timeout_ms == 0 || header.timeout_ms > MAX_USB_URB_TIMEOUT_MS {
        return Err(ProtocolError::UsbUrbTimeout);
    }
    let declared =
        usize::try_from(header.declared_length).map_err(|_| ProtocolError::UsbUrbPayloadSize)?;
    if declared > MAX_TRANSFER_BYTES || payload.len() > MAX_TRANSFER_BYTES {
        return Err(ProtocolError::UsbUrbPayloadSize);
    }
    match header.transfer_kind {
        TransferKind::Control => {
            if header.endpoint.number() != 0 || header.setup.is_none() {
                return Err(ProtocolError::UsbUrbSetup);
            }
        }
        TransferKind::Interrupt => {
            if header.endpoint.number() == 0 || header.setup.is_some() {
                return Err(ProtocolError::UsbUrbEndpoint);
            }
        }
    }
    let expected_payload = match header.endpoint.direction() {
        arcen_usb_bridge::TransferDirection::In => 0,
        arcen_usb_bridge::TransferDirection::Out => declared,
    };
    if payload.len() != expected_payload {
        return Err(ProtocolError::UsbUrbPayloadSize);
    }
    Ok(())
}

fn validate_usb_completion(
    header: UsbUrbCompletionHeader,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    let actual =
        usize::try_from(header.actual_length).map_err(|_| ProtocolError::UsbUrbPayloadSize)?;
    if actual > MAX_TRANSFER_BYTES || payload.len() != actual {
        return Err(ProtocolError::UsbUrbPayloadSize);
    }
    if header.status != UrbStatus::Success && !payload.is_empty() {
        return Err(ProtocolError::UsbUrbPayloadSize);
    }
    Ok(())
}

fn nonzero_generation(bytes: &[u8]) -> Result<AttachmentGeneration, ProtocolError> {
    let value = u64::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| ProtocolError::UsbUrbShortFrame)?,
    );
    NonZeroU64::new(value)
        .map(AttachmentGeneration::new)
        .ok_or(ProtocolError::UsbUrbGeneration)
}

fn nonzero_urb_id(bytes: &[u8]) -> Result<UrbId, ProtocolError> {
    let value = u32::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| ProtocolError::UsbUrbShortFrame)?,
    );
    NonZeroU32::new(value)
        .map(UrbId::new)
        .ok_or(ProtocolError::UsbUrbId)
}

fn encode_setup(out: &mut Vec<u8>, setup: SetupPacket) {
    out.push(setup.request_type);
    out.push(setup.request);
    out.extend_from_slice(&setup.value.to_le_bytes());
    out.extend_from_slice(&setup.index.to_le_bytes());
    out.extend_from_slice(&setup.length.to_le_bytes());
}

fn decode_setup(bytes: &[u8]) -> SetupPacket {
    SetupPacket {
        request_type: bytes[0],
        request: bytes[1],
        value: u16::from_le_bytes([bytes[2], bytes[3]]),
        index: u16::from_le_bytes([bytes[4], bytes[5]]),
        length: u16::from_le_bytes([bytes[6], bytes[7]]),
    }
}

const fn transfer_kind_byte(kind: TransferKind) -> u8 {
    match kind {
        TransferKind::Control => 0,
        TransferKind::Interrupt => 1,
    }
}

fn transfer_kind_from_byte(value: u8) -> Result<TransferKind, ProtocolError> {
    match value {
        0 => Ok(TransferKind::Control),
        1 => Ok(TransferKind::Interrupt),
        other => Err(ProtocolError::UsbUrbTransferKind(other)),
    }
}

const fn urb_status_byte(status: UrbStatus) -> u8 {
    match status {
        UrbStatus::Success => 0,
        UrbStatus::Cancelled => 1,
        UrbStatus::TimedOut => 2,
        UrbStatus::Stall => 3,
        UrbStatus::Disconnected => 4,
        UrbStatus::Protocol => 5,
        UrbStatus::Io => 6,
    }
}

fn urb_status_from_byte(value: u8) -> Result<UrbStatus, ProtocolError> {
    match value {
        0 => Ok(UrbStatus::Success),
        1 => Ok(UrbStatus::Cancelled),
        2 => Ok(UrbStatus::TimedOut),
        3 => Ok(UrbStatus::Stall),
        4 => Ok(UrbStatus::Disconnected),
        5 => Ok(UrbStatus::Protocol),
        6 => Ok(UrbStatus::Io),
        other => Err(ProtocolError::UsbUrbStatus(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_header_roundtrips() {
        let header = VideoHeader {
            frame_type: FrameType::VideoH264,
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv444,
            flags: VIDEO_KEYFRAME_FLAG,
            timestamp_ms: 0x0102_0304,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
        };
        let encoded = encode_video_header(header);
        assert_eq!(encoded.len(), VIDEO_HEADER_SIZE);
        assert_eq!(decode_video_header(&encoded).unwrap(), header);
    }

    #[test]
    fn region_video_header_has_exact_v1_bytes_and_roundtrips() {
        let header = VideoHeader {
            frame_type: FrameType::RegionVideoH265,
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv420,
            flags: VIDEO_KEYFRAME_FLAG,
            timestamp_ms: 0x0102_0304,
            monitor_id: 2,
            topology_generation: 0x0102_0304_0506_0708,
            stream_epoch: 0x1112_1314_1516_1718,
        };
        let encoded = encode_video_header(header);
        assert_eq!(
            encoded,
            vec![
                0x08, 0x02, 0x00, 0x01, 0x01, 0x02, 0x03, 0x04, 0x00, 0x02, 0x01, 0x02, 0x03, 0x04,
                0x05, 0x06, 0x07, 0x08, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
            ]
        );
        assert_eq!(encoded.len(), REGION_VIDEO_HEADER_SIZE);
        assert_eq!(decode_video_header(&encoded).unwrap(), header);
    }

    #[test]
    fn region_video_rejects_legacy_and_zero_epoch_shapes() {
        let mut legacy_region = vec![0; VIDEO_HEADER_SIZE];
        legacy_region[0] = FrameType::VideoH264 as u8;
        legacy_region[1] = VideoCodec::H264 as u8;
        legacy_region[8..10].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(
            decode_video_header(&legacy_region),
            Err(ProtocolError::LegacyVideoMonitorId)
        );

        let mut zero_epoch = encode_video_header(VideoHeader {
            frame_type: FrameType::RegionVideoH264,
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            flags: 0,
            timestamp_ms: 1,
            monitor_id: 2,
            topology_generation: 3,
            stream_epoch: 4,
        });
        zero_epoch[18..26].fill(0);
        assert_eq!(
            decode_video_header(&zero_epoch),
            Err(ProtocolError::RegionStreamEpoch)
        );
    }

    #[test]
    fn audio_header_roundtrips() {
        let header = AudioHeader {
            codec: AudioCodec::Opus,
            timestamp_ms: 0x0a0b_0c0d,
        };
        let encoded = encode_audio_header(header);
        assert_eq!(encoded, [0x10, 0x00, 0x00, 0x00, 0x0a, 0x0b, 0x0c, 0x0d]);
        assert_eq!(encoded.len(), AUDIO_HEADER_SIZE);
        assert_eq!(decode_audio_header(&encoded).unwrap(), header);

        let pcm = encode_audio_header(AudioHeader {
            codec: AudioCodec::Pcm,
            timestamp_ms: 0x0102_0304,
        });
        assert_eq!(pcm, [0x10, 0x01, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn microphone_v1_is_distinct_sequenced_and_bounded() {
        let header = MicrophoneHeader {
            codec: AudioCodec::Opus,
            sequence: u32::MAX,
            timestamp_ms: 0x0102_0304,
            generation: 7,
        };
        let encoded = encode_microphone_header(header).unwrap();
        assert_eq!(
            encoded,
            [
                0x11, 0x00, 0x01, 0x00, 0xff, 0xff, 0xff, 0xff, 0x01, 0x02, 0x03, 0x04, 0x00, 0x00,
                0x00, 0x07
            ]
        );
        let mut frame = encoded.to_vec();
        frame.push(0xaa);
        assert_eq!(decode_microphone_frame(&frame), Ok((header, &[0xaa][..])));

        let pcm_header = encode_microphone_header(MicrophoneHeader {
            codec: AudioCodec::Pcm,
            ..header
        })
        .unwrap();
        let mut pcm = pcm_header.to_vec();
        pcm.resize(MICROPHONE_HEADER_SIZE + MICROPHONE_PCM_BYTES, 0);
        assert_eq!(
            decode_microphone_frame(&pcm).unwrap().1.len(),
            MICROPHONE_PCM_BYTES
        );
    }

    #[test]
    fn microphone_v1_rejects_malformed_and_oversized_frames() {
        let header = MicrophoneHeader {
            codec: AudioCodec::Opus,
            sequence: 1,
            timestamp_ms: 0,
            generation: 1,
        };
        assert_eq!(
            encode_microphone_header(MicrophoneHeader {
                sequence: 0,
                ..header
            }),
            Err(ProtocolError::MicrophoneSequence)
        );
        let mut frame = encode_microphone_header(header).unwrap().to_vec();
        assert_eq!(
            decode_microphone_frame(&frame),
            Err(ProtocolError::MicrophonePayloadSize)
        );
        frame.resize(MICROPHONE_HEADER_SIZE + MAX_MICROPHONE_OPUS_BYTES + 1, 0);
        assert_eq!(
            decode_microphone_frame(&frame),
            Err(ProtocolError::MicrophonePayloadSize)
        );
        frame.truncate(MICROPHONE_HEADER_SIZE + 1);
        frame[2] = 2;
        assert_eq!(
            decode_microphone_frame(&frame),
            Err(ProtocolError::MicrophoneVersion(2))
        );
        frame[2] = MICROPHONE_PROTOCOL_VERSION;
        frame[3] = 1;
        assert_eq!(
            decode_microphone_frame(&frame),
            Err(ProtocolError::MicrophoneReserved)
        );
    }

    #[test]
    fn hid_frames_round_trip() {
        let header = HidDeviceAddedHeader {
            device_id: 3,
            vendor_id: 0x056A,
            product_id: 0x0396,
        };
        let desc = [0x05, 0x0D, 0x09, 0x04, 0xA1, 0x01];
        let encoded = encode_hid_device_added(header, &desc);
        assert_eq!(encoded[0], FrameType::HidDeviceAdded as u8);
        assert_eq!(encoded.len(), HID_DEVICE_ADDED_HEADER_SIZE + desc.len());
        let (decoded_hdr, decoded_desc) = decode_hid_device_added(&encoded).unwrap();
        assert_eq!(decoded_hdr, header);
        assert_eq!(decoded_desc, &desc);

        let removed = encode_hid_device_removed(3);
        assert_eq!(removed, [FrameType::HidDeviceRemoved as u8, 3]);
        assert_eq!(decode_hid_device_removed(&removed).unwrap(), 3);

        let report = [0x01, 0x80, 0x00, 0x40, 0x00];
        let report_frame = encode_hid_report(3, &report);
        assert_eq!(report_frame[0], FrameType::HidReport as u8);
        let (dev_id, data) = decode_hid_report(&report_frame).unwrap();
        assert_eq!(dev_id, 3);
        assert_eq!(data, &report);

        assert_eq!(
            decode_hid_device_added(&[FrameType::HidDeviceAdded as u8]).unwrap_err(),
            ProtocolError::HidShortFrame
        );
    }

    /// A hostile or buggy peer must never be able to claim a descriptor
    /// length beyond `MAX_HID_DESCRIPTOR_LEN`, regardless of how many actual
    /// bytes follow — this is the bound enforced before any kernel-facing
    /// HID parser (`/dev/uhid`) ever sees the data.
    #[test]
    fn rejects_oversize_hid_descriptor_claim() {
        let mut frame = vec![FrameType::HidDeviceAdded as u8, 1, 0x6A, 0x05, 0x17, 0x03];
        let oversize_len = (MAX_HID_DESCRIPTOR_LEN + 1) as u16;
        frame.extend_from_slice(&oversize_len.to_le_bytes());
        // No descriptor payload bytes are appended: the length claim alone
        // must be rejected before any short-frame check on trailing bytes.
        assert_eq!(
            decode_hid_device_added(&frame),
            Err(ProtocolError::HidDescriptorTooLarge)
        );
    }

    /// A hostile or buggy peer must never be able to smuggle an oversize HID
    /// input report past the wire boundary.
    #[test]
    fn rejects_oversize_hid_report() {
        let mut frame = vec![FrameType::HidReport as u8, 1];
        frame.resize(HID_MINIMAL_FRAME_SIZE + MAX_HID_REPORT_LEN + 1, 0xAA);
        assert_eq!(
            decode_hid_report(&frame),
            Err(ProtocolError::HidReportTooLarge)
        );

        // Exactly at the bound must still succeed.
        let mut at_bound = vec![FrameType::HidReport as u8, 1];
        at_bound.resize(HID_MINIMAL_FRAME_SIZE + MAX_HID_REPORT_LEN, 0xAA);
        assert!(decode_hid_report(&at_bound).is_ok());
    }

    #[test]
    fn usb_urb_frames_round_trip() {
        let generation = AttachmentGeneration::new(NonZeroU64::new(7).unwrap());
        let urb_id = UrbId::new(NonZeroU32::new(9).unwrap());
        let submit = UsbUrbSubmitHeader {
            generation,
            urb_id,
            endpoint: EndpointAddress(0x00),
            transfer_kind: TransferKind::Control,
            timeout_ms: 500,
            declared_length: 2,
            setup: Some(SetupPacket {
                request_type: 0x00,
                request: 0x09,
                value: 1,
                index: 0,
                length: 0,
            }),
        };
        let encoded = encode_usb_urb_submit(submit, &[0xaa, 0xbb]).unwrap();
        assert_eq!(encoded[0], FrameType::UsbBridgeUrbSubmit as u8);
        assert_eq!(encoded.len(), USB_URB_SUBMIT_HEADER_SIZE + 2);
        assert_eq!(
            decode_usb_urb_submit(&encoded),
            Ok((submit, &[0xaa, 0xbb][..]))
        );

        let cancel = encode_usb_urb_cancel(generation, urb_id);
        assert_eq!(decode_usb_urb_cancel(&cancel), Ok((generation, urb_id)));

        let completion = UsbUrbCompletionHeader {
            generation,
            urb_id,
            status: UrbStatus::Success,
            actual_length: 3,
        };
        let encoded = encode_usb_urb_complete(completion, &[1, 2, 3]).unwrap();
        assert_eq!(encoded[0], FrameType::UsbBridgeUrbComplete as u8);
        assert_eq!(
            decode_usb_urb_complete(&encoded),
            Ok((completion, &[1, 2, 3][..]))
        );
    }

    #[test]
    fn usb_urb_frames_reject_invalid_shapes_before_dispatch() {
        let generation = AttachmentGeneration::new(NonZeroU64::MIN);
        let urb_id = UrbId::new(NonZeroU32::MIN);
        let interrupt_in = UsbUrbSubmitHeader {
            generation,
            urb_id,
            endpoint: EndpointAddress(0x81),
            transfer_kind: TransferKind::Interrupt,
            timeout_ms: MAX_USB_URB_TIMEOUT_MS,
            declared_length: 10,
            setup: None,
        };
        assert!(encode_usb_urb_submit(interrupt_in, &[]).is_ok());
        assert_eq!(
            encode_usb_urb_submit(
                UsbUrbSubmitHeader {
                    timeout_ms: 0,
                    ..interrupt_in
                },
                &[]
            ),
            Err(ProtocolError::UsbUrbTimeout)
        );
        assert_eq!(
            encode_usb_urb_submit(interrupt_in, &[1]),
            Err(ProtocolError::UsbUrbPayloadSize)
        );
        assert_eq!(
            encode_usb_urb_complete(
                UsbUrbCompletionHeader {
                    generation,
                    urb_id,
                    status: UrbStatus::Cancelled,
                    actual_length: 1,
                },
                &[1],
            ),
            Err(ProtocolError::UsbUrbPayloadSize)
        );

        let mut zero_generation = encode_usb_urb_cancel(generation, urb_id);
        zero_generation[1..9].fill(0);
        assert_eq!(
            decode_usb_urb_cancel(&zero_generation),
            Err(ProtocolError::UsbUrbGeneration)
        );
    }

    #[test]
    fn rejects_short_video_header() {
        assert_eq!(
            decode_video_header(&[FrameType::VideoH264 as u8]).unwrap_err(),
            ProtocolError::ShortHeader
        );
    }

    #[test]
    fn constants_match_python_wire_contract() {
        assert_eq!(VIDEO_HEADER_SIZE, 10);
        assert_eq!(REGION_VIDEO_HEADER_SIZE, 26);
        assert_eq!(AUDIO_HEADER_SIZE, 8);
        assert_eq!(JPEG_HEADER_SIZE, 9);
        assert_eq!(PNG_MAX_BYTES, 64 * 1024 * 1024);
        assert_eq!(CHUNK_BYTES, 1024 * 1024);
        assert_eq!(PNG_MAGIC, b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn video_flags_carry_colour_truth_without_growing_the_header() {
        // The whole point of using flag bits: colour rides on every frame and
        // the header stays ten bytes.
        let flags =
            VideoHeader::encode_flags(true, BitDepth::Ten, ColorRange::Full, ColorMatrix::Identity);
        let header = VideoHeader {
            frame_type: FrameType::VideoH265,
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            flags,
            timestamp_ms: 7,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
        };
        let encoded = encode_video_header(header);
        assert_eq!(encoded.len(), VIDEO_HEADER_SIZE);
        let decoded = decode_video_header(&encoded).expect("round trip");
        assert!(decoded.is_keyframe());
        assert_eq!(decoded.bit_depth(), Ok(BitDepth::Ten));
        assert_eq!(decoded.color_range(), ColorRange::Full);
        assert_eq!(decoded.color_matrix(), Ok(ColorMatrix::Identity));
    }

    #[test]
    fn zero_flags_decode_as_the_legacy_eight_bit_limited_contract() {
        // A frame with no colour bits set must read as 8-bit limited BT.709,
        // which is exactly what every pre-colour encoder produced.
        let header = VideoHeader {
            frame_type: FrameType::VideoH265,
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            flags: 0,
            timestamp_ms: 0,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
        };
        assert_eq!(header.bit_depth(), Ok(BitDepth::Eight));
        assert_eq!(header.color_range(), ColorRange::Limited);
        assert_eq!(header.color_matrix(), Ok(ColorMatrix::Bt709));
        assert!(!header.is_keyframe());
    }

    #[test]
    fn colour_fields_are_independent_and_do_not_alias() {
        // Each field must survive the presence of the others; an overlapping
        // mask would make depth and matrix corrupt one another.
        for depth in [BitDepth::Eight, BitDepth::Ten, BitDepth::Twelve] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                for matrix in [
                    ColorMatrix::Bt709,
                    ColorMatrix::Identity,
                    ColorMatrix::Bt601,
                    ColorMatrix::Bt2020Ncl,
                ] {
                    for keyframe in [false, true] {
                        let flags = VideoHeader::encode_flags(keyframe, depth, range, matrix);
                        let header = VideoHeader {
                            frame_type: FrameType::VideoH265,
                            codec: VideoCodec::H265,
                            chroma: ChromaSubsampling::Yuv444,
                            flags,
                            timestamp_ms: 0,
                            monitor_id: 0,
                            topology_generation: 0,
                            stream_epoch: 0,
                        };
                        assert_eq!(header.bit_depth(), Ok(depth));
                        assert_eq!(header.color_range(), range);
                        assert_eq!(header.color_matrix(), Ok(matrix));
                        assert_eq!(header.flags & VIDEO_KEYFRAME_FLAG != 0, keyframe);
                    }
                }
            }
        }
    }

    #[test]
    fn reserved_bit_depth_is_rejected_rather_than_read_as_eight_bit() {
        // Bits 1-2 == 0b11 is reserved. A newer peer using it must not have
        // its frames silently decoded as eight-bit.
        let header = VideoHeader {
            frame_type: FrameType::VideoH265,
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            flags: VIDEO_BIT_DEPTH_MASK,
            timestamp_ms: 0,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
        };
        assert_eq!(
            header.bit_depth(),
            Err(ProtocolError::UnknownBitDepth(0x03))
        );
        assert_eq!(
            decode_video_header(&encode_video_header(header)),
            Err(ProtocolError::UnknownBitDepth(0x03))
        );
    }

    #[test]
    fn reserved_matrix_is_rejected_at_header_decode() {
        // Bits 4-6 == 0b100 is reserved. It must not fall back to BT.709.
        let header = VideoHeader {
            frame_type: FrameType::VideoH265,
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            flags: 0x40,
            timestamp_ms: 0,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
        };
        assert_eq!(
            header.color_matrix(),
            Err(ProtocolError::UnknownColorMatrix(0x04))
        );
        assert_eq!(
            decode_video_header(&encode_video_header(header)),
            Err(ProtocolError::UnknownColorMatrix(0x04))
        );
    }

    #[test]
    fn clipboard_header_matches_golden_bytes() {
        let header = ClipboardChunkHeader {
            kind: ClipboardContentKind::ImagePng,
            sequence: 0x0102_0304_0506_0708,
            total_size: 0x000f_ffff,
            offset: 0x0008_0000,
        };
        let encoded = encode_clipboard_chunk(header, &[0xaa, 0xbb]).unwrap();
        assert_eq!(
            &encoded[..CLIPBOARD_HEADER_SIZE],
            &[
                0x20, 0x01, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x00, 0x0f,
                0xff, 0xff, 0x00, 0x08, 0x00, 0x00
            ]
        );
        assert_eq!(
            decode_clipboard_chunk(&encoded).unwrap(),
            (header, &[0xaa, 0xbb][..])
        );
    }

    #[test]
    fn clipboard_header_rejects_reserved_and_bounds() {
        let header = ClipboardChunkHeader {
            kind: ClipboardContentKind::TextUtf8,
            sequence: 1,
            total_size: 2,
            offset: 0,
        };
        let mut encoded = encode_clipboard_chunk(header, &[1]).unwrap();
        encoded[2] = 1;
        assert_eq!(
            decode_clipboard_chunk(&encoded),
            Err(ProtocolError::ClipboardReserved)
        );
        assert_eq!(
            encode_clipboard_chunk(
                ClipboardChunkHeader {
                    sequence: 0,
                    ..header
                },
                &[1]
            ),
            Err(ProtocolError::ClipboardSequence)
        );
        assert_eq!(
            encode_clipboard_chunk(
                ClipboardChunkHeader {
                    total_size: 1,
                    offset: 1,
                    ..header
                },
                &[1]
            ),
            Err(ProtocolError::ClipboardOffset)
        );
        assert_eq!(
            encode_clipboard_chunk(header, &[]),
            Err(ProtocolError::ClipboardPayloadSize)
        );
        assert_eq!(
            decode_clipboard_chunk(&[FrameType::Clipboard as u8]),
            Err(ProtocolError::ShortHeader)
        );
        let mut unknown_kind = encode_clipboard_chunk(header, &[1]).unwrap();
        unknown_kind[1] = 0xff;
        assert_eq!(
            decode_clipboard_chunk(&unknown_kind),
            Err(ProtocolError::UnknownClipboardKind(0xff))
        );
        assert_eq!(
            encode_clipboard_chunk(
                ClipboardChunkHeader {
                    total_size: (HARD_MAX_CLIPBOARD_BYTES + 1) as u32,
                    ..header
                },
                &[1]
            ),
            Err(ProtocolError::ClipboardTotalSize)
        );
        assert_eq!(
            encode_clipboard_chunk(header, &vec![0; CHUNK_BYTES + 1]),
            Err(ProtocolError::ClipboardPayloadSize)
        );
    }
}
