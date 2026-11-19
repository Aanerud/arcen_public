// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Media Foundation hardware path (Alpha): DDA/WGC capture, GPU
//! BGRA -> NV12 conversion with the D3D11 video processor, and a vendor's
//! hardware encoder MFT (`mf_encoder`), all on the capture's device.
//!
//! Three entry points:
//! - [`run`]: the live stream (`encoder=mf-hw`), READY only after the first
//!   access unit's SPS proves the plan;
//! - [`run_admission`]: the Pier's admission probe (`mf-hw-admit=WxH`): one
//!   real encode per codec on the session's adapter, proven by the shared
//!   SPS parsers and reported as `[capenc] ADMITTED` lines;
//! - [`probe_matrix_attempt`]: one colour-matrix row.

use std::time::{Duration, Instant};

use arcen_media::video::{
    prove_encoded_access_unit, BackendUnavailableReason, ConversionBackend, EncoderBackend,
    HardwareEncoderAdmission,
};
use arcen_media::{ChromaSubsampling, VideoCodec, VideoConfiguration};
use windows::core::Interface;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, LUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

use crate::mf_encoder::{Config as EncoderConfig, Encoder, MfRuntime};
use crate::win::OutputSelector;
use crate::{log, ColorSpec, CursorCaptureMode};

/// Capture devices that share their textures with a Media Foundation
/// encoder need video support for the video processor and the encoder.
pub(crate) const MF_DEVICE_FLAGS: D3D11_CREATE_DEVICE_FLAG = D3D11_CREATE_DEVICE_FLAG(
    D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0,
);
const IDLE_KEEPALIVE: Duration = Duration::from_secs(1);
const NV12_POOL: usize = 4;
/// Frames an admission probe may submit before its first proven keyframe.
const ADMISSION_FRAMES: u32 = 8;

const BACKEND: EncoderBackend = EncoderBackend::MediaFoundationHardware;

/// The output colour space the video processor writes for `color`.
pub(crate) fn nv12_color_space(color: ColorSpec) -> Option<DXGI_COLOR_SPACE_TYPE> {
    use arcen_media::{ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics};
    if color.primaries != ColorPrimaries::Bt709
        || !matches!(
            color.transfer,
            TransferCharacteristics::Bt709 | TransferCharacteristics::Srgb
        )
    {
        return None;
    }
    match (color.matrix, color.range) {
        (ColorMatrix::Bt709, ColorRange::Limited) => {
            Some(DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709)
        }
        (ColorMatrix::Bt709, ColorRange::Full) => Some(DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709),
        (ColorMatrix::Bt601, ColorRange::Limited) => {
            Some(DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601)
        }
        (ColorMatrix::Bt601, ColorRange::Full) => Some(DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601),
        (ColorMatrix::Identity | ColorMatrix::Bt2020Ncl, _) => None,
    }
}

/// GPU BGRA -> NV12 into a small texture pool, so the encoder can still be
/// reading one picture while the next is converted.
struct Nv12Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    landing: ID3D11Texture2D,
    landing_view: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    width: u32,
    height: u32,
}

impl Nv12Converter {
    unsafe fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        fps: u32,
        color: ColorSpec,
    ) -> windows::core::Result<Self> {
        let output_space = nv12_color_space(color).ok_or_else(|| {
            windows::core::Error::new(E_INVALIDARG, "no NV12 colour space for this contract")
        })?;
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext1 = context.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: fps.max(1),
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator: ID3D11VideoProcessorEnumerator =
            video_device.CreateVideoProcessorEnumerator(&content)?;
        let input_support = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?;
        let output_support = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?;
        if input_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
            || output_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
        {
            return Err(windows::core::Error::new(
                E_FAIL,
                "the video processor cannot convert BGRA to NV12 on this adapter",
            ));
        }
        let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;
        video_context.VideoProcessorSetStreamFrameFormat(
            &processor,
            0,
            D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        );
        video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        video_context.VideoProcessorSetStreamColorSpace1(
            &processor,
            0,
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
        );
        video_context.VideoProcessorSetOutputColorSpace1(&processor, output_space);

        let landing = create_texture(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM, None)?;
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut landing_view = None;
        video_device.CreateVideoProcessorInputView(
            &landing,
            &enumerator,
            &input_desc,
            Some(&mut landing_view),
        )?;
        let landing_view = landing_view.ok_or_else(|| windows::core::Error::from(E_FAIL))?;

        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut outputs = Vec::with_capacity(NV12_POOL);
        for _ in 0..NV12_POOL {
            let texture = create_texture(device, width, height, DXGI_FORMAT_NV12, None)?;
            let mut view = None;
            video_device.CreateVideoProcessorOutputView(
                &texture,
                &enumerator,
                &output_desc,
                Some(&mut view),
            )?;
            outputs.push((
                texture,
                view.ok_or_else(|| windows::core::Error::from(E_FAIL))?,
            ));
        }
        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            video_context,
            processor,
            landing,
            landing_view,
            outputs,
            next: 0,
            width,
            height,
        })
    }

    /// Copies `source` (BGRA, the converter's size) into the landing
    /// texture. GPU-only; the landing copy is what keepalives and forced
    /// IDRs re-convert when capture has nothing new.
    unsafe fn land(&self, source: &ID3D11Texture2D) -> windows::core::Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        source.GetDesc(&mut desc);
        if desc.Width != self.width
            || desc.Height != self.height
            || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
        {
            return Err(windows::core::Error::new(
                E_INVALIDARG,
                format!(
                    "capture delivered {}x{} {:?}; the encoder runs {}x{} BGRA",
                    desc.Width, desc.Height, desc.Format, self.width, self.height
                ),
            ));
        }
        let destination: ID3D11Resource = self.landing.cast()?;
        let source: ID3D11Resource = source.cast()?;
        self.context.CopyResource(&destination, &source);
        Ok(())
    }

    /// Converts the landing texture into the next pooled NV12 texture.
    unsafe fn convert(&mut self) -> windows::core::Result<ID3D11Texture2D> {
        let (texture, view) = self.outputs[self.next].clone();
        self.next = (self.next + 1) % self.outputs.len();
        let mut stream: D3D11_VIDEO_PROCESSOR_STREAM = std::mem::zeroed();
        stream.Enable = true.into();
        stream.pInputSurface = std::mem::ManuallyDrop::new(Some(self.landing_view.clone()));
        let result = self.video_context.VideoProcessorBlt(
            &self.processor,
            &view,
            0,
            std::slice::from_ref(&stream),
        );
        std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
        result?;
        Ok(texture)
    }

    /// Fills the landing texture with `bgra` (tightly packed, this size).
    unsafe fn land_pixels(&self, bgra: &[u8]) -> windows::core::Result<()> {
        let staging = create_texture(
            &self.device,
            self.width,
            self.height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            Some(bgra),
        )?;
        self.land(&staging)
    }
}

unsafe fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    pixels: Option<&[u8]>,
) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let initial = pixels.map(|pixels| D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    });
    let mut texture = None;
    device.CreateTexture2D(
        &desc,
        initial.as_ref().map(std::ptr::from_ref),
        Some(&mut texture),
    )?;
    texture.ok_or_else(|| windows::core::Error::from(E_FAIL))
}

unsafe fn adapter_luid(device: &ID3D11Device) -> windows::core::Result<LUID> {
    let dxgi: IDXGIDevice = device.cast()?;
    Ok(dxgi.GetAdapter()?.GetDesc()?.AdapterLuid)
}

/// Opaque black, or a moving test pattern with edges and gradients so an
/// admission encode is a real picture rather than a flat field.
pub(crate) fn synthetic_bgra(width: u32, height: u32, frame: u32) -> Vec<u8> {
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
        let x = (index % width as usize) as u32;
        let y = (index / width as usize) as u32;
        let shifted = x.wrapping_add(frame.wrapping_mul(8));
        pixel[0] = (shifted * 255 / width.max(1)) as u8;
        pixel[1] = (y * 255 / height.max(1)) as u8;
        pixel[2] = if (shifted / 32 + y / 32).is_multiple_of(2) {
            224
        } else {
            32
        };
        pixel[3] = 255;
    }
    pixels
}

fn claimed(codec: VideoCodec, color: ColorSpec) -> VideoConfiguration {
    VideoConfiguration {
        codec,
        chroma: ChromaSubsampling::Yuv420,
        bit_depth: color.bit_depth,
        range: color.range,
        matrix: color.matrix,
        primaries: color.primaries,
        transfer: color.transfer,
    }
}

#[allow(clippy::too_many_arguments)]
fn encoder_config(
    codec: VideoCodec,
    width: u32,
    height: u32,
    fps: u32,
    color: ColorSpec,
    intent: arcen_media::EncodeIntent,
    priority: arcen_media::video::MotionPriority,
    max_bitrate_bps: Option<u32>,
) -> (EncoderConfig, crate::nvenc_policy::RateControlSizing) {
    let sizing = crate::nvenc_policy::rate_control_sizing(
        width,
        height,
        fps,
        ChromaSubsampling::Yuv420,
        arcen_media::BitDepth::Eight,
        priority,
        intent,
        max_bitrate_bps,
    );
    (
        EncoderConfig {
            width,
            height,
            fps,
            codec,
            average_bitrate_bps: sizing.average_bitrate_bps,
            vbv_buffer_bits: sizing.vbv_buffer_size_bits,
            // On-demand IDR only (the hardware pipelines' keyframe policy).
            gop_frames: 0,
            color,
        },
        sizing,
    )
}

/// Encodes synthetic frames until one keyframe proves `codec`, returning the
/// proof line and the mean submit-to-access-unit time.
unsafe fn prove_codec(
    device: &ID3D11Device,
    luid: LUID,
    converter: &mut Nv12Converter,
    codec: VideoCodec,
    fps: u32,
    color: ColorSpec,
) -> Result<(HardwareEncoderAdmission, f64), (BackendUnavailableReason, String)> {
    let (width, height) = (converter.width, converter.height);
    let (config, _) = encoder_config(
        codec,
        width,
        height,
        fps,
        color,
        arcen_media::EncodeIntent::Interactive,
        arcen_media::video::MotionPriority::Detail,
        None,
    );
    let mut encoder = Encoder::open(device, luid, &config)
        .map_err(|error| (error.unavailable_reason(), error.to_string()))?;
    let mut produced = Vec::new();
    let mut total_ms = 0.0;
    let mut submitted = 0u32;
    for frame in 0..ADMISSION_FRAMES {
        converter
            .land_pixels(&synthetic_bgra(width, height, frame))
            .and_then(|()| converter.convert())
            .and_then(|nv12| {
                let started = Instant::now();
                let before = produced.len();
                encoder.submit(&nv12, frame == 0, &mut produced)?;
                if produced.len() > before {
                    total_ms += started.elapsed().as_secs_f64() * 1000.0;
                    submitted += 1;
                }
                Ok(())
            })
            .map_err(|error| {
                (
                    BackendUnavailableReason::HardwareUnavailable,
                    format!("{}: encode failed: {error}", encoder.name),
                )
            })?;
        if let Some(first) = produced.first() {
            let format = prove_encoded_access_unit(&claimed(codec, color), width, height, first)
                .map_err(|error| {
                    (
                        BackendUnavailableReason::UnsupportedConfiguration,
                        format!("{}: SPS does not prove the request: {error}", encoder.name),
                    )
                })?;
            return Ok((
                HardwareEncoderAdmission {
                    backend: BACKEND,
                    codec,
                    width,
                    height,
                    profile_idc: format.profile_idc,
                    encoder_name: encoder.name.clone(),
                },
                if submitted == 0 {
                    0.0
                } else {
                    total_ms / f64::from(submitted)
                },
            ));
        }
    }
    Err((
        BackendUnavailableReason::HardwareUnavailable,
        format!(
            "{}: no access unit after {ADMISSION_FRAMES} frames",
            encoder.name
        ),
    ))
}

pub(crate) struct AdmitOpts<'a> {
    pub selector: &'a OutputSelector<'a>,
    pub codecs: Vec<VideoCodec>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub color: ColorSpec,
}

/// The Pier's admission probe: one real encode per codec on the selected
/// output's adapter. Prints an `ADMITTED` line on stdout per proven codec and
/// exits 0 when at least one is proven; otherwise announces the typed reason
/// and exits 6, which a Pier reads as "not admitted".
pub(crate) fn run_admission(opts: &AdmitOpts<'_>) -> i32 {
    let started = Instant::now();
    let found = match unsafe {
        crate::win::Capture::find_output_device(opts.selector, MF_DEVICE_FLAGS)
    } {
        Ok(found) => found,
        Err(error) => {
            log(&format!(
                "MF hardware admission: adapter init failed: {error:?}"
            ));
            let _ =
                crate::announce_unavailable(BACKEND, BackendUnavailableReason::HardwareUnavailable);
            return 6;
        }
    };
    log(&format!(
        "MF hardware admission bound {} -> adapter {} ({:?}) vendor=0x{:04x}",
        opts.selector.describe(),
        found.adapter_index,
        found.adapter_name,
        found.vendor_id
    ));
    let _runtime = match MfRuntime::start() {
        Ok(runtime) => runtime,
        Err(error) => {
            log(&format!(
                "MF hardware admission: Media Foundation did not start: {error}"
            ));
            let _ = crate::announce_unavailable(BACKEND, BackendUnavailableReason::RuntimeMissing);
            return 6;
        }
    };
    let luid = match unsafe { adapter_luid(&found.device) } {
        Ok(luid) => luid,
        Err(error) => {
            log(&format!("MF hardware admission: adapter LUID: {error}"));
            let _ =
                crate::announce_unavailable(BACKEND, BackendUnavailableReason::HardwareUnavailable);
            return 6;
        }
    };
    let mut converter = match unsafe {
        Nv12Converter::new(
            &found.device,
            &found.context,
            opts.width,
            opts.height,
            opts.fps,
            opts.color,
        )
    } {
        Ok(converter) => converter,
        Err(error) => {
            log(&format!(
                "MF hardware admission: NV12 conversion unavailable: {error}"
            ));
            let _ =
                crate::announce_unavailable(BACKEND, BackendUnavailableReason::HardwareUnavailable);
            return 6;
        }
    };
    let mut admitted = 0;
    let mut reason = BackendUnavailableReason::HardwareUnavailable;
    for codec in &opts.codecs {
        match unsafe {
            prove_codec(
                &found.device,
                luid,
                &mut converter,
                *codec,
                opts.fps,
                opts.color,
            )
        } {
            Ok((admission, mean_ms)) => {
                log(&format!(
                    "MF hardware admission: {} proven by {:?} profile_idc={} at {}x{} \
                     (mean submit-to-AU {mean_ms:.2} ms)",
                    codec.token(),
                    admission.encoder_name,
                    admission.profile_idc,
                    opts.width,
                    opts.height
                ));
                println!("{}", admission.format_line());
                admitted += 1;
            }
            Err((cause, detail)) => {
                log(&format!(
                    "MF hardware admission: {} not admitted ({cause:?}): {detail}",
                    codec.token()
                ));
                reason = cause;
            }
        }
    }
    log(&format!(
        "MF hardware admission finished in {} ms: {admitted} of {} codecs admitted",
        started.elapsed().as_millis(),
        opts.codecs.len()
    ));
    if admitted == 0 {
        let _ = crate::announce_unavailable(BACKEND, reason);
        return 6;
    }
    0
}

/// One probe-matrix row: a real encode and SPS proof at the probe size on
/// the first output's adapter.
pub(crate) fn probe_matrix_attempt(
    row: arcen_media::video::VideoVariant,
    color: ColorSpec,
    width: u32,
    height: u32,
) -> crate::probe_matrix::EncoderAttemptOutcome {
    use crate::probe_matrix::EncoderAttemptOutcome;
    if !matches!(row.video.codec, VideoCodec::H264 | VideoCodec::H265) {
        return EncoderAttemptOutcome::Unsupported {
            detail: "Media Foundation hardware encodes H.264 and HEVC only".to_string(),
        };
    }
    if let Err(error) = crate::mf_encoder::validate_color(color) {
        return EncoderAttemptOutcome::Unsupported {
            detail: error.message().to_string(),
        };
    }
    let selector = OutputSelector {
        global_output_index: 0,
        adapter_hint: None,
        adapter_output_index: None,
        device_name: None,
    };
    let outcome =
        (|| unsafe {
            let found = crate::win::Capture::find_output_device(&selector, MF_DEVICE_FLAGS)
                .map_err(|error| {
                    (
                        BackendUnavailableReason::HardwareUnavailable,
                        format!("{error:?}"),
                    )
                })?;
            let _runtime = MfRuntime::start()
                .map_err(|error| (BackendUnavailableReason::RuntimeMissing, error.to_string()))?;
            let luid = adapter_luid(&found.device).map_err(|error| {
                (
                    BackendUnavailableReason::HardwareUnavailable,
                    error.to_string(),
                )
            })?;
            let mut converter =
                Nv12Converter::new(&found.device, &found.context, width, height, 30, color)
                    .map_err(|error| {
                        (
                            BackendUnavailableReason::HardwareUnavailable,
                            error.to_string(),
                        )
                    })?;
            prove_codec(
                &found.device,
                luid,
                &mut converter,
                row.video.codec,
                30,
                color,
            )
        })();
    match outcome {
        Ok((admission, mean_ms)) => EncoderAttemptOutcome::Ok {
            sustained_fps: (mean_ms > 0.0).then(|| 1000.0 / mean_ms),
            bitrate_mbps: None,
            note: format!(
                "{} proved profile_idc {} (Alpha)",
                admission.encoder_name, admission.profile_idc
            ),
        },
        Err((BackendUnavailableReason::UnsupportedConfiguration, detail)) => {
            EncoderAttemptOutcome::Unsupported { detail }
        }
        Err((_, detail)) => EncoderAttemptOutcome::Failed { detail },
    }
}

pub(crate) struct RunOpts<'a> {
    pub selector: &'a OutputSelector<'a>,
    pub codec: VideoCodec,
    pub fps: u32,
    pub color: ColorSpec,
    pub cursor_mode: CursorCaptureMode,
    pub framed: bool,
    pub force_wgc: bool,
    pub force_dda: bool,
    pub intent: arcen_media::EncodeIntent,
    pub priority: arcen_media::video::MotionPriority,
    pub max_bitrate_bps: Option<u32>,
}

fn unavailable(reason: BackendUnavailableReason, detail: &str) -> i32 {
    log(&format!("MF hardware unavailable ({reason:?}): {detail}"));
    let _ = crate::announce_unavailable(BACKEND, reason);
    6
}

#[derive(Default)]
struct SecondStats {
    captured: u64,
    empty_polls: u64,
    encoded: u64,
    bytes: u64,
    encode_ms_sum: f64,
    encode_ms_max: f64,
    convert_ms_sum: f64,
    convert_ms_max: f64,
    converted: u64,
    skipped_no_new: u64,
}

/// The live `encoder=mf-hw` stream. Returns the process exit code: 6 with a
/// typed `UNAVAILABLE` before READY when the hardware path cannot serve, so
/// the Pier falls through to OpenH264.
pub(crate) fn run(opts: &RunOpts<'_>) -> i32 {
    if let Err(error) = crate::mf_encoder::validate_color(opts.color) {
        return unavailable(
            BackendUnavailableReason::UnsupportedConfiguration,
            &error.message().to_string(),
        );
    }
    let mut source = unsafe {
        crate::win::select_source(
            opts.selector,
            opts.force_wgc,
            opts.force_dda,
            opts.cursor_mode,
            false,
            false,
            MF_DEVICE_FLAGS,
        )
    };
    let device = source.device().clone();
    let context = source.context().clone();
    let (width, height) = (source.width(), source.height());
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return unavailable(
            BackendUnavailableReason::UnsupportedDisplay,
            &format!("capture is {width}x{height}; 4:2:0 needs even dimensions"),
        );
    }
    let _runtime = match MfRuntime::start() {
        Ok(runtime) => runtime,
        Err(error) => {
            return unavailable(BackendUnavailableReason::RuntimeMissing, &error.to_string())
        }
    };
    let luid = match unsafe { adapter_luid(&device) } {
        Ok(luid) => luid,
        Err(error) => {
            return unavailable(
                BackendUnavailableReason::HardwareUnavailable,
                &error.to_string(),
            )
        }
    };
    let (config, sizing) = encoder_config(
        opts.codec,
        width,
        height,
        opts.fps,
        opts.color,
        opts.intent,
        opts.priority,
        opts.max_bitrate_bps,
    );
    let mut encoder = match unsafe { Encoder::open(&device, luid, &config) } {
        Ok(encoder) => encoder,
        Err(error) => {
            let reason = error.unavailable_reason();
            return unavailable(reason, &error.to_string());
        }
    };
    let mut converter =
        match unsafe { Nv12Converter::new(&device, &context, width, height, opts.fps, opts.color) }
        {
            Ok(converter) => converter,
            Err(error) => {
                return unavailable(
                    BackendUnavailableReason::HardwareUnavailable,
                    &error.to_string(),
                )
            }
        };
    let ready_plan = match crate::resolved_media_plan(
        BACKEND,
        opts.codec.token(),
        opts.color,
        width,
        height,
        opts.fps,
        opts.cursor_mode,
    ) {
        Ok(plan) => plan,
        Err(error) => {
            return unavailable(BackendUnavailableReason::UnsupportedConfiguration, &error)
        }
    };
    log(&format!(
        "{}: {:?} serving {} {width}x{height} @ {} fps from {} (GPU NV12 conversion), \
         bitrate start={} ceiling={}",
        BACKEND.display_name(),
        encoder.name,
        opts.codec.token(),
        opts.fps,
        source.capture_backend().ready_token(),
        sizing.average_bitrate_bps,
        sizing.max_bitrate_bps
    ));

    let control = crate::spawn_control_thread("mf-hw");
    let mut fps = opts.fps.max(1);
    let mut target_dt = crate::frame_interval_from_fps(fps);
    let mut next = Instant::now();
    let mut latest: Option<ID3D11Texture2D> = None;
    let mut fresh = false;
    let mut first = true;
    let mut ready = false;
    let mut last_emit = Instant::now();
    let started = Instant::now();
    let mut second = Instant::now();
    let mut stats = SecondStats::default();
    let mut dbg = (0u64, 0u64, 0u64);
    let mut stdout = std::io::stdout();
    let mut produced = Vec::new();

    while !control.stop_requested() {
        let timeout = u32::try_from(
            next.saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(1, 16),
        )
        .unwrap_or(16);
        let mut land_error = None;
        let mut new_frame = false;
        let acquired = unsafe {
            source.acquire_into(timeout, &mut dbg, None, |texture| {
                match converter.land(texture) {
                    Ok(()) => new_frame = true,
                    Err(error) => land_error = Some(error),
                }
            })
        };
        if let Err(error) = acquired {
            log(&format!("MF hardware: capture failed: {error:?}"));
            return 3;
        }
        if let Some(error) = land_error {
            log(&format!("MF hardware: capture copy failed: {error}"));
            return 3;
        }
        if new_frame {
            stats.captured += 1;
            fresh = true;
        } else {
            stats.empty_polls += 1;
        }
        if !fresh && latest.is_none() && started.elapsed() >= Duration::from_secs(1) {
            let black = [0u8, 0, 0, 255].repeat((width as usize) * (height as usize));
            if let Err(error) = unsafe { converter.land_pixels(&black) } {
                log(&format!("MF hardware: black frame failed: {error}"));
                return 3;
            }
            log("MF hardware: no desktop frame after 1s — streaming black until content arrives");
            fresh = true;
        }

        if let Some(rate) = control.take_framerate_fps() {
            let rate = rate.clamp(1, BACKEND.contract().max_fps);
            if rate != fps {
                fps = rate;
                target_dt = crate::frame_interval_from_fps(fps);
                encoder.set_frame_rate(fps);
                log(&format!(
                    "MF hardware: live framerate reconfigured: fps={fps}"
                ));
            }
        }
        if let Some(bps) = control.take_bitrate_bps() {
            let bps = u32::try_from(bps).unwrap_or(u32::MAX).clamp(
                sizing.average_bitrate_bps / 4,
                sizing.max_bitrate_bps.max(1),
            );
            match unsafe { encoder.reconfigure_bitrate(bps) } {
                Ok(()) => log(&format!(
                    "MF hardware: live bitrate reconfigured: target={bps}bps"
                )),
                Err(error) => log(&format!(
                    "MF hardware: live bitrate reconfigure failed: {error}"
                )),
            }
        }

        let now = Instant::now();
        let idr_pending = control.idr_pending();
        let due = fresh || first || idr_pending || last_emit.elapsed() >= IDLE_KEEPALIVE;
        if !due && now >= next {
            stats.skipped_no_new += 1;
        }
        if now >= next && due && fresh {
            // Convert only what will be encoded: the landing copy holds the
            // newest capture, and a keepalive or forced IDR re-sends the last
            // converted picture.
            let convert_started = Instant::now();
            match unsafe { converter.convert() } {
                Ok(texture) => latest = Some(texture),
                Err(error) => {
                    log(&format!("MF hardware: NV12 conversion failed: {error}"));
                    return 3;
                }
            }
            let ms = convert_started.elapsed().as_secs_f64() * 1000.0;
            stats.convert_ms_sum += ms;
            stats.convert_ms_max = stats.convert_ms_max.max(ms);
            stats.converted += 1;
        }
        if let (true, true, Some(texture)) = (now >= next, due, latest.as_ref()) {
            let force = first || (idr_pending && control.take_idr());
            let encode_started = Instant::now();
            produced.clear();
            if let Err(error) = unsafe { encoder.submit(texture, force, &mut produced) } {
                if ready {
                    log(&format!("MF hardware: encode failed: {error}"));
                    return 5;
                }
                return unavailable(
                    BackendUnavailableReason::HardwareUnavailable,
                    &error.to_string(),
                );
            }
            let ms = encode_started.elapsed().as_secs_f64() * 1000.0;
            fresh = false;
            first = false;
            last_emit = now;
            for access_unit in produced.drain(..) {
                if !ready {
                    match prove_encoded_access_unit(
                        &claimed(opts.codec, opts.color),
                        width,
                        height,
                        &access_unit,
                    ) {
                        Ok(format) => log(&format!(
                            "MF hardware: first access unit proves {} profile_idc={} \
                             chroma_format_idc={} {}-bit visible {}x{} (coded {}x{}) vui={:?}",
                            opts.codec.token(),
                            format.profile_idc,
                            format.chroma_format_idc,
                            format.bit_depth_luma,
                            format.visible.2,
                            format.visible.3,
                            format.coded_width,
                            format.coded_height,
                            format.colour
                        )),
                        Err(error) => {
                            return unavailable(
                                BackendUnavailableReason::UnsupportedConfiguration,
                                &format!("the first access unit does not prove the plan: {error}"),
                            );
                        }
                    }
                    if crate::announce_ready_from_with_conversion(
                        ready_plan,
                        Some(source.capture_backend()),
                        Some(ConversionBackend::Gpu),
                    )
                    .is_err()
                    {
                        return 5;
                    }
                    ready = true;
                }
                stats.bytes += access_unit.len() as u64;
                if crate::write_access_unit(&mut stdout, &access_unit, opts.framed).is_err() {
                    return 0;
                }
            }
            stats.encoded += 1;
            stats.encode_ms_sum += ms;
            stats.encode_ms_max = stats.encode_ms_max.max(ms);
            next += target_dt;
            if next < now {
                next = now + target_dt;
            }
        }

        if second.elapsed() >= Duration::from_secs(1) {
            let encoded = stats.encoded.max(1) as f64;
            let converted = stats.converted.max(1) as f64;
            log(&format!(
                "enc_fps={} cap_fps={} avg_encode_ms={:.2} max_encode_ms={:.2} \
                 avg_conversion_ms={:.2} max_conversion_ms={:.2} kbps={} capture_new={} \
                 capture_empty={} timeout={} cursor_only={} pointer_updates={} \
                 encode_submitted={} encode_skipped_no_new={} capture_superseded={} \
                 backend=media-foundation-hw",
                stats.encoded,
                stats.captured,
                stats.encode_ms_sum / encoded,
                stats.encode_ms_max,
                stats.convert_ms_sum / converted,
                stats.convert_ms_max,
                stats.bytes * 8 / 1000,
                stats.captured,
                stats.empty_polls,
                dbg.1,
                dbg.2,
                source.take_pointer_updates(),
                stats.encoded,
                stats.skipped_no_new,
                source.take_raw_frames_superseded(),
            ));
            stats = SecondStats::default();
            dbg = (0, 0, 0);
            second = Instant::now();
        }
    }
    log("MF hardware: control closed; releasing the encoder and capture");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics};

    #[test]
    fn nv12_colour_space_follows_the_contract() {
        assert_eq!(
            nv12_color_space(ColorSpec::legacy(false)),
            Some(DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709)
        );
        assert_eq!(
            nv12_color_space(ColorSpec {
                range: ColorRange::Full,
                transfer: TransferCharacteristics::Srgb,
                ..ColorSpec::legacy(false)
            }),
            Some(DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709)
        );
        assert_eq!(
            nv12_color_space(ColorSpec {
                matrix: ColorMatrix::Bt601,
                ..ColorSpec::legacy(false)
            }),
            Some(DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601)
        );
        assert_eq!(
            nv12_color_space(ColorSpec {
                primaries: ColorPrimaries::Bt2020,
                ..ColorSpec::legacy(false)
            }),
            None
        );
    }

    #[test]
    fn synthetic_frames_are_opaque_and_move() {
        let first = synthetic_bgra(64, 32, 0);
        let second = synthetic_bgra(64, 32, 1);
        assert_eq!(first.len(), 64 * 32 * 4);
        assert!(first.chunks_exact(4).all(|pixel| pixel[3] == 255));
        assert_ne!(first, second);
    }
}
