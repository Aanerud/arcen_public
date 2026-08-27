// Media Foundation software H.264 encoder — the VMware-SVGA / no-NVENC path.
//
// Pipeline:
//   caller supplies (width, height, fps, bitrate_kbps, profile, gop_frames)
//   -> instantiate CLSID_CMSH264EncoderMFT via MFTEnumEx
//   -> SetOutputType (H264 with the requested bitrate/profile/framerate)
//   -> SetInputType  (NV12 with the same size + framerate)
//   -> ICodecAPI: CBR + AVLowLatencyMode + GOP + worker threads
//   -> ProcessMessage(BEGIN_STREAMING) / ProcessMessage(START_OF_STREAM)
//   per frame:
//     reuse a system-memory IMFSample/IMFMediaBuffer after the prior drain
//     lock, memcpy Y then UV planes from caller, unlock, SetCurrentLength
//     ProcessInput
//     drain ProcessOutput through a reused caller-owned sample -> AVCC bytes
//     convert to Annex-B, prepend SPS/PPS on IDR
//
// The SW MFT does not consume DXGI surfaces; we pass NV12 in system memory
// which the caller has already CPU-converted from BGRA. On VMware SVGA that
// is the only viable path: no HW MFT, no zero-copy, no D3D11 sample types.
//
// COM apartment: MTA. MF requires either MTA or STA; we run capenc's encode
// loop on a dedicated thread which we mark MTA once at Encoder construction.

use std::mem::size_of;
use std::ptr;

use windows::core::{Interface, GUID, PCWSTR, VARIANT};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, ICodecAPI, IMFAttributes, IMFMediaBuffer, IMFMediaType,
    IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFNominalRange, MFNominalRange_0_255, MFNominalRange_16_235, MFShutdown,
    MFStartup, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFVideoPrimaries, MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransFunc_sRGB,
    MFVideoTransferFunction, MFVideoTransferMatrix, MFVideoTransferMatrix_BT601,
    MFVideoTransferMatrix_BT709, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_FLAG_TRANSCODE_ONLY, MFT_INPUT_STREAM_HOLDS_BUFFERS,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFT_SET_TYPE_TEST_ONLY, MF_E_INVALIDMEDIATYPE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_LEVEL,
    MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_TRANSFORM_ASYNC, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use arcen_media::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
};

use crate::annexb::{avcc_to_annexb, parse_avc_decoder_config, prepend_parameter_sets};
use crate::{log, ColorSpec};

pub(crate) struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub gop_frames: u32,
    pub profile: H264Profile,
    /// The negotiated colour contract this stream must make truthful in its
    /// bitstream signalling. The inbox SW H.264 MFT is 8-bit 4:2:0 only, so
    /// [`Encoder::new`] rejects anything [`validate_mf_color`] cannot map.
    pub color: ColorSpec,
}

#[derive(Copy, Clone)]
pub(crate) enum H264Profile {
    #[allow(dead_code)]
    Baseline,
    Main,
    #[allow(dead_code)]
    High,
}

impl H264Profile {
    fn as_eavencomm(self) -> u32 {
        // eAVEncH264VProfile_* values from codecapi.h
        match self {
            H264Profile::Baseline => 66,
            H264Profile::Main => 77,
            H264Profile::High => 100,
        }
    }
}

pub(crate) struct Encoder {
    transform: IMFTransform,
    codec_api: ICodecAPI,
    input_stream_id: u32,
    output_stream_id: u32,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    gop_frames: u32,
    profile: H264Profile,
    color: ColorSpec,
    frame_index: u64,
    parameter_sets: Vec<Vec<u8>>,
    pending_idr: bool,
    /// GetOutputStreamInfo().cbSize, cached because it only changes on a
    /// stream change; 0 = stale, re-query on next ProcessOutput.
    output_buffer_size: u32,
    /// Reused ProcessOutput drain scratch — cleared, not reallocated, per frame.
    out_scratch: Vec<u8>,
    input_reuse_enabled: bool,
    input_sample: Option<SampleSlot>,
    output_sample: Option<SampleSlot>,
    output_provides_samples: bool,
    pool_stats: SamplePoolStats,
    _com: ComGuard,
    _mf: MfGuard,
}

struct SampleSlot {
    sample: IMFSample,
    buffer: IMFMediaBuffer,
    capacity: u32,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct SamplePoolStats {
    pub input_allocations: u64,
    pub input_reuses: u64,
    pub output_allocations: u64,
    pub output_reuses: u64,
}

/// RAII: CoInitializeEx(MTA) on construction; CoUninitialize on drop.
struct ComGuard;
impl ComGuard {
    fn new() -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self)
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// RAII: MFStartup on construction; MFShutdown on drop.
struct MfGuard;
impl MfGuard {
    fn new() -> windows::core::Result<Self> {
        unsafe { MFStartup(MF_VERSION, 0)? };
        Ok(Self)
    }
}
impl Drop for MfGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

impl Encoder {
    pub(crate) fn new(cfg: &Config) -> windows::core::Result<Self> {
        // Fail fast on an unsignallable colour contract before COM/MF start
        // up at all — the inbox SW H.264 MFT is 8-bit 4:2:0 only, and a
        // stream tagged wrong is worse than a stream that fails to start.
        validate_mf_color(cfg.color)?;

        let com = ComGuard::new()?;
        let mf = MfGuard::new()?;

        let transform = unsafe { create_h264_encoder_mft()? };

        // Properties that affect stream structure (notably B-frame count) must
        // be configured before the output media type is committed.
        let codec_api: ICodecAPI = transform.cast()?;
        unsafe { configure_codec_api(&codec_api, cfg)? };

        // Configure output type FIRST — the SW H.264 MFT rejects SetInputType
        // until an output type is set, which is documented but surprising.
        let output_type = unsafe { build_output_type(cfg)? };
        unsafe { transform.SetOutputType(0, &output_type, 0)? };

        let input_type = unsafe { build_input_type(cfg)? };
        unsafe { transform.SetInputType(0, &input_type, 0)? };

        let mut input_info = unsafe { std::mem::zeroed() };
        unsafe { transform.GetInputStreamInfo(0, &mut input_info)? };
        let input_reuse_enabled = input_info.dwFlags & MFT_INPUT_STREAM_HOLDS_BUFFERS.0 as u32 == 0;

        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        // Pull SPS/PPS from the configured output type so we can prepend them
        // on every IDR access unit. Some Windows builds omit the attribute; we
        // fall back to scanning the first sample for NAL types 7/8.
        let parameter_sets = unsafe { extract_parameter_sets(&output_type) };

        log(&format!(
            "MF H.264 SW encoder ready: {}x{} @ {} fps, {} kbps CBR, profile={}, \
             input_sample_reuse={}, range={:?}, matrix={:?}, primaries={:?}, transfer={:?}",
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.bitrate_kbps,
            match cfg.profile {
                H264Profile::Baseline => "baseline",
                H264Profile::Main => "main",
                H264Profile::High => "high",
            },
            input_reuse_enabled,
            cfg.color.range,
            cfg.color.matrix,
            cfg.color.primaries,
            cfg.color.transfer,
        ));

        Ok(Self {
            transform,
            codec_api,
            input_stream_id: 0,
            output_stream_id: 0,
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
            gop_frames: cfg.gop_frames,
            profile: cfg.profile,
            color: cfg.color,
            frame_index: 0,
            parameter_sets,
            pending_idr: true,
            output_buffer_size: 0,
            out_scratch: Vec::new(),
            input_reuse_enabled,
            input_sample: None,
            output_sample: None,
            output_provides_samples: false,
            pool_stats: SamplePoolStats::default(),
            _com: com,
            _mf: mf,
        })
    }

    /// Feed one NV12 frame into the encoder and drain zero-or-more Annex-B
    /// access units. The caller is expected to provide pre-converted NV12
    /// (`bgra_to_nv12`) with the same width/height as configured.
    pub(crate) fn encode_frame(
        &mut self,
        y_plane: &[u8],
        y_stride: usize,
        uv_plane: &[u8],
        uv_stride: usize,
        force_idr: bool,
    ) -> windows::core::Result<Option<Vec<u8>>> {
        if force_idr {
            self.pending_idr = true;
        }
        if self.pending_idr {
            unsafe {
                set_codec_u32(
                    &self.codec_api,
                    &CODECAPI_AVEncVideoForceKeyFrame,
                    1,
                    "AVEncVideoForceKeyFrame",
                )?
            };
        }

        let sample = unsafe { self.prepare_input_sample(y_plane, y_stride, uv_plane, uv_stride) }
            .map_err(|e| {
            windows::core::Error::new(e.code(), format!("build_sample: {}", e.message()))
        })?;

        unsafe {
            self.transform
                .ProcessInput(self.input_stream_id, &sample, 0)
                .map_err(|e| {
                    windows::core::Error::new(e.code(), format!("ProcessInput: {}", e.message()))
                })?
        };

        self.frame_index += 1;

        let mut out_bytes = std::mem::take(&mut self.out_scratch);
        out_bytes.clear();
        let mut is_key = false;
        loop {
            match unsafe { self.process_output(&mut out_bytes, &mut is_key) }.map_err(|e| {
                windows::core::Error::new(e.code(), format!("process_output: {}", e.message()))
            })? {
                ProcessOutputStatus::Sample => continue,
                ProcessOutputStatus::NeedMoreInput => break,
                ProcessOutputStatus::StreamChange => {
                    // Re-apply the same output type after a stream change so we
                    // keep the bitrate/framerate we asked for.
                    let cfg = self.current_config();
                    unsafe { configure_codec_api(&self.codec_api, &cfg)? };
                    let output_type = unsafe { build_output_type(&cfg)? };
                    unsafe { self.transform.SetOutputType(0, &output_type, 0)? };
                    self.parameter_sets = unsafe { extract_parameter_sets(&output_type) };
                    self.output_buffer_size = 0;
                    self.output_sample = None;
                }
            }
        }

        if out_bytes.is_empty() {
            self.out_scratch = out_bytes;
            return Ok(None);
        }

        // MF's H.264 output framing is byte-stream-dependent: some builds emit
        // AVCC (4-byte length + NALU), others emit Annex-B (00 00 00 01 + NALU)
        // — the MS SW MFT on Win11 26200 defaults to Annex-B. Detect which and
        // convert as needed so downstream always sees Annex-B.
        let mut au = Vec::with_capacity(out_bytes.len() + 128);
        if starts_with_annexb_start_code(&out_bytes) {
            au.extend_from_slice(&out_bytes);
        } else {
            let converted = avcc_to_annexb(&out_bytes, &mut au)
                .map_err(|e| windows::core::Error::new(E_FAIL, format!("AVCC->Annex-B: {e}")));
            if let Err(error) = converted {
                self.out_scratch = out_bytes;
                return Err(error);
            }
        }
        self.out_scratch = out_bytes;

        if is_key {
            if self.parameter_sets.is_empty() {
                // Scan the AU for NAL types 7 (SPS) and 8 (PPS) so we cache them
                // for the next IDR when the MFT didn't populate the attribute.
                self.parameter_sets = scan_parameter_sets(&au);
            }
            prepend_parameter_sets(&self.parameter_sets, &mut au);
        }

        Ok(Some(au))
    }

    fn current_config(&self) -> Config {
        Config {
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            gop_frames: self.gop_frames,
            profile: self.profile,
            color: self.color,
        }
    }

    pub(crate) fn sample_pool_stats(&self) -> SamplePoolStats {
        self.pool_stats
    }

    /// Fill a retained input sample when the MFT promises not to hold input
    /// buffers. `MF_E_TRANSFORM_NEED_MORE_INPUT` ends every frame drain before
    /// this slot is reused.
    unsafe fn prepare_input_sample(
        &mut self,
        y_plane: &[u8],
        y_stride: usize,
        uv_plane: &[u8],
        uv_stride: usize,
    ) -> windows::core::Result<IMFSample> {
        let w = self.width as usize;
        let h = self.height as usize;
        let total = w * h + w * (h / 2);
        let hns_per_frame = 10_000_000i64 / self.fps as i64;
        let pts = self.frame_index as i64 * hns_per_frame;

        if !self.input_reuse_enabled {
            self.pool_stats.input_allocations += 1;
            let slot = SampleSlot::new(total as u32)?;
            slot.write_nv12(
                y_plane,
                y_stride,
                uv_plane,
                uv_stride,
                w,
                h,
                pts,
                hns_per_frame,
            )?;
            return Ok(slot.sample);
        }

        let reused = self.input_sample.is_some();
        let slot = ensure_sample_slot(&mut self.input_sample, total as u32)?;
        if reused {
            self.pool_stats.input_reuses += 1;
        } else {
            self.pool_stats.input_allocations += 1;
        }
        slot.write_nv12(
            y_plane,
            y_stride,
            uv_plane,
            uv_stride,
            w,
            h,
            pts,
            hns_per_frame,
        )?;
        Ok(slot.sample.clone())
    }

    unsafe fn prepare_output_sample(&mut self) -> windows::core::Result<Option<IMFSample>> {
        if self.output_buffer_size == 0 {
            let stream_info = self.transform.GetOutputStreamInfo(self.output_stream_id)?;
            self.output_buffer_size = stream_info.cbSize.max(1);
            self.output_provides_samples =
                stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        }
        if self.output_provides_samples {
            return Ok(None);
        }

        let reused = self
            .output_sample
            .as_ref()
            .is_some_and(|slot| slot.capacity >= self.output_buffer_size);
        let slot = ensure_sample_slot(&mut self.output_sample, self.output_buffer_size)?;
        slot.reset()?;
        if reused {
            self.pool_stats.output_reuses += 1;
        } else {
            self.pool_stats.output_allocations += 1;
        }
        Ok(Some(slot.sample.clone()))
    }

    unsafe fn process_output(
        &mut self,
        out_bytes: &mut Vec<u8>,
        is_key: &mut bool,
    ) -> windows::core::Result<ProcessOutputStatus> {
        // The inbox SW H.264 MFT requires a caller-owned output sample. Retain
        // that sample and buffer after each synchronous drain rather than
        // creating two COM objects for every frame.
        let out_sample = self.prepare_output_sample()?;

        let data = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_stream_id,
            pSample: std::mem::ManuallyDrop::new(out_sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status: u32 = 0;
        let mut buffers = [data];

        let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
        let mut data = std::mem::replace(
            &mut buffers[0],
            MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_stream_id,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            },
        );
        // ProcessOutput owns neither COM field. Release both on success and
        // every routine error, including MF_E_TRANSFORM_NEED_MORE_INPUT.
        let sample = std::mem::ManuallyDrop::take(&mut data.pSample);
        let events = std::mem::ManuallyDrop::take(&mut data.pEvents);
        drop(events);

        match result {
            Ok(()) => {
                if let Some(sample) = sample {
                    // Determine if this AU is a keyframe.
                    let attrs: IMFAttributes = sample.cast()?;
                    let clean = attrs
                        .GetUINT32(
                            &windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint,
                        )
                        .unwrap_or(0);
                    if clean != 0 {
                        *is_key = true;
                        self.pending_idr = false;
                    }
                    copy_sample_bytes(&sample, out_bytes)?;
                }
                Ok(ProcessOutputStatus::Sample)
            }
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                drop(sample);
                Ok(ProcessOutputStatus::NeedMoreInput)
            }
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                drop(sample);
                Ok(ProcessOutputStatus::StreamChange)
            }
            Err(err) => {
                drop(sample);
                Err(err)
            }
        }
    }
}

impl SampleSlot {
    unsafe fn new(capacity: u32) -> windows::core::Result<Self> {
        let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(capacity)?;
        let sample: IMFSample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(Self {
            sample,
            buffer,
            capacity,
        })
    }

    unsafe fn reset(&self) -> windows::core::Result<()> {
        self.sample.DeleteAllItems()?;
        self.buffer.SetCurrentLength(0)
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn write_nv12(
        &self,
        y_plane: &[u8],
        y_stride: usize,
        uv_plane: &[u8],
        uv_stride: usize,
        width: usize,
        height: usize,
        pts: i64,
        duration: i64,
    ) -> windows::core::Result<()> {
        self.reset()?;

        let mut dst: *mut u8 = ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        self.buffer
            .Lock(&mut dst, Some(&mut max_len), Some(&mut cur_len))?;

        // Y plane, tightly packed at width bytes/row in the MF buffer.
        for row in 0..height {
            let src = &y_plane[row * y_stride..row * y_stride + width];
            let dst_row = std::slice::from_raw_parts_mut(dst.add(row * width), width);
            dst_row.copy_from_slice(src);
        }
        // UV plane immediately after Y.
        let uv_dst_base = dst.add(width * height);
        for row in 0..(height / 2) {
            let src = &uv_plane[row * uv_stride..row * uv_stride + width];
            let dst_row = std::slice::from_raw_parts_mut(uv_dst_base.add(row * width), width);
            dst_row.copy_from_slice(src);
        }

        self.buffer.Unlock()?;
        self.buffer
            .SetCurrentLength((width * height + width * (height / 2)) as u32)?;
        self.sample.SetSampleTime(pts)?;
        self.sample.SetSampleDuration(duration)?;
        Ok(())
    }
}

unsafe fn ensure_sample_slot(
    slot: &mut Option<SampleSlot>,
    capacity: u32,
) -> windows::core::Result<&mut SampleSlot> {
    if slot
        .as_ref()
        .is_none_or(|existing| existing.capacity < capacity)
    {
        *slot = Some(SampleSlot::new(capacity)?);
    }
    Ok(slot.as_mut().expect("sample slot initialized"))
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
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

enum ProcessOutputStatus {
    Sample,
    NeedMoreInput,
    StreamChange,
}

unsafe fn create_h264_encoder_mft() -> windows::core::Result<IMFTransform> {
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    // Prefer sync SW MFT; deliberately exclude hardware/async MFTs on VMware
    // SVGA where they either don't exist or misbehave.
    let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    // Not passing HARDWARE / ASYNCMFT / TRANSCODE_ONLY flags -> caller only
    // gets in-process SW encoders. `_` bindings silence unused-import warnings
    // when tuning the flag mask.
    let _ = (
        MFT_ENUM_FLAG_ASYNCMFT,
        MFT_ENUM_FLAG_HARDWARE,
        MFT_ENUM_FLAG_TRANSCODE_ONLY,
    );

    let mut activate_array: *mut Option<windows::Win32::Media::MediaFoundation::IMFActivate> =
        ptr::null_mut();
    let mut count: u32 = 0;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        flags,
        Some(&input_info),
        Some(&output_info),
        &mut activate_array,
        &mut count,
    )?;
    if count == 0 || activate_array.is_null() {
        return Err(windows::core::Error::new(
            E_FAIL,
            "no matching H.264 encoder MFT found",
        ));
    }
    // Move every activate OUT of the CoTask array before freeing it, so each
    // COM reference is owned by exactly one Rust value and released exactly
    // once (a `clone()` of a slice element is refcount-neutral and would leak
    // the originals). After the `ptr::read`s the array holds bit-copies that
    // must not be dropped through — only the memory itself is freed.
    let mut owned: Vec<Option<windows::Win32::Media::MediaFoundation::IMFActivate>> =
        Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        owned.push(ptr::read(activate_array.add(index)));
    }
    windows::Win32::System::Com::CoTaskMemFree(Some(activate_array as *const _));

    // First registered encoder wins; SortAndFilter already ranks them. Every
    // exit path from here on drops `owned`, releasing the other activates.
    let activate = owned[0].take().ok_or_else(|| {
        windows::core::Error::new(E_FAIL, "encoder MFTEnumEx returned a null activate")
    })?;

    let transform: IMFTransform = activate.ActivateObject()?;
    Ok(transform)
}

unsafe fn build_output_type(cfg: &Config) -> windows::core::Result<IMFMediaType> {
    let mt: IMFMediaType = MFCreateMediaType()?;
    mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    mt.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps * 1000)?;
    set_frame_size(&mt, cfg.width, cfg.height)?;
    set_frame_rate(&mt, cfg.fps, 1)?;
    set_pixel_aspect_ratio(&mt, 1, 1)?;
    mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    mt.SetUINT32(&MF_MT_MPEG2_PROFILE, cfg.profile.as_eavencomm())?;
    // Pick the smallest H.264 level that admits our (width, height, fps).
    // Anything up to 1080p60 fits Level 4.1 (Main profile ceiling); 4K30 needs
    // 5.1. The SW MFT rejects the whole media type with E_INVALIDARG if the
    // dims exceed the level's macroblock ceiling, which is what tripped VMware
    // SVGA's 3600x2260 mode on default Level 4.1.
    mt.SetUINT32(
        &MF_MT_MPEG2_LEVEL,
        pick_h264_level(cfg.width, cfg.height, cfg.fps),
    )?;
    // The H.264/AVC encoder reads MF_MT_VIDEO_NOMINAL_RANGE off the *output*
    // type to choose the VUI `video_full_range_flag` it writes into the SPS
    // (documented on MF_MT_VIDEO_NOMINAL_RANGE's own reference page); the
    // matrix/primaries/transfer attributes are set alongside it so the
    // encoded bitstream states a complete, truthful colour contract rather
    // than leaving a decoder to guess and crush blacks on a range mismatch.
    set_color_attributes(&mt, cfg.color)?;
    Ok(mt)
}

/// True if the buffer begins with an Annex-B start code
/// (`00 00 00 01` or `00 00 01`).
fn starts_with_annexb_start_code(bytes: &[u8]) -> bool {
    (bytes.len() >= 4 && bytes[..4] == [0, 0, 0, 1])
        || (bytes.len() >= 3 && bytes[..3] == [0, 0, 1])
}

/// eAVEncH264VLevel values from codecapi.h. Return the smallest level that
/// fits the source. Falls back to 5.2 (highest supported by the SW MFT).
fn pick_h264_level(width: u32, height: u32, fps: u32) -> u32 {
    let mbs = width.div_ceil(16) * height.div_ceil(16);
    let mbs_per_sec = mbs * fps.max(1);
    // (level_value, max_mbs_per_frame, max_mbs_per_sec)
    const LEVELS: &[(u32, u32, u32)] = &[
        (31, 3600, 108_000),     // Level 3.1  (720p30)
        (32, 5120, 216_000),     // Level 3.2  (720p60)
        (40, 8192, 245_760),     // Level 4    (1080p30)
        (42, 8704, 522_240),     // Level 4.2  (1080p60)
        (50, 22_080, 589_824),   // Level 5    (2K)
        (51, 36_864, 983_040),   // Level 5.1  (4K30)
        (52, 36_864, 2_073_600), // Level 5.2 (4K60)
    ];
    for (val, max_frame, max_rate) in LEVELS {
        if mbs <= *max_frame && mbs_per_sec <= *max_rate {
            return *val;
        }
    }
    52
}

unsafe fn build_input_type(cfg: &Config) -> windows::core::Result<IMFMediaType> {
    let mt: IMFMediaType = MFCreateMediaType()?;
    mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    set_frame_size(&mt, cfg.width, cfg.height)?;
    set_frame_rate(&mt, cfg.fps, 1)?;
    set_pixel_aspect_ratio(&mt, 1, 1)?;
    mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    // "Uncompressed Video Media Types" documents these four as the attributes
    // to set on a raw type when the source colour is known, so the encoder
    // reads the *input* NV12 frames as the same contract the output SPS ends
    // up stating rather than defaulting on one side only.
    set_color_attributes(&mt, cfg.color)?;
    Ok(mt)
}

unsafe fn set_frame_size(mt: &IMFMediaType, w: u32, h: u32) -> windows::core::Result<()> {
    let packed = ((w as u64) << 32) | (h as u64);
    mt.SetUINT64(&MF_MT_FRAME_SIZE, packed)
}

unsafe fn set_frame_rate(mt: &IMFMediaType, num: u32, den: u32) -> windows::core::Result<()> {
    let packed = ((num as u64) << 32) | (den as u64);
    mt.SetUINT64(&MF_MT_FRAME_RATE, packed)
}

unsafe fn set_pixel_aspect_ratio(
    mt: &IMFMediaType,
    num: u32,
    den: u32,
) -> windows::core::Result<()> {
    let packed = ((num as u64) << 32) | (den as u64);
    mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed)
}

/// Set the four colour-signalling attributes on one media type from the
/// negotiated colour contract.
///
/// Called for both the input (raw NV12) and output (H.264) types, so the
/// bitstream the encoder emits states what the frames it was fed actually
/// were rather than defaulting one side and hoping.
unsafe fn set_color_attributes(mt: &IMFMediaType, color: ColorSpec) -> windows::core::Result<()> {
    mt.SetUINT32(
        &MF_MT_VIDEO_NOMINAL_RANGE,
        mf_nominal_range(color.range).0 as u32,
    )?;
    mt.SetUINT32(&MF_MT_YUV_MATRIX, mf_yuv_matrix(color.matrix)?.0 as u32)?;
    mt.SetUINT32(
        &MF_MT_VIDEO_PRIMARIES,
        mf_video_primaries(color.primaries)?.0 as u32,
    )?;
    mt.SetUINT32(
        &MF_MT_TRANSFER_FUNCTION,
        mf_transfer_function(color.transfer)?.0 as u32,
    )?;
    Ok(())
}

/// Reject a colour contract the inbox SW H.264 MFT cannot make truthful
/// before COM/MF ever start up, rather than silently narrowing chroma, depth
/// or the VUI values it is asked to signal.
///
/// The MFT is NV12 (8-bit, 4:2:0) only, which the caller has already fixed by
/// construction, but the *colour* fields (range/matrix/primaries/transfer)
/// come from a negotiated plan that may legitimately ask for something this
/// backend cannot express — 4:4:4, 10-bit, BT.2020 or an HDR transfer
/// function all fall outside what `MFVideoTransferMatrix` /
/// `MFVideoPrimaries` / `MFVideoTransferFunction` can name for this MFT — and
/// that mismatch must fail loudly rather than emit a stream that disagrees
/// with the plan.
fn validate_mf_color(color: ColorSpec) -> windows::core::Result<()> {
    if color.chroma != ChromaSubsampling::Yuv420 {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            format!(
                "MF H.264 encoder accepts only 4:2:0 chroma (NV12); requested {:?}",
                color.chroma
            ),
        ));
    }
    if color.bit_depth != BitDepth::Eight {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            format!(
                "MF H.264 encoder accepts only 8-bit samples; requested {}-bit",
                color.bit_depth.bits()
            ),
        ));
    }
    mf_yuv_matrix(color.matrix)?;
    mf_video_primaries(color.primaries)?;
    mf_transfer_function(color.transfer)?;
    Ok(())
}

/// Map the negotiated range to `MFNominalRange`. Every [`ColorRange`] value
/// has a direct equivalent, so this mapping cannot fail — unlike matrix,
/// primaries and transfer, range is exactly the axis this workstream exists
/// to fix, and the inbox MFT documents both values as accepted on the output
/// type.
const fn mf_nominal_range(range: ColorRange) -> MFNominalRange {
    match range {
        ColorRange::Limited => MFNominalRange_16_235,
        ColorRange::Full => MFNominalRange_0_255,
    }
}

/// Map the negotiated matrix to `MFVideoTransferMatrix`.
///
/// [`ColorMatrix::Identity`] (GBR passthrough) has no NV12 representation at
/// all — that path never reaches this encoder — and `MFVideoTransferMatrix`
/// has no plain BT.2020 constant (only bit-depth-qualified
/// `BT2020_10`/`BT2020_12` variants that presuppose a 10/12-bit surface this
/// 8-bit-only MFT never has), so both are rejected rather than approximated.
fn mf_yuv_matrix(matrix: ColorMatrix) -> windows::core::Result<MFVideoTransferMatrix> {
    match matrix {
        ColorMatrix::Bt709 => Ok(MFVideoTransferMatrix_BT709),
        ColorMatrix::Bt601 => Ok(MFVideoTransferMatrix_BT601),
        ColorMatrix::Identity | ColorMatrix::Bt2020Ncl => Err(windows::core::Error::new(
            E_INVALIDARG,
            format!("MF H.264 cannot signal matrix {matrix:?}; only bt709 and bt601 are supported"),
        )),
    }
}

/// Map the negotiated primaries to `MFVideoPrimaries`. Only BT.709 is
/// supported: the wider gamuts Arcen offers elsewhere (BT.2020, Display P3)
/// are paired with the 10/12-bit paths this 8-bit-only MFT never takes.
fn mf_video_primaries(primaries: ColorPrimaries) -> windows::core::Result<MFVideoPrimaries> {
    match primaries {
        ColorPrimaries::Bt709 => Ok(MFVideoPrimaries_BT709),
        ColorPrimaries::Bt2020 | ColorPrimaries::DisplayP3 => Err(windows::core::Error::new(
            E_INVALIDARG,
            format!("MF H.264 cannot signal primaries {primaries:?}; only bt709 is supported"),
        )),
    }
}

/// Map the negotiated transfer characteristic to `MFVideoTransferFunction`.
/// PQ and HLG are HDR transfer functions Arcen only ever pairs with a
/// higher-bit-depth backend, so they are rejected here rather than silently
/// tagged as BT.709/sRGB.
fn mf_transfer_function(
    transfer: TransferCharacteristics,
) -> windows::core::Result<MFVideoTransferFunction> {
    match transfer {
        TransferCharacteristics::Bt709 => Ok(MFVideoTransFunc_709),
        TransferCharacteristics::Srgb => Ok(MFVideoTransFunc_sRGB),
        TransferCharacteristics::Pq | TransferCharacteristics::Hlg => {
            Err(windows::core::Error::new(
                E_INVALIDARG,
                format!(
                    "MF H.264 cannot signal transfer {transfer:?}; only bt709 and srgb are supported"
                ),
            ))
        }
    }
}

unsafe fn configure_codec_api(api: &ICodecAPI, cfg: &Config) -> windows::core::Result<()> {
    set_codec_u32(
        api,
        &CODECAPI_AVEncCommonRateControlMode,
        eAVEncCommonRateControlMode_CBR.0 as u32,
        "AVEncCommonRateControlMode=CBR",
    )?;
    set_codec_u32(
        api,
        &CODECAPI_AVEncCommonMeanBitRate,
        cfg.bitrate_kbps.saturating_mul(1000),
        "AVEncCommonMeanBitRate",
    )?;
    set_codec_bool(api, &CODECAPI_AVLowLatencyMode, true, "AVLowLatencyMode")?;
    set_codec_u32(
        api,
        &CODECAPI_AVEncMPVGOPSize,
        cfg.gop_frames.max(1),
        "AVEncMPVGOPSize",
    )?;
    set_codec_u32(
        api,
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        0,
        "AVEncMPVDefaultBPictureCount",
    )?;
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .saturating_sub(1)
        .clamp(1, 16) as u32;
    set_codec_u32(
        api,
        &CODECAPI_AVEncNumWorkerThreads,
        workers,
        "AVEncNumWorkerThreads",
    )?;
    Ok(())
}

unsafe fn set_codec_u32(
    api: &ICodecAPI,
    property: &GUID,
    value: u32,
    name: &str,
) -> windows::core::Result<()> {
    api.IsSupported(property).map_err(|error| {
        windows::core::Error::new(
            error.code(),
            format!("{name} unsupported: {}", error.message()),
        )
    })?;
    let value = VARIANT::from(value);
    api.SetValue(property, &value).map_err(|error| {
        windows::core::Error::new(error.code(), format!("set {name}: {}", error.message()))
    })
}

unsafe fn set_codec_bool(
    api: &ICodecAPI,
    property: &GUID,
    value: bool,
    name: &str,
) -> windows::core::Result<()> {
    api.IsSupported(property).map_err(|error| {
        windows::core::Error::new(
            error.code(),
            format!("{name} unsupported: {}", error.message()),
        )
    })?;
    let value = VARIANT::from(value);
    api.SetValue(property, &value).map_err(|error| {
        windows::core::Error::new(error.code(), format!("set {name}: {}", error.message()))
    })
}

// codecapi.h GUIDs (not exported by windows-rs 0.58 in a stable location, so
// vendor the ones we need. Values from `codecapi.h` in the Windows SDK.)
// {59B2C1E3-13F2-4C31-8DAC-8B7EA36D5E8B} etc.
#[allow(non_upper_case_globals)]
const CODECAPI_AVEncCommonRateControlMode: GUID = GUID::from_values(
    0x1c0608e9,
    0x370c,
    0x4710,
    [0x8a, 0x58, 0xcb, 0x61, 0x81, 0xc4, 0x24, 0x23],
);
#[allow(non_upper_case_globals)]
const CODECAPI_AVEncCommonMeanBitRate: GUID = GUID::from_values(
    0xf7222374,
    0x2144,
    0x4815,
    [0xb5, 0x50, 0xa3, 0x7f, 0x8e, 0x12, 0xee, 0x52],
);
#[allow(non_upper_case_globals)]
const CODECAPI_AVLowLatencyMode: GUID = GUID::from_values(
    0x9c27891a,
    0xed7a,
    0x40e1,
    [0x88, 0xe8, 0xb2, 0x27, 0x27, 0xa0, 0x24, 0xee],
);
#[allow(non_upper_case_globals)]
const CODECAPI_AVEncMPVGOPSize: GUID = GUID::from_values(
    0x95f31b26,
    0x95a4,
    0x41aa,
    [0x93, 0x03, 0x24, 0x6a, 0x7f, 0xc6, 0xee, 0xf1],
);
#[allow(non_upper_case_globals)]
const CODECAPI_AVEncNumWorkerThreads: GUID = GUID::from_values(
    0xb0c8bf60,
    0x16f7,
    0x4951,
    [0xa3, 0xb, 0x1d, 0xb1, 0x60, 0x92, 0x93, 0xd6],
);
#[allow(non_upper_case_globals)]
const CODECAPI_AVEncMPVDefaultBPictureCount: GUID = GUID::from_values(
    0x8d390aac,
    0xdc5c,
    0x4200,
    [0xb5, 0x7f, 0x81, 0x4d, 0x04, 0xba, 0xba, 0xb2],
);
#[allow(non_upper_case_globals)]
const CODECAPI_AVEncVideoForceKeyFrame: GUID = GUID::from_values(
    0x398c1b98,
    0x8353,
    0x475a,
    [0x9e, 0xf2, 0x8f, 0x26, 0x5d, 0x26, 0x03, 0x45],
);

unsafe fn extract_parameter_sets(mt: &IMFMediaType) -> Vec<Vec<u8>> {
    let size = match mt.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if size == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size as usize];
    if mt
        .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None)
        .is_err()
    {
        return Vec::new();
    }
    let ps = parse_avc_decoder_config(&buf);
    if !ps.is_empty() {
        return ps;
    }
    // Some encoders write raw Annex-B into the attribute; scan for start codes.
    scan_parameter_sets(&buf)
}

/// Scan an Annex-B byte stream for NAL type 7 (SPS) and 8 (PPS), returning
/// each NAL unit stripped of its start code.
fn scan_parameter_sets(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut params = Vec::new();
    let mut i = 0usize;
    let mut last_start: Option<usize> = None;
    while i + 3 < bytes.len() {
        let is_start3 = bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1;
        let is_start4 =
            bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 0 && bytes[i + 3] == 1;
        if is_start3 || is_start4 {
            let payload_start = i + if is_start4 { 4 } else { 3 };
            if let Some(prev) = last_start {
                let end = i;
                if end > prev {
                    let nal_type = bytes[prev] & 0x1f;
                    if nal_type == 7 || nal_type == 8 {
                        params.push(bytes[prev..end].to_vec());
                    }
                }
            }
            last_start = Some(payload_start);
            i = payload_start;
        } else {
            i += 1;
        }
    }
    if let Some(prev) = last_start {
        let nal_type = bytes[prev] & 0x1f;
        if nal_type == 7 || nal_type == 8 {
            params.push(bytes[prev..].to_vec());
        }
    }
    params
}

unsafe fn copy_sample_bytes(sample: &IMFSample, out: &mut Vec<u8>) -> windows::core::Result<()> {
    let buffer_count = sample.GetBufferCount()?;
    for i in 0..buffer_count {
        let buffer: IMFMediaBuffer = sample.GetBufferByIndex(i)?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len: u32 = 0;
        buffer.Lock(&mut ptr, None, Some(&mut cur_len))?;
        let slice = std::slice::from_raw_parts(ptr, cur_len as usize);
        out.extend_from_slice(slice);
        buffer.Unlock()?;
    }
    Ok(())
}

// Suppress unused warnings for the constants that windows-rs 0.58 does export
// but we import defensively — some MF SDK constants have moved between
// windows-rs releases and we want a compile error if the wrong feature is
// missing rather than a silent typo.
#[allow(dead_code)]
fn _link_check() {
    let _ = MF_TRANSFORM_ASYNC;
    let _ = MF_E_INVALIDMEDIATYPE;
    let _ = MFT_SET_TYPE_TEST_ONLY;
    let _ = PCWSTR::null();
    let _ = size_of::<GUID>();
}

#[cfg(test)]
mod tests {
    use arcen_media::{
        BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange,
        TransferCharacteristics,
    };

    use super::{
        mf_nominal_range, mf_transfer_function, mf_video_primaries, mf_yuv_matrix,
        validate_mf_color, Config, Encoder, H264Profile, MFNominalRange_0_255,
        MFNominalRange_16_235, MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransFunc_sRGB,
        MFVideoTransferMatrix_BT601, MFVideoTransferMatrix_BT709,
    };
    use crate::ColorSpec;

    #[test]
    #[ignore = "requires the Windows inbox H.264 Media Foundation transform"]
    fn inbox_transform_reuses_input_and_output_samples_after_warmup() {
        const WIDTH: usize = 64;
        const HEIGHT: usize = 64;
        let mut encoder = Encoder::new(&Config {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            fps: 30,
            bitrate_kbps: 1_000,
            gop_frames: 60,
            profile: H264Profile::Main,
            color: ColorSpec::legacy(false),
        })
        .expect("inbox H.264 MFT");
        let y = vec![16u8; WIDTH * HEIGHT];
        let uv = vec![128u8; WIDTH * HEIGHT / 2];

        for frame in 0..4 {
            encoder
                .encode_frame(&y, WIDTH, &uv, WIDTH, frame == 0)
                .expect("encode frame");
        }

        let stats = encoder.sample_pool_stats();
        assert_eq!(stats.input_allocations, 1);
        assert_eq!(stats.input_reuses, 3);
        assert_eq!(stats.output_allocations, 1);
        assert!(stats.output_reuses >= 3);
    }

    #[test]
    fn nominal_range_maps_limited_and_full() {
        assert_eq!(
            mf_nominal_range(ColorRange::Limited).0,
            MFNominalRange_16_235.0
        );
        assert_eq!(mf_nominal_range(ColorRange::Full).0, MFNominalRange_0_255.0);
    }

    #[test]
    fn yuv_matrix_maps_supported_values_and_rejects_the_rest() {
        assert_eq!(
            mf_yuv_matrix(ColorMatrix::Bt709).expect("bt709").0,
            MFVideoTransferMatrix_BT709.0
        );
        assert_eq!(
            mf_yuv_matrix(ColorMatrix::Bt601).expect("bt601").0,
            MFVideoTransferMatrix_BT601.0
        );
        assert!(mf_yuv_matrix(ColorMatrix::Identity).is_err());
        assert!(mf_yuv_matrix(ColorMatrix::Bt2020Ncl).is_err());
    }

    #[test]
    fn video_primaries_maps_bt709_and_rejects_wider_gamuts() {
        assert_eq!(
            mf_video_primaries(ColorPrimaries::Bt709).expect("bt709").0,
            MFVideoPrimaries_BT709.0
        );
        assert!(mf_video_primaries(ColorPrimaries::Bt2020).is_err());
        assert!(mf_video_primaries(ColorPrimaries::DisplayP3).is_err());
    }

    #[test]
    fn transfer_function_maps_bt709_and_srgb_and_rejects_hdr() {
        assert_eq!(
            mf_transfer_function(TransferCharacteristics::Bt709)
                .expect("bt709")
                .0,
            MFVideoTransFunc_709.0
        );
        assert_eq!(
            mf_transfer_function(TransferCharacteristics::Srgb)
                .expect("srgb")
                .0,
            MFVideoTransFunc_sRGB.0
        );
        assert!(mf_transfer_function(TransferCharacteristics::Pq).is_err());
        assert!(mf_transfer_function(TransferCharacteristics::Hlg).is_err());
    }

    #[test]
    fn validate_mf_color_accepts_the_legacy_contract_at_both_ranges() {
        assert!(validate_mf_color(ColorSpec::legacy(false)).is_ok());
        assert!(validate_mf_color(ColorSpec {
            range: ColorRange::Full,
            ..ColorSpec::legacy(false)
        })
        .is_ok());
    }

    #[test]
    fn validate_mf_color_rejects_444_chroma() {
        let error = validate_mf_color(ColorSpec::legacy(true)).expect_err("4:4:4 must be rejected");
        assert!(error.message().to_string().contains("4:2:0"));
    }

    #[test]
    fn validate_mf_color_rejects_above_eight_bit() {
        for depth in [BitDepth::Ten, BitDepth::Twelve] {
            let color = ColorSpec {
                bit_depth: depth,
                ..ColorSpec::legacy(false)
            };
            let error = validate_mf_color(color).expect_err("above 8-bit must be rejected");
            assert!(error.message().to_string().contains("8-bit"));
        }
    }

    #[test]
    fn validate_mf_color_rejects_unsignallable_matrix_primaries_and_transfer() {
        assert!(validate_mf_color(ColorSpec {
            matrix: ColorMatrix::Identity,
            ..ColorSpec::legacy(false)
        })
        .is_err());
        assert!(validate_mf_color(ColorSpec {
            matrix: ColorMatrix::Bt2020Ncl,
            ..ColorSpec::legacy(false)
        })
        .is_err());
        assert!(validate_mf_color(ColorSpec {
            primaries: ColorPrimaries::Bt2020,
            ..ColorSpec::legacy(false)
        })
        .is_err());
        assert!(validate_mf_color(ColorSpec {
            transfer: TransferCharacteristics::Pq,
            ..ColorSpec::legacy(false)
        })
        .is_err());
    }

    #[test]
    fn encoder_new_rejects_bad_color_before_touching_com_or_mf() {
        // A rejected colour contract must fail before any COM/MF setup, so
        // this must return an error (not hang, panic, or touch real hardware)
        // even on a machine with no Media Foundation H.264 MFT registered.
        let error = Encoder::new(&Config {
            width: 64,
            height: 64,
            fps: 30,
            bitrate_kbps: 1_000,
            gop_frames: 60,
            profile: H264Profile::Main,
            color: ColorSpec {
                chroma: ChromaSubsampling::Yuv444,
                ..ColorSpec::legacy(false)
            },
        })
        .map(|_| ())
        .expect_err("4:4:4 must be rejected before MFTEnumEx runs");
        assert!(error.message().to_string().contains("4:2:0"));
    }
}
