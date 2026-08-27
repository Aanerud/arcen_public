// Windows Media-Foundation live encode path (VMware SVGA / no-NVENC hosts).
//
// Sibling to `win.rs::run_encode` but wired to the SW H.264 MFT in
// `mf_encoder.rs`. Keeps the NVENC path completely untouched.
//
// Pipeline per iteration:
//   1. WGC.acquire_into hands us a D3D11 BGRA texture holding the newest frame.
//   2. CopyResource -> a CPU-readable staging texture (created lazily; resized
//      if WGC reports a new size after a monitor rescale).
//   3. Map(READ) -> BGRA rows in system memory with a source pitch.
//   4. bgra_to_nv12 converts into two heap Vec<u8> planes.
//   5. mf_encoder::Encoder::encode_frame -> Option<Vec<u8>> Annex-B AU.
//   6. write_access_unit to stdout, honouring the framed-v1 protocol.
//
// The SW MFT is CPU-only and cannot ingest D3D11 surfaces at all, so the round
// trip GPU->CPU is unavoidable on this path — but it lives here in the
// fallback path only, and NVENC hosts keep their zero-copy pipeline.
//
// Stdin still carries "IDR" commands; parent process death still exits us
// cleanly. Stats + IDR contract match win.rs::run_encode.

use std::time::{Duration, Instant};

use arcen_keel::{ActivityHint, BgraFrame, DamageSummary, HashKernel};
use arcen_media::video::ColorTransform;
use arcen_media::video::EncoderBackend;

/// Narrow a coded sample to the eight-bit plane byte these encoders consume.
///
/// Both Media Foundation and `OpenH264` are eight-bit only, so a transform
/// built for a deeper format cannot reach them; the clamp is a guard, not a
/// conversion.
fn as_coded_u8(code: i32) -> u8 {
    u8::try_from(code.clamp(0, 255)).unwrap_or(0)
}
#[cfg(feature = "software-h264")]
use arcen_media::video::{
    convert_bgra_to_i420, convert_bgra_to_i420_rows, I420Frame, I420FrameMut, SoftwareH264Config,
    SoftwareH264Encoder,
};
#[cfg(feature = "mf")]
use arcen_media::video::{convert_bgra_to_nv12, convert_bgra_to_nv12_rows, Nv12FrameMut};
use arcen_media::ForcedKeyframe;
use windows::core::Interface;
use windows::Win32::Foundation::{E_INVALIDARG, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};

use crate::log;
#[cfg(feature = "mf")]
use crate::mf_encoder::{Config as MfConfig, Encoder as MfEncoder, H264Profile, SamplePoolStats};
use crate::wgc::WgcCapture;
use crate::CursorCaptureMode;

/// Runtime parameters for the MF path. Kept small and explicit so callers
/// (win.rs::run) can construct one from CLI args + defaults without pulling
/// the whole config surface into this module.
#[cfg(feature = "mf")]
pub(crate) struct MfRunOpts {
    pub output_index: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub profile: H264Profile,
    pub gop_secs: u32,
    pub framed: bool,
    pub adapter_hint: Option<String>,
    pub adapter_output_index: Option<u32>,
    pub device_name: Option<String>,
    pub cursor_mode: CursorCaptureMode,
    /// The negotiated colour contract for this run — resolved once by the
    /// caller (`win.rs::run_with_args`, via `crate::requested_color`) from
    /// argv, so this struct never re-derives its own, potentially
    /// disagreeing, `ColorSpec::legacy(...)`. The SW H.264 MFT is 8-bit
    /// 4:2:0 only, so `mf_encoder::validate_mf_color` rejects anything else
    /// before COM/MF ever start up.
    pub color: crate::ColorSpec,
}

#[cfg(feature = "software-h264")]
pub(crate) struct OpenH264RunOpts {
    pub output_index: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub framed: bool,
    pub adapter_hint: Option<String>,
    pub adapter_output_index: Option<u32>,
    pub device_name: Option<String>,
    pub cursor_mode: CursorCaptureMode,
    /// See `MfRunOpts::color` — same reasoning, same caller.
    pub color: crate::ColorSpec,
}

struct CommonRunOpts {
    output_index: u32,
    fps: u32,
    bitrate_kbps: u32,
    framed: bool,
    adapter_hint: Option<String>,
    adapter_output_index: Option<u32>,
    device_name: Option<String>,
    cursor_mode: CursorCaptureMode,
    /// The negotiated colour contract for this run.
    color: crate::ColorSpec,
}

#[derive(Clone, Copy)]
enum EncoderKind {
    #[cfg(feature = "mf")]
    MediaFoundation { profile: H264Profile, gop_secs: u32 },
    #[cfg(feature = "software-h264")]
    OpenH264,
}

impl EncoderKind {
    const fn label(self) -> &'static str {
        match self {
            #[cfg(feature = "mf")]
            Self::MediaFoundation { .. } => "Media Foundation",
            #[cfg(feature = "software-h264")]
            Self::OpenH264 => "OpenH264",
        }
    }

    const fn requires_macroblock_alignment(&self) -> bool {
        match self {
            #[cfg(feature = "mf")]
            Self::MediaFoundation { .. } => true,
            #[cfg(feature = "software-h264")]
            Self::OpenH264 => false,
        }
    }
}

enum ActiveEncoder {
    #[cfg(feature = "mf")]
    MediaFoundation {
        encoder: MfEncoder,
        width: usize,
        y: Vec<u8>,
        uv: Vec<u8>,
        /// The colour contract this encoder was initialised with.
        ///
        /// Carried per encoder rather than read from a global so a resize or
        /// respawn cannot leave the converter and the bitstream signalling
        /// disagreeing about range.
        transform: ColorTransform,
    },
    #[cfg(feature = "software-h264")]
    OpenH264 {
        encoder: Box<SoftwareH264Encoder>,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
        transform: ColorTransform,
    },
}

enum EncodedOutput<'a> {
    #[cfg(feature = "mf")]
    Owned(Vec<u8>),
    Borrowed(&'a [u8]),
}

impl EncodedOutput<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mf")]
            Self::Owned(bytes) => bytes,
            Self::Borrowed(bytes) => bytes,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct PoolStats {
    input_allocations: u64,
    input_reuses: u64,
    output_allocations: u64,
    output_reuses: u64,
}

impl ActiveEncoder {
    fn new(
        kind: EncoderKind,
        width: u32,
        height: u32,
        opts: &CommonRunOpts,
    ) -> Result<Self, String> {
        let width_usize =
            usize::try_from(width).map_err(|_| "capture width does not fit usize".to_string())?;
        let height_usize =
            usize::try_from(height).map_err(|_| "capture height does not fit usize".to_string())?;
        let luma_len = width_usize
            .checked_mul(height_usize)
            .ok_or_else(|| "capture plane geometry overflow".to_string())?;
        match kind {
            #[cfg(feature = "mf")]
            EncoderKind::MediaFoundation { profile, gop_secs } => {
                let encoder = MfEncoder::new(&MfConfig {
                    width,
                    height,
                    fps: opts.fps,
                    bitrate_kbps: opts.bitrate_kbps,
                    gop_frames: opts.fps.saturating_mul(gop_secs.max(1)).max(1),
                    profile,
                    color: opts.color,
                })
                .map_err(|error| format!("MF encoder init failed: {error:?}"))?;
                Ok(Self::MediaFoundation {
                    encoder,
                    width: width_usize,
                    y: vec![0; luma_len],
                    uv: vec![0; luma_len / 2],
                    transform: opts.color.transform(),
                })
            }
            #[cfg(feature = "software-h264")]
            EncoderKind::OpenH264 => {
                let bitrate_bps = opts
                    .bitrate_kbps
                    .checked_mul(1_000)
                    .ok_or_else(|| "OpenH264 bitrate overflows bits per second".to_string())?;
                let threads = std::thread::available_parallelism()
                    .map_or(1, |count| u16::try_from(count.get().min(4)).unwrap_or(1));
                let encoder = SoftwareH264Encoder::new(SoftwareH264Config {
                    width,
                    height,
                    fps: opts.fps,
                    bitrate_bps,
                    num_threads: threads,
                    range: opts.color.range,
                    matrix: opts.color.matrix,
                    primaries: opts.color.primaries,
                    transfer: opts.color.transfer,
                })
                .map_err(|error| format!("OpenH264 encoder init failed: {error}"))?;
                Ok(Self::OpenH264 {
                    encoder: Box::new(encoder),
                    y: vec![0; luma_len],
                    u: vec![0; luma_len / 4],
                    v: vec![0; luma_len / 4],
                    transform: opts.color.transform(),
                })
            }
        }
    }

    const fn backend(&self) -> EncoderBackend {
        match self {
            #[cfg(feature = "mf")]
            Self::MediaFoundation { .. } => EncoderBackend::WindowsMediaFoundation,
            #[cfg(feature = "software-h264")]
            Self::OpenH264 { .. } => EncoderBackend::OpenH264,
        }
    }

    fn fill_black(&mut self) {
        match self {
            #[cfg(feature = "mf")]
            Self::MediaFoundation {
                y, uv, transform, ..
            } => {
                // Black is not always code 16. Filling a full-range stream
                // with the limited-range floor produces a visibly lifted
                // black, so the value comes from the transform rather than a
                // constant.
                y.fill(as_coded_u8(transform.luma(0, 0, 0)));
                uv.fill(as_coded_u8(transform.cb(0, 0, 0)));
            }
            #[cfg(feature = "software-h264")]
            Self::OpenH264 {
                y, u, v, transform, ..
            } => {
                y.fill(as_coded_u8(transform.luma(0, 0, 0)));
                u.fill(as_coded_u8(transform.cb(0, 0, 0)));
                v.fill(as_coded_u8(transform.cr(0, 0, 0)));
            }
        }
    }

    fn prepare_synthetic(
        &mut self,
        kind: arcen_media::RepresentativeFrameKind,
        dirty_basis_points: u16,
        frame: u8,
    ) {
        match self {
            #[cfg(feature = "mf")]
            Self::MediaFoundation { y, uv, .. } => {
                let changed = Self::synthetic_changed_len(y.len(), kind, dirty_basis_points);
                y[..changed].fill(frame);
                if kind == arcen_media::RepresentativeFrameKind::FullMotion {
                    uv.fill(frame.wrapping_add(96));
                }
            }
            #[cfg(feature = "software-h264")]
            Self::OpenH264 { y, u, v, .. } => {
                let changed = Self::synthetic_changed_len(y.len(), kind, dirty_basis_points);
                y[..changed].fill(frame);
                if kind == arcen_media::RepresentativeFrameKind::FullMotion {
                    u.fill(frame.wrapping_add(64));
                    v.fill(frame.wrapping_add(128));
                }
            }
        }
    }

    fn synthetic_changed_len(
        luma_len: usize,
        kind: arcen_media::RepresentativeFrameKind,
        dirty_basis_points: u16,
    ) -> usize {
        if kind == arcen_media::RepresentativeFrameKind::FullMotion {
            luma_len
        } else {
            luma_len
                .saturating_mul(usize::from(dirty_basis_points))
                .div_ceil(10_000)
                .max(1)
        }
    }

    fn convert_full(&mut self, frame: BgraFrame<'_>) -> Result<(), String> {
        match self {
            #[cfg(feature = "mf")]
            Self::MediaFoundation {
                y, uv, transform, ..
            } => {
                let mut destination = Nv12FrameMut::new(
                    frame.grid().width() as u32,
                    frame.grid().height() as u32,
                    y,
                    frame.grid().width(),
                    uv,
                    frame.grid().width(),
                )
                .map_err(|error| error.to_string())?;
                convert_bgra_to_nv12(frame, &mut destination, *transform)
                    .map_err(|error| error.to_string())
            }
            #[cfg(feature = "software-h264")]
            Self::OpenH264 {
                y, u, v, transform, ..
            } => {
                let width = frame.grid().width();
                let height = frame.grid().height();
                let mut destination = I420FrameMut::new(
                    width as u32,
                    height as u32,
                    y,
                    width,
                    u,
                    width / 2,
                    v,
                    width / 2,
                )
                .map_err(|error| error.to_string())?;
                convert_bgra_to_i420(frame, &mut destination, *transform)
                    .map_err(|error| error.to_string())
            }
        }
    }

    fn convert_rows(
        &mut self,
        frame: BgraFrame<'_>,
        rows: std::ops::Range<usize>,
    ) -> Result<(), String> {
        match self {
            #[cfg(feature = "mf")]
            Self::MediaFoundation {
                y, uv, transform, ..
            } => {
                let width = frame.grid().width();
                let height = frame.grid().height();
                let mut destination =
                    Nv12FrameMut::new(width as u32, height as u32, y, width, uv, width)
                        .map_err(|error| error.to_string())?;
                convert_bgra_to_nv12_rows(frame, &mut destination, rows, *transform)
                    .map_err(|error| error.to_string())
            }
            #[cfg(feature = "software-h264")]
            Self::OpenH264 {
                y, u, v, transform, ..
            } => {
                let width = frame.grid().width();
                let height = frame.grid().height();
                let mut destination = I420FrameMut::new(
                    width as u32,
                    height as u32,
                    y,
                    width,
                    u,
                    width / 2,
                    v,
                    width / 2,
                )
                .map_err(|error| error.to_string())?;
                convert_bgra_to_i420_rows(frame, &mut destination, rows, *transform)
                    .map_err(|error| error.to_string())
            }
        }
    }

    fn encode(&mut self, force: bool) -> Result<Option<EncodedOutput<'_>>, String> {
        match self {
            #[cfg(feature = "mf")]
            Self::MediaFoundation {
                encoder,
                width,
                y,
                uv,
                ..
            } => encoder
                .encode_frame(y, *width, uv, *width, force)
                .map(|output| output.map(EncodedOutput::Owned))
                .map_err(|error| format!("MF encode error: {error:?}")),
            #[cfg(feature = "software-h264")]
            Self::OpenH264 {
                encoder, y, u, v, ..
            } => {
                if force {
                    encoder.force_idr();
                }
                let config = encoder.config();
                let frame = I420Frame::new(
                    config.width,
                    config.height,
                    y,
                    config.width as usize,
                    u,
                    config.width as usize / 2,
                    v,
                    config.width as usize / 2,
                )
                .map_err(|error| error.to_string())?;
                encoder
                    .encode(frame)
                    .map(|output| output.map(|unit| EncodedOutput::Borrowed(unit.bytes)))
                    .map_err(|error| format!("OpenH264 encode error: {error}"))
            }
        }
    }

    fn pool_stats(&self) -> PoolStats {
        match self {
            #[cfg(feature = "mf")]
            Self::MediaFoundation { encoder, .. } => {
                let SamplePoolStats {
                    input_allocations,
                    input_reuses,
                    output_allocations,
                    output_reuses,
                } = encoder.sample_pool_stats();
                PoolStats {
                    input_allocations,
                    input_reuses,
                    output_allocations,
                    output_reuses,
                }
            }
            #[cfg(feature = "software-h264")]
            Self::OpenH264 { .. } => PoolStats::default(),
        }
    }
}

const IDLE_KEEPALIVE: Duration = Duration::from_secs(1);
const FULL_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// How long input/focus activity keeps a region responsive without new pixels.
const REGION_INPUT_WAKE_GRACE: Duration = Duration::from_millis(100);
const FULL_DAMAGE_ENTER_RATIO: f64 = 0.75;
const FULL_DAMAGE_EXIT_RATIO: f64 = 0.25;
const FULL_DAMAGE_PROBE_INTERVAL: u8 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DamageMode {
    Selective,
    FullDamage { frames_until_probe: u8 },
}

impl DamageMode {
    fn on_selective_sample(&mut self, summary: DamageSummary) {
        if summary.converted_row_ratio() >= FULL_DAMAGE_ENTER_RATIO {
            *self = Self::FullDamage {
                frames_until_probe: FULL_DAMAGE_PROBE_INTERVAL,
            };
        }
    }

    fn full_damage_action(&mut self) -> FullDamageAction {
        match self {
            Self::Selective => FullDamageAction::Probe,
            Self::FullDamage { frames_until_probe } if *frames_until_probe == 0 => {
                FullDamageAction::Probe
            }
            Self::FullDamage { frames_until_probe } => {
                *frames_until_probe -= 1;
                FullDamageAction::Bypass
            }
        }
    }

    fn on_full_damage_probe(&mut self, summary: DamageSummary) {
        if summary.converted_row_ratio() <= FULL_DAMAGE_EXIT_RATIO {
            *self = Self::Selective;
        } else {
            *self = Self::FullDamage {
                frames_until_probe: FULL_DAMAGE_PROBE_INTERVAL,
            };
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Selective => "selective",
            Self::FullDamage { .. } => "full-bypass",
        }
    }

    const fn probe_countdown(self) -> u8 {
        match self {
            Self::Selective => 0,
            Self::FullDamage { frames_until_probe } => frames_until_probe,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FullDamageAction {
    Bypass,
    Probe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversionCoverage {
    Selective,
    Full,
}

impl ConversionCoverage {
    const fn needs_emit(self, summary: DamageSummary) -> bool {
        matches!(self, Self::Full) || !summary.is_clean()
    }
}

#[derive(Debug)]
struct PipelineStats {
    enc_count: u64,
    enc_ms_sum: f64,
    enc_ms_max: f64,
    bytes_sum: u64,
    hash_ms_sum: f64,
    convert_ms_sum: f64,
    damage_samples: u64,
    damage_ratio_sum: f64,
    converted_row_ratio_sum: f64,
    dirty_blocks_sum: u64,
    dirty_rows_sum: u64,
    selective_frames: u64,
    full_bypass_frames: u64,
    full_probe_frames: u64,
    full_refreshes: u64,
    last_refresh_reason: &'static str,
}

impl PipelineStats {
    fn new() -> Self {
        Self {
            enc_count: 0,
            enc_ms_sum: 0.0,
            enc_ms_max: 0.0,
            bytes_sum: 0,
            hash_ms_sum: 0.0,
            convert_ms_sum: 0.0,
            damage_samples: 0,
            damage_ratio_sum: 0.0,
            converted_row_ratio_sum: 0.0,
            dirty_blocks_sum: 0,
            dirty_rows_sum: 0,
            selective_frames: 0,
            full_bypass_frames: 0,
            full_probe_frames: 0,
            full_refreshes: 0,
            last_refresh_reason: "none",
        }
    }

    fn record_damage(&mut self, summary: DamageSummary, hash_ms: f64) {
        self.hash_ms_sum += hash_ms;
        self.damage_samples += 1;
        self.damage_ratio_sum += summary.damage_ratio();
        self.converted_row_ratio_sum += summary.converted_row_ratio();
        self.dirty_blocks_sum += summary.dirty_blocks as u64;
        self.dirty_rows_sum += summary.dirty_block_rows as u64;
    }

    fn record_conversion(&mut self, elapsed_ms: f64) {
        self.convert_ms_sum += elapsed_ms;
    }

    fn record_encode(&mut self, elapsed_ms: f64, bytes: usize) {
        self.enc_count += 1;
        self.enc_ms_sum += elapsed_ms;
        self.enc_ms_max = self.enc_ms_max.max(elapsed_ms);
        self.bytes_sum += bytes as u64;
    }

    fn record_full_refresh(&mut self, reason: &'static str) {
        self.full_refreshes += 1;
        self.last_refresh_reason = reason;
    }

    fn log_and_reset(
        &mut self,
        capture: (u64, u64, u64),
        want_idr: bool,
        kernel: Option<HashKernel>,
        mode: DamageMode,
        pool: PoolStats,
        activity: &str,
    ) {
        let enc_avg = average(self.enc_ms_sum, self.enc_count);
        let hash_avg = average(self.hash_ms_sum, self.damage_samples);
        let dirty_blocks_avg = average_u64(self.dirty_blocks_sum, self.damage_samples);
        let dirty_rows_avg = average_u64(self.dirty_rows_sum, self.damage_samples);
        let damage_ratio_avg = average(self.damage_ratio_sum, self.damage_samples);
        let converted_row_ratio_avg = average(self.converted_row_ratio_sum, self.damage_samples);
        let kernel = kernel.map_or("none", |kernel| match kernel {
            HashKernel::Xxh3 => "Xxh3",
            HashKernel::Crc32c => "Crc32c",
        });
        log(&format!(
            "enc_fps={} avg_encode_ms={enc_avg:.2} max_encode_ms={:.2} kbps={} \
             capture_new={} capture_empty={} hash_kernel={kernel} avg_hash_ms={hash_avg:.2} \
             convert_ms={:.2} damage_samples={} avg_damage_ratio={damage_ratio_avg:.4} \
             avg_converted_row_ratio={converted_row_ratio_avg:.4} \
             avg_dirty_blocks={dirty_blocks_avg:.1} avg_dirty_rows={dirty_rows_avg:.1} \
             damage_mode={} probe_countdown={} selective_frames={} \
             full_bypass_frames={} full_probe_frames={} full_refreshes={} \
             last_refresh={} mft_input_allocations={} mft_input_reuses={} \
             mft_output_allocations={} mft_output_reuses={} want_idr={want_idr} {activity}",
            self.enc_count,
            self.enc_ms_max,
            self.bytes_sum * 8 / 1000,
            capture.0,
            capture.1 + capture.2,
            self.convert_ms_sum,
            self.damage_samples,
            mode.name(),
            mode.probe_countdown(),
            self.selective_frames,
            self.full_bypass_frames,
            self.full_probe_frames,
            self.full_refreshes,
            self.last_refresh_reason,
            pool.input_allocations,
            pool.input_reuses,
            pool.output_allocations,
            pool.output_reuses,
        ));
        *self = Self::new();
    }
}

fn average(sum: f64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        sum / count as f64
    }
}

fn average_u64(sum: u64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        sum as f64 / count as f64
    }
}

#[cfg(feature = "mf")]
pub(crate) fn run(opts: MfRunOpts) -> ! {
    let profile = opts.profile;
    let gop_secs = opts.gop_secs;
    std::process::exit(run_inner(
        common_mf_options(opts),
        EncoderKind::MediaFoundation { profile, gop_secs },
    ))
}

#[cfg(feature = "software-h264")]
pub(crate) fn run_openh264(opts: OpenH264RunOpts) -> ! {
    std::process::exit(run_inner(
        common_openh264_options(opts),
        EncoderKind::OpenH264,
    ))
}

#[cfg(feature = "mf")]
fn common_mf_options(opts: MfRunOpts) -> CommonRunOpts {
    CommonRunOpts {
        output_index: opts.output_index,
        fps: opts.fps,
        bitrate_kbps: opts.bitrate_kbps,
        framed: opts.framed,
        adapter_hint: opts.adapter_hint,
        adapter_output_index: opts.adapter_output_index,
        device_name: opts.device_name,
        cursor_mode: opts.cursor_mode,
        color: opts.color,
    }
}

#[cfg(feature = "software-h264")]
fn common_openh264_options(opts: OpenH264RunOpts) -> CommonRunOpts {
    CommonRunOpts {
        output_index: opts.output_index,
        fps: opts.fps,
        bitrate_kbps: opts.bitrate_kbps,
        framed: opts.framed,
        adapter_hint: opts.adapter_hint,
        adapter_output_index: opts.adapter_output_index,
        device_name: opts.device_name,
        cursor_mode: opts.cursor_mode,
        color: opts.color,
    }
}

#[cfg(feature = "mf")]
pub(crate) fn run_admission_probe(
    opts: MfRunOpts,
    probe: &crate::admission_probe::AdmissionProbeOptions,
) -> i32 {
    let profile = opts.profile;
    let gop_secs = opts.gop_secs;
    run_synthetic_admission(
        common_mf_options(opts),
        EncoderKind::MediaFoundation { profile, gop_secs },
        probe,
    )
}

#[cfg(feature = "software-h264")]
pub(crate) fn run_openh264_admission_probe(
    opts: OpenH264RunOpts,
    probe: &crate::admission_probe::AdmissionProbeOptions,
) -> i32 {
    run_synthetic_admission(common_openh264_options(opts), EncoderKind::OpenH264, probe)
}

fn run_synthetic_admission(
    opts: CommonRunOpts,
    encoder_kind: EncoderKind,
    probe: &crate::admission_probe::AdmissionProbeOptions,
) -> i32 {
    let (_device, _context, adapter_desc, _monitor) = match unsafe {
        pick_device(
            opts.output_index,
            opts.adapter_hint.as_deref(),
            opts.adapter_output_index,
            opts.device_name.as_deref(),
        )
    } {
        Ok(device) => device,
        Err(error) => {
            log(&format!(
                "software admission exact-output resolution failed: {error:?}"
            ));
            return 2;
        }
    };
    log(&format!(
        "software admission bound exact output on adapter {adapter_desc:?}"
    ));
    if encoder_kind.requires_macroblock_alignment()
        && !h264_surface_is_aligned(probe.width, probe.height)
    {
        log("software admission geometry is not 16-aligned");
        return 3;
    }
    let mut encoder = match ActiveEncoder::new(encoder_kind, probe.width, probe.height, &opts) {
        Ok(encoder) => encoder,
        Err(error) => {
            log(&format!("software admission encoder init failed: {error}"));
            return 4;
        }
    };
    encoder.fill_black();
    let mut frame = 0u8;
    let result = crate::admission_probe::run_probe_loop(probe, std::io::stdout().lock(), |input| {
        frame = frame.wrapping_add(1);
        encoder.prepare_synthetic(input.kind, input.dirty_ratio.basis_points(), frame);
        let started = Instant::now();
        let output = encoder.encode(input.force_idr)?;
        Ok(crate::admission_probe::ProbeEncodeResult {
            encode_latency: started.elapsed(),
            delivered: output.is_some_and(|output| !output.bytes().is_empty()),
        })
    });
    match result {
        Ok(()) => 0,
        Err(error) => {
            log(&format!("software admission probe failed: {error}"));
            5
        }
    }
}

fn run_inner(opts: CommonRunOpts, encoder_kind: EncoderKind) -> i32 {
    let backend_label = encoder_kind.label();
    // Keep the (virtual) display awake so WGC keeps receiving DWM composites.
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
    }
    log(&format!(
        "{backend_label}: display keep-awake armed (ES_DISPLAY_REQUIRED|ES_CONTINUOUS)"
    ));

    let (device, context, adapter_desc, monitor) = match unsafe {
        pick_device(
            opts.output_index,
            opts.adapter_hint.as_deref(),
            opts.adapter_output_index,
            opts.device_name.as_deref(),
        )
    } {
        Ok(v) => v,
        Err(e) => {
            log(&format!("{backend_label}: adapter init failed: {e:?}"));
            return 2;
        }
    };
    log(&format!("{backend_label}: bound adapter '{adapter_desc}'"));

    let capture = match unsafe {
        WgcCapture::from_device(device.clone(), context.clone(), monitor, opts.cursor_mode)
    } {
        Ok(c) => c,
        Err(e) => {
            log(&format!("{backend_label}: WGC init failed: {e:?}"));
            return 2;
        }
    };
    let cap_width = capture.width;
    let cap_height = capture.height;
    if cap_width == 0 || cap_height == 0 {
        log(&format!(
            "{backend_label}: capture reported invalid size {cap_width}x{cap_height}"
        ));
        return 3;
    }
    if encoder_kind.requires_macroblock_alignment()
        && !h264_surface_is_aligned(cap_width, cap_height)
    {
        log(&format!(
            "{backend_label}: capture {cap_width}x{cap_height} is not 16-aligned"
        ));
        return 3;
    }
    let (width, height) = (cap_width, cap_height);

    let mut encoder = match ActiveEncoder::new(encoder_kind, width, height, &opts) {
        Ok(e) => e,
        Err(e) => {
            log(&format!("software encoder init failed: {e}"));
            return 4;
        }
    };
    // `opts.color` — the exact value `ActiveEncoder::new` just configured
    // the encoder with, above — not a separately re-derived
    // `ColorSpec::legacy(...)`, so the READY line and the actual encode
    // cannot disagree.
    let ready_plan = match crate::resolved_media_plan(
        encoder.backend(),
        "h264",
        opts.color,
        width,
        height,
        opts.fps,
        opts.cursor_mode,
    ) {
        Ok(plan) => plan,
        Err(error) => {
            log(&error);
            return 4;
        }
    };

    let control = crate::spawn_control_thread("software encoder");

    let target_dt = crate::frame_interval_from_fps(opts.fps);
    let mut next = Instant::now();
    let mut first = true;
    let mut have_frame = false;
    // True when y_plane/uv_plane hold content the client has not seen yet.
    let mut dirty = false;
    let mut last_emit = Instant::now();
    let mut last_full_refresh = Instant::now();
    let mut staging: Option<ID3D11Texture2D> = None;
    // Region-owned activity scheduler (`arcen_media::RegionActivityScheduler`).
    // It owns the Keel damage tracker that already drives selective conversion,
    // so the same single hash pass now also feeds this region's 16x16 activity
    // grid, cadence recommendation, and bounded per-region diagnostics. It can
    // only ever *add* a mandatory service; the host-authoritative full-refresh,
    // IDR, and encoder/chroma policy below is untouched.
    let mut schedule = match crate::region_schedule::CaptureRegionScheduler::try_new(
        opts.output_index,
        width as usize,
        height as usize,
        opts.fps,
        IDLE_KEEPALIVE,
        FULL_REFRESH_INTERVAL,
        REGION_INPUT_WAKE_GRACE,
    ) {
        Ok(schedule) => schedule,
        Err(error) => {
            log(&format!(
                "{backend_label}: initialize region activity scheduler: {error}"
            ));
            return 4;
        }
    };
    let mut damage_mode = DamageMode::Selective;
    let mut damage_baseline_ready = false;
    let mut idr_planes_refreshed = false;
    let started = Instant::now();
    let mut sec = Instant::now();
    let mut stats = PipelineStats::new();
    let mut dbg = (0u64, 0u64, 0u64);
    let mut stdout = std::io::stdout();
    let mut ready_announced = false;

    let mut cap = capture;
    while !control.stop_requested() {
        let mut stage_error: Option<String> = None;
        let mut captured_new_frame = false;
        let _ = unsafe {
            cap.acquire_into(&mut dbg, &mut |tex: &ID3D11Texture2D| {
                if let Err(error) = stage_texture(
                    &device,
                    &context,
                    tex,
                    &mut staging,
                    width as usize,
                    height as usize,
                ) {
                    stage_error = Some(format!("{error:?}"));
                } else {
                    captured_new_frame = true;
                }
            })
        };
        if let Some(error) = stage_error {
            log(&format!("{backend_label}: stage error: {error}"));
            return 3;
        }

        // Serve black until content parity with the NVENC path.
        if !have_frame && started.elapsed().as_millis() >= 1000 {
            encoder.fill_black();
            have_frame = true;
            dirty = true;
            log(&format!(
                "{backend_label}: no desktop frame after 1s — streaming black until content arrives"
            ));
        }

        let now = Instant::now();
        let idr_pending = control.idr_pending();
        let mandatory_refresh = full_refresh_reason(
            staging.is_some(),
            damage_baseline_ready,
            idr_pending,
            idr_planes_refreshed,
            last_full_refresh.elapsed(),
        );
        if let Some(staging) = staging
            .as_ref()
            .filter(|_| captured_new_frame || mandatory_refresh.is_some())
        {
            let result = unsafe {
                process_staged_frame(
                    &context,
                    staging,
                    &mut schedule,
                    &mut damage_mode,
                    &mut damage_baseline_ready,
                    &mut encoder,
                    width as usize,
                    height as usize,
                    mandatory_refresh,
                    now,
                    control.take_input_activity(),
                    &mut stats,
                )
            };
            match result {
                Ok(content_changed) => {
                    have_frame = true;
                    dirty |= content_changed;
                    if mandatory_refresh.is_some() {
                        last_full_refresh = now;
                        if mandatory_refresh == Some("forced-idr") {
                            idr_planes_refreshed = true;
                        }
                    }
                }
                Err(error) => {
                    log(&format!(
                        "{backend_label}: Keel map/convert error: {error:?}"
                    ));
                    return 3;
                }
            }
        }

        let due = dirty || first || idr_pending || last_emit.elapsed() >= IDLE_KEEPALIVE;
        if now >= next && have_frame && due {
            // Consume only the request observed before mandatory-refresh
            // selection. A command racing in after that point stays pending
            // until its full-plane refresh runs on the next iteration.
            let requested_idr = idr_pending && control.take_idr();
            let force = first || requested_idr;
            if requested_idr {
                idr_planes_refreshed = false;
            }
            let t0 = Instant::now();
            match encoder.encode(force) {
                Ok(Some(au)) => {
                    let ms = elapsed_ms(t0);
                    first = false;
                    dirty = false;
                    last_emit = now;
                    let bytes = au.bytes();
                    if !ready_announced {
                        if bytes.is_empty()
                            || bytes.len() > crate::MAX_ACCESS_UNIT_BYTES
                            || crate::announce_ready(ready_plan).is_err()
                        {
                            log("could not emit READY after first in-memory access unit");
                            return 5;
                        }
                        ready_announced = true;
                    }
                    if crate::write_access_unit(&mut stdout, bytes, opts.framed).is_err() {
                        return 0;
                    }
                    stats.record_encode(ms, bytes.len());
                }
                Ok(None) => {
                    // Encoder buffered the frame; keep `dirty` so the next
                    // pacing tick drains it instead of waiting for keepalive.
                }
                Err(e) => {
                    log(&format!("software encode error: {e}"));
                    return 5;
                }
            }
            next += target_dt;
            if next < now {
                next = now + target_dt;
            }
        }

        if sec.elapsed().as_secs_f64() >= 1.0 {
            stats.log_and_reset(
                dbg,
                control.idr_pending(),
                schedule.kernel(),
                damage_mode,
                encoder.pool_stats(),
                &schedule.telemetry_fragment(),
            );
            dbg = (0, 0, 0);
            sec = Instant::now();
        }
    }
    log("software encoder control closed; dropping capture and encoder before exit");
    0
}

fn h264_surface_is_aligned(width: u32, height: u32) -> bool {
    width >= 16 && height >= 16 && width.is_multiple_of(16) && height.is_multiple_of(16)
}

fn full_refresh_reason(
    have_staging: bool,
    baseline_ready: bool,
    idr_pending: bool,
    idr_planes_refreshed: bool,
    since_last_refresh: Duration,
) -> Option<&'static str> {
    if !have_staging {
        None
    } else if !baseline_ready {
        Some("first-frame")
    } else if idr_pending && !idr_planes_refreshed {
        Some("forced-idr")
    } else if since_last_refresh >= FULL_REFRESH_INTERVAL {
        Some("periodic-2s")
    } else {
        None
    }
}

/// Copy the newest WGC texture into a reusable CPU-readable staging texture.
unsafe fn stage_texture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    staging: &mut Option<ID3D11Texture2D>,
    width: usize,
    height: usize,
) -> windows::core::Result<()> {
    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
    src.GetDesc(&mut src_desc);
    if src_desc.Width as usize != width || src_desc.Height as usize != height {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            format!(
                "capture size {}x{} differs from MF surface {width}x{height}",
                src_desc.Width, src_desc.Height
            ),
        ));
    }

    // Recreate staging if it's missing or size changed.
    let needs_new = match staging.as_ref() {
        None => true,
        Some(existing) => {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            existing.GetDesc(&mut d);
            d.Width != src_desc.Width || d.Height != src_desc.Height || d.Format != src_desc.Format
        }
    };
    if needs_new {
        let mut desc = src_desc;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        *staging = Some(tex.expect("staging texture"));
    }
    let stage = staging.as_ref().expect("staging texture ready");

    let src_res: ID3D11Resource = src.cast()?;
    let dst_res: ID3D11Resource = stage.cast()?;
    context.CopyResource(&dst_res, &src_res);
    Ok(())
}

/// Map the retained staging texture and run one synchronous pixel operation.
///
/// The staging texture remains valid after this call, allowing forced IDR and
/// periodic full refreshes even when WGC has not delivered another frame.
unsafe fn with_mapped_staging<T>(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    operation: impl FnOnce(&[u8], usize) -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    staging.GetDesc(&mut desc);
    let resource: ID3D11Resource = staging.cast()?;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    context.Map(&resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    if mapped.pData.is_null() {
        // The Map call SUCCEEDED, so the subresource is mapped and must be
        // unmapped even on this error path — a stuck mapping fails every
        // subsequent Map and kills the encode loop.
        context.Unmap(&resource, 0);
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_POINTER,
            "staging Map returned null",
        ));
    }
    let src_stride = mapped.RowPitch as usize;
    let src_len = src_stride * (desc.Height as usize);
    let bgra = std::slice::from_raw_parts(mapped.pData as *const u8, src_len);
    let result = operation(bgra, src_stride);
    context.Unmap(&resource, 0);
    result
}

#[allow(clippy::too_many_arguments)]
unsafe fn process_staged_frame(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    schedule: &mut crate::region_schedule::CaptureRegionScheduler,
    mode: &mut DamageMode,
    baseline_ready: &mut bool,
    encoder: &mut ActiveEncoder,
    width: usize,
    height: usize,
    mandatory_refresh: Option<&'static str>,
    now: Instant,
    input_activity: bool,
    stats: &mut PipelineStats,
) -> windows::core::Result<bool> {
    with_mapped_staging(context, staging, |bgra, src_stride| {
        let frame = BgraFrame::new(bgra, width, height, src_stride)
            .map_err(|error| windows::core::Error::new(E_INVALIDARG, error.to_string()))?;

        if let Some(reason) = mandatory_refresh {
            let hash_started = Instant::now();
            let decision = schedule.observe(
                frame,
                now,
                Some(forced_keyframe_for(reason)),
                input_activity,
                ActivityHint::None,
            );
            let summary = decision.summary;
            stats.record_damage(summary, elapsed_ms(hash_started));

            let convert_started = Instant::now();
            encoder
                .convert_full(frame)
                .map_err(|error| windows::core::Error::new(E_INVALIDARG, error))?;
            stats.record_conversion(elapsed_ms(convert_started));
            stats.record_full_refresh(reason);

            if *baseline_ready {
                match mode {
                    DamageMode::Selective => mode.on_selective_sample(summary),
                    DamageMode::FullDamage { .. } => mode.on_full_damage_probe(summary),
                }
            } else {
                *baseline_ready = true;
            }
            // Mandatory refreshes are not merely a local cache repair: submit
            // the rebuilt full planes so a theoretical prior hash collision is
            // healed on the client immediately.
            return Ok(ConversionCoverage::Full.needs_emit(summary) || decision.mandatory);
        }

        match mode {
            DamageMode::Selective => {
                let hash_started = Instant::now();
                let decision =
                    schedule.observe(frame, now, None, input_activity, ActivityHint::None);
                let summary = decision.summary;
                stats.record_damage(summary, elapsed_ms(hash_started));

                let convert_started = Instant::now();
                match schedule.damage_map() {
                    Some(map) => {
                        for rows in map.dirty_block_rows() {
                            encoder
                                .convert_rows(frame, rows)
                                .map_err(|error| windows::core::Error::new(E_INVALIDARG, error))?;
                        }
                    }
                    None => encoder
                        .convert_full(frame)
                        .map_err(|error| windows::core::Error::new(E_INVALIDARG, error))?,
                }
                stats.record_conversion(elapsed_ms(convert_started));
                stats.selective_frames += 1;
                mode.on_selective_sample(summary);
                // Measured activity may only *add* an emit here (a bounded
                // max-idle or keyframe deadline); it never suppresses one.
                Ok(ConversionCoverage::Selective.needs_emit(summary) || decision.mandatory)
            }
            DamageMode::FullDamage { .. } => match mode.full_damage_action() {
                FullDamageAction::Bypass => {
                    let convert_started = Instant::now();
                    encoder
                        .convert_full(frame)
                        .map_err(|error| windows::core::Error::new(E_INVALIDARG, error))?;
                    stats.record_conversion(elapsed_ms(convert_started));
                    stats.full_bypass_frames += 1;
                    // Bypass frames deliberately skip the hash, so the region
                    // records the service without an activity observation.
                    schedule.note_external_service(false);
                    Ok(true)
                }
                FullDamageAction::Probe => {
                    let hash_started = Instant::now();
                    let decision =
                        schedule.observe(frame, now, None, input_activity, ActivityHint::None);
                    let summary = decision.summary;
                    stats.record_damage(summary, elapsed_ms(hash_started));

                    let convert_started = Instant::now();
                    encoder
                        .convert_full(frame)
                        .map_err(|error| windows::core::Error::new(E_INVALIDARG, error))?;
                    stats.record_conversion(elapsed_ms(convert_started));
                    stats.full_probe_frames += 1;
                    mode.on_full_damage_probe(summary);
                    // The tracker baseline predates bypass frames, so a clean
                    // probe can still differ from the last frame submitted to
                    // the client. Every probe performs a full conversion and
                    // must therefore be emitted.
                    Ok(ConversionCoverage::Full.needs_emit(summary) || decision.mandatory)
                }
            },
        }
    })
}

/// Maps this host's authoritative full-refresh reason onto the shared
/// scheduler's forced-keyframe contract. The host decides *when*; the shared
/// adapter only records that measured activity may not override it.
fn forced_keyframe_for(reason: &str) -> ForcedKeyframe {
    match reason {
        "first-frame" => ForcedKeyframe::Startup,
        "forced-idr" => ForcedKeyframe::ClientRequest,
        _ => ForcedKeyframe::Recovery,
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Pick the D3D11 device and monitor for MF live encode.
///
/// Pier supplies adapter name + adapter-local output + Win32 device name after
/// resolving the configured target in the interactive session. Standalone
/// invocations retain the positional global attached-output index. Delegates
/// the actual enumeration/match/ambiguity decision to
/// `crate::win::resolve_output` — the same resolution DDA/WGC use — so an
/// `adapter=`/`adapter-output=`/`device=` selector picks the identical DXGI
/// output regardless of which encoder backend this process ends up running.
///
/// Returns (device, context, adapter_name, selected_monitor).
unsafe fn pick_device(
    global_output_index: u32,
    adapter_hint: Option<&str>,
    adapter_output_index: Option<u32>,
    device_name: Option<&str>,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext, String, HMONITOR)> {
    let selector = crate::win::OutputSelector {
        global_output_index,
        adapter_hint,
        adapter_output_index,
        device_name,
    };
    let resolved = crate::win::resolve_output(&selector)?;
    log(&format!(
        "software encoder: bound {} -> adapter {} ({:?})",
        selector.describe(),
        resolved.adapter_index,
        resolved.adapter_name
    ));

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        &resolved.adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )?;
    Ok((
        device.expect("D3D11 device"),
        context.expect("D3D11 context"),
        resolved.adapter_name,
        resolved.monitor,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        forced_keyframe_for, full_refresh_reason, h264_surface_is_aligned, ConversionCoverage,
        DamageMode, FullDamageAction, FULL_DAMAGE_PROBE_INTERVAL, FULL_REFRESH_INTERVAL,
    };
    use arcen_keel::scenario::{Scenario, ScenarioKind};
    use arcen_keel::{BgraFrame, DamageSummary, DamageTracker, KernelPreference};
    use arcen_media::video::ColorTransform;
    use arcen_media::video::{convert_bgra_to_nv12, convert_bgra_to_nv12_rows, Nv12FrameMut};
    use arcen_media::ForcedKeyframe;
    use std::time::Duration;

    #[test]
    fn every_host_full_refresh_reason_forces_a_region_keyframe() {
        // Measured activity may never suppress a service the host already
        // decided is mandatory, so each reason must map onto a forced keyframe.
        assert_eq!(
            forced_keyframe_for("first-frame"),
            ForcedKeyframe::Startup,
            "startup baseline"
        );
        assert_eq!(
            forced_keyframe_for("forced-idr"),
            ForcedKeyframe::ClientRequest,
            "client IDR request"
        );
        assert_eq!(
            forced_keyframe_for("periodic-2s"),
            ForcedKeyframe::Recovery,
            "periodic recovery refresh"
        );
        for reason in [
            full_refresh_reason(true, false, false, false, Duration::ZERO),
            full_refresh_reason(true, true, true, false, Duration::ZERO),
            full_refresh_reason(true, true, false, false, FULL_REFRESH_INTERVAL),
        ] {
            let reason = reason.expect("mandatory refresh reason");
            assert!(
                matches!(
                    forced_keyframe_for(reason),
                    ForcedKeyframe::Startup
                        | ForcedKeyframe::ClientRequest
                        | ForcedKeyframe::Recovery
                ),
                "{reason} must force a keyframe"
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bgra_to_nv12(
        source: &[u8],
        source_stride: usize,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
        width: usize,
        height: usize,
    ) {
        let frame = BgraFrame::new(source, width, height, source_stride).expect("BGRA");
        let mut destination =
            Nv12FrameMut::new(width as u32, height as u32, y, y_stride, uv, uv_stride)
                .expect("NV12");
        convert_bgra_to_nv12(
            frame,
            &mut destination,
            ColorTransform::legacy_bt709_limited(),
        )
        .expect("conversion");
    }

    #[allow(clippy::too_many_arguments)]
    fn bgra_to_nv12_rows(
        source: &[u8],
        source_stride: usize,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
        width: usize,
        height: usize,
        rows: std::ops::Range<usize>,
    ) {
        let frame = BgraFrame::new(source, width, height, source_stride).expect("BGRA");
        let mut destination =
            Nv12FrameMut::new(width as u32, height as u32, y, y_stride, uv, uv_stride)
                .expect("NV12");
        convert_bgra_to_nv12_rows(
            frame,
            &mut destination,
            rows,
            ColorTransform::legacy_bt709_limited(),
        )
        .expect("conversion");
    }

    #[test]
    fn mf_requires_an_aligned_capture_surface() {
        assert!(h264_surface_is_aligned(1920, 1088));
        assert!(h264_surface_is_aligned(1792, 1168));
        assert!(!h264_surface_is_aligned(1920, 1080));
        assert!(!h264_surface_is_aligned(1800, 1168));
        assert!(!h264_surface_is_aligned(1497, 826));
    }

    #[test]
    fn full_damage_bypass_is_bounded_and_hysteretic() {
        let mut mode = DamageMode::Selective;
        mode.on_selective_sample(DamageSummary {
            dirty_blocks: 1,
            total_blocks: 100,
            dirty_block_rows: 8,
            total_block_rows: 10,
        });
        assert!(matches!(mode, DamageMode::FullDamage { .. }));
        for _ in 0..FULL_DAMAGE_PROBE_INTERVAL {
            assert_eq!(mode.full_damage_action(), FullDamageAction::Bypass);
        }
        assert_eq!(mode.full_damage_action(), FullDamageAction::Probe);
        mode.on_full_damage_probe(DamageSummary {
            dirty_blocks: 99,
            total_blocks: 100,
            dirty_block_rows: 2,
            total_block_rows: 10,
        });
        assert_eq!(mode, DamageMode::Selective);
    }

    #[test]
    fn mandatory_refreshes_cover_first_idr_periodic_without_repeating_idr() {
        assert_eq!(
            full_refresh_reason(true, false, true, false, FULL_REFRESH_INTERVAL),
            Some("first-frame")
        );
        assert_eq!(
            full_refresh_reason(true, true, true, false, Duration::ZERO),
            Some("forced-idr")
        );
        assert_eq!(
            full_refresh_reason(true, true, true, true, Duration::ZERO),
            None
        );
        assert_eq!(
            full_refresh_reason(true, true, false, false, FULL_REFRESH_INTERVAL),
            Some("periodic-2s")
        );
        assert_eq!(
            full_refresh_reason(false, false, true, false, FULL_REFRESH_INTERVAL),
            None
        );
    }

    #[test]
    fn full_conversion_is_emitted_even_when_probe_hash_matches_old_baseline() {
        let clean_probe = DamageSummary {
            dirty_blocks: 0,
            total_blocks: 100,
            dirty_block_rows: 0,
            total_block_rows: 10,
        };
        assert!(ConversionCoverage::Full.needs_emit(clean_probe));
        assert!(!ConversionCoverage::Selective.needs_emit(clean_probe));
    }

    #[test]
    fn selective_conversion_matches_full_conversion_for_keel_corpus() {
        const WIDTH: usize = 96;
        const HEIGHT: usize = 64;
        for preference in [KernelPreference::Xxh3, KernelPreference::Crc32c] {
            for kind in [
                ScenarioKind::Idle,
                ScenarioKind::Typing,
                ScenarioKind::Drag,
                ScenarioKind::Scroll,
                ScenarioKind::Video,
                ScenarioKind::Burst,
            ] {
                let scenario = Scenario::new(WIDTH, HEIGHT, kind, 42);
                let mut tracker =
                    DamageTracker::new(WIDTH, HEIGHT, preference).expect("valid tracker");
                let mut bgra = Vec::new();
                let mut selected_y = vec![0u8; WIDTH * HEIGHT];
                let mut selected_uv = vec![0u8; WIDTH * HEIGHT / 2];

                for tick in 0..16 {
                    scenario.render(tick, &mut bgra);
                    let frame =
                        BgraFrame::new(&bgra, WIDTH, HEIGHT, scenario.stride()).expect("frame");
                    tracker.update(frame).expect("damage update");
                    for rows in tracker.damage_map().dirty_block_rows() {
                        bgra_to_nv12_rows(
                            &bgra,
                            scenario.stride(),
                            &mut selected_y,
                            WIDTH,
                            &mut selected_uv,
                            WIDTH,
                            WIDTH,
                            HEIGHT,
                            rows,
                        );
                    }

                    let mut full_y = vec![0u8; WIDTH * HEIGHT];
                    let mut full_uv = vec![0u8; WIDTH * HEIGHT / 2];
                    bgra_to_nv12(
                        &bgra,
                        scenario.stride(),
                        &mut full_y,
                        WIDTH,
                        &mut full_uv,
                        WIDTH,
                        WIDTH,
                        HEIGHT,
                    );
                    assert_eq!(selected_y, full_y, "{preference:?} {kind:?} tick {tick}");
                    assert_eq!(selected_uv, full_uv, "{preference:?} {kind:?} tick {tick}");
                }
            }
        }
    }
}
