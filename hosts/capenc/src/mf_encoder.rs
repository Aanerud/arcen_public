// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Media Foundation hardware encoder (Alpha): a GPU vendor's encoder MFT.
//!
//! Intel (Quick Sync), AMD (AMF) and other vendors register their hardware
//! H.264/HEVC encoders as asynchronous Media Foundation transforms. This
//! module drives one of them:
//!
//! - enumerates `MFT_ENUM_FLAG_HARDWARE` encoders **bound to the capture
//!   adapter** (`MFTEnum2` + `MFT_ENUM_ADAPTER_LUID`), so a second GPU's
//!   encoder is never handed another adapter's textures;
//! - shares the capture's D3D11 device through an `IMFDXGIDeviceManager`, so
//!   NV12 textures converted on the GPU reach the encoder without a host copy;
//! - unlocks and drives the transform through its event model
//!   (`METransformNeedInput` / `METransformHaveOutput`);
//! - sets low-latency mode, CBR with a VBV buffer, no B-frames, a GOP and
//!   on-demand IDR (`CODECAPI_AVEncVideoForceKeyFrame`), and slices where the
//!   encoder exposes them.
//!
//! Nothing here decides whether the encoder may serve a session: a Pier
//! admits it only after a real encode whose SPS the shared
//! `arcen_media::video::prove_encoded_access_unit` accepts.
//!
//! VMware SVGA and any adapter without a hardware encoder MFT enumerate
//! nothing and fail with `HardwareUnavailable`; the Pier then falls through to
//! OpenH264. The lab-only environment variable `ARCEN_LAB_MF_HW_ENUM=none`
//! makes enumeration report nothing, to prove that fallback on a host that
//! does have an MFT.

use std::ptr;
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, PWSTR, VARIANT};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, LUID};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Multithread, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main, eAVEncH265VProfile_Main_420_8,
    CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncSliceControlMode, CODECAPI_AVEncSliceControlSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer,
    IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator,
    IMFMediaType, IMFSample, IMFShutdown, IMFTransform, METransformHaveOutput,
    METransformNeedInput, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFMediaType_Video, MFNominalRange, MFNominalRange_0_255,
    MFNominalRange_16_235, MFShutdown, MFStartup, MFTEnum2, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransFunc_sRGB, MFVideoTransferFunction, MFVideoTransferMatrix,
    MFVideoTransferMatrix_BT601, MFVideoTransferMatrix_BT709, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_LEVEL, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

use arcen_media::annexb::{
    access_unit_is_keyframe, annex_b_parameter_sets, length_prefixed_to_annex_b,
    parse_avc_decoder_config, prepend_annex_b_units, starts_with_start_code, NalCodec,
};
use arcen_media::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
    VideoCodec,
};

use crate::{log, ColorSpec};

/// Lab-only: `none` makes enumeration report no hardware encoder.
pub(crate) const LAB_ENUM_ENV: &str = "ARCEN_LAB_MF_HW_ENUM";

/// How long one frame may wait for the transform to ask for input.
const INPUT_WAIT: Duration = Duration::from_millis(250);
/// How long a submitted frame may wait for its access unit before the loop
/// moves on (the output is then collected on a later call). Low-latency mode
/// returns one access unit per input, well inside this.
const OUTPUT_WAIT: Duration = Duration::from_millis(100);
const EVENT_POLL: Duration = Duration::from_micros(250);

pub(crate) struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: VideoCodec,
    pub average_bitrate_bps: u32,
    pub vbv_buffer_bits: u32,
    /// Frames between scheduled IDRs; `0` asks for no scheduled IDR (the
    /// longest GOP the encoder takes). On-demand IDRs come from
    /// [`Encoder::submit`].
    pub gop_frames: u32,
    pub color: ColorSpec,
}

/// One hardware encoder MFT bound to an adapter.
pub(crate) struct HardwareMft {
    activate: IMFActivate,
    pub name: String,
    pub vendor: String,
}

/// RAII: COM (MTA) and Media Foundation for this thread.
pub(crate) struct MfRuntime {
    _private: (),
}

impl MfRuntime {
    pub(crate) fn start() -> windows::core::Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            if let Err(error) = MFStartup(MF_VERSION, 0) {
                CoUninitialize();
                return Err(error);
            }
        }
        Ok(Self { _private: () })
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

fn subtype(codec: VideoCodec) -> windows::core::Result<GUID> {
    match codec {
        VideoCodec::H264 => Ok(MFVideoFormat_H264),
        VideoCodec::H265 => Ok(MFVideoFormat_HEVC),
        other => Err(windows::core::Error::new(
            E_INVALIDARG,
            format!(
                "Media Foundation hardware encodes h264 or h265, not {}",
                other.token()
            ),
        )),
    }
}

const fn nal_codec(codec: VideoCodec) -> NalCodec {
    match codec {
        VideoCodec::H265 => NalCodec::H265,
        _ => NalCodec::H264,
    }
}

/// Whether the lab switch asks enumeration to report nothing.
pub(crate) fn lab_hides_hardware(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("none"))
}

/// Hardware encoder MFTs for `codec` with NV12 input that belong to the
/// adapter `adapter_luid`, best first (`MFT_ENUM_FLAG_SORTANDFILTER`).
/// Needs a started [`MfRuntime`].
pub(crate) unsafe fn enumerate(
    adapter_luid: LUID,
    codec: VideoCodec,
) -> windows::core::Result<Vec<HardwareMft>> {
    if lab_hides_hardware(std::env::var(LAB_ENUM_ENV).ok().as_deref()) {
        log(&format!(
            "MF hardware: {LAB_ENUM_ENV}=none hides every hardware encoder MFT (lab switch)"
        ));
        return Ok(Vec::new());
    }
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec)?,
    };
    let mut attributes: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attributes, 1)?;
    let attributes = attributes.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    let mut luid = [0u8; 8];
    luid[..4].copy_from_slice(&adapter_luid.LowPart.to_le_bytes());
    luid[4..].copy_from_slice(&adapter_luid.HighPart.to_le_bytes());
    attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid)?;

    let mut array: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    MFTEnum2(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &attributes,
        &mut array,
        &mut count,
    )?;
    let mut found = Vec::with_capacity(count as usize);
    if !array.is_null() {
        // Move every activate out of the CoTask array before freeing it, so
        // each reference is released exactly once.
        for index in 0..count as usize {
            if let Some(activate) = ptr::read(array.add(index)) {
                let name = allocated_string(&activate, &MFT_FRIENDLY_NAME_Attribute)
                    .unwrap_or_else(|| "unnamed hardware encoder MFT".to_string());
                let vendor = allocated_string(&activate, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
                    .unwrap_or_default();
                found.push(HardwareMft {
                    activate,
                    name,
                    vendor,
                });
            }
        }
        CoTaskMemFree(Some(array as *const _));
    }
    Ok(found)
}

/// Every hardware encoder MFT on every DXGI adapter, for
/// `arcen-pier diagnose-host`: (adapter description, codec, MFT name, vendor).
pub(crate) fn inventory() -> Result<Vec<(String, VideoCodec, String, String)>, String> {
    let _runtime = MfRuntime::start().map_err(|error| format!("Media Foundation: {error}"))?;
    let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory1 =
        unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory1() }
            .map_err(|error| format!("DXGI: {error}"))?;
    let mut found = Vec::new();
    let mut index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        let end = desc
            .Description
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..end]);
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            for mft in unsafe { enumerate(desc.AdapterLuid, codec) }.unwrap_or_default() {
                found.push((name.clone(), codec, mft.name.clone(), mft.vendor.clone()));
            }
        }
    }
    Ok(found)
}

unsafe fn allocated_string(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut value = PWSTR::null();
    let mut length = 0u32;
    attributes
        .GetAllocatedString(key, &mut value, &mut length)
        .ok()?;
    let text = value.to_string().ok();
    CoTaskMemFree(Some(value.0 as *const _));
    text
}

pub(crate) struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    activate: IMFActivate,
    _manager: IMFDXGIDeviceManager,
    input_stream: u32,
    output_stream: u32,
    codec: VideoCodec,
    fps: u32,
    need_input: u32,
    next_pts: i64,
    output_provides_samples: bool,
    output_buffer_size: u32,
    parameter_sets: Vec<Vec<u8>>,
    pub name: String,
    pub vendor: String,
    /// The `ICodecAPI` properties the encoder accepted, for logs.
    pub applied: Vec<&'static str>,
}

impl Encoder {
    /// Opens the best hardware encoder MFT on `adapter_luid` that accepts
    /// `cfg`, trying each enumerated one in order. Needs a started
    /// [`MfRuntime`] on this thread for the encoder's whole life.
    pub(crate) unsafe fn open(
        device: &ID3D11Device,
        adapter_luid: LUID,
        cfg: &Config,
    ) -> Result<Self, OpenError> {
        validate_color(cfg.color).map_err(OpenError::Unsupported)?;
        subtype(cfg.codec).map_err(OpenError::Unsupported)?;
        if cfg.width == 0
            || cfg.height == 0
            || !cfg.width.is_multiple_of(2)
            || !cfg.height.is_multiple_of(2)
        {
            return Err(OpenError::Unsupported(windows::core::Error::new(
                E_INVALIDARG,
                format!("{}x{} is not an even 4:2:0 picture", cfg.width, cfg.height),
            )));
        }
        let candidates = enumerate(adapter_luid, cfg.codec).map_err(OpenError::Failed)?;
        if candidates.is_empty() {
            return Err(OpenError::NoHardware);
        }
        // The transform calls the device from its own threads.
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            let _ = multithread.SetMultithreadProtected(true);
        }
        let mut last = None;
        for candidate in candidates {
            match Self::open_one(device, candidate, cfg) {
                Ok(encoder) => return Ok(encoder),
                Err((name, error)) => {
                    log(&format!(
                        "MF hardware: encoder MFT {name:?} refused {} {}x{}: {error}",
                        cfg.codec.token(),
                        cfg.width,
                        cfg.height
                    ));
                    last = Some(error);
                }
            }
        }
        Err(OpenError::Failed(
            last.unwrap_or_else(|| windows::core::Error::from(E_FAIL)),
        ))
    }

    unsafe fn open_one(
        device: &ID3D11Device,
        candidate: HardwareMft,
        cfg: &Config,
    ) -> Result<Self, (String, windows::core::Error)> {
        let name = candidate.name.clone();
        let fail = |error: windows::core::Error| (name.clone(), error);
        let activate = candidate.activate;
        let opened = (|| -> windows::core::Result<Self> {
            let transform: IMFTransform = activate.ActivateObject()?;
            let attributes = transform.GetAttributes()?;
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(windows::core::Error::new(
                    E_FAIL,
                    "hardware MFT is not asynchronous",
                ));
            }
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err(windows::core::Error::new(
                    E_FAIL,
                    "hardware MFT does not take Direct3D 11 surfaces",
                ));
            }
            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            manager.ResetDevice(device, token)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            let (input_stream, output_stream) = stream_ids(&transform);
            let codec_api = transform.cast::<ICodecAPI>().ok();
            let mut applied = Vec::new();
            if let Some(api) = codec_api.as_ref() {
                configure_codec_api(api, cfg, &mut applied);
            }
            let output_type = build_output_type(cfg)?;
            transform.SetOutputType(output_stream, &output_type, 0)?;
            set_input_type(&transform, input_stream, cfg)?;

            let events: IMFMediaEventGenerator = transform.cast()?;
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let parameter_sets = transform
                .GetOutputCurrentType(output_stream)
                .map(|current| extract_parameter_sets(&current, cfg.codec))
                .unwrap_or_default();
            Ok(Self {
                transform,
                events,
                codec_api,
                activate: activate.clone(),
                _manager: manager,
                input_stream,
                output_stream,
                codec: cfg.codec,
                fps: cfg.fps.max(1),
                need_input: 0,
                next_pts: 0,
                output_provides_samples: false,
                output_buffer_size: 0,
                parameter_sets,
                name: candidate.name.clone(),
                vendor: candidate.vendor.clone(),
                applied,
            })
        })();
        match opened {
            Ok(mut encoder) => {
                encoder.refresh_output_info().map_err(fail)?;
                log(&format!(
                    "MF hardware encoder ready: {:?} ({}) {} {}x{} @ {} fps, {} bps CBR, \
                     vbv={} bits, gop={}, codecapi=[{}]",
                    encoder.name,
                    if encoder.vendor.is_empty() {
                        "vendor ?"
                    } else {
                        &encoder.vendor
                    },
                    cfg.codec.token(),
                    cfg.width,
                    cfg.height,
                    cfg.fps,
                    cfg.average_bitrate_bps,
                    cfg.vbv_buffer_bits,
                    cfg.gop_frames,
                    encoder.applied.join(",")
                ));
                Ok(encoder)
            }
            Err(error) => {
                let _ = activate.ShutdownObject();
                Err(fail(error))
            }
        }
    }

    unsafe fn refresh_output_info(&mut self) -> windows::core::Result<()> {
        let info = self.transform.GetOutputStreamInfo(self.output_stream)?;
        self.output_provides_samples =
            info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        self.output_buffer_size = info.cbSize.max(1);
        Ok(())
    }

    /// Submits one NV12 texture (from the device the encoder was opened on)
    /// and appends every Annex-B access unit the encoder has finished to
    /// `out`. `force_idr` makes this picture an IDR. The texture must not be
    /// written again until its access unit has come back.
    pub(crate) unsafe fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        force_idr: bool,
        out: &mut Vec<Vec<u8>>,
    ) -> windows::core::Result<()> {
        if !self.wait_for(out, INPUT_WAIT, |encoder, _| encoder.need_input > 0)? {
            return Err(windows::core::Error::new(
                E_FAIL,
                format!(
                    "hardware encoder asked for no input within {} ms",
                    INPUT_WAIT.as_millis()
                ),
            ));
        }
        if force_idr {
            if let Some(api) = self.codec_api.as_ref() {
                api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32))?;
            }
        }
        let buffer: IMFMediaBuffer =
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
            buffer.SetCurrentLength(two_d.GetContiguousLength()?)?;
        }
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        let duration = 10_000_000_i64 / i64::from(self.fps);
        sample.SetSampleTime(self.next_pts)?;
        sample.SetSampleDuration(duration)?;
        self.next_pts = self.next_pts.saturating_add(duration);
        self.transform.ProcessInput(self.input_stream, &sample, 0)?;
        self.need_input -= 1;
        let produced = out.len();
        self.wait_for(out, OUTPUT_WAIT, move |_, out| out.len() > produced)?;
        Ok(())
    }

    /// Pumps events until `done` holds or `budget` runs out; `false` on
    /// timeout.
    unsafe fn wait_for(
        &mut self,
        out: &mut Vec<Vec<u8>>,
        budget: Duration,
        done: impl Fn(&Self, &Vec<Vec<u8>>) -> bool,
    ) -> windows::core::Result<bool> {
        let deadline = Instant::now() + budget;
        loop {
            self.pump(out)?;
            if done(self, out) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(EVENT_POLL);
        }
    }

    unsafe fn pump(&mut self, out: &mut Vec<Vec<u8>>) -> windows::core::Result<()> {
        loop {
            let event = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => return Err(error),
            };
            event.GetStatus()?.ok()?;
            let kind = event.GetType()?;
            if kind == METransformNeedInput.0 as u32 {
                self.need_input = self.need_input.saturating_add(1);
            } else if kind == METransformHaveOutput.0 as u32 {
                self.collect_output(out)?;
            }
        }
    }

    unsafe fn collect_output(&mut self, out: &mut Vec<Vec<u8>>) -> windows::core::Result<()> {
        let sample = if self.output_provides_samples {
            None
        } else {
            let buffer = windows::Win32::Media::MediaFoundation::MFCreateMemoryBuffer(
                self.output_buffer_size,
            )?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            Some(sample)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_stream,
            pSample: std::mem::ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
        // The caller owns both COM fields after ProcessOutput, success or not.
        let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
        drop(std::mem::ManuallyDrop::take(&mut buffers[0].pEvents));
        match result {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let negotiated = self
                    .transform
                    .GetOutputAvailableType(self.output_stream, 0)?;
                self.transform
                    .SetOutputType(self.output_stream, &negotiated, 0)?;
                self.parameter_sets = extract_parameter_sets(&negotiated, self.codec);
                self.refresh_output_info()?;
                log("MF hardware: output stream changed; renegotiated the output type");
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        let Some(sample) = sample else {
            return Ok(());
        };
        let mut bytes = Vec::new();
        copy_sample_bytes(&sample, &mut bytes)?;
        if bytes.is_empty() {
            return Ok(());
        }
        let mut access_unit = if starts_with_start_code(&bytes) {
            bytes
        } else {
            let mut converted = Vec::with_capacity(bytes.len() + 64);
            length_prefixed_to_annex_b(&bytes, &mut converted).map_err(|error| {
                windows::core::Error::new(E_FAIL, format!("AVCC->Annex-B: {error}"))
            })?;
            converted
        };
        let codec = nal_codec(self.codec);
        let carried = annex_b_parameter_sets(&access_unit, codec);
        if carried.is_empty() {
            if access_unit_is_keyframe(&access_unit, codec) && !self.parameter_sets.is_empty() {
                prepend_annex_b_units(&self.parameter_sets, &mut access_unit);
            }
        } else {
            self.parameter_sets = carried.into_iter().map(<[u8]>::to_vec).collect();
        }
        out.push(access_unit);
        Ok(())
    }

    pub(crate) unsafe fn reconfigure_bitrate(&mut self, bps: u32) -> windows::core::Result<()> {
        let api = self
            .codec_api
            .as_ref()
            .ok_or_else(|| windows::core::Error::new(E_FAIL, "encoder has no ICodecAPI"))?;
        api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps.max(1)))
    }

    pub(crate) fn set_frame_rate(&mut self, fps: u32) {
        self.fps = fps.max(1);
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// Why no hardware encoder could be opened.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// No hardware encoder MFT is registered for this adapter and codec.
    NoHardware,
    /// The request is outside what this path encodes (colour, codec, size).
    Unsupported(windows::core::Error),
    /// Every candidate refused, or Media Foundation failed.
    Failed(windows::core::Error),
}

impl OpenError {
    pub(crate) const fn unavailable_reason(&self) -> arcen_media::video::BackendUnavailableReason {
        match self {
            Self::NoHardware | Self::Failed(_) => {
                arcen_media::video::BackendUnavailableReason::HardwareUnavailable
            }
            Self::Unsupported(_) => {
                arcen_media::video::BackendUnavailableReason::UnsupportedConfiguration
            }
        }
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHardware => {
                formatter.write_str("no hardware encoder MFT is registered for this adapter")
            }
            Self::Unsupported(error) => write!(formatter, "unsupported request: {error}"),
            Self::Failed(error) => write!(formatter, "{error}"),
        }
    }
}

unsafe fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let mut input = [0u32];
    let mut output = [0u32];
    // E_NOTIMPL means fixed stream 0 for both.
    if transform.GetStreamIDs(&mut input, &mut output).is_err() {
        return (0, 0);
    }
    (input[0], output[0])
}

/// Best effort: a vendor MFT may not expose every property. What it took is
/// recorded in `applied`; low latency, CBR and the bitrate are logged when
/// refused because they shape every frame.
unsafe fn configure_codec_api(api: &ICodecAPI, cfg: &Config, applied: &mut Vec<&'static str>) {
    let gop = if cfg.gop_frames == 0 {
        u32::from(u16::MAX)
    } else {
        cfg.gop_frames
    };
    let mut set = |property: &GUID, value: VARIANT, name: &'static str, essential: bool| {
        let result = api
            .IsSupported(property)
            .and_then(|()| api.SetValue(property, &value));
        match result {
            Ok(()) => applied.push(name),
            Err(error) if essential => {
                log(&format!("MF hardware: encoder refused {name}: {error}"));
            }
            Err(_) => {}
        }
    };
    set(
        &CODECAPI_AVLowLatencyMode,
        VARIANT::from(true),
        "low-latency",
        true,
    );
    set(
        &CODECAPI_AVEncCommonRateControlMode,
        VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
        "cbr",
        true,
    );
    set(
        &CODECAPI_AVEncCommonMeanBitRate,
        VARIANT::from(cfg.average_bitrate_bps.max(1)),
        "bitrate",
        true,
    );
    set(
        &CODECAPI_AVEncCommonBufferSize,
        VARIANT::from(cfg.vbv_buffer_bits.max(1)),
        "vbv",
        false,
    );
    set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(gop), "gop", false);
    set(
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        VARIANT::from(0u32),
        "no-b-frames",
        false,
    );
    if let Some(rows) = slice_rows(cfg.height) {
        // Mode 2: slice size counted in macroblock rows.
        set(
            &CODECAPI_AVEncSliceControlMode,
            VARIANT::from(2u32),
            "slice-mode-rows",
            false,
        );
        set(
            &CODECAPI_AVEncSliceControlSize,
            VARIANT::from(rows),
            "slice-rows",
            false,
        );
    }
}

/// Macroblock rows per slice for four slices, or `None` for a picture too
/// short to split.
pub(crate) fn slice_rows(height: u32) -> Option<u32> {
    let rows = height.div_ceil(16);
    (rows >= 8).then(|| rows.div_ceil(4))
}

unsafe fn build_output_type(cfg: &Config) -> windows::core::Result<IMFMediaType> {
    let media_type = MFCreateMediaType()?;
    media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    media_type.SetGUID(&MF_MT_SUBTYPE, &subtype(cfg.codec)?)?;
    media_type.SetUINT32(&MF_MT_AVG_BITRATE, cfg.average_bitrate_bps.max(1))?;
    set_ratio(&media_type, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
    set_ratio(&media_type, &MF_MT_FRAME_RATE, cfg.fps.max(1), 1)?;
    set_ratio(&media_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    match cfg.codec {
        VideoCodec::H264 => {
            media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            media_type.SetUINT32(
                &MF_MT_MPEG2_LEVEL,
                pick_h264_level(cfg.width, cfg.height, cfg.fps),
            )?;
        }
        _ => {
            media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
        }
    }
    set_color_attributes(&media_type, cfg.color)?;
    Ok(media_type)
}

unsafe fn set_input_type(
    transform: &IMFTransform,
    stream: u32,
    cfg: &Config,
) -> windows::core::Result<()> {
    let media_type = MFCreateMediaType()?;
    media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    set_ratio(&media_type, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
    set_ratio(&media_type, &MF_MT_FRAME_RATE, cfg.fps.max(1), 1)?;
    set_ratio(&media_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    set_color_attributes(&media_type, cfg.color)?;
    let direct = transform.SetInputType(stream, &media_type, 0);
    if direct.is_ok() {
        return Ok(());
    }
    // Some encoders accept only a type they offer: take their NV12 one and
    // give it our size and rate.
    let mut index = 0;
    while let Ok(available) = transform.GetInputAvailableType(stream, index) {
        index += 1;
        if available.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
            continue;
        }
        set_ratio(&available, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
        set_ratio(&available, &MF_MT_FRAME_RATE, cfg.fps.max(1), 1)?;
        return transform.SetInputType(stream, &available, 0);
    }
    direct
}

unsafe fn set_ratio(
    media_type: &IMFMediaType,
    key: &GUID,
    high: u32,
    low: u32,
) -> windows::core::Result<()> {
    media_type.SetUINT64(key, (u64::from(high) << 32) | u64::from(low))
}

/// eAVEncH264VLevel for the smallest level that fits the picture and rate.
pub(crate) fn pick_h264_level(width: u32, height: u32, fps: u32) -> u32 {
    let macroblocks = width.div_ceil(16) * height.div_ceil(16);
    let per_second = macroblocks.saturating_mul(fps.max(1));
    // (level, max macroblocks per frame, max macroblocks per second)
    const LEVELS: &[(u32, u32, u32)] = &[
        (31, 3_600, 108_000),
        (32, 5_120, 216_000),
        (40, 8_192, 245_760),
        (42, 8_704, 522_240),
        (50, 22_080, 589_824),
        (51, 36_864, 983_040),
        (52, 36_864, 2_073_600),
    ];
    LEVELS
        .iter()
        .find(|(_, frame, rate)| macroblocks <= *frame && per_second <= *rate)
        .map_or(52, |(level, _, _)| *level)
}

unsafe fn set_color_attributes(
    media_type: &IMFMediaType,
    color: ColorSpec,
) -> windows::core::Result<()> {
    media_type.SetUINT32(
        &MF_MT_VIDEO_NOMINAL_RANGE,
        nominal_range(color.range).0 as u32,
    )?;
    media_type.SetUINT32(&MF_MT_YUV_MATRIX, yuv_matrix(color.matrix)?.0 as u32)?;
    media_type.SetUINT32(
        &MF_MT_VIDEO_PRIMARIES,
        video_primaries(color.primaries)?.0 as u32,
    )?;
    media_type.SetUINT32(
        &MF_MT_TRANSFER_FUNCTION,
        transfer_function(color.transfer)?.0 as u32,
    )?;
    Ok(())
}

/// Rejects a colour contract this 8-bit 4:2:0 path cannot make truthful
/// before any encoder is touched.
pub(crate) fn validate_color(color: ColorSpec) -> windows::core::Result<()> {
    if color.chroma != ChromaSubsampling::Yuv420 {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            format!(
                "Media Foundation hardware encodes 4:2:0 (NV12) only; requested {:?}",
                color.chroma
            ),
        ));
    }
    if color.bit_depth != BitDepth::Eight {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            format!(
                "Media Foundation hardware encodes 8-bit only; requested {}-bit",
                color.bit_depth.bits()
            ),
        ));
    }
    yuv_matrix(color.matrix)?;
    video_primaries(color.primaries)?;
    transfer_function(color.transfer)?;
    Ok(())
}

const fn nominal_range(range: ColorRange) -> MFNominalRange {
    match range {
        ColorRange::Limited => MFNominalRange_16_235,
        ColorRange::Full => MFNominalRange_0_255,
    }
}

fn yuv_matrix(matrix: ColorMatrix) -> windows::core::Result<MFVideoTransferMatrix> {
    match matrix {
        ColorMatrix::Bt709 => Ok(MFVideoTransferMatrix_BT709),
        ColorMatrix::Bt601 => Ok(MFVideoTransferMatrix_BT601),
        ColorMatrix::Identity | ColorMatrix::Bt2020Ncl => Err(windows::core::Error::new(
            E_INVALIDARG,
            format!("Media Foundation hardware cannot signal matrix {matrix:?}"),
        )),
    }
}

fn video_primaries(primaries: ColorPrimaries) -> windows::core::Result<MFVideoPrimaries> {
    match primaries {
        ColorPrimaries::Bt709 => Ok(MFVideoPrimaries_BT709),
        ColorPrimaries::Bt2020 | ColorPrimaries::DisplayP3 => Err(windows::core::Error::new(
            E_INVALIDARG,
            format!("Media Foundation hardware cannot signal primaries {primaries:?}"),
        )),
    }
}

fn transfer_function(
    transfer: TransferCharacteristics,
) -> windows::core::Result<MFVideoTransferFunction> {
    match transfer {
        TransferCharacteristics::Bt709 => Ok(MFVideoTransFunc_709),
        TransferCharacteristics::Srgb => Ok(MFVideoTransFunc_sRGB),
        TransferCharacteristics::Pq | TransferCharacteristics::Hlg => {
            Err(windows::core::Error::new(
                E_INVALIDARG,
                format!("Media Foundation hardware cannot signal transfer {transfer:?}"),
            ))
        }
    }
}

unsafe fn extract_parameter_sets(media_type: &IMFMediaType, codec: VideoCodec) -> Vec<Vec<u8>> {
    let Ok(size) = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
        return Vec::new();
    };
    if size == 0 {
        return Vec::new();
    }
    let mut blob = vec![0u8; size as usize];
    if media_type
        .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
        .is_err()
    {
        return Vec::new();
    }
    if codec == VideoCodec::H264 {
        if let Some(units) = parse_avc_decoder_config(&blob) {
            return units.into_iter().map(<[u8]>::to_vec).collect();
        }
    }
    annex_b_parameter_sets(&blob, nal_codec(codec))
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect()
}

unsafe fn copy_sample_bytes(sample: &IMFSample, out: &mut Vec<u8>) -> windows::core::Result<()> {
    for index in 0..sample.GetBufferCount()? {
        let buffer = sample.GetBufferByIndex(index)?;
        let mut data: *mut u8 = ptr::null_mut();
        let mut length = 0u32;
        buffer.Lock(&mut data, None, Some(&mut length))?;
        if !data.is_null() {
            out.extend_from_slice(std::slice::from_raw_parts(data, length as usize));
        }
        buffer.Unlock()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lab_switch_hides_hardware_only_when_set_to_none() {
        assert!(lab_hides_hardware(Some("none")));
        assert!(lab_hides_hardware(Some(" NONE ")));
        assert!(!lab_hides_hardware(Some("")));
        assert!(!lab_hides_hardware(Some("all")));
        assert!(!lab_hides_hardware(None));
    }

    #[test]
    fn slices_split_a_desktop_into_four_and_leave_tiny_pictures_whole() {
        assert_eq!(slice_rows(1080), Some(17));
        assert_eq!(slice_rows(2160), Some(34));
        assert_eq!(slice_rows(64), None);
    }

    #[test]
    fn h264_level_fits_the_picture() {
        assert_eq!(pick_h264_level(1280, 720, 30), 31);
        assert_eq!(pick_h264_level(1920, 1080, 60), 42);
        assert_eq!(pick_h264_level(3840, 2160, 60), 52);
    }

    #[test]
    fn only_eight_bit_420_sdr_is_accepted() {
        assert!(validate_color(ColorSpec::legacy(false)).is_ok());
        assert!(validate_color(ColorSpec {
            range: ColorRange::Full,
            transfer: TransferCharacteristics::Srgb,
            ..ColorSpec::legacy(false)
        })
        .is_ok());
        assert!(validate_color(ColorSpec::legacy(true)).is_err());
        assert!(validate_color(ColorSpec {
            bit_depth: BitDepth::Ten,
            ..ColorSpec::legacy(false)
        })
        .is_err());
        assert!(validate_color(ColorSpec {
            transfer: TransferCharacteristics::Pq,
            ..ColorSpec::legacy(false)
        })
        .is_err());
        assert!(validate_color(ColorSpec {
            primaries: ColorPrimaries::Bt2020,
            ..ColorSpec::legacy(false)
        })
        .is_err());
        assert!(validate_color(ColorSpec {
            matrix: ColorMatrix::Identity,
            ..ColorSpec::legacy(false)
        })
        .is_err());
    }

    #[test]
    #[ignore = "needs a GPU with a hardware encoder MFT"]
    fn enumerates_the_default_adapters_encoders() {
        let _runtime = MfRuntime::start().expect("MF");
        let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory1 =
            unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory1() }.expect("DXGI");
        let adapter = unsafe { factory.EnumAdapters1(0) }.expect("adapter 0");
        let luid = unsafe { adapter.GetDesc1() }.expect("desc").AdapterLuid;
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let found = unsafe { enumerate(luid, codec) }.expect("enumerate");
            for mft in &found {
                eprintln!("{}: {} ({})", codec.token(), mft.name, mft.vendor);
            }
        }
    }
}
