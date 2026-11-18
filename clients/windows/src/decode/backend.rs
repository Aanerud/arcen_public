// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `VideoDecoderBackend` on Arcen's own D3D11 decoder: `arcen-media` parses
//! and manages pictures, this file turns each picture into DXVA buffers.

use arcen_deck_core::decode::{
    DecodedVideoFrame, NativeDecodedVideoFrame, SessionColor, VideoDecodeError,
    VideoDecoderBackend, VideoDecoderFactory, header_video_configuration,
};
use arcen_media::bitstream::BitstreamError;
use arcen_media::bitstream::h264::{H264AccessUnit, H264Parser};
use arcen_media::bitstream::hevc::{HevcAccessUnit, HevcParser};
use arcen_media::bitstream::picture::{
    OutputPicture, Slot, SlotTags, StreamCodecKind, StreamFormat,
};
use arcen_media::video::DecodeLatencyPolicy;
use arcen_protocol::{FrameType, VideoCodec, VideoHeader};

use super::device::{VideoDevice, profile_for};
use super::dxva::{
    Bitstream, bytes_of, h264_picture_parameters, h264_qmatrix, hevc_picture_parameters,
    hevc_qmatrix,
};
use super::session::{D3d11Picture, DecoderSession, PictureBuffers};
use super::shared::SharedExport;
use std::sync::Arc;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

/// Pictures the presenter may hold beyond the stream's own buffer: the
/// frame queue's latest picture, the one on glass and one in flight, plus
/// the frames a multi-monitor session's timeline holds back (ADR 0026).
pub const PRESENTER_SLOTS: u8 = 4 + arcen_deck_core::runtime::frame_sync::MAX_HELD_PER_MONITOR;

/// Shared textures per decoder when it runs on a device of its own: every
/// picture the presenters may hold, plus the one being written.
pub const SHARED_POOL: u8 = PRESENTER_SLOTS + 1;

static NEXT_DECODER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

enum Parser {
    H264(H264Parser),
    Hevc(HevcParser),
}

/// The picture model of either codec.
enum Picture {
    H264(Box<arcen_media::bitstream::h264::H264Picture>),
    Hevc(Box<arcen_media::bitstream::hevc::HevcPicture>),
}

/// What one access unit produced, codec-independent.
struct ParsedUnit {
    format_changed: bool,
    format: Option<StreamFormat>,
    decoded: Option<Slot>,
    output: Vec<OutputPicture>,
}

/// What the wire said about the access unit that decoded a slot.
#[derive(Debug, Clone, Copy)]
struct FrameTag {
    timestamp_ms: u32,
    video: arcen_media::VideoConfiguration,
}

/// Decoded slots and the pictures one access unit released, in output
/// order.
type DecodedUnit = (Option<Slot>, Vec<(Slot, D3d11Picture)>);

/// Arcen's D3D11 video decoder for one monitor's stream.
pub struct D3d11VideoDecoder {
    device: VideoDevice,
    parser: Option<(StreamCodecKind, Parser)>,
    session: Option<DecoderSession>,
    waiting_for_keyframe: bool,
    status_report: u32,
    session_color: SessionColor,
    collapsed: u64,
    name: &'static str,
    tags: SlotTags<FrameTag>,
    /// This decoder's number in the log (one decoder per monitor stream).
    decoder: u64,
    /// How pictures reach the presenters; a shared hand-over is made on
    /// first use.
    hand_over: HandOver,
    export: Option<SharedExport>,
}

/// How a decoder's pictures reach the presenters.
#[derive(Debug, Clone)]
pub enum HandOver {
    /// The decoder runs on the presenters' device: pictures are used in
    /// place.
    InPlace,
    /// The decoder runs on a device of its own: pictures are copied into
    /// shared textures (`super::shared`), only in these formats (the ones
    /// proven to read back exactly on another device; `None`: any, for the
    /// proof itself).
    Shared(Option<Arc<[DXGI_FORMAT]>>),
}

impl std::fmt::Debug for D3d11VideoDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("D3d11VideoDecoder")
            .field("adapter", &self.device.adapter_name)
            .field(
                "session",
                &self.session.as_ref().map(|session| session.profile.name),
            )
            .field("waiting_for_keyframe", &self.waiting_for_keyframe)
            .field("hand_over", &self.hand_over)
            .finish_non_exhaustive()
    }
}

fn bitstream_error(error: BitstreamError) -> VideoDecodeError {
    match error {
        BitstreamError::Unsupported(what) => {
            VideoDecodeError::Unsupported(format!("this Deck cannot decode {what}"))
        }
        other => VideoDecodeError::Backend(format!("bitstream: {other}")),
    }
}

impl D3d11VideoDecoder {
    #[must_use]
    pub fn new(device: VideoDevice) -> Self {
        Self {
            device,
            parser: None,
            session: None,
            waiting_for_keyframe: true,
            status_report: 0,
            session_color: SessionColor::default(),
            collapsed: 0,
            name: "d3d11va",
            tags: SlotTags::default(),
            decoder: NEXT_DECODER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            hand_over: HandOver::InPlace,
            export: None,
        }
    }

    /// A decoder on a device of its own: every picture is handed to the
    /// presenters through a shared texture (`super::shared`), in `formats`
    /// only (`None`: any).
    #[must_use]
    pub fn exporting(device: VideoDevice, formats: Option<Arc<[DXGI_FORMAT]>>) -> Self {
        Self {
            hand_over: HandOver::Shared(formats),
            ..Self::new(device)
        }
    }

    /// `picture` as the presenters receive it: through a shared texture
    /// when this decoder has a device of its own (`None`: every shared
    /// texture is still held, so the picture is dropped).
    ///
    /// # Errors
    ///
    /// [`VideoDecodeError::Unsupported`] for a format this driver does not
    /// share exactly (the connection chose a device of its own for a
    /// picture that needs none of those), [`VideoDecodeError::Backend`] for
    /// a failed hand-over.
    fn hand_over(
        &mut self,
        picture: D3d11Picture,
    ) -> Result<Option<D3d11Picture>, VideoDecodeError> {
        match &self.hand_over {
            HandOver::InPlace => return Ok(Some(picture)),
            HandOver::Shared(Some(formats)) if !formats.contains(&picture.format) => {
                return Err(VideoDecodeError::Unsupported(format!(
                    "this PC's graphics driver does not pass {} pictures between devices exactly",
                    super::shared::format_name(picture.format)
                )));
            }
            HandOver::Shared(_) => {}
        }
        if self.export.is_none() {
            self.export = Some(
                SharedExport::new(&self.device, SHARED_POOL, self.decoder)
                    .map_err(VideoDecodeError::Backend)?,
            );
        }
        let export = self
            .export
            .as_mut()
            .ok_or_else(|| VideoDecodeError::Backend("no picture hand-over".into()))?;
        export.export(&picture).map_err(VideoDecodeError::Backend)
    }

    /// The device this decoder runs on.
    #[must_use]
    pub const fn device(&self) -> &VideoDevice {
        &self.device
    }

    fn reset(&mut self) {
        if let Some((_, parser)) = &mut self.parser {
            match parser {
                Parser::H264(parser) => parser.reset(),
                Parser::Hevc(parser) => parser.reset(),
            }
        }
        self.tags.clear();
        self.waiting_for_keyframe = true;
    }

    /// Decodes one Annex B access unit of `codec` and returns the pictures
    /// ready to show (normally exactly the one just decoded; none when this
    /// decoder hands pictures over through shared textures and every one
    /// is still held).
    ///
    /// # Errors
    ///
    /// [`VideoDecodeError::Unsupported`] for a stream this adapter cannot
    /// decode (with the reason), [`VideoDecodeError::Backend`] for a
    /// malformed unit, a missing reference or a driver failure. After an
    /// error the decoder waits for a keyframe.
    pub fn decode_annex_b(
        &mut self,
        codec: StreamCodecKind,
        access_unit: &[u8],
    ) -> Result<Vec<D3d11Picture>, VideoDecodeError> {
        let (_, pictures) = self.decode_tagged(codec, access_unit)?;
        let mut shown = Vec::with_capacity(pictures.len());
        for (_, picture) in pictures {
            shown.extend(self.hand_over(picture)?);
        }
        Ok(shown)
    }

    /// [`Self::decode_annex_b`] with the slot this unit decoded into and the
    /// slot of every picture it released.
    fn decode_tagged(
        &mut self,
        codec: StreamCodecKind,
        access_unit: &[u8],
    ) -> Result<DecodedUnit, VideoDecodeError> {
        let result = self.decode_inner(codec, access_unit);
        if result.is_err() {
            self.reset();
        } else {
            self.waiting_for_keyframe = false;
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    fn decode_inner(
        &mut self,
        codec: StreamCodecKind,
        access_unit: &[u8],
    ) -> Result<DecodedUnit, VideoDecodeError> {
        if self
            .parser
            .as_ref()
            .is_none_or(|(active, _)| *active != codec)
        {
            let parser = match codec {
                StreamCodecKind::H264 => Parser::H264(H264Parser::new(PRESENTER_SLOTS)),
                StreamCodecKind::H265 => Parser::Hevc(HevcParser::new(PRESENTER_SLOTS)),
            };
            self.parser = Some((codec, parser));
            self.session = None;
        }
        let leases = self.session.as_ref().map(|session| session.leases.clone());
        let held = move |slot: Slot| leases.as_ref().is_some_and(|leases| leases.held(slot));
        let Some((_, parser)) = &mut self.parser else {
            return Err(VideoDecodeError::Backend("no parser".into()));
        };
        let (unit, picture, pool) = match parser {
            Parser::H264(parser) => {
                let H264AccessUnit {
                    format_changed,
                    format,
                    picture,
                    output,
                } = parser
                    .decode_access_unit(access_unit, &held)
                    .map_err(bitstream_error)?;
                let decoded = picture.as_ref().map(|picture| picture.slot);
                (
                    ParsedUnit {
                        format_changed,
                        format,
                        decoded,
                        output,
                    },
                    picture.map(|picture| Picture::H264(Box::new(picture))),
                    parser.pool_size(),
                )
            }
            Parser::Hevc(parser) => {
                let HevcAccessUnit {
                    format_changed,
                    format,
                    picture,
                    output,
                } = parser
                    .decode_access_unit(access_unit, &held)
                    .map_err(bitstream_error)?;
                let decoded = picture.as_ref().map(|picture| picture.slot);
                (
                    ParsedUnit {
                        format_changed,
                        format,
                        decoded,
                        output,
                    },
                    picture.map(|picture| Picture::Hevc(Box::new(picture))),
                    parser.pool_size(),
                )
            }
        };
        if unit.format_changed || (self.session.is_none() && unit.decoded.is_some()) {
            self.session = None;
            let format = unit
                .format
                .ok_or_else(|| VideoDecodeError::Backend("picture without a format".into()))?;
            let profile = profile_for(&format).map_err(VideoDecodeError::Unsupported)?;
            let session = DecoderSession::create(&self.device, profile, format, pool, self.decoder)
                .map_err(VideoDecodeError::Unsupported)?;
            if session.pool_size() < pool {
                return Err(VideoDecodeError::Backend("decoder pool too small".into()));
            }
            self.name = match profile.name {
                "h264" => "d3d11va-h264",
                "hevc-main" => "d3d11va-hevc-main",
                "hevc-main10" => "d3d11va-hevc-main10",
                "hevc-main-444" => "d3d11va-hevc-main-444",
                _ => "d3d11va-hevc-main10-444",
            };
            self.session = Some(session);
        }
        let Some(picture) = picture else {
            return Ok((None, Vec::new()));
        };
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| VideoDecodeError::Backend("no decoder session".into()))?;
        self.status_report = self.status_report.wrapping_add(1).max(1);
        match &picture {
            Picture::Hevc(picture) => {
                let parameters = hevc_picture_parameters(picture, self.status_report);
                let range_extension = session.profile.chroma_format_idc == 3
                    || picture.sps.profile_tier_level.profile_idc >= 4;
                let parameter_bytes = if range_extension {
                    bytes_of(&parameters)
                } else {
                    bytes_of(&parameters.params)
                };
                let matrix = hevc_qmatrix(picture);
                let bitstream = Bitstream::assemble(
                    picture
                        .slices
                        .iter()
                        .map(|slice| &access_unit[slice.nal.clone()]),
                );
                let slice_bytes = bitstream.slice_bytes();
                session
                    .decode(
                        picture.slot,
                        &PictureBuffers {
                            picture_parameters: parameter_bytes,
                            inverse_quantization: matrix.as_ref().map(bytes_of),
                            slice_control: &slice_bytes,
                            slice_count: u32::try_from(bitstream.slices.len()).unwrap_or(0),
                            macroblocks: 0,
                            bitstream: &bitstream.data,
                        },
                    )
                    .map_err(VideoDecodeError::Backend)?;
            }
            Picture::H264(picture) => {
                let parameters = h264_picture_parameters(picture, self.status_report);
                let matrix = h264_qmatrix(picture);
                let bitstream = Bitstream::assemble(
                    picture
                        .slices
                        .iter()
                        .map(|slice| &access_unit[slice.nal.clone()]),
                );
                let slice_bytes = bitstream.slice_bytes();
                session
                    .decode(
                        picture.slot,
                        &PictureBuffers {
                            picture_parameters: bytes_of(&parameters),
                            inverse_quantization: Some(bytes_of(&matrix)),
                            slice_control: &slice_bytes,
                            slice_count: u32::try_from(bitstream.slices.len()).unwrap_or(0),
                            macroblocks: picture.sps.pic_width_in_mbs
                                * picture.sps.frame_height_in_mbs(),
                            bitstream: &bitstream.data,
                        },
                    )
                    .map_err(VideoDecodeError::Backend)?;
            }
        }
        Ok((
            unit.decoded,
            unit.output
                .iter()
                .map(|output| (output.slot, session.picture(output.slot)))
                .collect(),
        ))
    }
}

impl VideoDecoderBackend for D3d11VideoDecoder {
    type NativeFrame = D3d11Picture;

    fn set_session_color(&mut self, session_color: SessionColor) {
        self.session_color = session_color;
    }

    fn set_prefer_native_frames(&mut self, _prefer: bool) -> bool {
        // Pictures always stay on the GPU; the Windows presenter samples
        // them (there is no CPU RGBA path).
        false
    }

    fn set_decode_latency_policy(&mut self, _policy: DecodeLatencyPolicy) {
        // Output is immediate for every contract: nothing to tune.
    }

    fn decode(
        &mut self,
        header: &VideoHeader,
        payload: &[u8],
    ) -> Result<Option<DecodedVideoFrame<D3d11Picture>>, VideoDecodeError> {
        let codec = match (header.codec, header.frame_type) {
            (VideoCodec::H264, _) | (_, FrameType::VideoH264) => StreamCodecKind::H264,
            (VideoCodec::H265, _) | (_, FrameType::VideoH265) => StreamCodecKind::H265,
            _ => return Err(VideoDecodeError::CodecPathNotWired(header.codec)),
        };
        let video = header_video_configuration(
            header,
            match codec {
                StreamCodecKind::H264 => arcen_media::VideoCodec::H264,
                StreamCodecKind::H265 => arcen_media::VideoCodec::H265,
            },
            self.session_color,
        )?;
        if self.waiting_for_keyframe && !header.is_keyframe() {
            return Ok(None);
        }
        let (decoded, mut pictures) = self.decode_tagged(codec, payload)?;
        // The parameter sets are what the decoder obeyed; a picture is never
        // presented under a contract its own stream contradicts.
        if let Some(reason) = self
            .session
            .as_ref()
            .and_then(|session| session.format.contradiction(&video))
        {
            self.reset();
            return Err(VideoDecodeError::Unsupported(format!(
                "refused a stream that contradicts its contract: {reason}"
            )));
        }
        if let Some(slot) = decoded {
            self.tags.decoded(
                slot,
                FrameTag {
                    timestamp_ms: header.timestamp_ms,
                    video,
                },
            );
        }
        let Some((slot, picture)) = pictures.pop() else {
            return Ok(None);
        };
        for (collapsed, _) in &pictures {
            self.tags.output(*collapsed);
        }
        self.collapsed += u64::try_from(pictures.len()).unwrap_or(0);
        // A reordered picture keeps the timestamp and contract of the unit
        // that decoded it, not of the unit that released it.
        let tag = self.tags.output(slot).ok_or_else(|| {
            VideoDecodeError::Backend("an output picture lost its access unit's identity".into())
        })?;
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| VideoDecodeError::Backend("no decoder session".into()))?;
        let profile_native = session.profile.native;
        let Some(picture) = self.hand_over(picture)? else {
            return Ok(None);
        };
        let (_, _, width, height) = picture.visible;
        Ok(Some(DecodedVideoFrame {
            width: width as usize,
            height: height as usize,
            rgba: Vec::new(),
            timestamp_ms: tag.timestamp_ms,
            pixel_format: format!("{profile_native:?}"),
            backend: self.name,
            native: Some(NativeDecodedVideoFrame {
                picture,
                format: profile_native,
                video: tag.video,
            }),
        }))
    }

    fn backend_name(&self) -> &'static str {
        self.name
    }

    fn is_hardware_accelerated(&self) -> Option<bool> {
        self.session.as_ref().map(|_| true)
    }

    fn wants_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    fn notify_discontinuity(&mut self) {
        self.reset();
    }

    fn collapsed_decode_callbacks(&self) -> u64 {
        self.collapsed
    }
}

/// Creates decoders: on a device of their own that hands pictures over
/// through shared textures, or on the presenter's device.
#[derive(Debug, Clone)]
pub struct D3d11DecoderFactory {
    device: VideoDevice,
    hand_over: HandOver,
}

// SAFETY: the D3D11 device is free-threaded and multithread-protected
// (`VideoDevice::from_device`); decoders are created on the worker thread.
// `D3d11VideoDecoder` holds the same interfaces.
#[allow(unsafe_code)]
unsafe impl Send for D3d11DecoderFactory {}

impl D3d11DecoderFactory {
    /// Decoders on the presenter's own device (`VideoDevice::from_gpu`).
    #[must_use]
    pub const fn new(device: VideoDevice) -> Self {
        Self {
            device,
            hand_over: HandOver::InPlace,
        }
    }

    /// Decoders on `device`, a device of their own
    /// (`VideoDevice::own_for`), whose pictures reach the presenters through
    /// shared textures in the proven `formats`.
    #[must_use]
    pub const fn own_device(device: VideoDevice, formats: Arc<[DXGI_FORMAT]>) -> Self {
        Self {
            device,
            hand_over: HandOver::Shared(Some(formats)),
        }
    }

    /// Whether the decoders have a device of their own.
    #[must_use]
    pub const fn has_own_device(&self) -> bool {
        matches!(self.hand_over, HandOver::Shared(_))
    }
}

impl VideoDecoderFactory for D3d11DecoderFactory {
    type Decoder = D3d11VideoDecoder;

    fn create(&self) -> D3d11VideoDecoder {
        D3d11VideoDecoder {
            hand_over: self.hand_over.clone(),
            ..D3d11VideoDecoder::new(self.device.clone())
        }
    }
}
