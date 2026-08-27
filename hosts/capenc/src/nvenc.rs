// NVENC encoder — D3D11 host-converted path. Loads nvEncodeAPI64.dll
// dynamically (no link dep, no CUDA) and drives NVENC via the vendored
// bindgen bindings (crate::nvenc_sys), generated from the MIT-licensed
// nv-codec-headers copy vendored under third_party/.
//
// w2-drop-argb: this used to hand NVENC the packed-BGRA `ARGB` buffer format
// and let the driver perform RGB->YCbCr with an undocumented, uncontrollable
// matrix and range — the single largest uncontrolled variable in the colour
// pipeline (see `ColorSpec`/`ColorTransform` in `arcen_media`). NVENC now
// only ever receives samples *we* converted: `NV_ENC_BUFFER_FORMAT_NV12`
// (4:2:0 8-bit), `_YUV420_10BIT` (4:2:0 10-bit / Main10), `_YUV444` (4:4:4
// 8-bit) or `_YUV444_10BIT` (4:4:4 10-bit), chosen by `resolve_pixel_format`.
// That trades the previous zero-copy GPU-only path for a CPU round trip: the
// captured D3D11 texture is copied into a CPU-readable staging texture,
// Mapped, converted with `arcen_media::video::convert_bgra_to_*`, and written
// straight into an NVENC-allocated system-memory input buffer
// (`nvEncCreateInputBuffer`/`nvEncLockInputBuffer`) instead of a registered,
// GPU-only D3D11 texture (`nvEncRegisterResource`). See `Encoder::stage` and
// `write_locked_from_bgra` for the concrete choice and why.
//
// PIPELINED (double-buffered): each slot owns its own input buffer and output
// bitstream. We SUBMIT frame N's EncodePicture, then LOCK frame N-1's
// bitstream. LockBitstream is the GPU sync point, so deferring it by one
// frame lets the CPU stage+submit the next frame while the GPU encodes the
// current one — that overlap is what sustains 4K60 (synchronous submit+lock
// serializes CPU and GPU and caps ~45-60 fps at 4K). Costs one frame of
// pipeline latency.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt::{Display, Formatter};
use std::mem::MaybeUninit;
use std::time::Instant;

// Zero-initialise a plain C struct for the driver.
//
// These are `#[repr(C)]` NVENC structs: we zero the memory, set `version`, and
// either fill the rest or let the driver fill it (`GetEncodePresetConfigEx`
// populates the config).
//
// This used to carry a comment claiming that `MaybeUninit::zeroed` was "the
// correct FFI idiom" because it "skips Rust's enum-validity assertion". That
// was wrong in a way worth recording. Four of the bindgen enums have no zero
// discriminant — NV_ENC_PARAMS_FRAME_FIELD_MODE, NV_ENC_STATE_RESTORE_TYPE,
// NV_ENC_PIC_STRUCT and NV_ENC_PIC_FLAGS all start at 1 — so materialising a
// struct containing one from zeroed bytes produced a value with an invalid
// discriminant. `MaybeUninit::zeroed().assume_init()` suppresses the *check*,
// not the *undefined behaviour*: the compiler is still entitled to assume the
// discriminant is one of the declared ones, and the driver filling the field
// later does not retroactively make the intervening value valid.
//
// Those four are now `#[repr(transparent)]` newtypes over `u32` with their
// values as associated constants, so zero is a representable value and this
// function is sound. Every other NVENC enum already includes 0.
//
// SAFETY: `T` is a `#[repr(C)]` NVENC struct whose fields are integers,
// pointers, arrays and newtypes over `u32`. All-zero is a valid bit pattern for
// every one of them, so the initialised value is valid.
#[inline]
unsafe fn zeroed<T>() -> T {
    MaybeUninit::<T>::zeroed().assume_init()
}

use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

use crate::nvenc_sys::nvEncodeAPI::*; // NV_ENC_* structs, typedefs, function list, NVENCAPI_VERSION
use crate::nvenc_sys::*; // GUIDs (guid.rs) + _VER version consts (version.rs), non-overlapping
                         // bindgen emitted rustified enums; bring the variants we use into scope so we
                         // can write NV_ENC_SUCCESS instead of _NVENCSTATUS::NV_ENC_SUCCESS everywhere.
use crate::nvenc_sys::nvEncodeAPI::_NVENCSTATUS::*;
use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_BUFFER_FORMAT::*;
use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_CAPS::*; // NV_ENC_CAPS_SUPPORT_YUV444_ENCODE
use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_DEVICE_TYPE::*;
use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_MEMORY_HEAP::*;
use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_QP_MAP_MODE::*;
use crate::nvenc_sys::nvEncodeAPI::NV_ENC_TUNING_INFO::*;

use arcen_keel::BgraFrame;
use arcen_media::video::{
    convert_bgra_to_i444, convert_bgra_to_i444_p16_rows, convert_bgra_to_nv12, ColorTransform,
    I444FrameMut, I444P16FrameMut, Nv12FrameMut, QpMapGeometry,
};
use arcen_media::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, EncodeIntent,
    TransferCharacteristics,
};

type CreateInstanceFn = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;

unsafe fn load_nvenc_runtime() -> windows::core::Result<HMODULE> {
    let name = "nvEncodeAPI64.dll"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    LoadLibraryExW(
        PCWSTR(name.as_ptr()),
        HANDLE::default(),
        LOAD_LIBRARY_SEARCH_SYSTEM32,
    )
}

/// H.273 colour primaries -> NVENC's enum. Shared by H.264/HEVC VUI
/// (`vui_parameters`) and AV1's sequence header colour fields
/// (`apply_av1_color`): `NV_ENC_CONFIG_AV1::colorPrimaries` is typed as the
/// exact same `NV_ENC_VUI_COLOR_PRIMARIES` enum VUI uses, so this mapping is
/// written once instead of duplicated per codec. An explicit match, not a
/// numeric cast: an H.273 value Arcen supports but NVENC does not name would
/// otherwise be written as a garbage discriminant.
fn nvenc_color_primaries(primaries: ColorPrimaries) -> NV_ENC_VUI_COLOR_PRIMARIES {
    use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_COLOR_PRIMARIES::*;

    match primaries {
        ColorPrimaries::Bt709 => NV_ENC_VUI_COLOR_PRIMARIES_BT709,
        ColorPrimaries::Bt2020 => NV_ENC_VUI_COLOR_PRIMARIES_BT2020,
        ColorPrimaries::DisplayP3 => NV_ENC_VUI_COLOR_PRIMARIES_SMPTE432,
    }
}

/// H.273 transfer characteristics -> NVENC's enum. See
/// `nvenc_color_primaries`: shared by VUI and AV1's `transferCharacteristics`,
/// which is the same `NV_ENC_VUI_TRANSFER_CHARACTERISTIC` enum.
fn nvenc_transfer_characteristics(
    transfer: TransferCharacteristics,
) -> NV_ENC_VUI_TRANSFER_CHARACTERISTIC {
    use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_TRANSFER_CHARACTERISTIC::*;

    match transfer {
        TransferCharacteristics::Bt709 => NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709,
        TransferCharacteristics::Srgb => NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB,
        TransferCharacteristics::Pq => NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084,
        TransferCharacteristics::Hlg => NV_ENC_VUI_TRANSFER_CHARACTERISTIC_ARIB_STD_B67,
    }
}

/// H.273 matrix coefficients -> NVENC's enum. See `nvenc_color_primaries`:
/// shared by VUI and AV1's `matrixCoefficients`, which is the same
/// `NV_ENC_VUI_MATRIX_COEFFS` enum. `NV_ENC_VUI_MATRIX_COEFFS_RGB` is H.273
/// `matrix_coefficients = 0`, the identity/GBR passthrough. NVENC exposes it
/// for both; whether a client can consume the result is a probe-matrix
/// question (and AV1 can never reach it here anyway -- identity requires
/// 4:4:4, which `PixelFormatRejection::Av1RequiresYuv420` already refuses).
fn nvenc_matrix_coefficients(matrix: ColorMatrix) -> NV_ENC_VUI_MATRIX_COEFFS {
    use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_MATRIX_COEFFS::*;

    match matrix {
        ColorMatrix::Identity => NV_ENC_VUI_MATRIX_COEFFS_RGB,
        ColorMatrix::Bt709 => NV_ENC_VUI_MATRIX_COEFFS_BT709,
        ColorMatrix::Bt601 => NV_ENC_VUI_MATRIX_COEFFS_SMPTE170M,
        ColorMatrix::Bt2020Ncl => NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL,
    }
}

/// Build the NVENC VUI block for one colour spec (H.264/HEVC only -- AV1
/// does not use VUI at all; see `apply_av1_color`).
fn vui_parameters(color: crate::ColorSpec) -> NV_ENC_CONFIG_H264_VUI_PARAMETERS {
    use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_VIDEO_FORMAT::*;

    let mut vui: NV_ENC_CONFIG_H264_VUI_PARAMETERS = unsafe { std::mem::zeroed() };
    // Both present flags must be set or the decoder is entitled to ignore
    // everything below them.
    vui.videoSignalTypePresentFlag = 1;
    vui.colourDescriptionPresentFlag = 1;
    vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui.videoFullRangeFlag = u32::from(matches!(color.range, ColorRange::Full));
    vui.colourPrimaries = nvenc_color_primaries(color.primaries);
    vui.transferCharacteristics = nvenc_transfer_characteristics(color.transfer);
    vui.colourMatrix = nvenc_matrix_coefficients(color.matrix);
    vui
}

/// Set AV1's sequence-header colour fields from one colour spec.
///
/// AV1 does not use H.264/HEVC VUI at all: its colour info lives directly in
/// the sequence header's `color_config()` (AV1 spec), which
/// `NV_ENC_CONFIG_AV1` mirrors with its own `colorPrimaries`/
/// `transferCharacteristics`/`matrixCoefficients` fields -- typed as the
/// exact same `NV_ENC_VUI_COLOR_PRIMARIES`/`_TRANSFER_CHARACTERISTIC`/
/// `_MATRIX_COEFFS` enums VUI uses, reused via `nvenc_color_primaries` and
/// friends -- plus a plain `u32` `colorRange` (there is no separate
/// present-flag pair to set, unlike VUI's
/// `videoSignalTypePresentFlag`/`colourDescriptionPresentFlag`).
fn apply_av1_color(config: &mut NV_ENC_CONFIG_AV1, color: crate::ColorSpec) {
    config.colorPrimaries = nvenc_color_primaries(color.primaries);
    config.transferCharacteristics = nvenc_transfer_characteristics(color.transfer);
    config.matrixCoefficients = nvenc_matrix_coefficients(color.matrix);
    config.colorRange = u32::from(matches!(color.range, ColorRange::Full));
}

/// One pipeline slot: its own NVENC-allocated system-memory input buffer and
/// its own output bitstream buffer, so frame N and N-1 never collide.
struct Slot {
    input_buffer: NV_ENC_INPUT_PTR,
    bitstream: NV_ENC_OUTPUT_PTR,
}

/// Which published frame each input slot currently holds.
///
/// The encode ring advances at the target FPS even when capture is idle, so
/// every submitted slot is refreshed from `Encoder::latest` or it would replay
/// that slot's older image. On a static desktop this republishes the *same*
/// bytes over and over: at 3008x1692 I444P16 that is over 30 MiB of host
/// memcpy per idle frame, roughly 900 MiB/s at 30 fps, for no change at all.
///
/// Tracking which generation each slot already contains removes exactly those
/// redundant copies and nothing else. The rules are deliberately pessimistic:
///
/// * a generation is minted only after a **complete successful publish**,
///   which is the point at which both the slot and `Encoder::latest` are known
///   to hold the same bytes;
/// * any failed or abandoned write marks the slot **unknown**, and unknown
///   always copies; and
/// * a slot is skipped only when it holds *exactly* the newest generation.
///
/// This reduces memory traffic on static and low-change content. It does not
/// make fresh motion encode faster, and must not be reported as if it did.
#[derive(Debug, Default)]
struct SlotGenerations {
    /// Newest completely published generation, or `None` before the first.
    latest: Option<u64>,
    /// Generation each slot holds, or `None` when its contents are unknown.
    slots: Vec<Option<u64>>,
}

impl SlotGenerations {
    fn new(slots: usize) -> Self {
        Self {
            latest: None,
            slots: vec![None; slots],
        }
    }

    /// Whether any frame has been published yet.
    const fn has_latest(&self) -> bool {
        self.latest.is_some()
    }

    /// A publish into `slot` completed: the slot and `latest` now agree.
    fn published(&mut self, slot: usize) {
        let generation = self.latest.map_or(1, |current| current.saturating_add(1));
        self.latest = Some(generation);
        if let Some(entry) = self.slots.get_mut(slot) {
            *entry = Some(generation);
        }
    }

    /// A write into `slot` failed or was abandoned; its contents are unknown.
    ///
    /// `latest` is untouched: a failed write to one slot says nothing about
    /// the frame that was already published successfully elsewhere.
    fn invalidated(&mut self, slot: usize) {
        if let Some(entry) = self.slots.get_mut(slot) {
            *entry = None;
        }
    }

    /// Whether `slot` must be refreshed from `latest` before submission.
    ///
    /// Any uncertainty answers `true`: an unknown slot, an out-of-range slot,
    /// or an older generation all copy.
    fn needs_copy(&self, slot: usize) -> bool {
        match (self.latest, self.slots.get(slot)) {
            (Some(latest), Some(Some(held))) => *held != latest,
            _ => true,
        }
    }

    /// A `latest -> slot` copy completed.
    fn copied(&mut self, slot: usize) {
        let latest = self.latest;
        if let Some(entry) = self.slots.get_mut(slot) {
            *entry = latest;
        }
    }
}

/// What [`Encoder::restage_latest`] had to do to put the newest published
/// frame into the slot the next submission will use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestageOutcome {
    /// Nothing has been published yet; the caller submits blank or skips.
    NoLatest,
    /// The slot already held exactly the newest frame; no copy was needed.
    AlreadyCurrent,
    /// The newest frame was copied into the slot.
    Copied,
}

impl RestageOutcome {
    /// Whether a frame is staged and ready to submit.
    #[must_use]
    pub const fn is_staged(self) -> bool {
        matches!(self, Self::AlreadyCurrent | Self::Copied)
    }
}

struct NvencLibrary(Option<HMODULE>);

impl NvencLibrary {
    fn new(module: HMODULE) -> Self {
        Self(Some(module))
    }
}

impl Drop for NvencLibrary {
    fn drop(&mut self) {
        if let Some(module) = self.0.take() {
            unsafe {
                let _ = FreeLibrary(module);
            }
        }
    }
}

/// Per-session damage tracking and QP-map state.
///
/// Separate from [`Encoder`] only so the three pieces that must agree on one
/// frame geometry are constructed together and cannot drift apart.
struct QpMapState {
    tracker: arcen_keel::DamageTracker,
    builder: arcen_media::video::QpDeltaMapBuilder,
    bias: arcen_media::video::QpBias,
    policy: crate::qp_map::QpMapPolicy,
    /// Set by `stage`, cleared by `encode`. A frame submitted without a fresh
    /// observation — `restage_latest`, or a blank submitted by the frame
    /// policy — carries no new damage, and biasing it from a stale map would
    /// describe a frame that is no longer on screen.
    observed: bool,
}

pub struct Encoder {
    _library: NvencLibrary,
    fl: NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    context: ID3D11DeviceContext,
    slots: Vec<Slot>,
    // CPU-readable copy of the newest desktop image. Feeding NVENC our own
    // conversion instead of ARGB (w2-drop-argb) means the pixel round trip is
    // no longer purely GPU-side: the captured texture is copied into this
    // staging texture and Mapped for CPU read, exactly mirroring win_mf.rs's
    // software-encode path (`with_mapped_staging` there).
    staging_tex: ID3D11Texture2D,
    format: PixelFormat,
    transform: ColorTransform,
    /// Damage-driven QP biasing, when the session asked for it and the driver
    /// accepted `qpMapMode` at init.
    ///
    /// Owned here rather than plumbed in from the capture loop because
    /// `stage` already holds the exact BGRA frame that is about to be
    /// encoded. Tracking damage anywhere else would risk describing a
    /// different frame than the one the map is applied to, which is the one
    /// mistake in this feature that produces a plausible-looking picture with
    /// the bias on the wrong blocks.
    qp_state: Option<QpMapState>,
    /// Entries a QP delta map must have for this session, or `0` when the
    /// encoder refused `qpMapMode` at init (see the trial init in `new`).
    /// Checked on every submission rather than trusted, because NVENC reads
    /// exactly this many entries and a short buffer would read past its end.
    qp_map_entries: usize,
    // The pitch NVENC itself reports for `format`/width/height (learned once,
    // at construction, from the first slot — see `Encoder::new`) and the
    // total bytes across every plane at that pitch. Every later Lock is
    // checked against this rather than trusted, since a silently different
    // pitch would misplace whole chroma planes.
    locked_pitch: u32,
    frame_bytes: usize,
    // Densely-mirrored bytes of the last frame NVENC's own buffer held,
    // captured straight from a locked input buffer (see `publish_bgra`).
    // The encode ring advances at the target FPS even when capture is idle;
    // every submitted slot must therefore be refreshed from here or it would
    // replay that slot's older image. Replaces the old GPU-side
    // `latest_tex`/`CopyResource` restage trick now that publishing a slot
    // means a host-memory Lock/write/Unlock instead of a GPU copy.
    latest: Vec<u8>,
    /// Which published frame each input slot holds, so an idle republish can
    /// skip a 30 MiB copy into a slot that already holds it. See
    /// [`SlotGenerations`].
    generations: SlotGenerations,
    // Slots awaiting LockBitstream.
    inflight: VecDeque<usize>,
    write_idx: usize, // slot the next stage()/encode() targets
    drain_policy: crate::nvenc_policy::OutputDrainPolicy,
    width: u32,
    height: u32,
    i444_conversion_workers: usize,
    /// Whether the staging texture holds a copied frame that has not yet been
    /// converted and published. See [`StagedCapture`].
    staged_capture: StagedCapture,
    last_copy_ms: f64,
    last_conversion_ms: f64,
    last_mirror_ms: f64,
    last_stage_timing: StageTiming,
}

/// Two-phase staging state: whether the CPU-readable staging texture currently
/// holds a captured frame that has not yet been converted and published.
///
/// Kept as its own type, with no D3D11 in it, so the transitions that matter
/// are testable on a machine with no GPU:
///
/// * a copy claims the staging texture, and a **second copy before a publish
///   replaces** the pending frame — a live desktop always wants the newest
///   image, never a queue of stale ones;
/// * a publish **consumes** the claim, so a second publish cannot re-encode
///   the same staging contents as if they were a fresh capture; and
/// * a failed copy **clears** the claim, because a partially issued copy
///   leaves the staging texture holding an indeterminate mixture of frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StagedCapture {
    pending: bool,
}

impl StagedCapture {
    /// A GPU copy into the staging texture completed.
    const fn copied(&mut self) {
        self.pending = true;
    }

    /// A copy failed or was abandoned; the staging contents are indeterminate.
    const fn copy_failed(&mut self) {
        self.pending = false;
    }

    /// Claim the pending frame for publish. `false` means nothing was copied.
    const fn take(&mut self) -> bool {
        let pending = self.pending;
        self.pending = false;
        pending
    }

    #[cfg(test)]
    const fn is_pending(self) -> bool {
        self.pending
    }
}

/// Where one staged frame spent its time.
///
/// The buckets each measure exactly one mechanism, so a hardware run can
/// attribute a regression without re-instrumenting. `copy_ms` is the
/// `CopyResource` issued while the DXGI frame is still held — the only part
/// that now blocks Desktop Duplication. `readback_ms` keeps its pre-split
/// meaning of copy plus CPU `Map`. `conversion_ms` is only the BGRA ->
/// coded-sample write into the locked NVENC buffer, and `mirror_ms` is only
/// the locked-buffer -> `Encoder::latest` copy that makes `restage_latest`
/// possible. The remainder of the caller's stage bucket is damage
/// observation, Lock/Unlock, and Unmap.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StageTiming {
    pub copy_ms: f64,
    pub readback_ms: f64,
    pub conversion_ms: f64,
    pub mirror_ms: f64,
}

struct EncoderInitGuard<'a> {
    fl: &'a NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    slots: Vec<Slot>,
}

impl Drop for EncoderInitGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            cleanup_slots(self.fl, self.enc, &mut self.slots);
            destroy_encoder(self.fl, &mut self.enc);
        }
    }
}

unsafe fn cleanup_slots(fl: &NV_ENCODE_API_FUNCTION_LIST, enc: *mut c_void, slots: &mut Vec<Slot>) {
    for slot in slots.drain(..) {
        cleanup_native_slot(fl, enc, slot.bitstream, slot.input_buffer);
    }
}

unsafe fn cleanup_native_slot(
    fl: &NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    bitstream: NV_ENC_OUTPUT_PTR,
    input_buffer: NV_ENC_INPUT_PTR,
) {
    if !bitstream.is_null() {
        if let Some(destroy) = fl.nvEncDestroyBitstreamBuffer {
            let _ = destroy(enc, bitstream);
        }
    }
    if !input_buffer.is_null() {
        if let Some(destroy) = fl.nvEncDestroyInputBuffer {
            let _ = destroy(enc, input_buffer);
        }
    }
}

unsafe fn destroy_encoder(fl: &NV_ENCODE_API_FUNCTION_LIST, enc: &mut *mut c_void) {
    if !enc.is_null() {
        if let Some(destroy) = fl.nvEncDestroyEncoder {
            let _ = destroy(*enc);
        }
        *enc = std::ptr::null_mut();
    }
}

macro_rules! nvchk {
    ($st:expr, $what:expr) => {{
        let st = $st;
        if st != NV_ENC_SUCCESS {
            return Err(format!("{} -> NVENC status {:?}", $what, st));
        }
    }};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NvencInitFailure {
    Unavailable,
    Unsupported,
    Fatal,
}

#[derive(Debug)]
pub(crate) struct NvencInitError {
    failure: NvencInitFailure,
    detail: String,
}

impl NvencInitError {
    fn runtime_unavailable(detail: impl Into<String>) -> Self {
        Self {
            failure: NvencInitFailure::Unavailable,
            detail: detail.into(),
        }
    }

    /// A `ColorSpec` NVENC cannot encode at all (see `PixelFormatRejection`),
    /// discovered before the driver is ever touched. `Unsupported` (not
    /// `Fatal`) because a software backend may still handle it — the same
    /// fallback semantics as an SDK-reported unsupported-parameter status.
    fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            failure: NvencInitFailure::Unsupported,
            detail: detail.into(),
        }
    }

    fn fatal(detail: impl Into<String>) -> Self {
        Self {
            failure: NvencInitFailure::Fatal,
            detail: detail.into(),
        }
    }

    fn from_status(status: NVENCSTATUS, operation: &'static str) -> Self {
        let failure = match status {
            NV_ENC_ERR_NO_ENCODE_DEVICE
            | NV_ENC_ERR_UNSUPPORTED_DEVICE
            | NV_ENC_ERR_DEVICE_NOT_EXIST => NvencInitFailure::Unavailable,
            NV_ENC_ERR_UNSUPPORTED_PARAM | NV_ENC_ERR_UNIMPLEMENTED => {
                NvencInitFailure::Unsupported
            }
            _ => NvencInitFailure::Fatal,
        };
        Self {
            failure,
            detail: format!("{operation} -> NVENC status {status:?}"),
        }
    }

    pub(crate) const fn allows_software_fallback(&self) -> bool {
        matches!(
            self.failure,
            NvencInitFailure::Unavailable | NvencInitFailure::Unsupported
        )
    }

    pub(crate) const fn unavailable_reason(
        &self,
    ) -> Option<arcen_media::video::BackendUnavailableReason> {
        use arcen_media::video::BackendUnavailableReason;
        match self.failure {
            NvencInitFailure::Unavailable => Some(BackendUnavailableReason::HardwareUnavailable),
            NvencInitFailure::Unsupported => {
                Some(BackendUnavailableReason::UnsupportedConfiguration)
            }
            NvencInitFailure::Fatal => None,
        }
    }
}

impl Display for NvencInitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

macro_rules! nvchk_init {
    ($st:expr, $what:expr) => {{
        let status = $st;
        if status != NV_ENC_SUCCESS {
            return Err(NvencInitError::from_status(status, $what));
        }
    }};
}

/// Query a single NVENC capability for a codec (e.g.
/// NV_ENC_CAPS_SUPPORT_YUV444_ENCODE). Returns the integer capability value
/// (0 = unsupported). Errors are swallowed to 0 — the caller treats a
/// non-positive result as "not advertised".
unsafe fn query_cap(
    fl: &NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    codec_guid: GUID,
    cap: NV_ENC_CAPS,
) -> i32 {
    let mut p: NV_ENC_CAPS_PARAM = zeroed();
    p.version = NV_ENC_CAPS_PARAM_VER;
    p.capsToQuery = cap;
    let mut val: ::core::ffi::c_int = 0;
    match fl.nvEncGetEncodeCaps {
        Some(f) => {
            if f(enc, codec_guid, &mut p, &mut val) != NV_ENC_SUCCESS {
                return 0;
            }
            val
        }
        None => 0,
    }
}

/// Report every colour capability the driver exposes for this codec.
///
/// Previously only `SUPPORT_YUV444_ENCODE` was queried, which is not enough to
/// decide anything: the caps are **independent booleans**, so a `true` for
/// 4:4:4 and a `true` for 10-bit together are not a guarantee that the
/// *combination* initialises. The only reliable answer is a trial
/// `NvEncInitializeEncoder`, so these values are logged as evidence for the
/// probe matrix rather than used as a gate.
unsafe fn log_color_capabilities(
    fl: &NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    codec_guid: GUID,
    codec: &str,
) {
    use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_CAPS::{
        NV_ENC_CAPS_SUPPORT_10BIT_ENCODE, NV_ENC_CAPS_SUPPORT_LOSSLESS_ENCODE,
        NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
    };
    let yuv444 = query_cap(fl, enc, codec_guid, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE);
    let ten_bit = query_cap(fl, enc, codec_guid, NV_ENC_CAPS_SUPPORT_10BIT_ENCODE);
    let lossless = query_cap(fl, enc, codec_guid, NV_ENC_CAPS_SUPPORT_LOSSLESS_ENCODE);
    crate::log(&format!(
        "NVENC caps (codec={codec}): yuv444={yuv444} ten_bit={ten_bit} lossless={lossless} \
         (independent booleans; the combination is only proven by InitializeEncoder)"
    ));
}

/// Whether this GPU/driver's NVENC enumerates `codec_guid` among the codecs
/// it can encode at all (`NvEncGetEncodeGUIDs`), independent of any specific
/// resolution/chroma/depth combination.
///
/// AV1 encode requires Ada Lovelace (RTX 40-series, L4, L40S) or newer (NVIDIA
/// Video Codec SDK support matrix). Unlike 4:4:4/10-bit, there is no
/// `NV_ENC_CAPS_*` boolean for "this GPU has an AV1 encoder" -- codec support
/// itself is answered only by whether `NvEncGetEncodeGUIDs` lists the codec's
/// GUID at all -- so this is checked explicitly, ahead of a trial
/// `NvEncInitializeEncoder` that would otherwise fail with an opaque NVENC
/// status and no mention of AV1 or generation at all. Only called for AV1
/// today (see `Encoder::new`); H.264/HEVC support has never needed this on
/// any GPU this codebase already runs on, and adding an extra driver round
/// trip to that long-proven path is exactly the kind of change "least
/// disruption" rules out.
unsafe fn encoder_enumerates_codec(
    fl: &NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    codec_guid: GUID,
) -> bool {
    let Some(get_count) = fl.nvEncGetEncodeGUIDCount else {
        return false;
    };
    let mut count: u32 = 0;
    if get_count(enc, &mut count) != NV_ENC_SUCCESS || count == 0 {
        return false;
    }
    let Some(get_guids) = fl.nvEncGetEncodeGUIDs else {
        return false;
    };
    let mut guids: Vec<GUID> = vec![GUID::default(); count as usize];
    let mut returned: u32 = 0;
    if get_guids(enc, guids.as_mut_ptr(), count, &mut returned) != NV_ENC_SUCCESS {
        return false;
    }
    guids[..(returned as usize).min(guids.len())].contains(&codec_guid)
}

/// The three codec strings `Encoder::new`'s caller (`win.rs`, ultimately
/// `arcen_media::VideoCodec::token`) ever passes, parsed once so every branch
/// below is an exhaustive `match` on a closed type instead of a repeated
/// string compare.
///
/// Before AV1, `codec != "h265"` was a correct (if implicit) test for "is
/// H.264" because those were the only two values `Encoder::new` was ever
/// given; it silently becomes wrong once `"av1"` exists too, since AV1 is
/// also `!= "h265"` and would take every H.264-only branch (an 8-bit-only
/// clamp that does not apply to AV1, an H.264 profile GUID instead of AV1's,
/// ...). Parsing once, here, closes that off at the type level: a `match`
/// that forgets an AV1 arm fails to compile instead of silently mishandling
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NvencCodec {
    H264,
    Hevc,
    Av1,
}

impl NvencCodec {
    /// The shared-vocabulary codec this NVENC codec encodes.
    ///
    /// Lets codec-shaped policy that already lives in `arcen-media` — QP-map
    /// geometry, for one — be looked up without a second table here that
    /// could drift out of step with it.
    const fn media_codec(self) -> arcen_media::VideoCodec {
        match self {
            Self::H264 => arcen_media::VideoCodec::H264,
            Self::Hevc => arcen_media::VideoCodec::H265,
            Self::Av1 => arcen_media::VideoCodec::Av1,
        }
    }

    /// Parses the exact codec token `Encoder::new` is called with (see
    /// `arcen_media::VideoCodec::token`). Anything else is `None` rather than
    /// a default, so a typo or an unhandled future codec fails with a named
    /// error instead of silently being treated as H.264 the way the old
    /// two-way string test would have.
    fn parse(codec: &str) -> Option<Self> {
        match codec {
            "h264" => Some(Self::H264),
            "h265" => Some(Self::Hevc),
            "av1" => Some(Self::Av1),
            _ => None,
        }
    }

    /// NVENC's codec GUID for `self` (`nvenc_sys::guid`).
    const fn codec_guid(self) -> GUID {
        match self {
            Self::H264 => NV_ENC_CODEC_H264_GUID,
            Self::Hevc => NV_ENC_CODEC_HEVC_GUID,
            Self::Av1 => NV_ENC_CODEC_AV1_GUID,
        }
    }
}

/// The concrete NVENC system-memory input layout selected for one
/// `ColorSpec` + codec pair.
///
/// Chosen once, at construction, by `resolve_pixel_format`. The NVIDIA Video
/// Codec SDK Programming Guide is explicit that `NvEncReconfigureEncoder`
/// cannot change bit depth or chroma format, so a different `PixelFormat`
/// always means destroying and recreating the whole `Encoder` — never
/// reconfiguring one in place. There is deliberately no `set_` method here.
///
/// `pub(crate)`, not private: `Encoder::pixel_format` and
/// `ensure_reconfigure_preserves_pixel_format` hand this type to callers
/// outside this module (a future live-reconfigure caller — see
/// `Encoder::ensure_reconfigurable_to`), and a type cannot be more private
/// than an item that exposes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PixelFormat {
    /// `NV_ENC_BUFFER_FORMAT_NV12`: semi-planar 4:2:0, 1 byte/sample (Y plane
    /// then one interleaved UV plane).
    Nv12,
    /// `NV_ENC_BUFFER_FORMAT_YUV420_10BIT`: semi-planar 4:2:0, 2
    /// bytes/sample, MSB-aligned (P010-style). NVIDIA's own reference
    /// (`NvEncoder.cpp` in the Video Codec SDK samples,
    /// `GetChromaSubPlaneOffsets`/`GetNumChromaPlanes`) documents this as
    /// semi-planar — Y then ONE interleaved UV plane, like NV12 at double
    /// the sample width — not three separate planes, despite how the name
    /// can read at a glance.
    P010,
    /// `NV_ENC_BUFFER_FORMAT_YUV444`: planar 4:4:4, 1 byte/sample (Y, U, V
    /// each full resolution).
    Yuv444_8,
    /// `NV_ENC_BUFFER_FORMAT_YUV444_10BIT`: planar 4:4:4, 2 bytes/sample,
    /// MSB-aligned.
    Yuv444_10,
}

impl PixelFormat {
    const fn buffer_format(self) -> NV_ENC_BUFFER_FORMAT {
        match self {
            Self::Nv12 => NV_ENC_BUFFER_FORMAT_NV12,
            Self::P010 => NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
            Self::Yuv444_8 => NV_ENC_BUFFER_FORMAT_YUV444,
            Self::Yuv444_10 => NV_ENC_BUFFER_FORMAT_YUV444_10BIT,
        }
    }

    const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Nv12 | Self::Yuv444_8 => 1,
            Self::P010 | Self::Yuv444_10 => 2,
        }
    }

    /// Whether chroma is one interleaved plane (4:2:0, subsampled both ways)
    /// or two full-resolution planes (4:4:4, no subsampling at all).
    const fn semi_planar(self) -> bool {
        matches!(self, Self::Nv12 | Self::P010)
    }

    /// The "minus 8" bit-depth encoding both HEVC's `pixelBitDepthMinus8` and
    /// AV1's `pixelBitDepthMinus8`/`inputPixelBitDepthMinus8` use (see the doc
    /// on `resolve_pixel_format` for what else was checked and ruled out for
    /// HEVC). 0 for eight-bit, 2 for ten — `NV_ENC_CONFIG_H264` has no
    /// equivalent field at all, because NVENC never encodes H.264 above eight
    /// bits.
    const fn bit_depth_minus8(self) -> u32 {
        match self {
            Self::Nv12 | Self::Yuv444_8 => 0,
            Self::P010 | Self::Yuv444_10 => 2,
        }
    }

    /// `chromaFormatIDC`: 1 for 4:2:0, 3 for 4:4:4 (ITU-T H.273 / NVENC
    /// convention). There is no 3rd value here for 4:2:2 because there is no
    /// 4:2:2 `PixelFormat` — see `PixelFormatRejection::Yuv422Unsupported`.
    const fn chroma_format_idc(self) -> u32 {
        match self {
            Self::Nv12 | Self::P010 => 1,
            Self::Yuv444_8 | Self::Yuv444_10 => 3,
        }
    }

    /// The `(chroma, bit depth)` `resolve_pixel_format` chose this format
    /// for — its exact inverse. Used only to ask "would this change chroma or
    /// bit depth?" (`ensure_reconfigure_preserves_pixel_format`) and to feed
    /// `rate_control_sizing` the same axes `Encoder::new` resolved, without
    /// re-deriving codec context just for that: `PixelFormat` already *is*
    /// the resolved chroma+depth pair, so this is a lookup, not a guess.
    const fn chroma_and_depth(self) -> (ChromaSubsampling, BitDepth) {
        match self {
            Self::Nv12 => (ChromaSubsampling::Yuv420, BitDepth::Eight),
            Self::P010 => (ChromaSubsampling::Yuv420, BitDepth::Ten),
            Self::Yuv444_8 => (ChromaSubsampling::Yuv444, BitDepth::Eight),
            Self::Yuv444_10 => (ChromaSubsampling::Yuv444, BitDepth::Ten),
        }
    }
}

/// A `ColorSpec` NVENC cannot encode at all, independent of GPU or driver.
///
/// Distinct from `NvencInitError`: this is resolved *before* touching the
/// driver at all, from the codec string and `ColorSpec` alone, so it is
/// exhaustively unit-testable without a GPU (see `pixel_format_tests`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PixelFormatRejection {
    /// NVENC defines `NV_ENC_BIT_DEPTH` nowhere and has no 12-bit buffer
    /// format or profile at any chroma subsampling; a twelve-bit `ColorSpec`
    /// must never silently truncate to ten.
    TwelveBitUnsupported,
    /// These vendored bindings define no 4:2:2 buffer format at all
    /// (`NV_ENC_BUFFER_FORMAT_NV16`/`_P210` exist in newer SDK headers, but
    /// grepping the bindgen output here finds neither — see the final
    /// report). `arcen_media::video::convert` also has no BGRA -> 4:2:2
    /// conversion, so this is unsupported twice over.
    Yuv422Unsupported,
    /// NVIDIA's own reference (`NvEncoder::CreateEncoder` in the Video Codec
    /// SDK samples) throws exactly this for a 10-bit buffer format with the
    /// H.264 codec GUID: NVENC never supports H.264 above eight bits.
    H264RequiresEightBit(BitDepth),
    /// `ColorMatrix::Identity` carries G, B and R directly in the coded
    /// planes (see `ColorTransform::luma`/`cb`/`cr`): subsampling any of them
    /// would discard three quarters of the red and blue channels, which is
    /// not what anyone means by an identity/GBR stream.
    /// `arcen_media::video::VideoVariant::is_coherent` already rejects this
    /// combination for anything built from a variant id, but `ColorSpec` is a
    /// freely-constructible public struct — this is the last line of defence
    /// for a caller that assembles one directly instead of going through a
    /// variant, so an incoherent request fails loudly here rather than
    /// quietly encoding a lossy, mislabelled stream.
    IdentityRequiresYuv444(ChromaSubsampling),
    /// NVENC exposes only `NV_ENC_AV1_PROFILE_MAIN_GUID` (AV1 Main profile),
    /// and the AV1 spec defines Main as 4:2:0 8/10-bit only -- there is no
    /// AV1 High/Professional GUID in these bindings for 4:4:4, so a 4:4:4
    /// request for AV1 must be refused rather than silently encoded at 4:2:0
    /// or left to fail deep inside `NvEncInitializeEncoder`.
    Av1RequiresYuv420(ChromaSubsampling),
}

impl Display for PixelFormatRejection {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TwelveBitUnsupported => formatter.write_str(
                "NVENC has no 12-bit buffer format or profile; BitDepth::Twelve cannot reach this encoder",
            ),
            Self::Yuv422Unsupported => formatter.write_str(
                "NVENC 4:2:2 buffer formats (NV16/P210) are absent from these vendored bindings \
                 and arcen_media has no BGRA -> 4:2:2 conversion; ChromaSubsampling::Yuv422 cannot reach this encoder",
            ),
            Self::H264RequiresEightBit(depth) => write!(
                formatter,
                "NVENC never encodes H.264 above 8 bits (no bit-depth field, no 10-bit profile); requested {depth:?}",
            ),
            Self::IdentityRequiresYuv444(chroma) => write!(
                formatter,
                "ColorMatrix::Identity (GBR passthrough) requires ChromaSubsampling::Yuv444; \
                 subsampling it at {chroma:?} would discard most of the red and blue channels",
            ),
            Self::Av1RequiresYuv420(chroma) => write!(
                formatter,
                "NVENC exposes only NV_ENC_AV1_PROFILE_MAIN_GUID (AV1 Main profile, 4:2:0 8/10-bit \
                 only); requested {chroma:?} cannot reach this encoder for AV1",
            ),
        }
    }
}

/// Resolve a colour spec into the concrete pixel format this encoder will
/// request from NVENC, or a typed reason it cannot.
///
/// Pure and GPU-free by design: every branch is exercised by
/// `pixel_format_tests` without a driver. `codec` is the closed
/// `NvencCodec` `Encoder::new` parses its `&str` argument into.
fn resolve_pixel_format(
    codec: NvencCodec,
    color: crate::ColorSpec,
) -> Result<PixelFormat, PixelFormatRejection> {
    if color.bit_depth == BitDepth::Twelve {
        return Err(PixelFormatRejection::TwelveBitUnsupported);
    }
    if color.chroma == ChromaSubsampling::Yuv422 {
        return Err(PixelFormatRejection::Yuv422Unsupported);
    }
    if color.matrix == ColorMatrix::Identity && color.chroma != ChromaSubsampling::Yuv444 {
        return Err(PixelFormatRejection::IdentityRequiresYuv444(color.chroma));
    }
    if codec == NvencCodec::H264 && color.bit_depth != BitDepth::Eight {
        return Err(PixelFormatRejection::H264RequiresEightBit(color.bit_depth));
    }
    if codec == NvencCodec::Av1 && color.chroma != ChromaSubsampling::Yuv420 {
        return Err(PixelFormatRejection::Av1RequiresYuv420(color.chroma));
    }
    Ok(match (color.chroma, color.bit_depth) {
        (ChromaSubsampling::Yuv420, BitDepth::Eight) => PixelFormat::Nv12,
        (ChromaSubsampling::Yuv420, BitDepth::Ten) => PixelFormat::P010,
        (ChromaSubsampling::Yuv444, BitDepth::Eight) => PixelFormat::Yuv444_8,
        (ChromaSubsampling::Yuv444, BitDepth::Ten) => PixelFormat::Yuv444_10,
        (ChromaSubsampling::Yuv422, _) | (_, BitDepth::Twelve) => {
            unreachable!("Yuv422 and Twelve are both rejected above")
        }
    })
}

/// Profile GUID to set for `codec`+`format`, when it differs from the
/// preset's own default.
///
/// `None` for H.264/HEVC + 4:2:0 8-bit: that is the contract capenc shipped
/// before colour was negotiable, the one case nobody working on this change
/// can run against a GPU, and its preset already carries the right default
/// profile — overriding it here on faith is exactly the kind of change
/// "least disruption" rules out.
///
/// AV1 is different: `NV_ENC_AV1_PROFILE_MAIN_GUID` is the *only* AV1
/// profile GUID these bindings define, covering both 8- and 10-bit Main, so
/// it is set explicitly for every AV1 pixel format that reaches this
/// function (`resolve_pixel_format` already rejects 4:4:4 for AV1, so only
/// `Nv12`/`P010` ever do) rather than assumed from a preset default: unlike
/// the untouched H.264 case, there is no GPU here to confirm an AV1 preset
/// default, and AV1 does not get the same benefit of the doubt as that
/// long-proven path.
fn profile_guid_override(codec: NvencCodec, format: PixelFormat) -> Option<GUID> {
    if codec == NvencCodec::Av1 {
        return Some(NV_ENC_AV1_PROFILE_MAIN_GUID);
    }
    match format {
        PixelFormat::Nv12 => None,
        PixelFormat::P010 => Some(NV_ENC_HEVC_PROFILE_MAIN10_GUID), // H.264 P010 already rejected upstream; AV1 handled above
        PixelFormat::Yuv444_8 => Some(if codec == NvencCodec::Hevc {
            NV_ENC_HEVC_PROFILE_FREXT_GUID
        } else {
            NV_ENC_H264_PROFILE_HIGH_444_GUID
        }),
        PixelFormat::Yuv444_10 => Some(NV_ENC_HEVC_PROFILE_FREXT_GUID), // H.264 already rejected upstream; AV1 never reaches 4:4:4
    }
}

/// Row count of one chroma plane for `format` at `luma_height`. Mirrors
/// NVIDIA's own `NvEncoder::GetChromaHeight` (Video Codec SDK samples): half
/// the luma rows, rounded up, for 4:2:0 (`Nv12`/`P010`); the full luma row
/// count for 4:4:4, which is never subsampled vertically.
const fn chroma_rows(format: PixelFormat, luma_height: u32) -> usize {
    let luma_height = luma_height as usize;
    if format.semi_planar() {
        luma_height.div_ceil(2)
    } else {
        luma_height
    }
}

/// Total bytes across every plane of `format` at `pitch` (as
/// `nvEncLockInputBuffer` reports it) for `width`x`height`. Mirrors NVIDIA's
/// own `NvEncoder::GetFrameSize`, generalised to a runtime pitch instead of
/// one derived purely from `width`, since the driver may pad rows for
/// alignment.
fn frame_bytes(format: PixelFormat, pitch: u32, height: u32) -> usize {
    let pitch = pitch as usize;
    let luma = pitch * height as usize;
    if format.semi_planar() {
        luma + pitch * chroma_rows(format, height)
    } else {
        luma * 3
    }
}

/// `NvEncGetEncodePresetConfigEx`'s frame-rate hint. NVENC's own rate
/// pacing (and therefore anything sized relative to it) is driven by this,
/// not by whatever real capture cadence the caller happens to run —
/// `Encoder::new` takes no `fps` parameter at all, so `rate_control_sizing`
/// is called with this exact constant to keep the two in agreement; see
/// `Encoder::new`'s `init.frameRateNum` assignment, the only other reader.
const NVENC_FRAME_RATE_HINT: u32 = 60;
use crate::nvenc_policy::{output_drain_policy, rate_control_sizing};
#[cfg(test)]
use crate::nvenc_policy::{vbv_buffer_frames, RateControlSizing};

/// `NvEncReconfigureEncoder` cannot change bit depth or chroma format — the
/// NVIDIA Video Codec SDK Programming Guide is explicit that both are fixed
/// for a session's whole lifetime and that changing either requires
/// destroying and recreating the encoder. There is no live reconfigure call
/// site in this codebase yet, but there is exactly one designed-in seam for
/// one ever to be added (`Encoder::ensure_reconfigurable_to`) — this is the
/// error it reports, named clearly enough that a future caller cannot miss
/// why a hot depth/chroma switch was refused instead of silently producing a
/// corrupt stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReconfigureLifecycleError {
    detail: String,
}

impl Display for ReconfigureLifecycleError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ReconfigureLifecycleError {}

/// Pure guard: whether `requested` still describes the same chroma and bit
/// depth as `current`. Deliberately independent of any live NVENC handle —
/// unlike almost everything else in this file, this can be (and is, in
/// `reconfigure_lifecycle_tests`) exercised by `cargo test` with no driver
/// and no GPU, so the one property this task exists to guarantee is checked
/// on every build, not only where real hardware happens to be present.
pub(crate) fn ensure_reconfigure_preserves_pixel_format(
    current: PixelFormat,
    requested: crate::ColorSpec,
) -> Result<(), ReconfigureLifecycleError> {
    let (current_chroma, current_depth) = current.chroma_and_depth();
    if current_depth != requested.bit_depth {
        return Err(ReconfigureLifecycleError {
            detail: format!(
                "NvEncReconfigureEncoder cannot change bit depth (NVIDIA Video Codec SDK \
                 Programming Guide): session was created at {current_depth:?}-bit, \
                 reconfigure requested {:?}-bit; destroy and recreate the Encoder instead",
                requested.bit_depth
            ),
        });
    }
    if current_chroma != requested.chroma {
        return Err(ReconfigureLifecycleError {
            detail: format!(
                "NvEncReconfigureEncoder cannot change chroma format (NVIDIA Video Codec SDK \
                 Programming Guide): session was created at {current_chroma:?}, reconfigure \
                 requested {:?}; destroy and recreate the Encoder instead",
                requested.chroma
            ),
        });
    }
    Ok(())
}

impl Encoder {
    /// codec: "h264", "h265" or "av1" (parsed once into `NvencCodec`; see its
    /// doc). `color` selects chroma, bit depth, range and matrix;
    /// `resolve_pixel_format` turns it into a concrete NVENC buffer format
    /// (or a typed rejection) and everything below is config built from that
    /// resolved `PixelFormat`, never from `color` directly, so a new
    /// combination can't drift between what was resolved and what NVENC was
    /// actually configured for.
    pub unsafe fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        codec: &str,
        color: crate::ColorSpec,
        intent: EncodeIntent,
        qp_map_policy: crate::qp_map::QpMapPolicy,
    ) -> Result<Self, NvencInitError> {
        let nvenc_codec = NvencCodec::parse(codec).ok_or_else(|| {
            NvencInitError::unsupported(format!(
                "unrecognized capenc codec token {codec:?}; NVENC handles \"h264\", \"h265\" or \"av1\""
            ))
        })?;
        let format = resolve_pixel_format(nvenc_codec, color)
            .map_err(|rejection| NvencInitError::unsupported(rejection.to_string()))?;
        // 1. Load the runtime DLL + the single entry point (rest is a fn table).
        let lib = load_nvenc_runtime().map_err(|error| {
            NvencInitError::runtime_unavailable(format!(
                "load SYSTEM32 nvEncodeAPI64.dll: {error:?}"
            ))
        })?;
        let library = NvencLibrary::new(lib);
        let proc = GetProcAddress(lib, PCSTR(c"NvEncodeAPICreateInstance".as_ptr().cast()))
            .ok_or_else(|| {
                NvencInitError::runtime_unavailable("NvEncodeAPICreateInstance not found")
            })?;
        let create: CreateInstanceFn = std::mem::transmute(proc);

        // 2. Fill the function-pointer table.
        let mut fl: NV_ENCODE_API_FUNCTION_LIST = zeroed();
        fl.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        nvchk_init!(create(&mut fl), "NvEncodeAPICreateInstance");

        // 3. Open a DirectX encode session bound to our D3D11 device.
        let mut sp: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = zeroed();
        sp.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        sp.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
        sp.device = device.as_raw();
        sp.apiVersion = NVENCAPI_VERSION;
        let mut enc: *mut c_void = std::ptr::null_mut();
        let open_session = fl
            .nvEncOpenEncodeSessionEx
            .ok_or_else(|| NvencInitError::fatal("missing nvEncOpenEncodeSessionEx"))?;
        nvchk_init!(open_session(&mut sp, &mut enc), "OpenEncodeSessionEx");
        let mut resources = EncoderInitGuard {
            fl: &fl,
            enc,
            slots: Vec::new(),
        };

        let codec_guid = nvenc_codec.codec_guid();
        // AV1 encode requires Ada Lovelace or newer; there is no
        // NV_ENC_CAPS_* boolean for codec support itself, so this is checked
        // via NvEncGetEncodeGUIDs (see `encoder_enumerates_codec`) ahead of
        // the preset/init calls below, so an unsupported GPU gets a clear,
        // typed refusal naming AV1 and the generation requirement instead of
        // an opaque status from deeper in NVENC. H.264/HEVC skip this: every
        // GPU/driver this codebase already runs on has always supported
        // them, so adding a round trip to that long-proven path would be
        // exactly the kind of change "least disruption" rules out.
        if nvenc_codec == NvencCodec::Av1
            && !encoder_enumerates_codec(resources.fl, resources.enc, codec_guid)
        {
            return Err(NvencInitError::unsupported(
                "AV1 encode requires NVENC Ada generation (RTX 40-series / L4 / L40S) or newer: \
                 this GPU's NvEncGetEncodeGUIDs() does not list NV_ENC_CODEC_AV1_GUID"
                    .to_string(),
            ));
        }
        let preset_guid = match intent {
            // P4 balances speed and quality; ULTRA_LOW_LATENCY drops lookahead
            // and B-frames and tightens VBV. Right for interactive desktop,
            // where latency is the top priority.
            EncodeIntent::Interactive => NV_ENC_PRESET_P4_GUID,
            // P6 spends materially more on each frame. Paired with
            // HIGH_QUALITY it re-enables lookahead and B-frames, which is
            // exactly the trade a colourist on a held frame wants and an
            // interactive user does not.
            //
            // Deliberately P6 rather than P7: P7 is close to P6 in quality
            // and much slower, and a grading session is still a live session,
            // not an offline export.
            EncodeIntent::Quality => NV_ENC_PRESET_P6_GUID,
        };
        let tuning = match intent {
            EncodeIntent::Interactive => NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            EncodeIntent::Quality => NV_ENC_TUNING_INFO_HIGH_QUALITY,
        };

        // 4. Let the driver fill a full config for that preset+tuning.
        let mut preset: NV_ENC_PRESET_CONFIG = zeroed();
        preset.version = NV_ENC_PRESET_CONFIG_VER;
        preset.presetCfg.version = NV_ENC_CONFIG_VER;
        let get_preset = fl
            .nvEncGetEncodePresetConfigEx
            .ok_or_else(|| NvencInitError::fatal("missing nvEncGetEncodePresetConfigEx"))?;
        nvchk_init!(
            get_preset(resources.enc, codec_guid, preset_guid, tuning, &mut preset),
            "GetEncodePresetConfigEx"
        );
        // Undo the driver's reordering defaults, for BOTH intents.
        //
        // P6 + HIGH_QUALITY arrives with B-frames and lookahead enabled, which
        // is right for a file and wrong for a session: Arcen timestamps an
        // access unit when it is read *out* of the encoder, so coding order
        // becomes the only order the client ever sees, and reordered output
        // plays forward, jumps back, then forward again. This was observed on
        // real hardware in grading mode before it was pinned here.
        //
        // See `EncodeIntent::REQUIRED_FRAME_INTERVAL_P` for the full reasoning
        // and for what must change before B-frames could ever be enabled.
        // Record what the driver actually chose, before overriding it.
        //
        // Every claim about why this override exists — that P6 +
        // HIGH_QUALITY arrives with B-frames and lookahead enabled — was
        // reasoned from documentation, never measured on a host. Logging the
        // pre-override values turns the premise into evidence, and costs one
        // line. Without it the override erases the only place the driver's
        // real defaults were ever visible.
        crate::log(&format!(
            "preset defaults (pre-override): intent={} frame_interval_p={} lookahead_depth={}",
            intent.token(),
            preset.presetCfg.frameIntervalP,
            preset.presetCfg.rcParams.lookaheadDepth,
        ));
        // `frameIntervalP` is signed in the NVENC ABI; the shared constant is
        // an unsigned count, so the conversion is explicit rather than `as`.
        preset.presetCfg.frameIntervalP =
            i32::try_from(EncodeIntent::REQUIRED_FRAME_INTERVAL_P).unwrap_or(1);
        preset
            .presetCfg
            .rcParams
            .set_enableLookahead(u32::from(intent.allows_lookahead()));
        // Clear the depth too, not just the enable bit. They are read by
        // different consumers: NVENC honours the bit, but `output_drain_policy`
        // sizes the in-flight window from the *depth*, so leaving a disabled
        // depth of 8 behind would reserve eight submissions of pipeline for a
        // lookahead that cannot happen — and the encoder holds `slots - 1`
        // frames before it emits anything, which is latency a live session
        // pays on every frame.
        preset.presetCfg.rcParams.lookaheadDepth = 0;
        preset.presetCfg.rcParams.set_zeroReorderDelay(1);
        match nvenc_codec {
            NvencCodec::Hevc => {
                preset
                    .presetCfg
                    .encodeCodecConfig
                    .hevcConfig
                    .set_outputAUD(1);
            }
            NvencCodec::H264 => {
                preset
                    .presetCfg
                    .encodeCodecConfig
                    .h264Config
                    .set_outputAUD(1);
            }
            // Keep low-overhead OBU framing (`outputAnnexBFormat = 0`) for
            // VideoToolbox samples, and repeat the Sequence Header on every
            // forced keyframe so the host can classify a self-contained
            // recovery point without parsing the full AV1 uncompressed header.
            NvencCodec::Av1 => {
                preset
                    .presetCfg
                    .encodeCodecConfig
                    .av1Config
                    .set_outputAnnexBFormat(0);
                preset
                    .presetCfg
                    .encodeCodecConfig
                    .av1Config
                    .set_repeatSeqHdr(1);
            }
        }

        // 4b. Chroma + bit depth: config-only delta over the D3D11 capture
        // path (see module doc). NVENC used to do the RGB->YCbCr conversion
        // itself off the packed-BGRA `ARGB` input for every chroma; now that
        // `stage()` feeds it real NV12/YUV444(_10BIT) samples, this step only
        // has to tell NVENC what it's receiving: chroma layout, bit depth and
        // profile, all driven off the one resolved `format` rather than a
        // bare `yuv444` bool.
        log_color_capabilities(resources.fl, resources.enc, codec_guid, codec);
        if matches!(format, PixelFormat::Yuv444_8 | PixelFormat::Yuv444_10) {
            // Honest capability log: NV_ENC_CAPS_SUPPORT_YUV444_ENCODE. We do
            // NOT hard-fail on a 0 here — InitializeEncoder is the authority
            // and returns a clear NVENC status if the GPU/driver rejects 4:4:4
            // (matching the Linux path, which assumes support). This just
            // surfaces the reason up-front in the log.
            let cap = query_cap(
                resources.fl,
                resources.enc,
                codec_guid,
                NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
            );
            crate::log(&format!(
                "NV_ENC_CAPS_SUPPORT_YUV444_ENCODE={cap} (codec={codec}) — requesting 4:4:4"
            ));
            if cap < 1 {
                crate::log(
                    "WARN: GPU/driver reports no 4:4:4 encode support; \
                     InitializeEncoder will fail if truly unsupported",
                );
            }
        }
        if matches!(format, PixelFormat::P010) {
            let cap = query_cap(
                resources.fl,
                resources.enc,
                codec_guid,
                NV_ENC_CAPS_SUPPORT_10BIT_ENCODE,
            );
            crate::log(&format!(
                "NV_ENC_CAPS_SUPPORT_10BIT_ENCODE={cap} (codec={codec}) — requesting Main10 4:2:0"
            ));
            if cap < 1 {
                crate::log(
                    "WARN: GPU/driver reports no 10-bit encode support; \
                     InitializeEncoder will fail if truly unsupported",
                );
            }
        }
        if let Some(profile) = profile_guid_override(nvenc_codec, format) {
            preset.presetCfg.profileGUID = profile;
        }
        match nvenc_codec {
            NvencCodec::Hevc => {
                preset
                    .presetCfg
                    .encodeCodecConfig
                    .hevcConfig
                    .set_chromaFormatIDC(format.chroma_format_idc());
                preset
                    .presetCfg
                    .encodeCodecConfig
                    .hevcConfig
                    .set_pixelBitDepthMinus8(format.bit_depth_minus8());
            }
            NvencCodec::Av1 => {
                // `resolve_pixel_format` already rejects 4:4:4 for AV1, so
                // `format` is always Nv12 or P010 here -- chromaFormatIDC is
                // always 1, but bit depth still needs to be told explicitly
                // (reusing the same "minus 8" machinery HEVC's Main10 path
                // uses, see `bit_depth_minus8`), since P010's 10-bit samples
                // would otherwise be silently misread as 8-bit. Both
                // `inputPixelBitDepthMinus8` (the surface capenc feeds NVENC)
                // and `pixelBitDepthMinus8` (the coded output) are set to the
                // same value: this never asks NVENC to change bit depth
                // between input and output.
                preset
                    .presetCfg
                    .encodeCodecConfig
                    .av1Config
                    .set_chromaFormatIDC(format.chroma_format_idc());
                preset
                    .presetCfg
                    .encodeCodecConfig
                    .av1Config
                    .set_inputPixelBitDepthMinus8(format.bit_depth_minus8());
                preset
                    .presetCfg
                    .encodeCodecConfig
                    .av1Config
                    .set_pixelBitDepthMinus8(format.bit_depth_minus8());
            }
            NvencCodec::H264 => {
                if matches!(format, PixelFormat::Yuv444_8) {
                    // NV_ENC_CONFIG_H264 has no bit-depth field at all: NVENC
                    // never encodes H.264 above 8 bits, and
                    // `resolve_pixel_format` already rejects any H.264
                    // request that isn't (this is the only non-default
                    // H.264 chroma config there is left to set).
                    preset
                        .presetCfg
                        .encodeCodecConfig
                        .h264Config
                        .chromaFormatIDC = format.chroma_format_idc();
                }
                // (H.264 + Nv12 — 4:2:0, 8-bit — is untouched: the exact
                // config the preset already produced before colour was
                // negotiable.)
            }
        }
        // 4c. Colour signalling. Until now capenc wrote no VUI at all, so every
        // stream it produced was untagged and a decoder had to guess. A
        // decoder that guesses limited range on full-range content crushes
        // blacks and clips whites, which for a grading session is not a
        // cosmetic problem. H.264/HEVC get VUI (NVENC reuses the H.264 VUI
        // struct for HEVC); AV1 does not use VUI at all -- its colour info
        // lives directly in the sequence header, which `apply_av1_color`
        // writes onto `NV_ENC_CONFIG_AV1` instead (see its doc).
        match nvenc_codec {
            NvencCodec::Hevc | NvencCodec::H264 => {
                let vui = vui_parameters(color);
                if matches!(nvenc_codec, NvencCodec::Hevc) {
                    preset
                        .presetCfg
                        .encodeCodecConfig
                        .hevcConfig
                        .hevcVUIParameters = vui;
                } else {
                    preset
                        .presetCfg
                        .encodeCodecConfig
                        .h264Config
                        .h264VUIParameters = vui;
                }
                crate::log(&format!(
                    "VUI: full_range={} primaries={:?} transfer={:?} matrix={:?}",
                    vui.videoFullRangeFlag,
                    vui.colourPrimaries,
                    vui.transferCharacteristics,
                    vui.colourMatrix
                ));
            }
            NvencCodec::Av1 => {
                apply_av1_color(&mut preset.presetCfg.encodeCodecConfig.av1Config, color);
                let av1_config = preset.presetCfg.encodeCodecConfig.av1Config;
                crate::log(&format!(
                    "AV1 colour (sequence header): full_range={} primaries={:?} \
                     transfer={:?} matrix={:?}",
                    av1_config.colorRange,
                    av1_config.colorPrimaries,
                    av1_config.transferCharacteristics,
                    av1_config.matrixCoefficients
                ));
            }
        }
        // 4d. Rate control. `nvEncGetEncodePresetConfigEx` just filled
        // `rcParams` with a preset default that has no notion of chroma or
        // bit depth at all — see `rate_control_sizing` for why that default
        // is implicitly an 8-bit 4:2:0 number and would starve every other
        // row in the colour matrix. Only the bitrate/VBV fields are
        // overwritten; `rateControlMode`/QP bounds/lookahead stay whatever
        // the preset chose.
        {
            let (rc_chroma, rc_depth) = format.chroma_and_depth();
            let sizing = rate_control_sizing(
                width,
                height,
                NVENC_FRAME_RATE_HINT,
                rc_chroma,
                rc_depth,
                intent,
            );
            preset.presetCfg.rcParams.averageBitRate = sizing.average_bitrate_bps;
            preset.presetCfg.rcParams.maxBitRate = sizing.max_bitrate_bps;
            preset.presetCfg.rcParams.vbvBufferSize = sizing.vbv_buffer_size_bits;
            preset.presetCfg.rcParams.vbvInitialDelay = sizing.vbv_buffer_size_bits;
            crate::log(&format!(
                "rate control: {width}x{height} chroma={rc_chroma:?} depth={rc_depth:?}-bit -> \
                 average={} max={} vbv_bits={} (preset default replaced; see rate_control_sizing)",
                sizing.average_bitrate_bps, sizing.max_bitrate_bps, sizing.vbv_buffer_size_bits,
            ));
        }
        // Ask for a per-block QP delta map only when the caller selected a
        // map-bearing policy. `Off` must initialize with the feature disabled:
        // a speculative DELTA trial would make its log and capability result
        // untruthful.
        //
        // There is no `NV_ENC_CAPS_*`
        // boolean for delta-map support — unlike the emphasis map, which has
        // one and is H.264-only — so the only honest probe is a trial init,
        // exactly as with every other NVENC capability *combination* in this
        // file. A GPU or codec that refuses it must still get a working
        // session, so a refusal falls back to no map rather than failing the
        // encoder outright.
        //
        // Set before `init.encodeConfig` takes its pointer, so the value the
        // driver reads is unambiguously the one written here.
        preset.presetCfg.rcParams.qpMapMode = if qp_map_policy.submits_map() {
            NV_ENC_QP_MAP_DELTA
        } else {
            NV_ENC_QP_MAP_DISABLED
        };

        let mut init: NV_ENC_INITIALIZE_PARAMS = zeroed();
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = codec_guid;
        init.presetGUID = preset_guid;
        init.encodeWidth = width;
        init.encodeHeight = height;
        init.darWidth = width;
        init.darHeight = height;
        init.frameRateNum = NVENC_FRAME_RATE_HINT;
        init.frameRateDen = 1;
        init.enablePTD = 1;
        init.tuningInfo = tuning;
        init.encodeConfig = &mut preset.presetCfg;
        let initialize = fl
            .nvEncInitializeEncoder
            .ok_or_else(|| NvencInitError::fatal("missing nvEncInitializeEncoder"))?;
        let mut qp_map_supported = qp_map_policy.submits_map();
        let mut init_status = initialize(resources.enc, &mut init);
        if qp_map_supported && init_status != NV_ENC_SUCCESS {
            // Log the first failure rather than letting the retry hide it: if
            // the real cause was something other than the QP map, the second
            // attempt fails too and this line is what explains why.
            crate::log(&format!(
                "InitializeEncoder with qpMapMode=DELTA -> {init_status:?}; \
                 retrying without a QP map"
            ));
            qp_map_supported = false;
            preset.presetCfg.rcParams.qpMapMode = NV_ENC_QP_MAP_DISABLED;
            // Re-point at the same config after mutating it. Redundant to the
            // driver, which already holds this address, but it keeps the write
            // above plainly live rather than something a reader has to reason
            // about through a raw pointer.
            init.encodeConfig = &mut preset.presetCfg;
            init_status = initialize(resources.enc, &mut init);
        }
        nvchk_init!(init_status, "InitializeEncoder");
        let qp_map_entries = if qp_map_supported {
            QpMapGeometry::for_codec(nvenc_codec.media_codec())
                .map_or(0, |geometry| geometry.entry_count(width, height))
        } else {
            0
        };
        crate::log(&format!(
            "QP delta map: policy={} {} ({} entries, {:?} geometry)",
            qp_map_policy.token(),
            if qp_map_entries > 0 {
                "available"
            } else {
                "unavailable"
            },
            qp_map_entries,
            QpMapGeometry::for_codec(nvenc_codec.media_codec()),
        ));

        // 6. Build the preset/cap-sized pipeline slots (NVENC-allocated input buffer +
        // output bitstream — see the module doc for why this replaced the
        // registered-D3D11-texture path).
        let drain_policy = output_drain_policy(
            intent,
            preset.presetCfg.frameIntervalP,
            preset.presetCfg.rcParams.lookaheadDepth,
            query_cap(
                resources.fl,
                resources.enc,
                codec_guid,
                NV_ENC_CAPS_NUM_MAX_BFRAMES,
            ),
        );
        crate::log(&format!(
            "output drain: intent={} frame_interval_p={} lookahead={} max_inflight={} slots={} \
             (preset/cap-sized blocking drain)",
            intent.token(),
            preset.presetCfg.frameIntervalP,
            preset.presetCfg.rcParams.lookaheadDepth,
            drain_policy.max_inflight(),
            drain_policy.slot_count(),
        ));
        for i in 0..drain_policy.slot_count() {
            resources.slots.push(
                Self::make_slot(resources.fl, resources.enc, width, height, format)
                    .map_err(|error| NvencInitError::fatal(format!("slot {i}: {error}")))?,
            );
        }
        // Learn the pitch NVENC actually gives this format/width/height (it
        // may pad rows for alignment) and zero-fill every slot so a
        // `frame_policy::FrameAction::SubmitBlank` before the first real
        // `stage()` call encodes deterministic black instead of whatever the
        // driver happened to allocate. All slots share identical creation
        // parameters, so every one is expected to report the same pitch;
        // `zero_slot` returns it and this loop asserts they agree rather than
        // silently trusting it.
        let mut locked_pitch: Option<u32> = None;
        for slot in &resources.slots {
            let pitch = Self::zero_slot(resources.fl, resources.enc, slot, format, height)
                .map_err(NvencInitError::fatal)?;
            match locked_pitch {
                None => locked_pitch = Some(pitch),
                Some(expected) if expected != pitch => {
                    return Err(NvencInitError::fatal(format!(
                        "NVENC reported different pitch ({pitch}) for identically-created \
                         input buffers (expected {expected})"
                    )));
                }
                Some(_) => {}
            }
        }
        let locked_pitch = locked_pitch.expect("output drain policy always allocates a slot");
        let frame_bytes = frame_bytes(format, locked_pitch, height);
        let i444_conversion_workers = if format == PixelFormat::Yuv444_10 {
            std::thread::available_parallelism()
                .map_or(1, usize::from)
                .min(8)
                .min(height as usize)
        } else {
            1
        };
        if i444_conversion_workers > 1 {
            crate::log(&format!(
                "I444P16 conversion workers: {i444_conversion_workers}"
            ));
        }

        // CPU-readable copy of the newest desktop image (see module doc).
        let staging_tex =
            Self::make_staging_texture(device, width, height).map_err(NvencInitError::fatal)?;

        let slots = std::mem::take(&mut resources.slots);
        let slot_count = slots.len();
        resources.enc = std::ptr::null_mut();
        drop(resources);

        Ok(Self {
            _library: library,
            fl,
            enc,
            context: context.clone(),
            slots,
            staging_tex,
            format,
            transform: color.transform(),
            qp_state: None,
            qp_map_entries,
            locked_pitch,
            frame_bytes,
            latest: vec![0u8; frame_bytes],
            generations: SlotGenerations::new(slot_count),
            inflight: VecDeque::with_capacity(drain_policy.max_inflight()),
            write_idx: 0,
            drain_policy,
            width,
            height,
            i444_conversion_workers,
            last_conversion_ms: 0.0,
            last_copy_ms: 0.0,
            staged_capture: StagedCapture::default(),
            last_mirror_ms: 0.0,
            last_stage_timing: StageTiming::default(),
        })
    }

    /// The chroma subsampling and bit depth this session was created for —
    /// fixed for its whole lifetime (see `ensure_reconfigurable_to`).
    ///
    /// No production call site exists yet — see `ensure_reconfigurable_to` —
    /// so this is exercised only indirectly, via that method's own doc, until
    /// a live reconfigure caller is added; `#[allow(dead_code)]` for the same
    /// reason `CursorCaptureMode::requires_wgc` carries one, in `lib.rs`.
    #[allow(dead_code)]
    pub(crate) const fn pixel_format(&self) -> PixelFormat {
        self.format
    }

    /// Whether `requested` could be applied to this already-running session
    /// via `NvEncReconfigureEncoder` without destroying and recreating it.
    ///
    /// There is no live reconfigure call site in this codebase yet — every
    /// colour change today goes through a fresh `Encoder::new` — but this is
    /// the seam a future one (for example, a live bitrate or range/matrix
    /// change) must call first: `NvEncReconfigureEncoder` cannot change bit
    /// depth or chroma format (NVIDIA Video Codec SDK Programming Guide), and
    /// silently forwarding a request that changes either would not fail —
    /// it would produce a corrupt stream. This makes that mistake
    /// structurally impossible instead of merely documented. The underlying
    /// guard (`ensure_reconfigure_preserves_pixel_format`) is a pure function
    /// of `PixelFormat`/`ColorSpec` and is what `reconfigure_lifecycle_tests`
    /// exercises directly, since building a live `Encoder` needs a real
    /// device; `#[allow(dead_code)]` for the same reason `pixel_format` does.
    #[allow(dead_code)]
    pub(crate) fn ensure_reconfigurable_to(
        &self,
        requested: crate::ColorSpec,
    ) -> Result<(), ReconfigureLifecycleError> {
        ensure_reconfigure_preserves_pixel_format(self.pixel_format(), requested)
    }

    /// Create one slot: an NVENC-allocated system-memory input buffer (see
    /// the module doc for why this replaced a registered D3D11 texture),
    /// plus an output bitstream buffer.
    unsafe fn make_slot(
        fl: &NV_ENCODE_API_FUNCTION_LIST,
        enc: *mut c_void,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<Slot, String> {
        let mut cb: NV_ENC_CREATE_INPUT_BUFFER = zeroed();
        cb.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
        cb.width = width;
        cb.height = height;
        cb.bufferFmt = format.buffer_format();
        cb.memoryHeap = NV_ENC_MEMORY_HEAP_AUTOSELECT;
        let create_input = fl
            .nvEncCreateInputBuffer
            .ok_or_else(|| "missing nvEncCreateInputBuffer".to_string())?;
        nvchk!(create_input(enc, &mut cb), "CreateInputBuffer");
        let input_buffer = cb.inputBuffer;

        let mut bb: NV_ENC_CREATE_BITSTREAM_BUFFER = zeroed();
        bb.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        let create_bitstream = match fl.nvEncCreateBitstreamBuffer {
            Some(create_bitstream) => create_bitstream,
            None => {
                if let Some(destroy) = fl.nvEncDestroyInputBuffer {
                    let _ = destroy(enc, input_buffer);
                }
                return Err("missing nvEncCreateBitstreamBuffer".to_string());
            }
        };
        let status = create_bitstream(enc, &mut bb);
        if status != NV_ENC_SUCCESS {
            if let Some(destroy) = fl.nvEncDestroyInputBuffer {
                let _ = destroy(enc, input_buffer);
            }
            return Err(format!("CreateBitstreamBuffer -> NVENC status {status:?}"));
        }

        Ok(Slot {
            input_buffer,
            bitstream: bb.bitstreamBuffer,
        })
    }

    /// Lock `slot`'s input buffer, zero-fill every plane it holds for
    /// `format`/`height`, and return the pitch NVENC reported for it. See the
    /// call site in `Encoder::new` for why this runs once per slot at
    /// construction.
    unsafe fn zero_slot(
        fl: &NV_ENCODE_API_FUNCTION_LIST,
        enc: *mut c_void,
        slot: &Slot,
        format: PixelFormat,
        height: u32,
    ) -> Result<u32, String> {
        let mut lock: NV_ENC_LOCK_INPUT_BUFFER = zeroed();
        lock.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
        lock.inputBuffer = slot.input_buffer;
        let lock_fn = fl
            .nvEncLockInputBuffer
            .ok_or_else(|| "missing nvEncLockInputBuffer".to_string())?;
        nvchk!(lock_fn(enc, &mut lock), "LockInputBuffer(zero-init)");
        let bytes = frame_bytes(format, lock.pitch, height);
        std::ptr::write_bytes(lock.bufferDataPtr.cast::<u8>(), 0, bytes);
        let unlock_fn = fl
            .nvEncUnlockInputBuffer
            .ok_or_else(|| "missing nvEncUnlockInputBuffer".to_string())?;
        nvchk!(
            unlock_fn(enc, slot.input_buffer),
            "UnlockInputBuffer(zero-init)"
        );
        Ok(lock.pitch)
    }

    /// CPU-readable staging texture the newest desktop image is CopyResource'd
    /// into before being Mapped for the BGRA -> coded-sample conversion (see
    /// `stage`).
    unsafe fn make_staging_texture(
        device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .map_err(|e| format!("CreateTexture2D(staging): {e:?}"))?;
        staging.ok_or_else(|| "staging texture null".to_string())
    }

    /// GPU->GPU copy of the DXGI-acquired desktop frame into our CPU-readable
    /// staging texture. This is the **only** step that needs the acquired
    /// surface, so it is the only step that must run before `ReleaseFrame`:
    /// DXGI recycles the acquired surface on the next `AcquireNextFrame`, but
    /// the staging texture is ours.
    ///
    /// Pair every call with [`Self::convert_and_publish_staging`]. Splitting
    /// them is the point: the CPU `Map`, colour conversion, NVENC write and
    /// mirror copy that follow used to run inside the `AcquireNextFrame`
    /// callback, holding Desktop Duplication for the whole ~13 ms rather than
    /// the ~1 ms the copy needs, which stopped DXGI accumulating the next
    /// frame while we were still working on this one.
    ///
    /// Newest-frame semantics are preserved: a second copy before a publish
    /// deliberately replaces the pending frame rather than queueing it.
    ///
    /// Unlike the old ARGB path this round trip is no longer purely GPU-side
    /// (see module doc): NVENC must see samples in a format and colour space
    /// *we* chose, and no in-hardware path other than a CPU round trip is
    /// available without a custom compute shader — a materially bigger change
    /// this task deliberately did not make.
    pub unsafe fn copy_acquired_texture(
        &mut self,
        acquired: &ID3D11Texture2D,
    ) -> Result<(), String> {
        // A failed copy leaves the staging texture holding an indeterminate
        // mixture of the old and new frames, so drop the claim first and only
        // re-establish it once the copy has actually been issued.
        self.staged_capture.copy_failed();
        let copy_started = Instant::now();
        let src: ID3D11Resource = acquired.cast().map_err(|e| format!("cast src: {e:?}"))?;
        let dst: ID3D11Resource = self
            .staging_tex
            .cast()
            .map_err(|e| format!("cast staging: {e:?}"))?;
        self.context.CopyResource(&dst, &src);
        self.last_copy_ms = copy_started.elapsed().as_secs_f64() * 1000.0;
        self.staged_capture.copied();
        Ok(())
    }

    /// Copy, convert and publish one frame in a single call.
    ///
    /// Convenience for the self-test, admission-probe and probe-matrix paths,
    /// which own their source texture outright and have no DXGI frame to
    /// release early, so splitting the phases would buy them nothing.
    ///
    /// The live capture loop deliberately does **not** use this: it calls
    /// [`Self::copy_acquired_texture`] inside the `AcquireNextFrame` callback
    /// and [`Self::convert_and_publish_staging`] after `ReleaseFrame`, so
    /// Desktop Duplication is held for the GPU copy only.
    pub unsafe fn stage(&mut self, acquired: &ID3D11Texture2D) -> Result<(), String> {
        self.copy_acquired_texture(acquired)?;
        self.convert_and_publish_staging()
    }

    /// Map the staging texture copied by [`Self::copy_acquired_texture`],
    /// observe damage from it, convert it, and write it straight into the
    /// current slot's locked NVENC input buffer.
    ///
    /// Safe to call only after a successful copy: publishing without one would
    /// re-encode whatever the staging texture happened to hold, which is a
    /// stale desktop presented as a fresh capture. That is refused explicitly
    /// rather than tolerated.
    pub unsafe fn convert_and_publish_staging(&mut self) -> Result<(), String> {
        if !self.staged_capture.take() {
            return Err("publish requested with no copied frame staged".to_string());
        }
        let map_started = Instant::now();
        let dst: ID3D11Resource = self
            .staging_tex
            .cast()
            .map_err(|e| format!("cast staging: {e:?}"))?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("Map(staging): {e:?}"))?;
        // `readback_ms` keeps its pre-split meaning — the GPU copy plus the
        // CPU map — so runs recorded before the split stay comparable.
        // `copy_ms` is the new sub-measurement: it is the part that still
        // holds the DXGI frame.
        let map_ms = map_started.elapsed().as_secs_f64() * 1000.0;
        let readback_ms = self.last_copy_ms + map_ms;
        let src_stride = mapped.RowPitch as usize;
        let src_len = src_stride * self.height as usize;
        let bgra_bytes = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), src_len);
        let publish_result = match BgraFrame::new(
            bgra_bytes,
            self.width as usize,
            self.height as usize,
            src_stride,
        ) {
            Ok(bgra) => {
                // Observe damage from the same frame that is about to be
                // converted and encoded, before `publish_bgra` consumes it.
                if let Some(state) = self.qp_state.as_mut() {
                    match state.tracker.update(bgra) {
                        Ok(_) => state.observed = true,
                        Err(error) => {
                            // Damage is an optimisation, never a correctness
                            // requirement: a tracker failure must cost this
                            // frame its bias, not the session its encode.
                            state.observed = false;
                            crate::log(&format!("QP map: damage update failed: {error}"));
                        }
                    }
                }
                self.publish_bgra(bgra)
            }
            Err(error) => Err(error.to_string()),
        };
        self.context.Unmap(&dst, 0);
        self.last_stage_timing = StageTiming {
            copy_ms: self.last_copy_ms,
            readback_ms,
            conversion_ms: self.last_conversion_ms,
            mirror_ms: self.last_mirror_ms,
        };
        publish_result?;
        Ok(())
    }

    pub const fn stage_timing(&self) -> StageTiming {
        self.last_stage_timing
    }

    /// Convert `bgra` directly into the current slot's locked NVENC input
    /// buffer, then mirror those exact bytes into `self.latest` so
    /// `restage_latest` can republish them without re-running the
    /// conversion.
    ///
    /// A new generation is minted only once both writes have completed, since
    /// that is the point at which the slot and `self.latest` are known to hold
    /// the same bytes. Anything short of that leaves the slot marked unknown,
    /// and unknown always copies.
    unsafe fn publish_bgra(&mut self, bgra: BgraFrame<'_>) -> Result<(), String> {
        let slot = self.write_idx;
        self.generations.invalidated(slot);
        let mut lock: NV_ENC_LOCK_INPUT_BUFFER = zeroed();
        lock.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
        lock.inputBuffer = self.slots[slot].input_buffer;
        let lock_fn = self
            .fl
            .nvEncLockInputBuffer
            .ok_or_else(|| "missing nvEncLockInputBuffer".to_string())?;
        nvchk!(lock_fn(self.enc, &mut lock), "LockInputBuffer");

        let conversion_started = Instant::now();
        let write_result = self.check_locked_pitch(lock.pitch).and_then(|()| unsafe {
            write_locked_from_bgra(
                self.format,
                self.transform,
                bgra,
                lock.bufferDataPtr.cast::<u8>(),
                lock.pitch,
                (self.width, self.height),
                self.i444_conversion_workers,
            )
        });
        self.last_conversion_ms = conversion_started.elapsed().as_secs_f64() * 1000.0;
        self.last_mirror_ms = 0.0;
        if write_result.is_ok() {
            let mirror_started = Instant::now();
            std::ptr::copy_nonoverlapping(
                lock.bufferDataPtr.cast::<u8>(),
                self.latest.as_mut_ptr(),
                self.frame_bytes,
            );
            self.last_mirror_ms = mirror_started.elapsed().as_secs_f64() * 1000.0;
        }

        let unlock_fn = self
            .fl
            .nvEncUnlockInputBuffer
            .ok_or_else(|| "missing nvEncUnlockInputBuffer".to_string())?;
        let unlock_status = unlock_fn(self.enc, self.slots[slot].input_buffer);
        write_result?;
        if unlock_status != NV_ENC_SUCCESS {
            return Err(format!(
                "UnlockInputBuffer -> NVENC status {unlock_status:?}"
            ));
        }
        self.generations.published(slot);
        Ok(())
    }

    /// NVENC is expected to report the same pitch on every Lock for
    /// identically-created input buffers (checked once at construction — see
    /// `Encoder::new`). This is the runtime guard: silently trusting a
    /// changed pitch would misplace whole chroma planes.
    fn check_locked_pitch(&self, pitch: u32) -> Result<(), String> {
        if pitch == self.locked_pitch {
            Ok(())
        } else {
            Err(format!(
                "NVENC pitch changed between calls ({} -> {pitch})",
                self.locked_pitch
            ))
        }
    }

    /// Republish the last frame `stage()` successfully converted into the
    /// current ring slot, for an idle-frame submission when no new capture
    /// arrived. Without this, the ring cycles through stale slot contents and
    /// the desktop visibly alternates between old/new states.
    ///
    /// The copy is skipped when the slot already holds exactly the newest
    /// published generation — the steady state on a static desktop once the
    /// ring has rotated once. That skips a 30 MiB host memcpy *and* the
    /// Lock/Unlock around it, because the slot contents are already correct
    /// and NVENC never modifies an input buffer it has read. Any uncertainty
    /// copies; see [`SlotGenerations`].
    ///
    /// The targeted slot is `write_idx`, which `next_writable_slot` guarantees
    /// is not in flight, so this never mutates a buffer NVENC is still
    /// reading.
    pub unsafe fn restage_latest(&mut self) -> Result<RestageOutcome, String> {
        if !self.generations.has_latest() {
            return Ok(RestageOutcome::NoLatest);
        }
        let slot = self.write_idx;
        debug_assert!(
            !self.inflight.contains(&slot),
            "restage targeted an in-flight slot"
        );
        if !self.generations.needs_copy(slot) {
            return Ok(RestageOutcome::AlreadyCurrent);
        }
        self.generations.invalidated(slot);
        let mut lock: NV_ENC_LOCK_INPUT_BUFFER = zeroed();
        lock.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
        lock.inputBuffer = self.slots[slot].input_buffer;
        let lock_fn = self
            .fl
            .nvEncLockInputBuffer
            .ok_or_else(|| "missing nvEncLockInputBuffer".to_string())?;
        nvchk!(lock_fn(self.enc, &mut lock), "LockInputBuffer(restage)");

        let copy_result = self.check_locked_pitch(lock.pitch).map(|()| unsafe {
            std::ptr::copy_nonoverlapping(
                self.latest.as_ptr(),
                lock.bufferDataPtr.cast::<u8>(),
                self.frame_bytes,
            );
        });

        let unlock_fn = self
            .fl
            .nvEncUnlockInputBuffer
            .ok_or_else(|| "missing nvEncUnlockInputBuffer".to_string())?;
        let unlock_status = unlock_fn(self.enc, self.slots[slot].input_buffer);
        copy_result?;
        if unlock_status != NV_ENC_SUCCESS {
            return Err(format!(
                "UnlockInputBuffer -> NVENC status {unlock_status:?}"
            ));
        }
        self.generations.copied(slot);
        Ok(RestageOutcome::Copied)
    }

    /// Entries a QP delta map must have for this session.
    ///
    /// `0` means this encoder has no QP-map path — either the codec has no
    /// geometry, or the driver refused `qpMapMode` when the session was
    /// initialised. Callers size their [`arcen_media::video::QpDeltaMapBuilder`]
    /// from this and skip building a map entirely when it is zero.
    #[must_use]
    pub const fn qp_map_entries(&self) -> usize {
        self.qp_map_entries
    }

    /// Turn damage-driven QP biasing on for this session.
    ///
    /// Returns whether it actually engaged. `false` means the request was
    /// honestly refused — the driver declined `qpMapMode` at init, the codec
    /// has no QP-map geometry, or the damage tracker could not be built for
    /// this frame size — and the session encodes exactly as it did before.
    /// A caller that logs this gets a truthful record instead of assuming a
    /// feature it may not have.
    pub fn enable_qp_map(
        &mut self,
        policy: crate::qp_map::QpMapPolicy,
        bias: arcen_media::video::QpBias,
        codec: arcen_media::VideoCodec,
    ) -> bool {
        if !policy.submits_map() || self.qp_map_entries == 0 {
            self.qp_state = None;
            return false;
        }
        let Ok(builder) =
            arcen_media::video::QpDeltaMapBuilder::new(codec, self.width, self.height)
        else {
            self.qp_state = None;
            return false;
        };
        if builder.entry_count() != self.qp_map_entries {
            // The geometry we would submit disagrees with what this session
            // was sized for. Refuse rather than send a map NVENC will read
            // the wrong number of entries from.
            crate::log(&format!(
                "QP map disabled: builder wants {} entries, session expects {}",
                builder.entry_count(),
                self.qp_map_entries
            ));
            self.qp_state = None;
            return false;
        }
        let Ok(tracker) = arcen_keel::DamageTracker::new(
            self.width as usize,
            self.height as usize,
            arcen_keel::KernelPreference::Auto,
        ) else {
            self.qp_state = None;
            return false;
        };
        self.qp_state = Some(QpMapState {
            tracker,
            builder,
            bias,
            policy,
            observed: false,
        });
        true
    }

    fn next_writable_slot(&self) -> usize {
        self.slots
            .iter()
            .enumerate()
            .find_map(|(slot, _)| {
                (!self.inflight.iter().any(|inflight| *inflight == slot)).then_some(slot)
            })
            .expect("output drain policy always reserves one writable slot")
    }

    /// Lock the oldest output after the preset/cap-sized priming threshold.
    /// The synchronous path deliberately never sets
    /// `doNotWait`; older Linux drivers can crash on that mode.
    unsafe fn drain_oldest(&mut self) -> Result<Option<Vec<u8>>, String> {
        let done_slot = *self
            .inflight
            .front()
            .ok_or_else(|| "NVENC drain requested with no in-flight slot".to_string())?;
        let mut lock: NV_ENC_LOCK_BITSTREAM = zeroed();
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.slots[done_slot].bitstream;
        let lock_status = (self.fl.nvEncLockBitstream.unwrap())(self.enc, &mut lock);
        if lock_status != NV_ENC_SUCCESS {
            return Err(format!("LockBitstream -> NVENC status {lock_status:?}"));
        }

        let len = lock.bitstreamSizeInBytes as usize;
        let ptr = lock.bitstreamBufferPtr as *const u8;
        let data = std::slice::from_raw_parts(ptr, len).to_vec();
        let unlock_status =
            (self.fl.nvEncUnlockBitstream.unwrap())(self.enc, self.slots[done_slot].bitstream);
        if unlock_status != NV_ENC_SUCCESS {
            return Err(format!("UnlockBitstream -> NVENC status {unlock_status:?}"));
        }
        let completed = self.inflight.pop_front();
        debug_assert_eq!(completed, Some(done_slot));
        Ok(Some(data))
    }

    /// Submit the currently-staged slot for encode and return one ready
    /// Annex-B access unit, if any. `force_idr` requests a real IDR on the
    /// submitted frame (new client).
    ///
    /// When damage-driven QP biasing is engaged (see [`Self::enable_qp_map`]),
    /// the per-block map is built here from the damage observed by the most
    /// recent [`Self::stage`] call.
    ///
    /// The map is **suppressed on an IDR**: every block of a keyframe is coded
    /// intra, so "unchanged since the previous frame" describes nothing the
    /// encoder can act on, and a clean-region penalty applied there would be
    /// baked into the reference that every following frame predicts from. It
    /// is likewise suppressed for any frame submitted without a fresh
    /// observation, because a stale map describes a frame that has already
    /// been replaced.
    pub unsafe fn encode(&mut self, force_idr: bool) -> Result<Option<Vec<u8>>, String> {
        let slot = self.write_idx;

        let mut pic: NV_ENC_PIC_PARAMS = zeroed();
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.inputWidth = self.width;
        pic.inputHeight = self.height;
        pic.inputBuffer = self.slots[slot].input_buffer;
        pic.outputBitstream = self.slots[slot].bitstream;
        pic.bufferFmt = self.format.buffer_format();
        pic.pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
        let expected_entries = self.qp_map_entries;
        if let Some(state) = self.qp_state.as_mut() {
            let fresh = std::mem::take(&mut state.observed);
            let map = if force_idr || !fresh {
                Some(state.builder.build_neutral())
            } else {
                let bias = match state.policy {
                    crate::qp_map::QpMapPolicy::Neutral => arcen_media::video::QpBias::NEUTRAL,
                    _ => state.bias,
                };
                match crate::qp_map::fill_qp_delta_map(
                    &mut state.builder,
                    state.tracker.damage_map(),
                    bias,
                    false,
                ) {
                    Ok(map) => Some(map),
                    Err(error) => {
                        crate::log(&format!("QP map: build failed, encoding unbiased: {error}"));
                        None
                    }
                }
            };
            if let Some(map) = map {
                debug_assert_eq!(map.len(), expected_entries);
                if map.len() == expected_entries {
                    pic.qpDeltaMap = map.as_ptr().cast_mut();
                    pic.qpDeltaMapSize = u32::try_from(map.len()).unwrap_or(0);
                }
            }
        }
        if force_idr {
            pic.encodePicFlags = NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR.0
                | NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS.0;
            // Belt-and-braces: some drivers (observed: GRID vGPU R5xx) do not
            // honor FORCEIDR alone when PTD is enabled and pictureType was
            // zero-initialized to P — set the explicit type too.
            pic.pictureType = _NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
            crate::log(&format!(
                "EncodePicture: forced IDR submitted (slot {slot}, flags={:#x})",
                pic.encodePicFlags
            ));
        }
        let enc_status = (self.fl.nvEncEncodePicture.unwrap())(self.enc, &mut pic);
        if enc_status != NV_ENC_SUCCESS && enc_status != NV_ENC_ERR_NEED_MORE_INPUT {
            return Err(format!("EncodePicture -> {enc_status:?}"));
        }

        self.inflight.push_back(slot);
        // Drain the moment the encoder says a frame is ready, instead of
        // always filling the window first.
        //
        // `max_inflight` used to be a mandatory priming depth: Quality held
        // eight submissions before returning anything, which at 30 fps is a
        // measured 233 ms of latency added to every frame — on the mode a
        // colourist selects. That depth was tuned when the preset still had
        // B-frames and lookahead enabled and output genuinely lagged input.
        //
        // The encode status is a *pessimistic* oracle: NVENC returns
        // `NEED_MORE_INPUT` on some Linux drivers even when a blocking
        // `LockBitstream` would complete, but it never claims `SUCCESS`
        // without a frame to collect. So `SUCCESS` can be trusted to mean
        // "drain now", while `NEED_MORE_INPUT` falls back to exactly the
        // previous depth-based behaviour. A driver that reports honestly gets
        // one-in, one-out; a driver that does not is no worse off than before,
        // and neither can deadlock.
        //
        // `max_inflight` is therefore now a ceiling, not a target.
        let output_ready = enc_status == NV_ENC_SUCCESS;
        if !output_ready && self.inflight.len() < self.drain_policy.max_inflight() {
            self.write_idx = self.next_writable_slot();
            return Ok(None);
        }
        let output = self.drain_oldest()?;
        self.write_idx = self.next_writable_slot();
        Ok(output)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if let Some(encode) = self.fl.nvEncEncodePicture {
                let mut eos: NV_ENC_PIC_PARAMS = zeroed();
                eos.version = NV_ENC_PIC_PARAMS_VER;
                eos.encodePicFlags = NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_EOS.0;
                let _ = encode(self.enc, &mut eos);
            }
            // EOS makes every accepted output lockable before resources are
            // destroyed. Discard the bytes, but complete lock/unlock so a
            // delayed quality pipeline cannot leave live bitstreams behind.
            while matches!(self.drain_oldest(), Ok(Some(_))) {}
            self.inflight.clear();
            cleanup_slots(&self.fl, self.enc, &mut self.slots);
            destroy_encoder(&self.fl, &mut self.enc);
        }
    }
}

/// Convert `bgra` into `format`'s coded samples, writing directly into an
/// NVENC system-memory input buffer that is currently locked (`ptr`/`pitch`
/// exactly as `nvEncLockInputBuffer` returned them).
///
/// # Safety
/// `ptr` must be valid and writable for `frame_bytes(format, pitch, height)`
/// bytes (guaranteed by the driver for a buffer created with
/// `nvEncCreateInputBuffer(width, height, format.buffer_format())` that is
/// currently locked and not yet unlocked) and, for the 16-bit formats,
/// aligned for `u16` (guaranteed in practice: driver allocations back a
/// format the driver itself knows is 2-bytes/sample, and are never as
/// tightly aligned as 2 bytes to begin with).
unsafe fn write_locked_from_bgra(
    format: PixelFormat,
    transform: ColorTransform,
    bgra: BgraFrame<'_>,
    ptr: *mut u8,
    pitch: u32,
    dimensions: (u32, u32),
    i444_conversion_workers: usize,
) -> Result<(), String> {
    let (width, height) = dimensions;
    let pitch = pitch as usize;
    match format {
        PixelFormat::Nv12 => {
            let luma_len = pitch * height as usize;
            let uv_len = pitch * chroma_rows(format, height);
            let y = std::slice::from_raw_parts_mut(ptr, luma_len);
            let uv = std::slice::from_raw_parts_mut(ptr.add(luma_len), uv_len);
            let mut frame =
                Nv12FrameMut::new(width, height, y, pitch, uv, pitch).map_err(|e| e.to_string())?;
            convert_bgra_to_nv12(bgra, &mut frame, transform).map_err(|e| e.to_string())
        }
        PixelFormat::Yuv444_8 => {
            let plane_len = pitch * height as usize;
            let y = std::slice::from_raw_parts_mut(ptr, plane_len);
            let u = std::slice::from_raw_parts_mut(ptr.add(plane_len), plane_len);
            let v = std::slice::from_raw_parts_mut(ptr.add(plane_len * 2), plane_len);
            let mut frame = I444FrameMut::new(width, height, [y, u, v], [pitch, pitch, pitch])
                .map_err(|e| e.to_string())?;
            convert_bgra_to_i444(bgra, &mut frame, transform).map_err(|e| e.to_string())
        }
        PixelFormat::Yuv444_10 => {
            // NVENC's pitch is in bytes; each row holds `width` u16 samples,
            // so the sample stride is pitch/bytes_per_sample (the driver
            // guarantees a pitch evenly divisible by it for a 2-byte format).
            let stride = pitch / format.bytes_per_sample();
            let plane_samples = stride * height as usize;
            let ptr16 = ptr.cast::<u16>();
            let y = std::slice::from_raw_parts_mut(ptr16, plane_samples);
            let u = std::slice::from_raw_parts_mut(ptr16.add(plane_samples), plane_samples);
            let v = std::slice::from_raw_parts_mut(ptr16.add(plane_samples * 2), plane_samples);
            convert_bgra_to_i444_p16_parallel(
                bgra,
                [y, u, v],
                [stride, stride, stride],
                width,
                height,
                transform,
                i444_conversion_workers,
            )
        }
        PixelFormat::P010 => {
            let stride = pitch / format.bytes_per_sample();
            let luma_samples = stride * height as usize;
            let uv_samples = stride * chroma_rows(format, height);
            let ptr16 = ptr.cast::<u16>();
            let y = std::slice::from_raw_parts_mut(ptr16, luma_samples);
            let uv = std::slice::from_raw_parts_mut(ptr16.add(luma_samples), uv_samples);
            write_p010_rows(
                transform,
                bgra,
                y,
                stride,
                uv,
                stride,
                width as usize,
                height as usize,
            )
        }
    }
}

fn split_i444_plane_rows<'a>(
    mut plane: &'a mut [u16],
    stride: usize,
    row_counts: &[usize],
) -> Result<Vec<&'a mut [u16]>, String> {
    let mut chunks = Vec::with_capacity(row_counts.len());
    for &rows in row_counts {
        let samples = stride
            .checked_mul(rows)
            .ok_or_else(|| "I444P16 conversion chunk size overflow".to_string())?;
        if plane.len() < samples {
            return Err("I444P16 conversion plane is shorter than its row chunks".to_string());
        }
        let (chunk, remaining) = plane.split_at_mut(samples);
        chunks.push(chunk);
        plane = remaining;
    }
    if !plane.is_empty() {
        return Err("I444P16 conversion row chunks did not cover the plane".to_string());
    }
    Ok(chunks)
}

struct I444P16ConversionJob<'source, 'destination> {
    source: BgraFrame<'source>,
    planes: [&'destination mut [u16]; 3],
    strides: [usize; 3],
    width: u32,
    height: u32,
    transform: ColorTransform,
}

impl I444P16ConversionJob<'_, '_> {
    fn run(self) -> Result<(), String> {
        let mut destination =
            I444P16FrameMut::new(self.width, self.height, self.planes, self.strides)
                .map_err(|error| error.to_string())?;
        convert_bgra_to_i444_p16_rows(
            self.source,
            &mut destination,
            0..self.height as usize,
            self.transform,
        )
        .map_err(|error| error.to_string())
    }
}

fn convert_bgra_to_i444_p16_parallel(
    source: BgraFrame<'_>,
    planes: [&mut [u16]; 3],
    strides: [usize; 3],
    width: u32,
    height: u32,
    transform: ColorTransform,
    workers: usize,
) -> Result<(), String> {
    let geometry = source.grid();
    if geometry.width() != width as usize || geometry.height() != height as usize {
        return Err("source and I444P16 destination geometry do not match".to_string());
    }

    let height = height as usize;
    if height == 0 {
        return Err("I444P16 conversion height must be non-zero".to_string());
    }
    let workers = workers.max(1).min(height);
    let base_rows = height / workers;
    let extra_rows = height % workers;
    let row_counts = (0..workers)
        .map(|worker| base_rows + usize::from(worker < extra_rows))
        .collect::<Vec<_>>();

    let [y, u, v] = planes;
    let y_chunks = split_i444_plane_rows(y, strides[0], &row_counts)?;
    let u_chunks = split_i444_plane_rows(u, strides[1], &row_counts)?;
    let v_chunks = split_i444_plane_rows(v, strides[2], &row_counts)?;
    let source_stride = source.stride();
    let source_pixels = source.pixels();
    let mut first_row = 0usize;
    let mut jobs = Vec::with_capacity(workers);

    for (((y, u), v), rows) in y_chunks
        .into_iter()
        .zip(u_chunks)
        .zip(v_chunks)
        .zip(row_counts)
    {
        let last_row = first_row + rows;
        let source_start = first_row
            .checked_mul(source_stride)
            .ok_or_else(|| "I444P16 source chunk offset overflow".to_string())?;
        let source_end = last_row
            .checked_mul(source_stride)
            .ok_or_else(|| "I444P16 source chunk offset overflow".to_string())?;
        let chunk_source = BgraFrame::new(
            &source_pixels[source_start..source_end],
            width as usize,
            rows,
            source_stride,
        )
        .map_err(|error| error.to_string())?;
        jobs.push(I444P16ConversionJob {
            source: chunk_source,
            planes: [y, u, v],
            strides,
            width,
            height: rows as u32,
            transform,
        });
        first_row = last_row;
    }

    if jobs.len() == 1 {
        return jobs.pop().expect("one conversion job").run();
    }

    std::thread::scope(|scope| {
        let current_job = jobs.pop().expect("at least two conversion jobs");
        let handles = jobs
            .into_iter()
            .enumerate()
            .map(|(index, job)| {
                std::thread::Builder::new()
                    .name(format!("arcen-i444-{index}"))
                    .spawn_scoped(scope, move || job.run())
                    .map_err(|error| format!("could not spawn I444P16 conversion worker: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut result = current_job.run();
        for handle in handles {
            match handle.join() {
                Ok(worker_result) if result.is_ok() => result = worker_result,
                Ok(_) => {}
                Err(_) if result.is_ok() => {
                    result = Err("I444P16 conversion worker panicked".to_string());
                }
                Err(_) => {}
            }
        }
        result
    })
}

/// Convert `bgra` to `NV_ENC_BUFFER_FORMAT_YUV420_10BIT` samples: semi-planar
/// 4:2:0, MSB-aligned 16-bit (see `PixelFormat::P010`'s doc for the layout).
///
/// `arcen_media::video::convert` does not (yet) expose a BGRA -> 4:2:0 10-bit
/// conversion — only 8-bit NV12/I420 and 8/16-bit 4:4:4 (see
/// `shared/media/src/video/convert.rs`). This mirrors that module's own 2x2
/// box-filter chroma subsampling (`convert_rows`) by hand, because this
/// change is confined to `hosts/capenc/src` and cannot add a shared
/// conversion function there. **A reviewer should consider upstreaming a
/// `convert_bgra_to_nv12_p16` + matching semi-planar 16-bit frame type into
/// `arcen_media` so this isn't a second, separately-tested copy of the
/// algorithm** — see the final report for why this was done here instead of
/// left unsupported like `Yuv422Unsupported`.
#[allow(clippy::too_many_arguments)] // one parameter per plane/stride, like arcen_media's own I420Frame::new.
fn write_p010_rows(
    transform: ColorTransform,
    bgra: BgraFrame<'_>,
    y: &mut [u16],
    y_stride: usize,
    uv: &mut [u16],
    uv_stride: usize,
    width: usize,
    height: usize,
) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("YUV420_10BIT frame dimensions must be non-zero".to_string());
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err("YUV420_10BIT frame dimensions must both be even".to_string());
    }
    for row_pair in 0..height / 2 {
        let top = row_pair * 2;
        let s0 = bgra.active_row(top).ok_or("source row out of range")?;
        let s1 = bgra.active_row(top + 1).ok_or("source row out of range")?;
        let (y_head, y_tail) = y.split_at_mut((top + 1) * y_stride);
        let y0 = &mut y_head[top * y_stride..top * y_stride + width];
        let y1 = &mut y_tail[..width];

        for (pair, (p0, p1)) in s0.chunks_exact(8).zip(s1.chunks_exact(8)).enumerate() {
            let x = pair * 2;
            y0[x] = transform.pack_p16(transform.luma(p0[0], p0[1], p0[2]));
            y0[x + 1] = transform.pack_p16(transform.luma(p0[4], p0[5], p0[6]));
            y1[x] = transform.pack_p16(transform.luma(p1[0], p1[1], p1[2]));
            y1[x + 1] = transform.pack_p16(transform.luma(p1[4], p1[5], p1[6]));

            // Same box filter as arcen_media's convert_rows: average the 2x2
            // BGR block once, then convert, rather than converting four
            // times and averaging codes.
            let mean = |a: u8, b: u8, c: u8, d: u8| -> u8 {
                ((i32::from(a) + i32::from(b) + i32::from(c) + i32::from(d)) >> 2) as u8
            };
            let b = mean(p0[0], p0[4], p1[0], p1[4]);
            let g = mean(p0[1], p0[5], p1[1], p1[5]);
            let r = mean(p0[2], p0[6], p1[2], p1[6]);
            let u_sample = transform.pack_p16(transform.cb(b, g, r));
            let v_sample = transform.pack_p16(transform.cr(b, g, r));
            let offset = row_pair * uv_stride + x;
            uv[offset] = u_sample;
            uv[offset + 1] = v_sample;
        }
    }
    Ok(())
}

#[cfg(test)]
mod init_error_tests {
    use super::*;
    use std::sync::Mutex;

    static CLEANUP_EVENTS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

    unsafe extern "C" fn destroy_bitstream(
        _encoder: *mut c_void,
        _bitstream: NV_ENC_OUTPUT_PTR,
    ) -> NVENCSTATUS {
        CLEANUP_EVENTS.lock().unwrap().push("bitstream");
        NV_ENC_SUCCESS
    }

    unsafe extern "C" fn destroy_input_buffer(
        _encoder: *mut c_void,
        _input_buffer: NV_ENC_INPUT_PTR,
    ) -> NVENCSTATUS {
        CLEANUP_EVENTS.lock().unwrap().push("input_buffer");
        NV_ENC_SUCCESS
    }

    unsafe extern "C" fn destroy_session(_encoder: *mut c_void) -> NVENCSTATUS {
        CLEANUP_EVENTS.lock().unwrap().push("encoder");
        NV_ENC_SUCCESS
    }

    #[test]
    fn only_typed_unavailable_session_and_unsupported_statuses_allow_fallback() {
        for status in [
            NV_ENC_ERR_NO_ENCODE_DEVICE,
            NV_ENC_ERR_UNSUPPORTED_DEVICE,
            NV_ENC_ERR_DEVICE_NOT_EXIST,
            NV_ENC_ERR_UNSUPPORTED_PARAM,
            NV_ENC_ERR_UNIMPLEMENTED,
        ] {
            assert!(NvencInitError::from_status(status, "test").allows_software_fallback());
        }
        for status in [
            NV_ENC_ERR_INVALID_PARAM,
            NV_ENC_ERR_OUT_OF_MEMORY,
            NV_ENC_ERR_NOT_ENOUGH_BUFFER,
            NV_ENC_ERR_GENERIC,
            NV_ENC_ERR_RESOURCE_REGISTER_FAILED,
            NV_ENC_ERR_LOCK_BUSY,
            NV_ENC_ERR_ENCODER_BUSY,
        ] {
            assert!(!NvencInitError::from_status(status, "test").allows_software_fallback());
        }
    }

    #[test]
    fn a_colour_spec_rejection_is_reported_as_unsupported_not_fatal() {
        let error =
            NvencInitError::unsupported(PixelFormatRejection::TwelveBitUnsupported.to_string());
        assert!(error.allows_software_fallback());
        assert_eq!(
            error.unavailable_reason(),
            Some(arcen_media::video::BackendUnavailableReason::UnsupportedConfiguration)
        );
    }

    #[test]
    fn native_handles_are_cleaned_in_dependency_order_exactly_once() {
        CLEANUP_EVENTS.lock().unwrap().clear();
        let mut functions: NV_ENCODE_API_FUNCTION_LIST = unsafe { std::mem::zeroed() };
        functions.nvEncDestroyBitstreamBuffer = Some(destroy_bitstream);
        functions.nvEncDestroyInputBuffer = Some(destroy_input_buffer);
        functions.nvEncDestroyEncoder = Some(destroy_session);
        let mut encoder = 1usize as *mut c_void;

        unsafe {
            cleanup_native_slot(
                &functions,
                encoder,
                2usize as NV_ENC_OUTPUT_PTR,
                3usize as NV_ENC_INPUT_PTR,
            );
            destroy_encoder(&functions, &mut encoder);
            destroy_encoder(&functions, &mut encoder);
        }

        assert_eq!(
            CLEANUP_EVENTS.lock().unwrap().as_slice(),
            ["bitstream", "input_buffer", "encoder"]
        );
        assert!(encoder.is_null());
    }
}

#[cfg(test)]
mod slot_generation_tests {
    use super::{RestageOutcome, SlotGenerations};

    const DEPTH: usize = 4;

    /// A fresh encoder has published nothing, so nothing may be skipped.
    #[test]
    fn nothing_is_current_before_the_first_publish() {
        let generations = SlotGenerations::new(DEPTH);
        assert!(!generations.has_latest());
        for slot in 0..DEPTH {
            assert!(generations.needs_copy(slot));
        }
        // An out-of-range slot is uncertainty, and uncertainty copies.
        assert!(generations.needs_copy(DEPTH));
    }

    /// The static-desktop steady state: one publish, then the ring rotates.
    /// Every other slot must be refreshed exactly once, and from the second
    /// rotation onwards nothing is copied at all.
    #[test]
    fn a_static_frame_is_copied_once_per_slot_and_never_again() {
        let mut generations = SlotGenerations::new(DEPTH);
        generations.published(0);
        assert!(generations.has_latest());
        assert!(!generations.needs_copy(0));

        let mut copies = 0;
        for rotation in 0..8 {
            for slot in 0..DEPTH {
                if rotation == 0 && slot == 0 {
                    continue; // the slot the publish itself wrote
                }
                if generations.needs_copy(slot) {
                    copies += 1;
                    generations.copied(slot);
                }
            }
        }
        assert_eq!(
            copies,
            DEPTH - 1,
            "a static frame was copied more than once per slot"
        );
    }

    /// A new capture must invalidate every other slot, or the ring would
    /// republish the previous desktop as if it were current. This is the
    /// failure mode that made the desktop visibly alternate between states.
    #[test]
    fn a_new_publish_makes_every_other_slot_stale_again() {
        let mut generations = SlotGenerations::new(DEPTH);
        generations.published(0);
        for slot in 1..DEPTH {
            generations.copied(slot);
            assert!(!generations.needs_copy(slot));
        }

        generations.published(1);
        assert!(!generations.needs_copy(1));
        for slot in [0, 2, 3] {
            assert!(
                generations.needs_copy(slot),
                "slot {slot} kept a stale frame after a new publish"
            );
        }
    }

    /// A failed write leaves the buffer holding an unknown mixture, so it must
    /// copy on the next submission even though `latest` did not move.
    #[test]
    fn a_failed_write_forces_the_next_copy_without_losing_latest() {
        let mut generations = SlotGenerations::new(DEPTH);
        generations.published(2);
        assert!(!generations.needs_copy(2));

        generations.invalidated(2);
        assert!(generations.has_latest(), "a failed slot write lost latest");
        assert!(generations.needs_copy(2));

        generations.copied(2);
        assert!(!generations.needs_copy(2));
    }

    /// A publish that fails part way through must not mint a generation: the
    /// slot would then be skipped while holding a half-written frame.
    #[test]
    fn an_abandoned_publish_never_mints_a_generation() {
        let mut generations = SlotGenerations::new(DEPTH);
        generations.published(0);
        generations.copied(1);

        // Publish into slot 1 starts (invalidate) and then fails.
        generations.invalidated(1);
        assert!(generations.needs_copy(1));
        // The frame published into slot 0 is still the newest known good one.
        assert!(!generations.needs_copy(0));
    }

    /// Generations must keep increasing across a full ring wrap, so a slot
    /// written many rotations ago can never compare equal to the newest one.
    #[test]
    fn generations_do_not_collide_across_ring_wraps() {
        let mut generations = SlotGenerations::new(DEPTH);
        generations.published(0);
        generations.copied(1);
        for round in 0..64 {
            generations.published(round % DEPTH);
        }
        // Slot 1 has not been touched since the very first frame.
        assert!(
            generations.needs_copy(1),
            "an ancient slot compared equal to the newest generation"
        );
    }

    /// Delayed output holds slots in flight, but the tracker only ever answers
    /// about the slot the caller is about to write, so a longer drain queue
    /// cannot change the bookkeeping.
    #[test]
    fn delayed_output_does_not_disturb_untouched_slots() {
        let mut generations = SlotGenerations::new(DEPTH);
        generations.published(0);
        generations.copied(1);
        generations.copied(2);
        // Nothing else happens for a while: no drain, no capture.
        for _ in 0..16 {
            assert!(!generations.needs_copy(1));
            assert!(!generations.needs_copy(2));
            assert!(generations.needs_copy(3));
        }
    }

    #[test]
    fn only_a_staged_outcome_may_be_submitted() {
        assert!(RestageOutcome::Copied.is_staged());
        assert!(RestageOutcome::AlreadyCurrent.is_staged());
        assert!(!RestageOutcome::NoLatest.is_staged());
    }
}

#[cfg(test)]
mod staged_capture_tests {
    use super::StagedCapture;

    /// The DXGI frame is released between copy and publish, so the two phases
    /// are separately observable and their ordering has to be enforced rather
    /// than assumed.
    #[test]
    fn a_copy_must_precede_every_publish() {
        let mut staged = StagedCapture::default();
        assert!(!staged.is_pending());
        // Publish without copy: refused, and the refusal is not sticky.
        assert!(!staged.take());
        assert!(!staged.is_pending());

        staged.copied();
        assert!(staged.is_pending());
        assert!(staged.take());
        // Publish consumes the claim, so the same staging contents can never
        // be announced twice as a fresh capture.
        assert!(!staged.is_pending());
        assert!(!staged.take());
    }

    /// A live desktop always wants the newest image. Two copies before a
    /// publish must collapse to one pending frame, never queue.
    #[test]
    fn a_second_copy_replaces_the_pending_frame_instead_of_queueing_it() {
        let mut staged = StagedCapture::default();
        staged.copied();
        staged.copied();
        staged.copied();
        assert!(staged.take());
        assert!(!staged.take());
    }

    /// A failed copy leaves the staging texture holding an indeterminate
    /// mixture of frames, so the claim must be dropped rather than published.
    #[test]
    fn a_failed_copy_resets_the_claim_and_recovers_on_the_next_copy() {
        let mut staged = StagedCapture::default();
        staged.copied();
        staged.copy_failed();
        assert!(!staged.is_pending());
        assert!(!staged.take());

        // The next successful copy re-establishes it normally.
        staged.copied();
        assert!(staged.take());
    }

    /// The steady-state loop: copy, release, publish, repeat.
    #[test]
    fn alternating_copy_and_publish_never_leaves_a_frame_behind() {
        let mut staged = StagedCapture::default();
        for _ in 0..8 {
            staged.copied();
            assert!(staged.take());
            assert!(!staged.is_pending());
        }
    }
}

#[cfg(test)]
mod pixel_format_tests {
    use super::*;
    use arcen_media::{ColorPrimaries, ColorRange, TransferCharacteristics};

    fn color(chroma: ChromaSubsampling, bit_depth: BitDepth) -> crate::ColorSpec {
        crate::ColorSpec {
            chroma,
            bit_depth,
            range: ColorRange::Limited,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }

    #[test]
    fn resolves_every_supported_combination_for_both_codecs() {
        for codec in [NvencCodec::H264, NvencCodec::Hevc] {
            assert_eq!(
                resolve_pixel_format(codec, color(ChromaSubsampling::Yuv420, BitDepth::Eight)),
                Ok(PixelFormat::Nv12),
                "{codec:?} 4:2:0 8-bit"
            );
        }
        assert_eq!(
            resolve_pixel_format(
                NvencCodec::Hevc,
                color(ChromaSubsampling::Yuv420, BitDepth::Ten)
            ),
            Ok(PixelFormat::P010)
        );
        for codec in [NvencCodec::H264, NvencCodec::Hevc] {
            assert_eq!(
                resolve_pixel_format(codec, color(ChromaSubsampling::Yuv444, BitDepth::Eight)),
                Ok(PixelFormat::Yuv444_8),
                "{codec:?} 4:4:4 8-bit"
            );
        }
        assert_eq!(
            resolve_pixel_format(
                NvencCodec::Hevc,
                color(ChromaSubsampling::Yuv444, BitDepth::Ten)
            ),
            Ok(PixelFormat::Yuv444_10)
        );
    }

    #[test]
    fn resolves_av1_main_profile_at_both_bit_depths_yuv420_only() {
        assert_eq!(
            resolve_pixel_format(
                NvencCodec::Av1,
                color(ChromaSubsampling::Yuv420, BitDepth::Eight)
            ),
            Ok(PixelFormat::Nv12),
            "AV1 Main 4:2:0 8-bit reuses the same Nv12 surface as H.264/HEVC"
        );
        assert_eq!(
            resolve_pixel_format(
                NvencCodec::Av1,
                color(ChromaSubsampling::Yuv420, BitDepth::Ten)
            ),
            Ok(PixelFormat::P010),
            "AV1 Main 4:2:0 10-bit reuses the same P010 surface as HEVC Main10"
        );
    }

    #[test]
    fn rejects_av1_yuv444_at_every_depth_it_would_otherwise_accept() {
        for depth in [BitDepth::Eight, BitDepth::Ten] {
            assert_eq!(
                resolve_pixel_format(NvencCodec::Av1, color(ChromaSubsampling::Yuv444, depth)),
                Err(PixelFormatRejection::Av1RequiresYuv420(
                    ChromaSubsampling::Yuv444
                )),
                "AV1 {depth:?}-bit 4:4:4 must be refused: NVENC exposes only \
                 NV_ENC_AV1_PROFILE_MAIN_GUID (4:2:0)"
            );
        }
        assert!(
            PixelFormatRejection::Av1RequiresYuv420(ChromaSubsampling::Yuv444)
                .to_string()
                .contains("Yuv444"),
            "the error must name the offending chroma"
        );
    }

    #[test]
    fn rejects_twelve_bit_at_every_chroma_before_anything_else() {
        for chroma in [
            ChromaSubsampling::Yuv420,
            ChromaSubsampling::Yuv422,
            ChromaSubsampling::Yuv444,
        ] {
            for codec in [NvencCodec::H264, NvencCodec::Hevc, NvencCodec::Av1] {
                assert_eq!(
                    resolve_pixel_format(codec, color(chroma, BitDepth::Twelve)),
                    Err(PixelFormatRejection::TwelveBitUnsupported),
                    "{codec:?} {chroma:?} 12-bit must never silently truncate to 10"
                );
            }
        }
    }

    #[test]
    fn rejects_yuv422_at_every_supported_depth() {
        for depth in [BitDepth::Eight, BitDepth::Ten] {
            for codec in [NvencCodec::H264, NvencCodec::Hevc, NvencCodec::Av1] {
                assert_eq!(
                    resolve_pixel_format(codec, color(ChromaSubsampling::Yuv422, depth)),
                    Err(PixelFormatRejection::Yuv422Unsupported),
                    "{codec:?} 4:2:2 {depth:?}"
                );
            }
        }
    }

    #[test]
    fn rejects_h264_above_eight_bits_for_every_chroma_the_h265_path_accepts() {
        for chroma in [ChromaSubsampling::Yuv420, ChromaSubsampling::Yuv444] {
            assert_eq!(
                resolve_pixel_format(NvencCodec::H264, color(chroma, BitDepth::Ten)),
                Err(PixelFormatRejection::H264RequiresEightBit(BitDepth::Ten))
            );
        }
    }

    fn identity_color(chroma: ChromaSubsampling, bit_depth: BitDepth) -> crate::ColorSpec {
        crate::ColorSpec {
            matrix: ColorMatrix::Identity,
            ..color(chroma, bit_depth)
        }
    }

    #[test]
    fn identity_matrix_is_accepted_only_at_yuv444() {
        for codec in [NvencCodec::H264, NvencCodec::Hevc] {
            assert_eq!(
                resolve_pixel_format(
                    codec,
                    identity_color(ChromaSubsampling::Yuv444, BitDepth::Eight)
                ),
                Ok(PixelFormat::Yuv444_8),
                "{codec:?} identity 4:4:4 8-bit must reach the encoder unchanged"
            );
        }
        assert_eq!(
            resolve_pixel_format(
                NvencCodec::Hevc,
                identity_color(ChromaSubsampling::Yuv444, BitDepth::Ten)
            ),
            Ok(PixelFormat::Yuv444_10),
            "identity 4:4:4 10-bit (the GBR probe-matrix row) must reach the encoder"
        );
    }

    #[test]
    fn identity_matrix_below_yuv444_is_rejected_with_a_typed_error_naming_the_chroma() {
        // Subsampling GBR would discard three quarters of the red and blue
        // channels — this is the same rule
        // `arcen_media::video::VideoVariant::is_coherent` enforces at the
        // variant-id layer, checked again here because `ColorSpec` can be
        // built directly without ever going through a variant id.
        for depth in [BitDepth::Eight, BitDepth::Ten] {
            assert_eq!(
                resolve_pixel_format(
                    NvencCodec::Hevc,
                    identity_color(ChromaSubsampling::Yuv420, depth)
                ),
                Err(PixelFormatRejection::IdentityRequiresYuv444(
                    ChromaSubsampling::Yuv420
                )),
                "identity at 4:2:0 {depth:?}-bit must be refused, not silently encoded"
            );
        }
        // 4:2:2 is unsupported in its own right (`Yuv422Unsupported`, checked
        // first in `resolve_pixel_format`), and that reason must win rather
        // than being masked by the identity-specific one.
        assert_eq!(
            resolve_pixel_format(
                NvencCodec::Hevc,
                identity_color(ChromaSubsampling::Yuv422, BitDepth::Ten)
            ),
            Err(PixelFormatRejection::Yuv422Unsupported)
        );
        assert!(
            PixelFormatRejection::IdentityRequiresYuv444(ChromaSubsampling::Yuv420)
                .to_string()
                .contains("Yuv420"),
            "the error must name the offending chroma, not just say \"unsupported\""
        );
    }

    #[test]
    fn identity_matrix_on_av1_is_rejected_by_the_chroma_rule_not_silently_dropped() {
        // AV1 Main profile is 4:2:0-only, so the *only* chroma where an
        // identity (GBR) matrix would otherwise be legal (Yuv444) is exactly
        // the one AV1 cannot reach at all; `Av1RequiresYuv420` must be the
        // reported reason.
        for depth in [BitDepth::Eight, BitDepth::Ten] {
            assert_eq!(
                resolve_pixel_format(
                    NvencCodec::Av1,
                    identity_color(ChromaSubsampling::Yuv444, depth)
                ),
                Err(PixelFormatRejection::Av1RequiresYuv420(
                    ChromaSubsampling::Yuv444
                ))
            );
        }
    }

    #[test]
    fn codec_guid_selects_the_right_nvenc_codec_for_all_three_codecs() {
        assert_eq!(NvencCodec::H264.codec_guid(), NV_ENC_CODEC_H264_GUID);
        assert_eq!(NvencCodec::Hevc.codec_guid(), NV_ENC_CODEC_HEVC_GUID);
        assert_eq!(NvencCodec::Av1.codec_guid(), NV_ENC_CODEC_AV1_GUID);
    }

    #[test]
    fn nvenc_codec_parses_the_exact_three_tokens_capenc_uses_and_nothing_else() {
        assert_eq!(NvencCodec::parse("h264"), Some(NvencCodec::H264));
        assert_eq!(NvencCodec::parse("h265"), Some(NvencCodec::Hevc));
        assert_eq!(NvencCodec::parse("av1"), Some(NvencCodec::Av1));
        // No aliasing and no default: an unrecognised token must be `None`,
        // not silently treated as H.264 the way `codec != "h265"` used to.
        assert_eq!(NvencCodec::parse("hevc"), None);
        assert_eq!(NvencCodec::parse("vp9"), None);
        assert_eq!(NvencCodec::parse("H264"), None);
        assert_eq!(NvencCodec::parse(""), None);
    }

    #[test]
    fn profile_override_matches_hevc_rext_and_main10_only_where_expected() {
        assert_eq!(
            profile_guid_override(NvencCodec::H264, PixelFormat::Nv12),
            None
        );
        assert_eq!(
            profile_guid_override(NvencCodec::Hevc, PixelFormat::Nv12),
            None
        );
        assert_eq!(
            profile_guid_override(NvencCodec::Hevc, PixelFormat::P010),
            Some(NV_ENC_HEVC_PROFILE_MAIN10_GUID)
        );
        assert_eq!(
            profile_guid_override(NvencCodec::H264, PixelFormat::Yuv444_8),
            Some(NV_ENC_H264_PROFILE_HIGH_444_GUID)
        );
        assert_eq!(
            profile_guid_override(NvencCodec::Hevc, PixelFormat::Yuv444_8),
            Some(NV_ENC_HEVC_PROFILE_FREXT_GUID)
        );
        assert_eq!(
            profile_guid_override(NvencCodec::Hevc, PixelFormat::Yuv444_10),
            Some(NV_ENC_HEVC_PROFILE_FREXT_GUID)
        );
        // AV1 always gets its one and only profile GUID, at both bit depths
        // -- never assumed from the preset default (see the doc on
        // `profile_guid_override`).
        assert_eq!(
            profile_guid_override(NvencCodec::Av1, PixelFormat::Nv12),
            Some(NV_ENC_AV1_PROFILE_MAIN_GUID)
        );
        assert_eq!(
            profile_guid_override(NvencCodec::Av1, PixelFormat::P010),
            Some(NV_ENC_AV1_PROFILE_MAIN_GUID)
        );
    }

    #[test]
    fn apply_av1_color_maps_every_colorspec_field_onto_the_sequence_header() {
        use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
        use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
        use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;

        let full_range_bt2020 = crate::ColorSpec {
            range: ColorRange::Full,
            matrix: ColorMatrix::Bt2020Ncl,
            primaries: ColorPrimaries::Bt2020,
            transfer: TransferCharacteristics::Pq,
            ..color(ChromaSubsampling::Yuv420, BitDepth::Ten)
        };
        let mut config: NV_ENC_CONFIG_AV1 = unsafe { zeroed() };
        apply_av1_color(&mut config, full_range_bt2020);
        assert_eq!(config.colorPrimaries, NV_ENC_VUI_COLOR_PRIMARIES_BT2020);
        assert_eq!(
            config.transferCharacteristics,
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084
        );
        assert_eq!(
            config.matrixCoefficients,
            NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL
        );
        assert_eq!(config.colorRange, 1, "full range must signal colorRange=1");
    }

    #[test]
    fn apply_av1_color_signals_limited_range_as_zero_and_bt709_defaults() {
        use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
        use crate::nvenc_sys::nvEncodeAPI::_NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;

        let limited_bt709 = color(ChromaSubsampling::Yuv420, BitDepth::Eight);
        let mut config: NV_ENC_CONFIG_AV1 = unsafe { zeroed() };
        apply_av1_color(&mut config, limited_bt709);
        assert_eq!(config.colorPrimaries, NV_ENC_VUI_COLOR_PRIMARIES_BT709);
        assert_eq!(
            config.transferCharacteristics,
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709
        );
        assert_eq!(config.matrixCoefficients, NV_ENC_VUI_MATRIX_COEFFS_BT709);
        assert_eq!(
            config.colorRange, 0,
            "limited range must signal colorRange=0"
        );
    }

    #[test]
    fn av1_and_vui_colour_paths_never_drift_apart() {
        // The AV1 sequence header and the H.264/HEVC VUI struct are built
        // from the exact same `nvenc_color_primaries`/
        // `nvenc_transfer_characteristics`/`nvenc_matrix_coefficients`
        // helpers, so this asserts they can never silently diverge.
        for primaries in [
            ColorPrimaries::Bt709,
            ColorPrimaries::Bt2020,
            ColorPrimaries::DisplayP3,
        ] {
            let spec = crate::ColorSpec {
                primaries,
                ..color(ChromaSubsampling::Yuv420, BitDepth::Eight)
            };
            assert_eq!(
                vui_parameters(spec).colourPrimaries,
                nvenc_color_primaries(primaries)
            );
        }
        for transfer in [
            TransferCharacteristics::Bt709,
            TransferCharacteristics::Srgb,
            TransferCharacteristics::Pq,
            TransferCharacteristics::Hlg,
        ] {
            let spec = crate::ColorSpec {
                transfer,
                ..color(ChromaSubsampling::Yuv420, BitDepth::Eight)
            };
            assert_eq!(
                vui_parameters(spec).transferCharacteristics,
                nvenc_transfer_characteristics(transfer)
            );
        }
        for matrix in [
            ColorMatrix::Identity,
            ColorMatrix::Bt709,
            ColorMatrix::Bt601,
            ColorMatrix::Bt2020Ncl,
        ] {
            let spec = crate::ColorSpec {
                matrix,
                ..color(ChromaSubsampling::Yuv444, BitDepth::Eight)
            };
            assert_eq!(
                vui_parameters(spec).colourMatrix,
                nvenc_matrix_coefficients(matrix)
            );
        }
    }

    #[test]
    fn buffer_format_and_chroma_format_idc_match_the_nvenc_convention() {
        assert_eq!(PixelFormat::Nv12.buffer_format(), NV_ENC_BUFFER_FORMAT_NV12);
        assert_eq!(
            PixelFormat::P010.buffer_format(),
            NV_ENC_BUFFER_FORMAT_YUV420_10BIT
        );
        assert_eq!(
            PixelFormat::Yuv444_8.buffer_format(),
            NV_ENC_BUFFER_FORMAT_YUV444
        );
        assert_eq!(
            PixelFormat::Yuv444_10.buffer_format(),
            NV_ENC_BUFFER_FORMAT_YUV444_10BIT
        );
        assert_eq!(PixelFormat::Nv12.chroma_format_idc(), 1);
        assert_eq!(PixelFormat::P010.chroma_format_idc(), 1);
        assert_eq!(PixelFormat::Yuv444_8.chroma_format_idc(), 3);
        assert_eq!(PixelFormat::Yuv444_10.chroma_format_idc(), 3);
        assert_eq!(PixelFormat::Nv12.bit_depth_minus8(), 0);
        assert_eq!(PixelFormat::Yuv444_8.bit_depth_minus8(), 0);
        assert_eq!(PixelFormat::P010.bit_depth_minus8(), 2);
        assert_eq!(PixelFormat::Yuv444_10.bit_depth_minus8(), 2);
        assert_eq!(PixelFormat::Nv12.bytes_per_sample(), 1);
        assert_eq!(PixelFormat::Yuv444_8.bytes_per_sample(), 1);
        assert_eq!(PixelFormat::P010.bytes_per_sample(), 2);
        assert_eq!(PixelFormat::Yuv444_10.bytes_per_sample(), 2);
        assert!(PixelFormat::Nv12.semi_planar());
        assert!(PixelFormat::P010.semi_planar());
        assert!(!PixelFormat::Yuv444_8.semi_planar());
        assert!(!PixelFormat::Yuv444_10.semi_planar());
    }

    #[test]
    fn chroma_and_depth_is_the_exact_inverse_of_resolve_pixel_format() {
        // Every accepted combination in `resolves_every_supported_combination_for_both_codecs`
        // must round-trip back through this — it is the join point
        // `ensure_reconfigure_preserves_pixel_format` and `rate_control_sizing`
        // both rely on to recover chroma/depth from an already-built format.
        assert_eq!(
            PixelFormat::Nv12.chroma_and_depth(),
            (ChromaSubsampling::Yuv420, BitDepth::Eight)
        );
        assert_eq!(
            PixelFormat::P010.chroma_and_depth(),
            (ChromaSubsampling::Yuv420, BitDepth::Ten)
        );
        assert_eq!(
            PixelFormat::Yuv444_8.chroma_and_depth(),
            (ChromaSubsampling::Yuv444, BitDepth::Eight)
        );
        assert_eq!(
            PixelFormat::Yuv444_10.chroma_and_depth(),
            (ChromaSubsampling::Yuv444, BitDepth::Ten)
        );
    }

    #[test]
    fn chroma_rows_halves_and_rounds_up_for_4_2_0_but_not_4_4_4() {
        assert_eq!(chroma_rows(PixelFormat::Nv12, 1080), 540);
        assert_eq!(chroma_rows(PixelFormat::P010, 1080), 540);
        // Odd luma height: NVIDIA's own GetChromaHeight rounds up, matching
        // (height + 1) / 2 — verified against the reference in the module
        // doc on `chroma_rows`.
        assert_eq!(chroma_rows(PixelFormat::Nv12, 3), 2);
        assert_eq!(chroma_rows(PixelFormat::Yuv444_8, 1080), 1080);
        assert_eq!(chroma_rows(PixelFormat::Yuv444_10, 3), 3);
    }

    #[test]
    fn frame_bytes_matches_nvidias_own_get_frame_size_formula() {
        // NV12 at an unpadded pitch == width: width * (height + chroma_rows).
        assert_eq!(
            frame_bytes(PixelFormat::Nv12, 1920, 1080),
            1920 * (1080 + 540)
        );
        // P010: same shape, but the caller passes a pitch already doubled for
        // 2 bytes/sample, matching NVIDIA's own GetFrameSize which multiplies
        // the whole 4:2:0 formula by 2 for this format.
        assert_eq!(
            frame_bytes(PixelFormat::P010, 1920 * 2, 1080),
            1920 * 2 * (1080 + 540)
        );
        // YUV444: three identical full-resolution planes.
        assert_eq!(
            frame_bytes(PixelFormat::Yuv444_8, 1920, 1080),
            1920 * 1080 * 3
        );
        assert_eq!(
            frame_bytes(PixelFormat::Yuv444_10, 1920 * 2, 1080),
            1920 * 2 * 1080 * 3
        );
        // A padded pitch (driver alignment) must scale every plane, not just
        // the ones a naive width-based pitch would have gotten right.
        assert_eq!(
            frame_bytes(PixelFormat::Nv12, 2048, 1080),
            2048 * (1080 + 540)
        );
    }

    fn bgra_pixel(r: u8, g: u8, b: u8) -> [u8; 4] {
        [b, g, r, 0xff]
    }

    /// Build a 4x2 BGRA source (two 2x2 blocks side by side) as a flat byte
    /// buffer, ready for `BgraFrame::new`.
    fn small_bgra_source() -> Vec<u8> {
        let rows: [[[u8; 4]; 4]; 2] = [
            [
                bgra_pixel(255, 0, 0),
                bgra_pixel(0, 255, 0),
                bgra_pixel(0, 0, 255),
                bgra_pixel(255, 255, 255),
            ],
            [
                bgra_pixel(10, 20, 30),
                bgra_pixel(40, 50, 60),
                bgra_pixel(70, 80, 90),
                bgra_pixel(100, 110, 120),
            ],
        ];
        rows.into_iter().flatten().flatten().collect()
    }

    #[test]
    fn p010_luma_matches_the_transform_msb_aligned_and_chroma_is_box_filtered() {
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        let source = small_bgra_source();
        let bgra = BgraFrame::new(&source, 4, 2, 16).expect("valid BGRA");

        let mut y = vec![0u16; 4 * 2];
        let mut uv = vec![0u16; 4]; // one interleaved (U, V) pair per 2x2 block, 2 blocks wide

        write_p010_rows(transform, bgra, &mut y, 4, &mut uv, 4, 4, 2).expect("valid conversion");

        // Every luma sample equals ColorTransform's own value, packed the
        // same way `ColorTransform::pack_p16` documents (MSB-aligned: a
        // 10-bit code `v` stored as `v << 6`).
        let pixels = [
            (255u8, 0u8, 0u8),
            (0, 255, 0),
            (0, 0, 255),
            (255, 255, 255),
            (10, 20, 30),
            (40, 50, 60),
            (70, 80, 90),
            (100, 110, 120),
        ];
        for (index, &(r, g, b)) in pixels.iter().enumerate() {
            let expected = transform.pack_p16(transform.luma(b, g, r));
            assert_eq!(y[index], expected, "luma sample {index}");
            // MSB alignment: the low 6 bits of a 10-bit sample are always 0,
            // and shifting back down recovers the un-packed code exactly.
            assert_eq!(y[index] & 0x3F, 0, "sample {index} not MSB-aligned");
            assert_eq!(
                transform.unpack_p16(y[index]),
                transform.luma(b, g, r),
                "sample {index} does not round-trip through pack/unpack"
            );
        }

        // Chroma is the box filter over each 2x2 block, not per-pixel
        // conversion: average BGR first, convert once (matches
        // arcen_media::video::convert::convert_rows exactly).
        let mean = |a: u8, b: u8, c: u8, d: u8| -> u8 {
            ((i32::from(a) + i32::from(b) + i32::from(c) + i32::from(d)) >> 2) as u8
        };
        let (b0, g0, r0) = (
            mean(0, 0, 30, 60),
            mean(0, 255, 20, 50),
            mean(255, 0, 10, 40),
        );
        assert_eq!(uv[0], transform.pack_p16(transform.cb(b0, g0, r0)));
        assert_eq!(uv[1], transform.pack_p16(transform.cr(b0, g0, r0)));
        let (b1, g1, r1) = (
            mean(255, 255, 90, 120),
            mean(0, 255, 80, 110),
            mean(0, 255, 70, 100),
        );
        assert_eq!(uv[2], transform.pack_p16(transform.cb(b1, g1, r1)));
        assert_eq!(uv[3], transform.pack_p16(transform.cr(b1, g1, r1)));
    }

    #[test]
    fn p010_rejects_odd_or_zero_dimensions() {
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Limited, BitDepth::Ten);
        let source = small_bgra_source();
        let bgra = BgraFrame::new(&source, 4, 2, 16).expect("valid BGRA");
        let mut y = vec![0u16; 8];
        let mut uv = vec![0u16; 4];

        assert!(write_p010_rows(transform, bgra, &mut y, 4, &mut uv, 4, 3, 2).is_err());
        assert!(write_p010_rows(transform, bgra, &mut y, 4, &mut uv, 4, 4, 1).is_err());
        assert!(write_p010_rows(transform, bgra, &mut y, 4, &mut uv, 4, 0, 2).is_err());
    }

    /// The 8 (r, g, b) triples `small_bgra_source` encodes, in the same
    /// row-major pixel order `write_locked_from_bgra` writes planes in.
    fn small_bgra_pixels() -> [(u8, u8, u8); 8] {
        [
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (255, 255, 255),
            (10, 20, 30),
            (40, 50, 60),
            (70, 80, 90),
            (100, 110, 120),
        ]
    }

    /// Mirrors `arcen_media::video::convert`'s own private `as_u8` (clamp to
    /// `[0, 255]` then truncate) so the cross-check below compares against
    /// the exact same narrowing `write_locked_from_bgra`'s `Yuv444_8` path
    /// applies, not a bare `as` cast that could silently wrap instead of
    /// clamping.
    fn as_u8_for_test(value: i32) -> u8 {
        value.clamp(0, 255) as u8
    }

    #[test]
    fn identity_matrix_writes_g_b_r_directly_into_y_cb_cr_planes_at_8bit_444() {
        // The GBR/identity claim this task exists to prove reachable: with
        // `ColorMatrix::Identity`, `ColorTransform::luma`/`cb`/`cr` return G,
        // B and R with no RGB -> YCbCr conversion (see
        // `shared/media/src/video/convert.rs`), and 4:4:4 has no subsampling
        // to blend or discard any of them — so the Y/Cb/Cr planes
        // `write_locked_from_bgra` produces for `PixelFormat::Yuv444_8` must
        // equal G/B/R exactly, not merely proportionally: full range spans
        // every 8-bit code, so scaling an 8-bit component into an 8-bit code
        // is the identity function (see `ColorTransform::scale_identity`).
        let transform =
            ColorTransform::new(ColorMatrix::Identity, ColorRange::Full, BitDepth::Eight);
        let source = small_bgra_source();
        let bgra = BgraFrame::new(&source, 4, 2, 16).expect("valid BGRA");
        let (width, height, pitch) = (4u32, 2u32, 4u32);
        let plane_len = (pitch * height) as usize;
        let mut buffer = vec![0u8; plane_len * 3];

        unsafe {
            write_locked_from_bgra(
                PixelFormat::Yuv444_8,
                transform,
                bgra,
                buffer.as_mut_ptr(),
                pitch,
                (width, height),
                1,
            )
        }
        .expect("valid conversion");

        let (y, u, v) = (
            &buffer[..plane_len],
            &buffer[plane_len..plane_len * 2],
            &buffer[plane_len * 2..],
        );
        for (index, &(r, g, b)) in small_bgra_pixels().iter().enumerate() {
            assert_eq!(y[index], g, "Y[{index}] must carry G unconverted, not luma");
            assert_eq!(
                u[index], b,
                "Cb[{index}] must carry B unconverted, not chroma"
            );
            assert_eq!(
                v[index], r,
                "Cr[{index}] must carry R unconverted, not chroma"
            );
            // Cross-checked against the same public accessors the task
            // describes: "`.luma()` returns G, `.cb()` returns B, `.cr()`
            // returns R when the matrix is identity".
            assert_eq!(y[index], as_u8_for_test(transform.luma(b, g, r)));
            assert_eq!(u[index], as_u8_for_test(transform.cb(b, g, r)));
            assert_eq!(v[index], as_u8_for_test(transform.cr(b, g, r)));
        }
    }

    #[test]
    fn identity_matrix_writes_g_b_r_directly_into_y_cb_cr_planes_at_10bit_444() {
        // Same claim, generalised to the ten-bit 4:4:4 identity row that is
        // actually in `PROBE_MATRIX` (`hevc-444-10-full-identity`). Ten-bit
        // identity is a linear scale rather than an exact passthrough (see
        // `ColorTransform::scale_identity`), so this compares against the
        // same public `luma`/`cb`/`cr` accessors rather than a hand-derived
        // scaled constant — the point under test is plane *order*
        // (Y=G-derived, Cb=B-derived, Cr=R-derived), which
        // `p010_luma_matches_the_transform_msb_aligned_and_chroma_is_box_filtered`
        // already establishes the pack/unpack arithmetic for.
        let transform = ColorTransform::new(ColorMatrix::Identity, ColorRange::Full, BitDepth::Ten);
        let source = small_bgra_source();
        let bgra = BgraFrame::new(&source, 4, 2, 16).expect("valid BGRA");
        let (width, height, pitch) = (4u32, 2u32, 8u32); // 8 bytes/row = 4 u16 samples/row
        let plane_samples = (width * height) as usize;
        let mut buffer = vec![0u8; plane_samples * 3 * 2];

        unsafe {
            write_locked_from_bgra(
                PixelFormat::Yuv444_10,
                transform,
                bgra,
                buffer.as_mut_ptr(),
                pitch,
                (width, height),
                1,
            )
        }
        .expect("valid conversion");

        let words: Vec<u16> = buffer
            .chunks_exact(2)
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
            .collect();
        let (y, u, v) = (
            &words[..plane_samples],
            &words[plane_samples..plane_samples * 2],
            &words[plane_samples * 2..],
        );
        for (index, &(r, g, b)) in small_bgra_pixels().iter().enumerate() {
            assert_eq!(
                y[index],
                transform.pack_p16(transform.luma(b, g, r)),
                "Y[{index}] must be G's identity scaling, not luma"
            );
            assert_eq!(
                u[index],
                transform.pack_p16(transform.cb(b, g, r)),
                "Cb[{index}] must be B's identity scaling, not chroma"
            );
            assert_eq!(
                v[index],
                transform.pack_p16(transform.cr(b, g, r)),
                "Cr[{index}] must be R's identity scaling, not chroma"
            );
        }
    }

    #[test]
    fn parallel_i444_p16_conversion_matches_serial_with_padding_and_uneven_rows() {
        let (width, height) = (17u32, 13u32);
        let source_stride = width as usize * 4 + 12;
        let mut source = vec![0u8; source_stride * height as usize];
        for row in 0..height as usize {
            for column in 0..width as usize {
                let offset = row * source_stride + column * 4;
                source[offset] = ((row * 17 + column * 3) & 0xff) as u8;
                source[offset + 1] = ((row * 7 + column * 11) & 0xff) as u8;
                source[offset + 2] = ((row * 13 + column * 5) & 0xff) as u8;
                source[offset + 3] = 0xff;
            }
        }
        let bgra = BgraFrame::new(&source, width as usize, height as usize, source_stride).unwrap();
        let pitch = (width + 7) * 2;
        let plane_samples = pitch as usize / 2 * height as usize;
        let mut serial = vec![0u8; plane_samples * 3 * 2];
        let mut parallel = vec![0u8; plane_samples * 3 * 2];
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);

        unsafe {
            write_locked_from_bgra(
                PixelFormat::Yuv444_10,
                transform,
                bgra,
                serial.as_mut_ptr(),
                pitch,
                (width, height),
                1,
            )
            .unwrap();
            write_locked_from_bgra(
                PixelFormat::Yuv444_10,
                transform,
                bgra,
                parallel.as_mut_ptr(),
                pitch,
                (width, height),
                4,
            )
            .unwrap();
        }

        assert_eq!(parallel, serial);
    }
}

#[cfg(test)]
mod rate_control_tests {
    use super::*;

    const FOUR_K: (u32, u32) = (3840, 2160);
    const FULL_HD: (u32, u32) = (1920, 1080);

    /// Latency-first sizing, which is what every assertion below was written
    /// against and what capenc encodes unless a session asks otherwise.
    fn sizing(
        width: u32,
        height: u32,
        fps: u32,
        chroma: ChromaSubsampling,
        depth: BitDepth,
    ) -> RateControlSizing {
        rate_control_sizing(width, height, fps, chroma, depth, EncodeIntent::Interactive)
    }

    /// The grading intent must buy a bigger VBV buffer, and nothing else.
    ///
    /// Bitrate is a function of how many samples there are, which intent does
    /// not change. What intent buys is room for the encoder to even out a hard
    /// frame rather than clipping quality to stay inside a tight budget.
    #[test]
    fn quality_intent_widens_the_vbv_buffer_without_moving_bitrate() {
        let (width, height) = FULL_HD;
        let args = (width, height, 60, ChromaSubsampling::Yuv444, BitDepth::Ten);
        let interactive = rate_control_sizing(
            args.0,
            args.1,
            args.2,
            args.3,
            args.4,
            EncodeIntent::Interactive,
        );
        let quality = rate_control_sizing(
            args.0,
            args.1,
            args.2,
            args.3,
            args.4,
            EncodeIntent::Quality,
        );

        assert_eq!(
            interactive.average_bitrate_bps, quality.average_bitrate_bps,
            "intent must not change how many bits per second the format needs",
        );
        assert_eq!(
            interactive.max_bitrate_bps, quality.max_bitrate_bps,
            "intent must not change the ceiling",
        );
        assert!(
            quality.vbv_buffer_size_bits > interactive.vbv_buffer_size_bits,
            "grading must get a larger smoothing buffer: {} vs {}",
            quality.vbv_buffer_size_bits,
            interactive.vbv_buffer_size_bits,
        );
    }

    /// The interactive buffer must stay a latency buffer, not a smoothing one.
    #[test]
    fn interactive_vbv_stays_well_under_a_second_of_bits() {
        let sizing = sizing(1920, 1080, 60, ChromaSubsampling::Yuv420, BitDepth::Eight);
        assert!(
            sizing.vbv_buffer_size_bits < sizing.average_bitrate_bps / 4,
            "interactive VBV should be a small fraction of a second of bits",
        );
    }

    #[test]
    fn baseline_8bit_420_matches_the_documented_formula() {
        let (width, height) = FULL_HD;
        let sizing = sizing(
            width,
            height,
            60,
            ChromaSubsampling::Yuv420,
            BitDepth::Eight,
        );
        let expected =
            (f64::from(width) * f64::from(height) * 60.0 * 1.5 * 1.0 * 0.05).round() as u32;
        assert_eq!(sizing.average_bitrate_bps, expected);
        assert_eq!(sizing.max_bitrate_bps, sizing.average_bitrate_bps);
        // A reasonable, low-latency ballpark: neither a rounding artefact
        // near zero nor a runaway number.
        assert!((5_000_000..15_000_000).contains(&sizing.average_bitrate_bps));
    }

    #[test]
    fn yuv444_is_exactly_double_yuv420_at_the_same_depth() {
        for depth in [BitDepth::Eight, BitDepth::Ten] {
            let (width, height, fps) = (FOUR_K.0, FOUR_K.1, 60);
            let yuv420 = sizing(width, height, fps, ChromaSubsampling::Yuv420, depth);
            let yuv444 = sizing(width, height, fps, ChromaSubsampling::Yuv444, depth);
            assert_eq!(
                yuv444.average_bitrate_bps,
                yuv420.average_bitrate_bps * 2,
                "4:4:4 has exactly 2x 4:2:0's coded samples/pixel (3.0 vs 1.5), so its default \
                 bitrate must be exactly double at {depth:?}-bit, not the same number"
            );
        }
    }

    #[test]
    fn yuv422_sits_two_thirds_of_the_way_from_420_to_444() {
        let (width, height, fps) = (FOUR_K.0, FOUR_K.1, 60);
        let yuv420 = sizing(
            width,
            height,
            fps,
            ChromaSubsampling::Yuv420,
            BitDepth::Eight,
        );
        let yuv422 = sizing(
            width,
            height,
            fps,
            ChromaSubsampling::Yuv422,
            BitDepth::Eight,
        );
        let yuv444 = sizing(
            width,
            height,
            fps,
            ChromaSubsampling::Yuv444,
            BitDepth::Eight,
        );
        // 1.5 (420) < 2.0 (422) < 3.0 (444) samples/pixel.
        assert!(yuv420.average_bitrate_bps < yuv422.average_bitrate_bps);
        assert!(yuv422.average_bitrate_bps < yuv444.average_bitrate_bps);
    }

    #[test]
    fn ten_bit_scales_up_from_eight_bit_by_the_documented_25_percent() {
        let (width, height, fps) = (FOUR_K.0, FOUR_K.1, 60);
        for chroma in [ChromaSubsampling::Yuv420, ChromaSubsampling::Yuv444] {
            let eight = sizing(width, height, fps, chroma, BitDepth::Eight);
            let ten = sizing(width, height, fps, chroma, BitDepth::Ten);
            let ratio = f64::from(ten.average_bitrate_bps) / f64::from(eight.average_bitrate_bps);
            assert!(
                (ratio - 1.25).abs() < 0.001,
                "{chroma:?}: expected a 25% bump for ten-bit, got {ratio}"
            );
        }
    }

    #[test]
    fn four_k_60_yuv444_10bit_is_not_starved_relative_to_the_8bit_420_baseline() {
        // The scenario the task names directly: the combined chroma+depth
        // multiplier for the grading-reference row (4:4:4, 10-bit) over the
        // 4:2:0 8-bit baseline is 2.0 * 1.25 == 2.5x, at the same resolution
        // and frame rate — never merely "the same bitrate" or less.
        let (width, height, fps) = (FOUR_K.0, FOUR_K.1, 60);
        let baseline = sizing(
            width,
            height,
            fps,
            ChromaSubsampling::Yuv420,
            BitDepth::Eight,
        );
        let target = sizing(width, height, fps, ChromaSubsampling::Yuv444, BitDepth::Ten);
        let ratio = f64::from(target.average_bitrate_bps) / f64::from(baseline.average_bitrate_bps);
        assert!(
            (ratio - 2.5).abs() < 0.001,
            "4:4:4 10-bit must be sized at 2.5x the 4:2:0 8-bit baseline, got {ratio}x"
        );
    }

    #[test]
    fn scales_linearly_with_resolution_and_frame_rate() {
        let base = sizing(1920, 1080, 30, ChromaSubsampling::Yuv420, BitDepth::Eight);
        let double_width = sizing(3840, 1080, 30, ChromaSubsampling::Yuv420, BitDepth::Eight);
        let double_fps = sizing(1920, 1080, 60, ChromaSubsampling::Yuv420, BitDepth::Eight);
        assert_eq!(
            double_width.average_bitrate_bps,
            base.average_bitrate_bps * 2
        );
        assert_eq!(double_fps.average_bitrate_bps, base.average_bitrate_bps * 2);
    }

    #[test]
    fn vbv_buffer_is_a_couple_of_frames_of_the_average_bitrate() {
        let sizing = sizing(1920, 1080, 60, ChromaSubsampling::Yuv420, BitDepth::Eight);
        let expected_bits = (f64::from(sizing.average_bitrate_bps) / 60.0
            * vbv_buffer_frames(EncodeIntent::Interactive))
        .round() as u32;
        assert_eq!(sizing.vbv_buffer_size_bits, expected_bits);
        // Small relative to a whole second of bitrate — this is a low-latency
        // buffer, not a smoothing one.
        assert!(sizing.vbv_buffer_size_bits < sizing.average_bitrate_bps);
    }

    #[test]
    fn zero_fps_does_not_panic_or_divide_by_zero() {
        let sizing = sizing(1920, 1080, 0, ChromaSubsampling::Yuv420, BitDepth::Eight);
        assert!(sizing.average_bitrate_bps > 0);
        assert!(sizing.vbv_buffer_size_bits > 0);
    }
}

#[cfg(test)]
mod reconfigure_lifecycle_tests {
    use super::*;
    use arcen_media::{ColorPrimaries, ColorRange, TransferCharacteristics};

    fn color(
        chroma: ChromaSubsampling,
        bit_depth: BitDepth,
        matrix: ColorMatrix,
    ) -> crate::ColorSpec {
        crate::ColorSpec {
            chroma,
            bit_depth,
            range: ColorRange::Full,
            matrix,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }

    #[test]
    fn same_chroma_and_depth_is_always_reconfigurable() {
        // Range, matrix, primaries and transfer are free to change — only
        // chroma and bit depth are fixed for the session's lifetime.
        for matrix in [
            ColorMatrix::Bt709,
            ColorMatrix::Bt601,
            ColorMatrix::Bt2020Ncl,
        ] {
            assert!(
                ensure_reconfigure_preserves_pixel_format(
                    PixelFormat::Yuv444_10,
                    color(ChromaSubsampling::Yuv444, BitDepth::Ten, matrix),
                )
                .is_ok(),
                "matrix-only changes ({matrix:?}) must never be refused"
            );
        }
    }

    #[test]
    fn a_bit_depth_hot_switch_fails_loudly_and_names_bit_depth() {
        let error = ensure_reconfigure_preserves_pixel_format(
            PixelFormat::Nv12, // 4:2:0, 8-bit
            color(ChromaSubsampling::Yuv420, BitDepth::Ten, ColorMatrix::Bt709),
        )
        .expect_err("NvEncReconfigureEncoder cannot change bit depth");
        let message = error.to_string();
        assert!(
            message.contains("NvEncReconfigureEncoder") && message.contains("bit depth"),
            "error must name the constraint by its real API and axis, got: {message}"
        );
    }

    #[test]
    fn a_chroma_hot_switch_fails_loudly_and_names_chroma() {
        let error = ensure_reconfigure_preserves_pixel_format(
            PixelFormat::Yuv444_8, // 4:4:4, 8-bit
            color(
                ChromaSubsampling::Yuv420,
                BitDepth::Eight,
                ColorMatrix::Bt709,
            ),
        )
        .expect_err("NvEncReconfigureEncoder cannot change chroma format");
        let message = error.to_string();
        assert!(
            message.contains("NvEncReconfigureEncoder") && message.contains("chroma"),
            "error must name the constraint by its real API and axis, got: {message}"
        );
    }

    #[test]
    fn a_simultaneous_chroma_and_depth_hot_switch_is_still_refused() {
        // Bit depth is checked first (see `ensure_reconfigure_preserves_pixel_format`),
        // but the point of this guard is that neither axis is ever silently
        // forwarded — a combined change must still fail, not partially apply.
        assert!(ensure_reconfigure_preserves_pixel_format(
            PixelFormat::Nv12, // 4:2:0, 8-bit
            color(ChromaSubsampling::Yuv444, BitDepth::Ten, ColorMatrix::Bt709),
        )
        .is_err());
    }
}
