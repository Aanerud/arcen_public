// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The decoder seam: what a Deck's hardware video decoder must provide.
//!
//! The media worker (feature `runtime`) owns one decoder per presented
//! monitor, creates each on its own thread through a
//! [`VideoDecoderFactory`], and talks to it only through
//! [`VideoDecoderBackend`]. Implementations:
//!
//! - macOS: `VideoToolbox`, native picture `CVPixelBuffer`
//!   (`clients/macos/src/pipeline/video_decoder.rs`).
//! - Windows: our own `ID3D11VideoDecoder` (DXVA) decoder, native picture
//!   a D3D11 texture on a decode device of its own, shared with the
//!   presenters' devices where [`decode_device_split`] allows it, otherwise
//!   a texture slice on the presenter's device
//!   (`clients/windows/src/decode/`, ADR 0024). The portable
//!   bitstream parsing, DPB and reference lists live in `arcen-media`.
//!
//! # Contract rules every backend keeps
//!
//! - **Low latency, no reorder.** Arcen Piers emit no B-frames; one access
//!   unit in yields at most one picture out, in decode order, as soon as it
//!   is decoded. A backend never holds a picture back for reordering.
//! - **Fresh decoder on reconnect.** A reconnect builds a new decoder via
//!   the factory and waits for a keyframe; queued frames from the old
//!   connection are discarded, never decoded late (`clients/macos/AGENTS.md`).
//! - **Truthful capabilities.** [`DecodeCapabilities`] is what the backend
//!   *proved* on this machine (on Windows: on the adapter that owns the
//!   window's monitor), never an assumption: the Pier honours the claim.
//! - **Visible refusal.** A stream the backend cannot decode (for example
//!   HEVC `RExt` 4:4:4 10-bit on an adapter without `HEVC_VLD_MAIN10_444`)
//!   fails with [`VideoDecodeError::Unsupported`] and a reason; it is never
//!   silently decoded at a lower fidelity.

use std::fmt;

use arcen_media::video::DecodeLatencyPolicy;
use arcen_media::{ColorPrimaries, TransferCharacteristics, VideoConfiguration};
use arcen_protocol::{ProtocolError, VideoCodec, VideoHeader};

/// Which decoder family produced the capabilities and the frames. Only used
/// to label the hello's `decoder_backend` and the stream statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecoderBackendFamily {
    /// Nothing has been probed yet.
    #[default]
    Unprobed,
    /// macOS `VideoToolbox`.
    VideoToolbox,
    /// Windows: Arcen's own `ID3D11VideoDecoder` (DXVA) decoder.
    D3d11Video,
}

/// What this Deck proved it can decode, sent in the client hello.
///
/// Field for field the macOS Deck's probe result, plus the backend family,
/// so the hello is built by one shared function for every Deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate hello capability the Pier reads"
)]
pub struct DecodeCapabilities {
    /// The backend that answered.
    pub backend: DecoderBackendFamily,
    /// H.264 (High, 4:2:0, 8-bit) decodes.
    pub h264: bool,
    /// HEVC Main decodes.
    pub h265: bool,
    /// AV1 Main decodes. Capped by the platform profile
    /// (`arcen_media::deck_platform`).
    pub av1: bool,
    /// HEVC `RExt` 4:4:4 decodes (Grading/HDR). On Windows this means
    /// `HEVC_VLD_MAIN_444`/`HEVC_VLD_MAIN10_444` exist *and* a probe
    /// keyframe decoded (ADR 0024).
    pub yuv444: bool,
    /// Ten-bit decodes (HEVC Main10; with `yuv444`, Main 4:4:4 10 → Y410).
    pub main10: bool,
    /// Twelve-bit decodes.
    pub main12: bool,
    /// Full-range signalling is honoured.
    pub full_range: bool,
    /// Identity (GBR) matrix decodes faithfully.
    pub identity_matrix: bool,
    /// BT.601 matrix extensions are honoured.
    pub bt601_matrix: bool,
    /// BT.2020 NCL matrix extensions are honoured.
    pub bt2020_ncl_matrix: bool,
    /// Whether the primary probed codec decoded in hardware; `None` when
    /// unknown.
    pub hardware_accelerated: Option<bool>,
}

impl DecodeCapabilities {
    /// The hello's `decoder_backend` label, for example `videotoolbox-hw` or
    /// `d3d11va-hw`; `unsupported` when neither H.264 nor HEVC decodes.
    #[must_use]
    pub const fn decoder_backend_label(&self) -> &'static str {
        if !self.h264 && !self.h265 {
            return "unsupported";
        }
        match (self.backend, self.hardware_accelerated) {
            (DecoderBackendFamily::VideoToolbox, Some(true)) => "videotoolbox-hw",
            (DecoderBackendFamily::VideoToolbox, Some(false)) => "videotoolbox-sw",
            (DecoderBackendFamily::VideoToolbox, None) => "videotoolbox",
            (DecoderBackendFamily::D3d11Video, Some(true)) => "d3d11va-hw",
            (DecoderBackendFamily::D3d11Video, Some(false)) => "d3d11va-sw",
            (DecoderBackendFamily::D3d11Video, None) => "d3d11va",
            (DecoderBackendFamily::Unprobed, _) => "unprobed",
        }
    }

    /// The decode flags as the initial video request carries them
    /// (`srgb_transfer`, a presentation fact, left false).
    #[must_use]
    pub const fn video_capabilities(&self) -> arcen_protocol::messages::ClientVideoCapabilitiesMsg {
        arcen_protocol::messages::ClientVideoCapabilitiesMsg {
            h264: self.h264,
            h265: self.h265,
            av1: self.av1,
            yuv444: self.yuv444,
            main10: self.main10,
            main12: self.main12,
            full_range: self.full_range,
            identity_matrix: self.identity_matrix,
            bt601_matrix: self.bt601_matrix,
            bt2020_ncl_matrix: self.bt2020_ncl_matrix,
            srgb_transfer: false,
        }
    }
}

/// The two colour axes a session negotiates once and a packet header never
/// carries, read back from the Pier's `ServerColorCaps`. `transfer` decides
/// whether presentation is HDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionColor {
    /// Colour primaries the Pier says it is sending.
    pub primaries: ColorPrimaries,
    /// Transfer characteristics the Pier says it is sending.
    pub transfer: TransferCharacteristics,
}

impl Default for SessionColor {
    /// BT.709 SDR: the correct assumption for a Pier that reports neither.
    fn default() -> Self {
        Self {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }
}

impl SessionColor {
    /// What the host says it actually serves: its hello's `active_*`
    /// colour axes rather than what this Deck asked for, so a downgraded
    /// session is presented as what arrives. An axis the host does not
    /// report keeps the BT.709 SDR default rather than a guess. Every
    /// consumer of a hello (the media worker, the CLI smokes) teaches its
    /// decoders this before the first keyframe.
    #[must_use]
    pub fn from_hello(hello: &arcen_protocol::messages::ServerHelloMsg) -> Self {
        let active = arcen_media::colour_check::ColourAxes::from_hello(hello);
        let presentation = arcen_media::VideoColorMetadata::from_optional_or_legacy(
            active.range,
            active.matrix,
            active.primaries,
            active.transfer,
        )
        .presentation_axes();
        Self {
            primaries: presentation.primaries,
            transfer: presentation.transfer,
        }
    }
}

/// The memory layout of a native decoded picture, so the presenter picks
/// its sampling path without knowing the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePictureFormat {
    /// 4:2:0 8-bit, Y plane + interleaved `CbCr` (`NV12`, `CoreVideo` `420v`/`420f`).
    Nv12,
    /// 4:2:0 10-bit in 16-bit containers, biplanar (`P010`, `CoreVideo` `x420`).
    P010,
    /// 4:4:4 8-bit packed (`AYUV`; HEVC `RExt` Main 4:4:4 on D3D11).
    Ayuv,
    /// 4:4:4 10-bit packed 10:10:10:2 (`Y410`; HEVC `RExt` Main 4:4:4 10 on
    /// D3D11: the Grading/HDR picture on Windows).
    Y410,
    /// 4:4:4 10-bit biplanar (`CoreVideo` `xf44`: the Grading/HDR picture on
    /// macOS).
    BiPlanar444Ten,
    /// Already-converted 8-bit BGRA.
    Bgra8,
    /// Anything else, by its platform four-character code.
    Other(u32),
}

impl NativePictureFormat {
    /// Whether the picture carries more than eight bits per sample.
    #[must_use]
    pub const fn is_ten_bit(self) -> bool {
        matches!(self, Self::P010 | Self::Y410 | Self::BiPlanar444Ten)
    }

    /// Whether chroma is at full resolution.
    #[must_use]
    pub const fn is_full_chroma(self) -> bool {
        matches!(
            self,
            Self::Ayuv | Self::Y410 | Self::BiPlanar444Ten | Self::Bgra8
        )
    }
}

/// A decoded picture that stays on the GPU, with the complete video
/// configuration it was decoded under.
///
/// `N` is the backend's [`VideoDecoderBackend::NativeFrame`].
#[derive(Debug, Clone)]
pub struct NativeDecodedVideoFrame<N> {
    /// The native picture (macOS `CVPixelBuffer`; Windows a texture slice
    /// plus the fence its decode signalled).
    pub picture: N,
    /// Its layout.
    pub format: NativePictureFormat,
    /// Codec, chroma, depth, range, matrix, primaries and transfer: stamped
    /// from the packet header plus the [`SessionColor`].
    pub video: VideoConfiguration,
}

/// One decoded frame as the media worker publishes it.
#[derive(Debug, Clone)]
pub struct DecodedVideoFrame<N> {
    /// Picture width in pixels.
    pub width: usize,
    /// Picture height in pixels.
    pub height: usize,
    /// CPU RGBA fallback for the egui path. Empty when the frame was
    /// published for a dedicated presenter only; then `native` is set.
    pub rgba: Vec<u8>,
    /// The packet's wire timestamp; also the key the Exact still image hub
    /// uses to find the fence that preceded this access unit
    /// (`DeckRefineHub::frame_tag`).
    pub timestamp_ms: u32,
    /// Human-readable pixel format for statistics (for example `Y410`).
    pub pixel_format: String,
    /// The backend label for statistics.
    pub backend: &'static str,
    /// The zero-copy picture, when the backend produced one.
    pub native: Option<NativeDecodedVideoFrame<N>>,
}

/// Why a packet did not decode. The monitor router turns every error into
/// keyframe recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoDecodeError {
    /// The codec has no decode path in this backend.
    CodecPathNotWired(VideoCodec),
    /// This backend does not run on this platform.
    UnsupportedPlatform,
    /// A reserved wire colour value; never decoded as a legacy default.
    InvalidWireColor(ProtocolError),
    /// The stream needs something this machine did not prove (profile,
    /// chroma, depth or size); the reason is shown to the user.
    Unsupported(String),
    /// The native decoder failed.
    Backend(String),
}

impl fmt::Display for VideoDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CodecPathNotWired(codec) => {
                write!(formatter, "no native {codec:?} decode path on this Deck")
            }
            Self::UnsupportedPlatform => {
                formatter.write_str("native decoding is not available on this platform")
            }
            Self::InvalidWireColor(error) => {
                write!(formatter, "invalid video wire colour metadata: {error:?}")
            }
            Self::Unsupported(reason) | Self::Backend(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for VideoDecodeError {}

/// One hardware decoder for one monitor's stream.
///
/// Not `Send`: a decoder lives and dies on the media worker thread that
/// created it through [`VideoDecoderFactory::create`].
///
/// # Native pictures and fences
///
/// When [`decode`](Self::decode) returns a frame with `native` set, the
/// picture must be safe for the presenter's device to sample *as handed
/// over*:
///
/// - either the decode completed on the CPU timeline (`VideoToolbox` output
///   callback), or
/// - [`Self::NativeFrame`] carries a GPU fence the presenter waits on
///   GPU-side before sampling (D3D11: `ID3D11Fence` + value signalled by the
///   decoder's context, waited with `ID3D11DeviceContext4::Wait`).
///
/// The backend must not reuse a picture's surface (a DPB or output slot)
/// while any clone of that `NativeFrame` is alive: the native type holds a
/// reference to its slot, and the backend sizes its output pool for its
/// reference frames plus the presenter queue. This is what makes Exact
/// still image fences frame-exact: the presenter composites tiles valid at
/// fence *F* over exactly the picture that followed *F* on the video lane
/// (`present::RefineFrameTag`).
pub trait VideoDecoderBackend {
    /// The decoder's native picture.
    type NativeFrame: Clone + fmt::Debug;

    /// Records the session's colour axes so decoded frames carry the
    /// stream's real primaries and transfer. Idempotent; called on every
    /// hello.
    fn set_session_color(&mut self, session_color: SessionColor);

    /// Whether the presenter wants zero-copy native pictures (`true`) or
    /// CPU RGBA for the egui path (`false`). Returns whether the preference
    /// changed, in which case the caller requests a keyframe.
    fn set_prefer_native_frames(&mut self, prefer: bool) -> bool;

    /// Applies the served pipeline's latency policy (Auto vs Speed) to the
    /// next decoder session this backend creates.
    fn set_decode_latency_policy(&mut self, policy: DecodeLatencyPolicy);

    /// Decodes one access unit. `Ok(None)` means the packet was consumed
    /// without a picture (for example parameter sets only, or waiting for a
    /// keyframe).
    ///
    /// # Errors
    ///
    /// Any [`VideoDecodeError`]; the caller drops the frame and asks the
    /// Pier for a keyframe.
    fn decode(
        &mut self,
        header: &VideoHeader,
        payload: &[u8],
    ) -> Result<Option<DecodedVideoFrame<Self::NativeFrame>>, VideoDecodeError>;

    /// A short label for statistics (for example `d3d11va-hevc-main10-444`).
    fn backend_name(&self) -> &'static str;

    /// Whether the current decoder session runs in hardware; `None` before
    /// the first session or when the platform cannot tell.
    fn is_hardware_accelerated(&self) -> Option<bool>;

    /// Whether the backend discards everything until a keyframe arrives.
    fn wants_keyframe(&self) -> bool;

    /// The stream jumped (ingress drop, monitor reattach): forget reference
    /// state and wait for a keyframe.
    fn notify_discontinuity(&mut self);

    /// Pictures discarded because one access unit produced more than one
    /// (reordered output reaching a client that cannot reorder). Expected
    /// to stay at zero.
    fn collapsed_decode_callbacks(&self) -> u64 {
        0
    }
}

/// Creates decoders on the media worker thread.
///
/// The factory is `Send` so the session can hand it to the worker; the
/// decoders it creates need not be. On Windows the factory carries the
/// presenter's D3D11 device (decoder and presenter share one device,
/// ADR 0024).
pub trait VideoDecoderFactory: Send + 'static {
    /// The decoder this factory creates.
    type Decoder: VideoDecoderBackend;

    /// A fresh decoder (one per monitor, and a new one on every reconnect).
    fn create(&self) -> Self::Decoder;
}

/// The complete video configuration of a frame decoded from the wire: the
/// codec the decoder ran, the chroma, depth, range and matrix the packet
/// header carries, and the session's primaries and transfer, which no packet
/// header carries ([`SessionColor`]). Every Deck labels its pictures with
/// this one mapping.
#[must_use]
pub fn wire_video_configuration(
    codec: arcen_media::VideoCodec,
    chroma: arcen_protocol::ChromaSubsampling,
    bit_depth: arcen_protocol::wire::BitDepth,
    range: arcen_protocol::wire::ColorRange,
    matrix: arcen_protocol::wire::ColorMatrix,
    session_color: SessionColor,
) -> VideoConfiguration {
    use arcen_protocol::wire;
    VideoConfiguration {
        codec,
        chroma: match chroma {
            arcen_protocol::ChromaSubsampling::Yuv420 => arcen_media::ChromaSubsampling::Yuv420,
            arcen_protocol::ChromaSubsampling::Yuv422 => arcen_media::ChromaSubsampling::Yuv422,
            arcen_protocol::ChromaSubsampling::Yuv444 => arcen_media::ChromaSubsampling::Yuv444,
        },
        bit_depth: match bit_depth {
            wire::BitDepth::Eight => arcen_media::BitDepth::Eight,
            wire::BitDepth::Ten => arcen_media::BitDepth::Ten,
            wire::BitDepth::Twelve => arcen_media::BitDepth::Twelve,
        },
        range: match range {
            wire::ColorRange::Full => arcen_media::ColorRange::Full,
            wire::ColorRange::Limited => arcen_media::ColorRange::Limited,
        },
        matrix: match matrix {
            wire::ColorMatrix::Bt709 => arcen_media::ColorMatrix::Bt709,
            wire::ColorMatrix::Identity => arcen_media::ColorMatrix::Identity,
            wire::ColorMatrix::Bt601 => arcen_media::ColorMatrix::Bt601,
            wire::ColorMatrix::Bt2020Ncl => arcen_media::ColorMatrix::Bt2020Ncl,
        },
        primaries: session_color.primaries,
        transfer: session_color.transfer,
    }
}

/// [`wire_video_configuration`] for one packet header.
///
/// # Errors
///
/// [`VideoDecodeError::InvalidWireColor`] when the header's depth or matrix
/// bits hold a reserved value.
pub fn header_video_configuration(
    header: &VideoHeader,
    codec: arcen_media::VideoCodec,
    session_color: SessionColor,
) -> Result<VideoConfiguration, VideoDecodeError> {
    let bit_depth = header
        .bit_depth()
        .map_err(VideoDecodeError::InvalidWireColor)?;
    let matrix = header
        .color_matrix()
        .map_err(VideoDecodeError::InvalidWireColor)?;
    Ok(wire_video_configuration(
        codec,
        header.chroma,
        bit_depth,
        header.color_range(),
        matrix,
        session_color,
    ))
}

/// PCI vendor ids [`decode_device_split`] knows.
pub mod pci_vendor {
    pub const INTEL: u32 = 0x8086;
    pub const NVIDIA: u32 = 0x10DE;
    pub const AMD: u32 = 0x1002;
}

/// The first NVIDIA driver release (×100) whose shared textures between
/// devices are reliable (Moonlight's documented boundary, about 471.11; it
/// also excludes Fermi, whose last driver is 391).
pub const NVIDIA_SPLIT_MIN_RELEASE: u32 = 47_111;

/// What a Deck knows about a GPU before it decodes on a device separate
/// from the devices that present (the Windows Deck): the presenters open
/// the decoded pictures as shared textures and order their reads with
/// shared fences, so a blocking present can never stall decode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeDeviceFacts {
    /// The adapter's PCI vendor id.
    pub vendor_id: u32,
    /// The user-mode driver version as Windows reports it (four fields,
    /// most significant first), when known.
    pub driver_version: Option<[u16; 4]>,
    /// The driver shares more than the guaranteed texture formats
    /// (D3D11 extended resource sharing).
    pub extended_resource_sharing: bool,
    /// The devices have GPU fences that can be shared between them.
    pub shared_fences: bool,
    /// Pixel shaders run at 16-bit minimum precision (on AMD: Vega and
    /// later).
    pub min_precision_16_bit: bool,
}

/// An NVIDIA driver version's release number ×100 (`31.0.15.5384` →
/// 55384, i.e. 553.84): the last digit of the third field and the fourth.
#[must_use]
pub fn nvidia_release(version: [u16; 4]) -> u32 {
    u32::from(version[2] % 10) * 10_000 + u32::from(version[3])
}

/// Whether a Deck may decode on a device of its own, separate from the
/// presenters' devices, or why not (then decoder and presenters share one
/// device).
///
/// The split needs extended resource sharing and shareable fences, and
/// only adapters where it is known to work: Intel; NVIDIA from driver
/// 471.11; AMD from Vega (16-bit minimum precision). Pre-Vega AMD corrupts
/// pictures, older NVIDIA drivers and Qualcomm and Moore Threads drivers
/// break shared decode textures (known exclusions,
/// `docs/architecture/deck-presentation-timing.md`). Unknown vendors
/// share one device.
///
/// # Errors
///
/// The reason, for the log.
pub fn decode_device_split(facts: &DecodeDeviceFacts) -> Result<(), &'static str> {
    if !facts.extended_resource_sharing {
        return Err("the driver has no extended resource sharing");
    }
    if !facts.shared_fences {
        return Err("the driver has no shareable fences");
    }
    match facts.vendor_id {
        pci_vendor::INTEL => Ok(()),
        pci_vendor::NVIDIA => match facts.driver_version {
            Some(version) if nvidia_release(version) >= NVIDIA_SPLIT_MIN_RELEASE => Ok(()),
            Some(_) => Err("NVIDIA drivers before 471.11 corrupt shared decode textures"),
            None => Err("the NVIDIA driver version is unknown"),
        },
        pci_vendor::AMD if facts.min_precision_16_bit => Ok(()),
        pci_vendor::AMD => Err("AMD GPUs before Vega corrupt shared decode textures"),
        _ => Err("this GPU vendor is not qualified for a separate decode device"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qualified(vendor_id: u32) -> DecodeDeviceFacts {
        DecodeDeviceFacts {
            vendor_id,
            driver_version: None,
            extended_resource_sharing: true,
            shared_fences: true,
            min_precision_16_bit: false,
        }
    }

    #[test]
    fn a_separate_decode_device_only_where_it_is_known_to_work() {
        assert_eq!(decode_device_split(&qualified(pci_vendor::INTEL)), Ok(()));
        let nvidia = |version| DecodeDeviceFacts {
            driver_version: version,
            ..qualified(pci_vendor::NVIDIA)
        };
        assert_eq!(nvidia_release([30, 0, 14, 7111]), 47_111);
        assert_eq!(nvidia_release([31, 0, 15, 5384]), 55_384);
        assert!(decode_device_split(&nvidia(Some([30, 0, 14, 7111]))).is_ok());
        assert!(decode_device_split(&nvidia(Some([31, 0, 15, 5384]))).is_ok());
        assert!(decode_device_split(&nvidia(Some([30, 0, 14, 7110]))).is_err());
        assert!(decode_device_split(&nvidia(Some([23, 21, 13, 9135]))).is_err());
        assert!(decode_device_split(&nvidia(None)).is_err());
        assert!(decode_device_split(&qualified(pci_vendor::AMD)).is_err());
        let vega = DecodeDeviceFacts {
            min_precision_16_bit: true,
            ..qualified(pci_vendor::AMD)
        };
        assert!(decode_device_split(&vega).is_ok());
        // Qualcomm, Moore Threads, Microsoft Basic Render: one device.
        for vendor in [0x5143, 0x1ED5, 0x1414] {
            assert!(decode_device_split(&qualified(vendor)).is_err());
        }
        let no_sharing = DecodeDeviceFacts {
            extended_resource_sharing: false,
            ..qualified(pci_vendor::INTEL)
        };
        assert!(decode_device_split(&no_sharing).is_err());
        let no_fences = DecodeDeviceFacts {
            shared_fences: false,
            ..qualified(pci_vendor::INTEL)
        };
        assert!(decode_device_split(&no_fences).is_err());
    }

    #[test]
    fn backend_labels_follow_the_family_and_hardware_answer() {
        let mut capabilities = DecodeCapabilities {
            backend: DecoderBackendFamily::D3d11Video,
            h265: true,
            hardware_accelerated: Some(true),
            ..DecodeCapabilities::default()
        };
        assert_eq!(capabilities.decoder_backend_label(), "d3d11va-hw");
        capabilities.backend = DecoderBackendFamily::VideoToolbox;
        capabilities.hardware_accelerated = None;
        assert_eq!(capabilities.decoder_backend_label(), "videotoolbox");
        capabilities.h265 = false;
        assert_eq!(capabilities.decoder_backend_label(), "unsupported");
    }

    #[test]
    fn grading_pictures_are_ten_bit_full_chroma_on_both_decks() {
        for format in [
            NativePictureFormat::Y410,
            NativePictureFormat::BiPlanar444Ten,
        ] {
            assert!(format.is_ten_bit() && format.is_full_chroma(), "{format:?}");
        }
        assert!(NativePictureFormat::P010.is_ten_bit());
        assert!(!NativePictureFormat::P010.is_full_chroma());
        assert!(!NativePictureFormat::Nv12.is_ten_bit());
    }

    #[test]
    fn a_grading_header_maps_to_ten_bit_full_range_444_with_the_session_colour() {
        let header = VideoHeader {
            frame_type: arcen_protocol::FrameType::VideoH265,
            codec: VideoCodec::H265,
            chroma: arcen_protocol::ChromaSubsampling::Yuv444,
            flags: VideoHeader::encode_flags(
                true,
                arcen_protocol::wire::BitDepth::Ten,
                arcen_protocol::wire::ColorRange::Full,
                arcen_protocol::wire::ColorMatrix::Bt709,
            ),
            timestamp_ms: 1,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
        };
        let session = SessionColor {
            primaries: ColorPrimaries::Bt2020,
            transfer: TransferCharacteristics::Pq,
        };
        let video = header_video_configuration(&header, arcen_media::VideoCodec::H265, session)
            .expect("valid wire colour");
        assert_eq!(video.codec, arcen_media::VideoCodec::H265);
        assert_eq!(video.chroma, arcen_media::ChromaSubsampling::Yuv444);
        assert_eq!(video.bit_depth, arcen_media::BitDepth::Ten);
        assert_eq!(video.range, arcen_media::ColorRange::Full);
        assert_eq!(video.matrix, arcen_media::ColorMatrix::Bt709);
        assert_eq!(video.primaries, ColorPrimaries::Bt2020);
        assert_eq!(video.transfer, TransferCharacteristics::Pq);
    }

    #[test]
    fn the_session_colour_is_what_the_host_says_it_serves() {
        let hello = |caps: serde_json::Value| -> arcen_protocol::messages::ServerHelloMsg {
            serde_json::from_value(serde_json::json!({
                "type": "server_hello",
                "codec": "h265",
                "color_caps": caps,
            }))
            .expect("minimal server_hello parses")
        };
        let hdr = hello(serde_json::json!({
            "active_bit_depth": "10", "active_range": "limited", "active_matrix": "bt2020nc",
            "active_primaries": "bt2020", "active_transfer": "pq",
        }));
        assert_eq!(
            SessionColor::from_hello(&hdr),
            SessionColor {
                primaries: ColorPrimaries::Bt2020,
                transfer: TransferCharacteristics::Pq,
            }
        );
        // A host that reports neither axis is BT.709 SDR.
        let legacy = hello(serde_json::json!({ "active_bit_depth": "8" }));
        assert_eq!(SessionColor::from_hello(&legacy), SessionColor::default());
    }
}
