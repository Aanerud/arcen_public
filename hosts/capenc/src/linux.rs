// Linux host implementation: NvFBC Shared CUDA capture + NVENC CUDA encode.
//
// This mirrors win.rs' process contract: stdout is Annex-B H.264/HEVC,
// stdin accepts "IDR", stderr carries diagnostics and 1 Hz stats. NVIDIA
// entry points are loaded at runtime so the binary has no build-time CUDA,
// NvFBC, or NVENC dependency.

use crate::linux_policy::{
    linux_capture_backend, LinuxCaptureBackend, RequestedEncoder, SubmissionGate, SubmissionMode,
};
use crate::log;
use arcen_media::video::{BackendUnavailableReason, EncoderBackend};

use std::ffi::{c_char, c_void, CStr, CString};
use std::fmt::{Display, Formatter};
use std::mem::MaybeUninit;
use std::ptr;
use std::time::{Duration, Instant};

const NVFBC_DAMAGE_SOURCE: &str = "unavailable_to_cuda";
const IDLE_KEEPALIVE: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) enum NativeStartupError {
    Unavailable {
        reason: BackendUnavailableReason,
        detail: String,
    },
    Fatal(String),
}

impl NativeStartupError {
    fn unavailable(reason: BackendUnavailableReason, detail: impl Into<String>) -> Self {
        Self::Unavailable {
            reason,
            detail: detail.into(),
        }
    }

    pub(crate) fn fatal(detail: impl Into<String>) -> Self {
        Self::Fatal(detail.into())
    }
}

impl Display for NativeStartupError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { detail, .. } | Self::Fatal(detail) => formatter.write_str(detail),
        }
    }
}

const fn nvfbc_cursor_flag(mode: crate::CursorCaptureMode) -> u32 {
    if mode.include_cursor() {
        1
    } else {
        0
    }
}

fn log_error(message: &str) {
    log(&format!("ERROR: {message}"));
}

fn log_unavailable(reason: BackendUnavailableReason, detail: &str) {
    log(&format!("native NVENC unavailable: {detail}"));
    let _ = crate::announce_unavailable(EncoderBackend::NativeNvenc, reason);
}

#[derive(Debug)]
struct PipelineStats {
    emitted: u64,
    submitted: u64,
    capture_samples: u64,
    capture_ms_sum: f64,
    capture_ms_max: f64,
    stage_samples: u64,
    stage_ms_sum: f64,
    stage_ms_max: f64,
    encode_ms_sum: f64,
    encode_ms_max: f64,
    bytes: u64,
    first_emits: u64,
    idr_emits: u64,
    activity_emits: u64,
    keepalive_emits: u64,
    pipeline_flushes: u64,
    capture_recreates: u64,
    stale_outputs_dropped: u64,
}

impl PipelineStats {
    const fn new() -> Self {
        Self {
            emitted: 0,
            submitted: 0,
            capture_samples: 0,
            capture_ms_sum: 0.0,
            capture_ms_max: 0.0,
            stage_samples: 0,
            stage_ms_sum: 0.0,
            stage_ms_max: 0.0,
            encode_ms_sum: 0.0,
            encode_ms_max: 0.0,
            bytes: 0,
            first_emits: 0,
            idr_emits: 0,
            activity_emits: 0,
            keepalive_emits: 0,
            pipeline_flushes: 0,
            capture_recreates: 0,
            stale_outputs_dropped: 0,
        }
    }

    fn record_capture(&mut self, capture_ms: f64) {
        self.capture_samples += 1;
        self.capture_ms_sum += capture_ms;
        self.capture_ms_max = self.capture_ms_max.max(capture_ms);
    }

    fn record_stage(&mut self, stage_ms: f64) {
        self.stage_samples += 1;
        self.stage_ms_sum += stage_ms;
        self.stage_ms_max = self.stage_ms_max.max(stage_ms);
    }

    fn record_submission(&mut self, mode: SubmissionMode, encode_ms: f64) {
        self.submitted += 1;
        self.encode_ms_sum += encode_ms;
        self.encode_ms_max = self.encode_ms_max.max(encode_ms);
        match mode {
            SubmissionMode::FirstFrame => self.first_emits += 1,
            SubmissionMode::Idr => self.idr_emits += 1,
            SubmissionMode::Activity => self.activity_emits += 1,
            SubmissionMode::Keepalive => self.keepalive_emits += 1,
            SubmissionMode::PipelineFlush => self.pipeline_flushes += 1,
        }
    }

    fn record_emitted(&mut self, bytes: usize) {
        self.emitted += 1;
        self.bytes += bytes as u64;
    }

    const fn record_capture_recreate(&mut self) {
        self.capture_recreates += 1;
    }

    const fn record_stale_output_drop(&mut self) {
        self.stale_outputs_dropped += 1;
    }

    fn log_and_reset(
        &mut self,
        capture: (u64, u64, u64),
        want_idr: bool,
        capture_backend: &str,
        damage_source: &str,
    ) {
        let average_encode_ms = if self.submitted == 0 {
            0.0
        } else {
            self.encode_ms_sum / self.submitted as f64
        };
        let average_capture_ms = if self.capture_samples == 0 {
            0.0
        } else {
            self.capture_ms_sum / self.capture_samples as f64
        };
        let average_stage_ms = if self.stage_samples == 0 {
            0.0
        } else {
            self.stage_ms_sum / self.stage_samples as f64
        };
        log(&format!(
            "enc_fps={} encode_submitted={} avg_capture_ms={average_capture_ms:.2} \
             max_capture_ms={:.2} avg_stage_ms={average_stage_ms:.2} max_stage_ms={:.2} \
             avg_encode_ms={average_encode_ms:.2} max_encode_ms={:.2} kbps={} \
             capture_backend={capture_backend} capture_new={} capture_old={} \
             capture_direct={} want_idr={want_idr} emit_first={} emit_idr={} \
             emit_activity={} emit_keepalive={} pipeline_flush={} \
             capture_recreated={} stale_outputs_dropped={} \
             damage_source={damage_source}",
            self.emitted,
            self.submitted,
            self.capture_ms_max,
            self.stage_ms_max,
            self.encode_ms_max,
            self.bytes * 8 / 1000,
            capture.0,
            capture.1,
            capture.2,
            self.first_emits,
            self.idr_emits,
            self.activity_emits,
            self.keepalive_emits,
            self.pipeline_flushes,
            self.capture_recreates,
            self.stale_outputs_dropped,
        ));
        *self = Self::new();
    }
}

fn discard_stale_output<T>(
    output: Option<T>,
    stale_outputs_to_drop: &mut usize,
) -> (Option<T>, bool) {
    if output.is_some() && *stale_outputs_to_drop != 0 {
        *stale_outputs_to_drop -= 1;
        (None, true)
    } else {
        (output, false)
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::discard_stale_output;

    #[test]
    fn source_recreation_discards_every_pre_recreation_output() {
        let mut stale = 3;
        assert_eq!(discard_stale_output::<u8>(None, &mut stale), (None, false));
        assert_eq!(stale, 3, "no output means nothing was drained");
        for value in 1..=3 {
            assert_eq!(discard_stale_output(Some(value), &mut stale), (None, true));
        }
        assert_eq!(stale, 0);
        assert_eq!(
            discard_stale_output(Some(4), &mut stale),
            (Some(4), false),
            "the first post-recreation output must be exposed"
        );
    }
}

#[inline]
unsafe fn zeroed<T>() -> T {
    MaybeUninit::<T>::zeroed().assume_init()
}

pub mod dl {
    use super::*;

    const RTLD_NOW: i32 = 2;

    #[link(name = "dl")]
    extern "C" {
        fn dlopen(filename: *const c_char, flag: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> i32;
        fn dlerror() -> *const c_char;
    }

    fn last_error() -> String {
        unsafe {
            let err = dlerror();
            if err.is_null() {
                "unknown dlerror".to_string()
            } else {
                CStr::from_ptr(err).to_string_lossy().into_owned()
            }
        }
    }

    pub unsafe fn open(name: &str) -> Result<*mut c_void, String> {
        let c_name = CString::new(name).map_err(|_| format!("invalid library name: {name:?}"))?;
        let lib = dlopen(c_name.as_ptr(), RTLD_NOW);
        if lib.is_null() {
            Err(format!("dlopen({name}): {}", last_error()))
        } else {
            Ok(lib)
        }
    }

    pub unsafe fn sym(lib: *mut c_void, name: &str) -> Result<*mut c_void, String> {
        let c_name = CString::new(name).map_err(|_| format!("invalid symbol name: {name:?}"))?;
        let ptr = dlsym(lib, c_name.as_ptr());
        if ptr.is_null() {
            Err(format!("dlsym({name}): {}", last_error()))
        } else {
            Ok(ptr)
        }
    }

    pub unsafe fn close(lib: *mut c_void) {
        if !lib.is_null() {
            let _ = dlclose(lib);
        }
    }
}

pub mod cuda {
    use super::*;

    pub type CUdeviceptr = u64;

    type CUcontext = *mut c_void;
    type CUdevice = i32;
    type CUresult = i32;

    const CUDA_SUCCESS: CUresult = 0;
    const CUDA_ERROR_OUT_OF_MEMORY: CUresult = 2;
    const CUDA_ERROR_INSUFFICIENT_DRIVER: CUresult = 35;
    const CUDA_ERROR_NO_DEVICE: CUresult = 100;
    const CUDA_ERROR_INVALID_DEVICE: CUresult = 101;
    const CUDA_ERROR_SYSTEM_DRIVER_MISMATCH: CUresult = 803;

    type CuInit = unsafe extern "C" fn(u32) -> CUresult;
    type CuDeviceGetCount = unsafe extern "C" fn(*mut i32) -> CUresult;
    type CuDeviceGet = unsafe extern "C" fn(*mut CUdevice, i32) -> CUresult;
    type CuCtxCreate = unsafe extern "C" fn(*mut CUcontext, u32, CUdevice) -> CUresult;
    type CuCtxDestroy = unsafe extern "C" fn(CUcontext) -> CUresult;
    type CuCtxSetCurrent = unsafe extern "C" fn(CUcontext) -> CUresult;
    type CuMemAlloc = unsafe extern "C" fn(*mut CUdeviceptr, usize) -> CUresult;
    type CuMemFree = unsafe extern "C" fn(CUdeviceptr) -> CUresult;
    type CuMemcpyDtoD = unsafe extern "C" fn(CUdeviceptr, CUdeviceptr, usize) -> CUresult;
    // Device<->host: added for the Linux CUDA NVENC path's 10-bit surfaces
    // (`nvenc_cuda.rs`'s `stage_converted`), which need `arcen_media`'s
    // BGRA -> MSB-aligned-16-bit conversion to run on the CPU — there is no
    // CUDA kernel compiled into this crate to do that arithmetic on the
    // device (see that module's doc). Every driver that exposes
    // `cuMemcpyDtoD_v2`/`cuMemAlloc_v2` (already required above) has exposed
    // these two since the CUDA 2.0 driver API; unlike the truly optional
    // NVENC/NvFBC entry points, a missing symbol here would mean a CUDA
    // installation too old to run any of this file's own zero-copy paths
    // either.
    type CuMemcpyDtoH = unsafe extern "C" fn(*mut c_void, CUdeviceptr, usize) -> CUresult;
    type CuMemcpyHtoD = unsafe extern "C" fn(CUdeviceptr, *const c_void, usize) -> CUresult;
    type CuMemcpy2D = unsafe extern "C" fn(*const Memcpy2D) -> CUresult;
    type CuMemsetD8 = unsafe extern "C" fn(CUdeviceptr, u8, usize) -> CUresult;

    const CU_MEMORYTYPE_DEVICE: u32 = 2;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Memcpy2D {
        src_x_in_bytes: usize,
        src_y: usize,
        src_memory_type: u32,
        src_host: *const c_void,
        src_device: CUdeviceptr,
        src_array: *mut c_void,
        src_pitch: usize,
        dst_x_in_bytes: usize,
        dst_y: usize,
        dst_memory_type: u32,
        dst_host: *mut c_void,
        dst_device: CUdeviceptr,
        dst_array: *mut c_void,
        dst_pitch: usize,
        width_in_bytes: usize,
        height: usize,
    }

    #[derive(Clone, Copy)]
    struct Fns {
        cu_ctx_destroy: CuCtxDestroy,
        cu_ctx_set_current: CuCtxSetCurrent,
        cu_mem_alloc: CuMemAlloc,
        cu_mem_free: CuMemFree,
        cu_memcpy_dtod: CuMemcpyDtoD,
        cu_memcpy_dtoh: CuMemcpyDtoH,
        cu_memcpy_htod: CuMemcpyHtoD,
        cu_memcpy_2d: CuMemcpy2D,
        cu_memset_d8: CuMemsetD8,
    }

    static mut FNS: Option<Fns> = None;

    pub struct Context {
        raw: CUcontext,
        _lib: *mut c_void,
    }

    impl Context {
        pub fn as_raw(&self) -> *mut c_void {
            self.raw
        }
    }

    impl Drop for Context {
        fn drop(&mut self) {
            unsafe {
                if let Some(fns) = FNS {
                    let _ = (fns.cu_ctx_destroy)(self.raw);
                }
                dl::close(self._lib);
            }
        }
    }

    fn check(st: CUresult, what: &str) -> Result<(), String> {
        if st == CUDA_SUCCESS {
            Ok(())
        } else {
            Err(format!("{what} -> CUDA status {st}"))
        }
    }

    fn startup_status(st: CUresult, what: &'static str) -> Result<(), NativeStartupError> {
        if st == CUDA_SUCCESS {
            return Ok(());
        }
        let detail = format!("{what} -> CUDA status {st}");
        match st {
            CUDA_ERROR_INSUFFICIENT_DRIVER | CUDA_ERROR_SYSTEM_DRIVER_MISMATCH => Err(
                NativeStartupError::unavailable(BackendUnavailableReason::RuntimeMissing, detail),
            ),
            CUDA_ERROR_NO_DEVICE => Err(NativeStartupError::unavailable(
                BackendUnavailableReason::HardwareUnavailable,
                detail,
            )),
            CUDA_ERROR_INVALID_DEVICE | CUDA_ERROR_OUT_OF_MEMORY => {
                Err(NativeStartupError::fatal(detail))
            }
            _ => Err(NativeStartupError::fatal(detail)),
        }
    }

    unsafe fn load_startup_fn<T>(lib: *mut c_void, name: &str) -> Result<T, NativeStartupError> {
        let symbol = dl::sym(lib, name).map_err(|error| {
            NativeStartupError::unavailable(BackendUnavailableReason::RuntimeMissing, error)
        })?;
        Ok(std::mem::transmute_copy(&symbol))
    }

    pub unsafe fn init_from_env() -> Result<Context, NativeStartupError> {
        let ordinal = match std::env::var("ARCEN_CUDA_DEVICE") {
            Ok(value) => value.parse::<i32>().map_err(|_| {
                NativeStartupError::fatal("ARCEN_CUDA_DEVICE must be a signed integer")
            })?,
            Err(std::env::VarError::NotPresent) => 0,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(NativeStartupError::fatal(
                    "ARCEN_CUDA_DEVICE must be valid UTF-8",
                ));
            }
        };
        init(ordinal)
    }

    pub unsafe fn init(ordinal: i32) -> Result<Context, NativeStartupError> {
        let lib = dl::open("libcuda.so.1").map_err(|error| {
            NativeStartupError::unavailable(BackendUnavailableReason::RuntimeMissing, error)
        })?;

        let cu_init: CuInit = load_startup_fn(lib, "cuInit")?;
        let cu_device_get_count: CuDeviceGetCount = load_startup_fn(lib, "cuDeviceGetCount")?;
        let cu_device_get: CuDeviceGet = load_startup_fn(lib, "cuDeviceGet")?;
        let cu_ctx_create: CuCtxCreate = load_startup_fn(lib, "cuCtxCreate_v2")?;

        let fns = Fns {
            cu_ctx_destroy: load_startup_fn(lib, "cuCtxDestroy_v2")?,
            cu_ctx_set_current: load_startup_fn(lib, "cuCtxSetCurrent")?,
            cu_mem_alloc: load_startup_fn(lib, "cuMemAlloc_v2")?,
            cu_mem_free: load_startup_fn(lib, "cuMemFree_v2")?,
            cu_memcpy_dtod: load_startup_fn(lib, "cuMemcpyDtoD_v2")?,
            cu_memcpy_dtoh: load_startup_fn(lib, "cuMemcpyDtoH_v2")?,
            cu_memcpy_htod: load_startup_fn(lib, "cuMemcpyHtoD_v2")?,
            cu_memcpy_2d: load_startup_fn(lib, "cuMemcpy2D_v2")?,
            cu_memset_d8: load_startup_fn(lib, "cuMemsetD8_v2")?,
        };

        startup_status(cu_init(0), "cuInit")?;

        let mut count = 0i32;
        startup_status(cu_device_get_count(&mut count), "cuDeviceGetCount")?;
        if count <= 0 {
            return Err(NativeStartupError::unavailable(
                BackendUnavailableReason::HardwareUnavailable,
                "no CUDA devices reported by libcuda",
            ));
        }
        if ordinal < 0 || ordinal >= count {
            return Err(NativeStartupError::fatal(format!(
                "ARCEN_CUDA_DEVICE={ordinal} outside available CUDA device range 0..{}",
                count - 1
            )));
        }

        let mut dev = 0;
        startup_status(cu_device_get(&mut dev, ordinal), "cuDeviceGet")?;

        let mut ctx = ptr::null_mut();
        startup_status(cu_ctx_create(&mut ctx, 0, dev), "cuCtxCreate_v2")?;
        startup_status((fns.cu_ctx_set_current)(ctx), "cuCtxSetCurrent")?;
        FNS = Some(fns);
        log(&format!("CUDA ready: device={ordinal}/{count}"));

        Ok(Context {
            raw: ctx,
            _lib: lib,
        })
    }

    unsafe fn fns() -> Result<Fns, String> {
        FNS.ok_or_else(|| "CUDA not initialized".to_string())
    }

    pub unsafe fn mem_alloc(bytes: usize) -> Result<CUdeviceptr, String> {
        let fns = fns()?;
        let mut ptr = 0;
        check((fns.cu_mem_alloc)(&mut ptr, bytes), "cuMemAlloc_v2")?;
        Ok(ptr)
    }

    #[allow(dead_code)]
    pub unsafe fn mem_free(ptr: CUdeviceptr) -> Result<(), String> {
        check((fns()?.cu_mem_free)(ptr), "cuMemFree_v2")
    }

    pub unsafe fn memcpy_dtod(
        dst: CUdeviceptr,
        src: CUdeviceptr,
        bytes: usize,
    ) -> Result<(), String> {
        check((fns()?.cu_memcpy_dtod)(dst, src, bytes), "cuMemcpyDtoD_v2")
    }

    /// Device -> host: copies `bytes` from `src` (device) into `dst` (a
    /// host buffer at least `bytes` long, e.g. `nvenc_cuda.rs`'s
    /// `Encoder::host_src`). Synchronous — the NULL stream used here blocks
    /// the calling thread until the copy completes, so the caller sees a
    /// fully up to date host buffer once this returns.
    pub unsafe fn memcpy_dtoh(
        dst: *mut c_void,
        src: CUdeviceptr,
        bytes: usize,
    ) -> Result<(), String> {
        check((fns()?.cu_memcpy_dtoh)(dst, src, bytes), "cuMemcpyDtoH_v2")
    }

    /// Host -> device: copies `bytes` from `src` (a host buffer at least
    /// `bytes` long, e.g. `nvenc_cuda.rs`'s `Encoder::host_dst`) into `dst`
    /// (device). Synchronous, same NULL-stream semantics as `memcpy_dtoh`.
    pub unsafe fn memcpy_htod(
        dst: CUdeviceptr,
        src: *const c_void,
        bytes: usize,
    ) -> Result<(), String> {
        check((fns()?.cu_memcpy_htod)(dst, src, bytes), "cuMemcpyHtoD_v2")
    }

    pub unsafe fn memcpy_2d_device(
        dst: CUdeviceptr,
        dst_pitch: usize,
        src: CUdeviceptr,
        src_pitch: usize,
        width_bytes: usize,
        height: usize,
    ) -> Result<(), String> {
        let copy = Memcpy2D {
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            src_device: src,
            src_pitch,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_device: dst,
            dst_pitch,
            width_in_bytes: width_bytes,
            height,
            ..Default::default()
        };
        check((fns()?.cu_memcpy_2d)(&copy), "cuMemcpy2D_v2")
    }

    pub unsafe fn memset_d8(dst: CUdeviceptr, value: u8, bytes: usize) -> Result<(), String> {
        check((fns()?.cu_memset_d8)(dst, value, bytes), "cuMemsetD8_v2")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn startup_statuses_only_allow_documented_unavailability() {
            for status in [
                CUDA_ERROR_INSUFFICIENT_DRIVER,
                CUDA_ERROR_SYSTEM_DRIVER_MISMATCH,
                CUDA_ERROR_NO_DEVICE,
            ] {
                assert!(matches!(
                    startup_status(status, "test"),
                    Err(NativeStartupError::Unavailable { .. })
                ));
            }
            for status in [CUDA_ERROR_INVALID_DEVICE, CUDA_ERROR_OUT_OF_MEMORY, -1] {
                assert!(matches!(
                    startup_status(status, "test"),
                    Err(NativeStartupError::Fatal(_))
                ));
            }
        }
    }
}

#[allow(non_snake_case)]
mod nvfbc {
    use super::*;
    use cuda::CUdeviceptr;

    pub type Status = i32;
    type Bool = u32;
    type SessionHandle = u64;

    const SUCCESS: Status = 0;
    const ERR_MAX_CLIENTS: Status = 6;
    const ERR_UNSUPPORTED: Status = 7;
    const ERR_MUST_RECREATE: Status = 16;

    const FALSE: Bool = 0;
    const TRUE: Bool = 1;

    const VERSION_MAJOR: u32 = 1;
    const VERSION_MINOR: u32 = 7;
    const VERSION: u32 = VERSION_MINOR | (VERSION_MAJOR << 8);

    const CAPTURE_SHARED_CUDA: u32 = 1;
    const TRACKING_OUTPUT: u32 = 1;
    const TRACKING_SCREEN: u32 = 2;
    // Linux NvFBC has no ten-bit capture format. At all.
    //
    // Checked against the current NvFBC header for Linux (2025), not
    // assumed: `NVFBC_BUFFER_FORMAT` is exactly `ARGB, RGB, NV12, YUV444P,
    // RGBA, BGRA`, and the header names BGRA as the native format. There is
    // no `ARGB10` and no other wide member.
    //
    // This is worth stating explicitly because the NVIDIA Capture SDK
    // documentation *does* describe an `ARGB10` output format, and a search
    // will surface it — but that belongs to the legacy Windows-era
    // interfaces (`NvFBCToSys`, `NvFBCToDx9Vid`, `NvFBCToCuda`), which are a
    // different API from the Linux NvFBC used here. Reading that
    // documentation and concluding Linux NvFBC can capture ten bits is an
    // easy and expensive mistake.
    //
    // The consequence is structural: on Linux, `DefaultDepth 30` in the X
    // configuration gives a ten-bit *framebuffer*, but capturing it through
    // NvFBC still yields eight bits per channel, so a PQ-signalled stream
    // off this path is eight-bit content in a wider container. Genuine
    // ten-bit Linux capture needs a different backend — `XShmGetImage`
    // against the depth-30 screen (see `linux_x11`, which now accepts depth
    // 30) or a DRM/PipeWire path — and that plumbing into NVENC does not
    // exist yet.
    const BUFFER_FORMAT_BGRA: u32 = 5;
    const BUFFER_FORMAT_YUV444P: u32 = 3;
    const TOCUDA_GRAB_FLAGS_NOWAIT_IF_NEW_FRAME_READY: u32 = 1 << 2;

    const OUTPUT_MAX: usize = 5;
    const OUTPUT_NAME_LEN: usize = 128;

    fn startup_status(status: Status, detail: impl Into<String>) -> NativeStartupError {
        let detail = detail.into();
        match status {
            ERR_MAX_CLIENTS => {
                NativeStartupError::unavailable(BackendUnavailableReason::SessionLimit, detail)
            }
            ERR_UNSUPPORTED => NativeStartupError::unavailable(
                BackendUnavailableReason::UnsupportedConfiguration,
                detail,
            ),
            _ => NativeStartupError::fatal(detail),
        }
    }

    fn struct_version<T>(ver: u32) -> u32 {
        std::mem::size_of::<T>() as u32 | (ver << 16) | (VERSION << 24)
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct BoxRect {
        pub x: u32,
        pub y: u32,
        pub w: u32,
        pub h: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Size {
        pub w: u32,
        pub h: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct OutputInfo {
        pub dwId: u32,
        pub name: [c_char; OUTPUT_NAME_LEN],
        pub trackedBox: BoxRect,
    }

    impl Default for OutputInfo {
        fn default() -> Self {
            Self {
                dwId: 0,
                name: [0; OUTPUT_NAME_LEN],
                trackedBox: BoxRect::default(),
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct FrameGrabInfo {
        pub dwWidth: u32,
        pub dwHeight: u32,
        pub dwByteSize: u32,
        pub dwCurrentFrame: u32,
        pub bIsNewFrame: Bool,
        pub ulTimestampUs: u64,
        pub dwMissedFrames: u32,
        pub bRequiredPostProcessing: Bool,
        pub bDirectCapture: Bool,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct CreateHandleParams {
        pub dwVersion: u32,
        pub privateData: *const c_void,
        pub privateDataSize: u32,
        pub bExternallyManagedContext: Bool,
        pub glxCtx: *mut c_void,
        pub glxFBConfig: *mut c_void,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct DestroyHandleParams {
        pub dwVersion: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct GetStatusParams {
        pub dwVersion: u32,
        pub bIsCapturePossible: Bool,
        pub bCurrentlyCapturing: Bool,
        pub bCanCreateNow: Bool,
        pub screenSize: Size,
        pub bXRandRAvailable: Bool,
        pub outputs: [OutputInfo; OUTPUT_MAX],
        pub dwOutputNum: u32,
        pub dwNvFBCVersion: u32,
        pub bInModeset: Bool,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct CreateCaptureSessionParams {
        pub dwVersion: u32,
        pub eCaptureType: u32,
        pub eTrackingType: u32,
        pub dwOutputId: u32,
        pub captureBox: BoxRect,
        pub frameSize: Size,
        pub bWithCursor: Bool,
        pub bDisableAutoModesetRecovery: Bool,
        pub bRoundFrameSize: Bool,
        pub dwSamplingRateMs: u32,
        pub bPushModel: Bool,
        pub bAllowDirectCapture: Bool,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct DestroyCaptureSessionParams {
        pub dwVersion: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    // Public NvFBC 1.7 and 1.9 headers keep ToCuda setup at version 1 with
    // exactly these two fields. Diff-map fields belong to ToSys/ToGL; appending
    // them here would create an invalid ABI rather than enable a capability.
    pub struct ToCudaSetupParams {
        pub dwVersion: u32,
        pub eBufferFormat: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct ToCudaGrabFrameParams {
        pub dwVersion: u32,
        pub dwFlags: u32,
        pub pCUDADeviceBuffer: *mut c_void,
        pub pFrameGrabInfo: *mut FrameGrabInfo,
        pub dwTimeoutMs: u32,
    }

    type GetLastErrorStr = unsafe extern "C" fn(SessionHandle) -> *const c_char;
    type CreateHandle = unsafe extern "C" fn(*mut SessionHandle, *mut CreateHandleParams) -> Status;
    type DestroyHandle = unsafe extern "C" fn(SessionHandle, *mut DestroyHandleParams) -> Status;
    type GetStatus = unsafe extern "C" fn(SessionHandle, *mut GetStatusParams) -> Status;
    type CreateCaptureSession =
        unsafe extern "C" fn(SessionHandle, *mut CreateCaptureSessionParams) -> Status;
    type DestroyCaptureSession =
        unsafe extern "C" fn(SessionHandle, *mut DestroyCaptureSessionParams) -> Status;
    type ToCudaSetup = unsafe extern "C" fn(SessionHandle, *mut ToCudaSetupParams) -> Status;
    type ToCudaGrabFrame =
        unsafe extern "C" fn(SessionHandle, *mut ToCudaGrabFrameParams) -> Status;
    type BindContext = unsafe extern "C" fn(SessionHandle, *mut c_void) -> Status;
    type ReleaseContext = unsafe extern "C" fn(SessionHandle, *mut c_void) -> Status;
    type ToSysSetup = unsafe extern "C" fn(SessionHandle, *mut c_void) -> Status;
    type ToSysGrabFrame = unsafe extern "C" fn(SessionHandle, *mut c_void) -> Status;
    type ToGlSetup = unsafe extern "C" fn(SessionHandle, *mut c_void) -> Status;
    type ToGlGrabFrame = unsafe extern "C" fn(SessionHandle, *mut c_void) -> Status;

    #[repr(C)]
    #[allow(non_snake_case)]
    #[derive(Clone, Copy, Default)]
    pub struct ApiFunctionList {
        pub dwVersion: u32,
        pub nvFBCGetLastErrorStr: Option<GetLastErrorStr>,
        pub nvFBCCreateHandle: Option<CreateHandle>,
        pub nvFBCDestroyHandle: Option<DestroyHandle>,
        pub nvFBCGetStatus: Option<GetStatus>,
        pub nvFBCCreateCaptureSession: Option<CreateCaptureSession>,
        pub nvFBCDestroyCaptureSession: Option<DestroyCaptureSession>,
        pub nvFBCToSysSetUp: Option<ToSysSetup>,
        pub nvFBCToSysGrabFrame: Option<ToSysGrabFrame>,
        pub nvFBCToCudaSetUp: Option<ToCudaSetup>,
        pub nvFBCToCudaGrabFrame: Option<ToCudaGrabFrame>,
        pub pad1: *mut c_void,
        pub pad2: *mut c_void,
        pub pad3: *mut c_void,
        pub nvFBCBindContext: Option<BindContext>,
        pub nvFBCReleaseContext: Option<ReleaseContext>,
        pub pad4: *mut c_void,
        pub pad5: *mut c_void,
        pub pad6: *mut c_void,
        pub pad7: *mut c_void,
        pub nvFBCToGLSetUp: Option<ToGlSetup>,
        pub nvFBCToGLGrabFrame: Option<ToGlGrabFrame>,
    }

    type CreateInstance = unsafe extern "C" fn(*mut ApiFunctionList) -> Status;

    fn output_name(output: &OutputInfo) -> String {
        let len = output
            .name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(output.name.len());
        let bytes: Vec<u8> = output.name[..len].iter().map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub struct Capture {
        fl: ApiFunctionList,
        session: SessionHandle,
        lib: *mut c_void,
        handle_alive: bool,
        session_alive: bool,
        create_params: CreateCaptureSessionParams,
        buffer_format: u32,
        pub width: u32,
        pub height: u32,
    }

    #[derive(Clone, Copy)]
    pub struct CapturedFrame {
        pub device_ptr: CUdeviceptr,
        pub byte_size: usize,
        pub pitch: usize,
    }

    pub enum GrabOutcome {
        New(CapturedFrame),
        NoFrame,
        Recreated,
    }

    fn frame_pitch(
        width: u32,
        height: u32,
        byte_size: u32,
        buffer_format: u32,
    ) -> Result<usize, String> {
        let (row_bytes, planes) = if buffer_format == BUFFER_FORMAT_YUV444P {
            (width as usize, 3usize)
        } else {
            ((width as usize) * 4, 1usize)
        };
        let rows = (height as usize)
            .checked_mul(planes)
            .ok_or_else(|| "NvFBC frame row count overflow".to_string())?;
        let byte_size = byte_size as usize;
        if rows == 0 || !byte_size.is_multiple_of(rows) {
            return Err(format!(
                "NvFBC frame size {byte_size} is not divisible by {rows} rows"
            ));
        }
        let pitch = byte_size / rows;
        if pitch < row_bytes {
            return Err(format!(
                "NvFBC pitch {pitch} is smaller than visible row size {row_bytes}"
            ));
        }
        Ok(pitch)
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            unsafe {
                self.destroy_capture_session();
                if self.handle_alive {
                    if let Some(destroy) = self.fl.nvFBCDestroyHandle {
                        let mut p = DestroyHandleParams {
                            dwVersion: struct_version::<DestroyHandleParams>(1),
                        };
                        let _ = destroy(self.session, &mut p);
                    }
                    self.handle_alive = false;
                }
                dl::close(self.lib);
            }
        }
    }

    impl Capture {
        pub unsafe fn new(
            output_index: u32,
            yuv444: bool,
            cursor_mode: crate::CursorCaptureMode,
        ) -> Result<Self, NativeStartupError> {
            if std::env::var_os("DISPLAY").is_none() {
                return Err(NativeStartupError::unavailable(
                    BackendUnavailableReason::UnsupportedDisplay,
                    "DISPLAY is not set; NvFBC needs an X11 display",
                ));
            }

            let lib = dl::open("libnvidia-fbc.so.1").map_err(|error| {
                NativeStartupError::unavailable(BackendUnavailableReason::RuntimeMissing, error)
            })?;
            let create_instance: CreateInstance =
                std::mem::transmute(dl::sym(lib, "NvFBCCreateInstance").map_err(|error| {
                    NativeStartupError::unavailable(BackendUnavailableReason::RuntimeMissing, error)
                })?);

            let mut fl: ApiFunctionList = zeroed();
            fl.dwVersion = VERSION;
            let st = create_instance(&mut fl);
            if st != SUCCESS {
                dl::close(lib);
                return Err(startup_status(
                    st,
                    format!("NvFBCCreateInstance -> status {st}"),
                ));
            }

            let create_handle = fl.nvFBCCreateHandle.ok_or_else(|| {
                NativeStartupError::fatal("NvFBC function table missing nvFBCCreateHandle")
            })?;
            let mut session = 0;
            let mut ch = CreateHandleParams {
                dwVersion: struct_version::<CreateHandleParams>(2),
                ..Default::default()
            };
            let st = create_handle(&mut session, &mut ch);
            if st != SUCCESS {
                let msg = format!("NvFBCCreateHandle -> status {st}");
                dl::close(lib);
                return Err(startup_status(st, msg));
            }

            let mut cap = Self {
                fl,
                session,
                lib,
                handle_alive: true,
                session_alive: false,
                create_params: CreateCaptureSessionParams::default(),
                buffer_format: if yuv444 {
                    BUFFER_FORMAT_YUV444P
                } else {
                    BUFFER_FORMAT_BGRA
                },
                width: 0,
                height: 0,
            };

            let gs = cap.get_status().map_err(NativeStartupError::fatal)?;
            if gs.bIsCapturePossible == FALSE {
                return Err(NativeStartupError::unavailable(
                    BackendUnavailableReason::HardwareUnavailable,
                    "NvFBC reports capture is not possible on this system",
                ));
            }
            if gs.bCanCreateNow == FALSE {
                return Err(NativeStartupError::unavailable(
                    BackendUnavailableReason::SessionLimit,
                    "NvFBC reports a capture session cannot be created now",
                ));
            }

            let output_count = gs.dwOutputNum.min(OUTPUT_MAX as u32);
            let (tracking, output_id, frame_size) =
                if gs.bXRandRAvailable == TRUE && output_index < output_count {
                    let output = gs.outputs[output_index as usize];
                    let size = Size {
                        w: output.trackedBox.w,
                        h: output.trackedBox.h,
                    };
                    if size.w > 0 && size.h > 0 {
                        log(&format!(
                            "bound RandR output {} id={} name={:?} size={}x{}",
                            output_index,
                            output.dwId,
                            output_name(&output),
                            size.w,
                            size.h
                        ));
                        (TRACKING_OUTPUT, output.dwId, size)
                    } else {
                        (TRACKING_SCREEN, 0, gs.screenSize)
                    }
                } else {
                    (TRACKING_SCREEN, 0, gs.screenSize)
                };

            cap.width = frame_size.w;
            cap.height = frame_size.h;
            cap.create_params = CreateCaptureSessionParams {
                dwVersion: struct_version::<CreateCaptureSessionParams>(6),
                eCaptureType: CAPTURE_SHARED_CUDA,
                eTrackingType: tracking,
                dwOutputId: output_id,
                captureBox: BoxRect::default(),
                frameSize: frame_size,
                bWithCursor: super::nvfbc_cursor_flag(cursor_mode),
                bDisableAutoModesetRecovery: FALSE,
                bRoundFrameSize: FALSE,
                dwSamplingRateMs: 16,
                bPushModel: TRUE,
                bAllowDirectCapture: FALSE,
            };

            cap.create_capture_session()?;
            log(&format!(
                "NvFBC ready: {}x{} output={} driver_ver={} capture_type=shared_cuda cursor={}",
                cap.width,
                cap.height,
                output_index,
                gs.dwNvFBCVersion,
                if cursor_mode.include_cursor() {
                    "host"
                } else {
                    "local"
                }
            ));
            log("WARNING: NvFBC ToCuda API 1.7 exposes no diff-map fields; \
                 damage_source=unavailable_to_cuda; continuing with bIsNewFrame cadence");
            Ok(cap)
        }

        unsafe fn get_status(&self) -> Result<GetStatusParams, String> {
            let get_status = self
                .fl
                .nvFBCGetStatus
                .ok_or_else(|| "NvFBC function table missing nvFBCGetStatus".to_string())?;
            let mut gs = GetStatusParams {
                dwVersion: struct_version::<GetStatusParams>(2),
                ..Default::default()
            };
            let st = get_status(self.session, &mut gs);
            if st == SUCCESS {
                Ok(gs)
            } else {
                Err(self.error("NvFBCGetStatus", st))
            }
        }

        unsafe fn create_capture_session(&mut self) -> Result<(), NativeStartupError> {
            let create = self.fl.nvFBCCreateCaptureSession.ok_or_else(|| {
                NativeStartupError::fatal("NvFBC function table missing nvFBCCreateCaptureSession")
            })?;
            let setup = self.fl.nvFBCToCudaSetUp.ok_or_else(|| {
                NativeStartupError::fatal("NvFBC function table missing nvFBCToCudaSetUp")
            })?;

            let st = create(self.session, &mut self.create_params);
            if st != SUCCESS {
                return Err(startup_status(
                    st,
                    self.error("NvFBCCreateCaptureSession", st),
                ));
            }
            self.session_alive = true;

            let mut setup_params = ToCudaSetupParams {
                dwVersion: struct_version::<ToCudaSetupParams>(1),
                eBufferFormat: self.buffer_format,
            };
            let st = setup(self.session, &mut setup_params);
            if st != SUCCESS {
                self.destroy_capture_session();
                return Err(startup_status(st, self.error("NvFBCToCudaSetUp", st)));
            }
            Ok(())
        }

        unsafe fn destroy_capture_session(&mut self) {
            if self.session_alive {
                if let Some(destroy) = self.fl.nvFBCDestroyCaptureSession {
                    let mut p = DestroyCaptureSessionParams {
                        dwVersion: struct_version::<DestroyCaptureSessionParams>(1),
                    };
                    let _ = destroy(self.session, &mut p);
                }
                self.session_alive = false;
            }
        }

        fn error(&self, where_: &str, st: Status) -> String {
            unsafe {
                if let Some(last) = self.fl.nvFBCGetLastErrorStr {
                    let ptr = last(self.session);
                    if !ptr.is_null() {
                        let msg = CStr::from_ptr(ptr).to_string_lossy();
                        if !msg.is_empty() {
                            return format!("{where_}: {msg} (status {st})");
                        }
                    }
                }
            }
            format!("{where_} -> NvFBC status {st}")
        }

        pub unsafe fn grab(
            &mut self,
            timeout_ms: u32,
            dbg: &mut (u64, u64, u64),
        ) -> Result<GrabOutcome, String> {
            let grab = self
                .fl
                .nvFBCToCudaGrabFrame
                .ok_or_else(|| "NvFBC function table missing nvFBCToCudaGrabFrame".to_string())?;

            let mut src = 0 as CUdeviceptr;
            let mut fi: FrameGrabInfo = zeroed();
            let mut gp = ToCudaGrabFrameParams {
                dwVersion: struct_version::<ToCudaGrabFrameParams>(2),
                dwFlags: TOCUDA_GRAB_FLAGS_NOWAIT_IF_NEW_FRAME_READY,
                pCUDADeviceBuffer: &mut src as *mut CUdeviceptr as *mut c_void,
                pFrameGrabInfo: &mut fi as *mut FrameGrabInfo,
                dwTimeoutMs: timeout_ms,
            };

            let st = grab(self.session, &mut gp);
            if st == ERR_MUST_RECREATE {
                log("NvFBC modeset detected - recreating capture session");
                self.destroy_capture_session();
                self.create_capture_session()
                    .map_err(|error| error.to_string())?;
                return Ok(GrabOutcome::Recreated);
            }
            if st != SUCCESS {
                return Err(self.error("NvFBCToCudaGrabFrame", st));
            }
            if fi.bDirectCapture == TRUE {
                dbg.2 += 1;
            }
            if fi.bIsNewFrame != TRUE {
                dbg.1 += 1;
                return Ok(GrabOutcome::NoFrame);
            }
            if fi.dwWidth != self.width || fi.dwHeight != self.height {
                return Err(format!(
                    "NvFBC dimension drift: expected {}x{}, got {}x{}",
                    self.width, self.height, fi.dwWidth, fi.dwHeight
                ));
            }
            if src == 0 || fi.dwByteSize == 0 {
                dbg.1 += 1;
                return Ok(GrabOutcome::NoFrame);
            }
            let pitch = frame_pitch(fi.dwWidth, fi.dwHeight, fi.dwByteSize, self.buffer_format)?;
            dbg.0 += 1;
            Ok(GrabOutcome::New(CapturedFrame {
                device_ptr: src,
                byte_size: fi.dwByteSize as usize,
                pitch,
            }))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn derives_padded_yuv444_plane_pitch_from_frame_size() {
            let width = 2560;
            let height = 1440;
            let padded_pitch = 2688usize;
            let byte_size = (padded_pitch * height as usize * 3) as u32;

            assert_eq!(
                frame_pitch(width, height, byte_size, BUFFER_FORMAT_YUV444P).unwrap(),
                padded_pitch
            );
        }

        #[test]
        fn rejects_yuv444_frame_size_that_cannot_describe_three_planes() {
            let error =
                frame_pitch(2560, 1440, 2560 * 1440 * 3 + 1, BUFFER_FORMAT_YUV444P).unwrap_err();

            assert!(error.contains("not divisible"));
        }

        #[test]
        fn to_cuda_api_1_7_layout_has_no_diff_map_fields() {
            assert_eq!(std::mem::size_of::<ToCudaSetupParams>(), 8);
            assert_eq!(std::mem::offset_of!(ToCudaSetupParams, eBufferFormat), 4);
            assert_eq!(struct_version::<ToCudaSetupParams>(1), 0x0701_0008);

            assert_eq!(std::mem::size_of::<ToCudaGrabFrameParams>(), 32);
            assert_eq!(
                std::mem::offset_of!(ToCudaGrabFrameParams, pCUDADeviceBuffer),
                8
            );
            assert_eq!(
                std::mem::offset_of!(ToCudaGrabFrameParams, pFrameGrabInfo),
                16
            );
            assert_eq!(std::mem::offset_of!(ToCudaGrabFrameParams, dwTimeoutMs), 24);
            assert_eq!(struct_version::<ToCudaGrabFrameParams>(2), 0x0702_0020);
        }

        #[test]
        fn nvfbc_cursor_flag_matches_fixed_mode() {
            assert_eq!(
                super::super::nvfbc_cursor_flag(crate::CursorCaptureMode::Local),
                FALSE
            );
            assert_eq!(
                super::super::nvfbc_cursor_flag(crate::CursorCaptureMode::Host),
                TRUE
            );
        }

        #[test]
        fn startup_status_preserves_typed_fallback_boundaries() {
            assert!(matches!(
                startup_status(ERR_MAX_CLIENTS, "max clients"),
                NativeStartupError::Unavailable {
                    reason: BackendUnavailableReason::SessionLimit,
                    ..
                }
            ));
            assert!(matches!(
                startup_status(ERR_UNSUPPORTED, "unsupported"),
                NativeStartupError::Unavailable {
                    reason: BackendUnavailableReason::UnsupportedConfiguration,
                    ..
                }
            ));
            for status in [1, 2, 3, 4, 5, 8, 9, 10, 11, 12, 13, 14, 15, 17] {
                assert!(matches!(
                    startup_status(status, format!("status {status}")),
                    NativeStartupError::Fatal(_)
                ));
            }
        }
    }
}

struct EncodeOptions<'a> {
    codec: &'a str,
    fps: u32,
    /// NvFBC's own capture buffer shape (`PixelFormat::nvfbc_capture_is_yuv444`).
    /// Used only by the eight-bit path; the depth-30 XShm path never opens
    /// NvFBC.
    yuv444: bool,
    framed: bool,
    cursor_mode: crate::CursorCaptureMode,
    /// The negotiated colour contract this run's `Encoder` was actually
    /// constructed with (see `run_with_args`, this struct's only builder) —
    /// never a separately re-derived `ColorSpec::legacy(...)`, so the READY
    /// line built below cannot disagree with what the encoder produces.
    color: crate::ColorSpec,
}

/// The NvFBC encode loop.
///
/// This function is intentionally eight-bit-only. `run_with_args` routes every
/// deeper contract to [`run_wide_encode`] before constructing NvFBC, and this
/// loop repeats that invariant defensively so an eight-bit source can never be
/// announced as ten-bit.
unsafe fn run_encode(
    _cuda_ctx: cuda::Context,
    mut cap: nvfbc::Capture,
    mut encoder: crate::nvenc_cuda::Encoder,
    options: EncodeOptions<'_>,
) -> i32 {
    let EncodeOptions {
        codec,
        fps,
        yuv444,
        framed,
        cursor_mode,
        color,
    } = options;
    if color.bit_depth != arcen_media::BitDepth::Eight {
        log_error(
            "NvFBC was selected for a depth above eight; refusing to encode an 8-bit capture \
             in a wider container",
        );
        return 2;
    }
    log(&format!(
        "NVENC ready: {}x{} codec={} chroma={:?} depth={:?}-bit (NvFBC capture={})",
        cap.width,
        cap.height,
        codec,
        color.chroma,
        color.bit_depth,
        if yuv444 { "yuv444p" } else { "bgra" }
    ));
    let control = crate::spawn_control_thread("NVENC");

    let mut stdout = std::io::stdout();
    let target_dt = crate::frame_interval_from_fps(fps);
    let mut next = Instant::now();
    let mut latest_frame = None;
    let mut announced_layout = false;
    let mut submission_gate = SubmissionGate::new(IDLE_KEEPALIVE);
    let mut last_emit = Instant::now();
    let mut stale_outputs_to_drop = 0usize;
    let mut ready_announced = false;

    let mut dbg = (0u64, 0u64, 0u64); // new, old/timeout, direct-capture frames
    let mut sec = Instant::now();
    let mut stats = PipelineStats::new();

    while !control.stop_requested() {
        match cap.grab(2, &mut dbg) {
            Ok(nvfbc::GrabOutcome::New(frame)) => {
                if !announced_layout {
                    log(&format!(
                        "NvFBC frame layout: bytes={} pitch={} visible_row={} planes={}",
                        frame.byte_size,
                        frame.pitch,
                        if yuv444 { cap.width } else { cap.width * 4 },
                        if yuv444 { 3 } else { 1 }
                    ));
                    announced_layout = true;
                }
                latest_frame = Some(frame);
                submission_gate.note_frame();
            }
            Ok(nvfbc::GrabOutcome::NoFrame) => {}
            Ok(nvfbc::GrabOutcome::Recreated) => {
                // NvFBC owns the captured CUDA pointer. Session destruction
                // invalidates it, so no keepalive/flush/IDR may reuse it.
                latest_frame = None;
                submission_gate.reset();
                announced_layout = false;
                stale_outputs_to_drop = encoder.pending_output_count();
                stats.record_capture_recreate();
            }
            Err(e) => {
                log_error(&format!("capture error: {e}"));
                return 3;
            }
        }

        let now = Instant::now();
        if now >= next {
            let idr_pending = control.idr_pending();
            let mode = submission_gate.decision(idr_pending, last_emit.elapsed());
            if let (Some(frame), Some(mode)) = (latest_frame, mode) {
                // NvFBC reuses one shared CUDA buffer, while NVENC rotates
                // through its own input ring. Restage the retained latest frame
                // into the current slot on every actual submission so static
                // desktops cannot alternate with stale ring contents.
                let stage_started = Instant::now();
                if let Err(e) = encoder.stage(frame.device_ptr, frame.pitch) {
                    log_error(&format!("stage failed: {e}"));
                    return 5;
                }
                stats.record_stage(stage_started.elapsed().as_secs_f64() * 1000.0);
                // Consume only the request observed before the cadence
                // decision. A request racing in later remains pending.
                let requested_idr = idr_pending && control.take_idr();
                let force = mode == SubmissionMode::FirstFrame || requested_idr;
                if requested_idr && mode != SubmissionMode::FirstFrame {
                    log("consuming IDR request");
                }

                let t0 = Instant::now();
                match encoder.encode(force) {
                    Ok(out) => {
                        let ms = t0.elapsed().as_secs_f64() * 1000.0;
                        stats.record_submission(mode, ms);
                        submission_gate.on_submitted(mode, out.is_some());
                        last_emit = now;
                        let (out, stale_dropped) =
                            discard_stale_output(out, &mut stale_outputs_to_drop);
                        if stale_dropped {
                            stats.record_stale_output_drop();
                        }
                        if let Some(au) = out {
                            if !ready_announced {
                                if let Err(error) = crate::validate_access_unit(&au, framed) {
                                    log_error(&format!(
                                        "first access unit failed output validation: {error}"
                                    ));
                                    return 5;
                                }
                                let plan = match crate::resolved_media_plan(
                                    EncoderBackend::NativeNvenc,
                                    codec,
                                    color,
                                    cap.width,
                                    cap.height,
                                    fps,
                                    cursor_mode,
                                ) {
                                    Ok(plan) => plan,
                                    Err(error) => {
                                        log_error(&error);
                                        return 5;
                                    }
                                };
                                if let Err(error) = crate::announce_ready_from(
                                    plan,
                                    Some(arcen_media::video::CaptureBackend::NvFbc),
                                ) {
                                    log_error(&format!("emit READY: {error}"));
                                    return 5;
                                }
                                ready_announced = true;
                            }
                            if crate::write_access_unit(&mut stdout, &au, framed).is_err() {
                                return 0;
                            }
                            stats.record_emitted(au.len());
                        }
                    }
                    Err(e) => {
                        log_error(&format!("encode error: {e}"));
                        return 5;
                    }
                }
            }
            next += target_dt;
            if next < now {
                next = now + target_dt;
            }
        }

        if sec.elapsed().as_secs_f64() >= 1.0 {
            stats.log_and_reset(dbg, control.idr_pending(), "nvfbc", NVFBC_DAMAGE_SOURCE);
            dbg = (0, 0, 0);
            sec = Instant::now();
        }
    }
    log("NVENC control closed; dropping encoder and capture before exit");
    0
}

/// Native NVENC loop for a genuine depth-30 X11 source.
///
/// Unlike [`run_encode`], this path never opens NvFBC. Each serviced frame is
/// copied by MIT-SHM into host memory, converted from the root visual's actual
/// RGB10 channel masks into the negotiated ten-bit NVENC surface, then uploaded
/// to the current CUDA input slot. The existing eight-bit NvFBC path remains
/// device-to-device and does not execute any of this code.
unsafe fn run_wide_encode(
    _cuda_ctx: cuda::Context,
    mut capture: crate::linux_x11::X11Capture,
    mut encoder: crate::nvenc_cuda::Encoder,
    options: EncodeOptions<'_>,
) -> i32 {
    let EncodeOptions {
        codec,
        fps,
        framed,
        cursor_mode,
        color,
        ..
    } = options;
    let width = capture.width();
    let height = capture.height();
    if color.bit_depth == arcen_media::BitDepth::Eight {
        log_error("XShm wide capture was selected for an eight-bit contract");
        return 2;
    }
    log(&format!(
        "NVENC wide capture ready: {width}x{height} codec={codec} chroma={:?} \
         depth={:?}-bit matrix={:?} primaries={:?} transfer={:?} source={} transport={}",
        color.chroma,
        color.bit_depth,
        color.matrix,
        color.primaries,
        color.transfer,
        capture.pixel_format_token(),
        capture.transfer_token(),
    ));
    let control = crate::spawn_control_thread("NVENC-XShm");
    let mut stdout = std::io::stdout();
    let target_dt = crate::frame_interval_from_fps(fps);
    let mut next = Instant::now();
    let mut submission_gate = SubmissionGate::new(IDLE_KEEPALIVE);
    submission_gate.note_frame();
    let mut last_emit = Instant::now();
    let mut stale_outputs_to_drop = 0usize;
    let mut ready_announced = false;
    let mut capture_counts = (0u64, 0u64, 0u64);
    let mut sec = Instant::now();
    let mut stats = PipelineStats::new();
    let mut precision_attempts = 0u8;
    let mut precision_proven = false;

    while !control.stop_requested() {
        match capture.poll_activity() {
            Ok(crate::linux_x11::Activity::Damage) => submission_gate.note_frame(),
            Ok(crate::linux_x11::Activity::Modeset) => {
                capture = match capture.recreate() {
                    Ok(capture) => capture,
                    Err(error) => {
                        log_error(&format!("X11 modeset recreation failed: {error}"));
                        return 3;
                    }
                };
                submission_gate.reset();
                submission_gate.note_frame();
                stale_outputs_to_drop = encoder.pending_output_count();
                stats.record_capture_recreate();
            }
            Ok(crate::linux_x11::Activity::None) => {
                capture_counts.1 = capture_counts.1.saturating_add(1);
                if !capture.has_damage() {
                    submission_gate.note_frame();
                }
            }
            Err(error) => {
                log_error(&format!("X11 capture event failed: {error}"));
                return 3;
            }
        }

        let now = Instant::now();
        if now >= next {
            let idr_pending = control.idr_pending();
            let mode = submission_gate.decision(idr_pending, last_emit.elapsed());
            if let Some(mode) = mode {
                let capture_started = Instant::now();
                let frame = match capture.capture_wide() {
                    Ok(frame) => frame,
                    Err(error) => {
                        log_error(&format!("X11 depth-30 capture failed: {error}"));
                        return 3;
                    }
                };
                stats.record_capture(capture_started.elapsed().as_secs_f64() * 1000.0);
                capture_counts.0 = capture_counts.0.saturating_add(1);
                if frame.width != width as usize || frame.height != height as usize {
                    log_error("X11 depth-30 frame geometry changed without a modeset");
                    return 3;
                }
                if !precision_proven && precision_attempts < 30 {
                    let precision = frame.precision_stats(8);
                    let off_grid_basis_points = if precision.sampled_components == 0 {
                        0
                    } else {
                        precision.off_eight_bit_grid.saturating_mul(10_000)
                            / precision.sampled_components
                    };
                    if precision_attempts == 0 || precision.off_eight_bit_grid != 0 {
                        log(&format!(
                            "X11 RGB10 source precision: sampled_components={} \
                             off_8bit_grid={} off_8bit_grid_bps={} min={} max={}",
                            precision.sampled_components,
                            precision.off_eight_bit_grid,
                            off_grid_basis_points,
                            precision.minimum,
                            precision.maximum,
                        ));
                    }
                    precision_proven = precision.off_eight_bit_grid != 0;
                    precision_attempts = precision_attempts.saturating_add(1);
                }

                let stage_started = Instant::now();
                if let Err(error) = encoder.stage_wide_host(frame.bytes, frame.stride, frame.layout)
                {
                    log_error(&format!("stage X11 depth-30 frame failed: {error}"));
                    return 5;
                }
                stats.record_stage(stage_started.elapsed().as_secs_f64() * 1000.0);

                let requested_idr = idr_pending && control.take_idr();
                let force = mode == SubmissionMode::FirstFrame || requested_idr;
                if requested_idr && mode != SubmissionMode::FirstFrame {
                    log("consuming IDR request");
                }

                let encode_started = Instant::now();
                match encoder.encode(force) {
                    Ok(output) => {
                        stats.record_submission(
                            mode,
                            encode_started.elapsed().as_secs_f64() * 1000.0,
                        );
                        submission_gate.on_submitted(mode, output.is_some());
                        last_emit = now;
                        let (output, stale_dropped) =
                            discard_stale_output(output, &mut stale_outputs_to_drop);
                        if stale_dropped {
                            stats.record_stale_output_drop();
                        }
                        if let Some(access_unit) = output {
                            if !ready_announced {
                                if let Err(error) =
                                    crate::validate_access_unit(&access_unit, framed)
                                {
                                    log_error(&format!(
                                        "first access unit failed output validation: {error}"
                                    ));
                                    return 5;
                                }
                                let plan = match crate::resolved_media_plan(
                                    EncoderBackend::NativeNvenc,
                                    codec,
                                    color,
                                    width,
                                    height,
                                    fps,
                                    cursor_mode,
                                ) {
                                    Ok(plan) => plan,
                                    Err(error) => {
                                        log_error(&error);
                                        return 5;
                                    }
                                };
                                if let Err(error) = crate::announce_ready_from(
                                    plan,
                                    Some(arcen_media::video::CaptureBackend::XShm),
                                ) {
                                    log_error(&format!("emit READY: {error}"));
                                    return 5;
                                }
                                ready_announced = true;
                            }
                            if crate::write_access_unit(&mut stdout, &access_unit, framed).is_err()
                            {
                                return 0;
                            }
                            stats.record_emitted(access_unit.len());
                        }
                    }
                    Err(error) => {
                        log_error(&format!("encode error: {error}"));
                        return 5;
                    }
                }
            }
            next += target_dt;
            if next < now {
                next = now + target_dt;
            }
        }

        if sec.elapsed().as_secs_f64() >= 1.0 {
            let damage_source = if capture.has_damage() {
                "xdamage"
            } else {
                "full_poll"
            };
            stats.log_and_reset(capture_counts, control.idr_pending(), "xshm", damage_source);
            capture_counts = (0, 0, 0);
            sec = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    log("NVENC-XShm control closed; dropping encoder and capture before exit");
    0
}

struct SelftestCudaAllocation(cuda::CUdeviceptr);

impl Drop for SelftestCudaAllocation {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the allocation returned by
        // `cuMemAlloc`; the encoder is dropped before this allocation.
        unsafe {
            let _ = cuda::mem_free(self.0);
        }
    }
}

unsafe fn run_selftest(
    cuda_ctx: cuda::Context,
    codec: &str,
    w: u32,
    h: u32,
    color: crate::ColorSpec,
    intent: arcen_media::EncodeIntent,
    qp_map_policy: crate::qp_map::QpMapPolicy,
    yuv444: bool,
    framed: bool,
) -> i32 {
    let frame_bytes = (w as usize) * (h as usize) * if yuv444 { 3 } else { 4 };
    let src = match cuda::mem_alloc(frame_bytes) {
        Ok(ptr) => ptr,
        Err(e) => {
            log_error(&format!("selftest cuMemAlloc failed: {e}"));
            return 6;
        }
    };
    let src = SelftestCudaAllocation(src);
    let _ = cuda::memset_d8(src.0, 0, frame_bytes);

    let mut encoder = match crate::nvenc_cuda::Encoder::new(
        cuda_ctx.as_raw(),
        w,
        h,
        codec,
        color,
        intent,
        qp_map_policy,
    ) {
        Ok(e) => e,
        Err(e) => {
            log_error(&format!("NVENC init failed: {e}"));
            return 4;
        }
    };
    log(&format!(
        "NVENC selftest: {w}x{h} codec={codec} chroma={:?} depth={:?}-bit (CUDA memset content, \
         source={})",
        color.chroma,
        color.bit_depth,
        if yuv444 { "yuv444p" } else { "bgra" }
    ));

    let mut stdout = std::io::stdout();
    let target_dt = Duration::from_micros(16_666);
    let mut next = Instant::now();
    let mut frame = 0u32;
    let mut sec = Instant::now();
    let (mut cnt, mut ms_sum, mut ms_max, mut bytes) = (0u64, 0.0f64, 0.0f64, 0u64);

    loop {
        let t0 = Instant::now();
        if let Err(e) = cuda::memset_d8(src.0, (frame & 0xff) as u8, frame_bytes) {
            log_error(&format!("selftest memset: {e}"));
            return 7;
        }
        let source_pitch = if yuv444 { w } else { w * 4 };
        if let Err(e) = encoder.stage(src.0, source_pitch as usize) {
            log_error(&format!("selftest stage: {e}"));
            return 7;
        }
        match encoder.encode(frame == 0) {
            Ok(out) => {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                if let Some(au) = out {
                    if crate::write_access_unit(&mut stdout, &au, framed).is_err() {
                        return 0;
                    }
                    cnt += 1;
                    ms_sum += ms;
                    if ms > ms_max {
                        ms_max = ms;
                    }
                    bytes += au.len() as u64;
                }
            }
            Err(e) => {
                log_error(&format!("selftest encode: {e}"));
                return 5;
            }
        }
        frame = frame.wrapping_add(1);

        if sec.elapsed().as_secs_f64() >= 1.0 {
            let avg = if cnt > 0 { ms_sum / cnt as f64 } else { 0.0 };
            log(&format!(
                "SELFTEST enc/s={cnt} stage+encode avg_ms={avg:.2} max_ms={ms_max:.2} mbps={:.1}",
                (bytes * 8) as f64 / 1_000_000.0
            ));
            cnt = 0;
            ms_sum = 0.0;
            ms_max = 0.0;
            bytes = 0;
            sec = Instant::now();
        }

        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next += target_dt;
        if next < now {
            next = now + target_dt;
        }
    }
}

unsafe fn run_admission_probe(
    cuda_ctx: cuda::Context,
    capture: nvfbc::Capture,
    codec: &str,
    color: crate::ColorSpec,
    intent: arcen_media::EncodeIntent,
    qp_map_policy: crate::qp_map::QpMapPolicy,
    yuv444: bool,
    options: &crate::admission_probe::AdmissionProbeOptions,
) -> i32 {
    if capture.width != options.width || capture.height != options.height {
        log_error(&format!(
            "admission probe geometry {}x{} differs from exact NvFBC output {}x{}",
            options.width, options.height, capture.width, capture.height
        ));
        return 2;
    }
    let frame_bytes = match (options.width as usize)
        .checked_mul(options.height as usize)
        .and_then(|pixels| pixels.checked_mul(if yuv444 { 3 } else { 4 }))
    {
        Some(bytes) if bytes > 0 => bytes,
        _ => {
            log_error("admission probe frame geometry overflow");
            return 2;
        }
    };
    let source = match cuda::mem_alloc(frame_bytes) {
        Ok(pointer) => SelftestCudaAllocation(pointer),
        Err(error) => {
            log_error(&format!("admission probe cuMemAlloc failed: {error}"));
            return 6;
        }
    };
    let _ = cuda::memset_d8(source.0, 0, frame_bytes);
    let mut encoder = match crate::nvenc_cuda::Encoder::new(
        cuda_ctx.as_raw(),
        options.width,
        options.height,
        codec,
        color,
        intent,
        qp_map_policy,
    ) {
        Ok(encoder) => encoder,
        Err(error) => {
            log_error(&format!("admission probe NVENC init failed: {error}"));
            return 4;
        }
    };
    let source_pitch = if yuv444 {
        options.width
    } else {
        options.width.saturating_mul(4)
    };
    let mut frame = 0u8;
    let result =
        crate::admission_probe::run_probe_loop(options, std::io::stdout().lock(), |input| {
            frame = frame.wrapping_add(1);
            let changed_bytes = if input.kind == arcen_media::RepresentativeFrameKind::FullMotion {
                frame_bytes
            } else {
                frame_bytes
                    .saturating_mul(usize::from(input.dirty_ratio.basis_points()))
                    .div_ceil(10_000)
                    .max(1)
            };
            cuda::memset_d8(source.0, frame, changed_bytes)
                .map_err(|error| format!("admission probe synthetic update: {error}"))?;
            let started = Instant::now();
            encoder
                .stage(source.0, source_pitch as usize)
                .map_err(|error| format!("admission probe stage: {error}"))?;
            let output = encoder
                .encode(input.force_idr)
                .map_err(|error| format!("admission probe encode: {error}"))?;
            Ok(crate::admission_probe::ProbeEncodeResult {
                encode_latency: started.elapsed(),
                delivered: output.is_some(),
            })
        });
    drop(encoder);
    drop(capture);
    drop(source);
    drop(cuda_ctx);
    match result {
        Ok(()) => 0,
        Err(error) => {
            log_error(&error);
            5
        }
    }
}

unsafe fn run_wide_admission_probe(
    cuda_ctx: cuda::Context,
    mut capture: crate::linux_x11::X11Capture,
    codec: &str,
    color: crate::ColorSpec,
    intent: arcen_media::EncodeIntent,
    qp_map_policy: crate::qp_map::QpMapPolicy,
    options: &crate::admission_probe::AdmissionProbeOptions,
) -> i32 {
    if capture.width() != options.width || capture.height() != options.height {
        log_error(&format!(
            "admission probe geometry {}x{} differs from exact X11 output {}x{}",
            options.width,
            options.height,
            capture.width(),
            capture.height()
        ));
        return 2;
    }
    let mut encoder = match crate::nvenc_cuda::Encoder::new(
        cuda_ctx.as_raw(),
        options.width,
        options.height,
        codec,
        color,
        intent,
        qp_map_policy,
    ) {
        Ok(encoder) => encoder,
        Err(error) => {
            log_error(&format!("wide admission probe NVENC init failed: {error}"));
            return 4;
        }
    };
    if qp_map_policy.submits_map() {
        let engaged = encoder.enable_qp_map(
            qp_map_policy,
            arcen_media::video::QpBias::default(),
            arcen_media::VideoCodec::from_token(codec).unwrap_or(arcen_media::VideoCodec::H264),
        );
        log(&format!(
            "wide admission QP map policy={} engaged={engaged}",
            qp_map_policy.token()
        ));
    }
    let result =
        crate::admission_probe::run_probe_loop(options, std::io::stdout().lock(), |input| {
            let started = Instant::now();
            let frame = capture
                .capture_wide()
                .map_err(|error| format!("wide admission XShm capture: {error}"))?;
            encoder
                .stage_wide_host(frame.bytes, frame.stride, frame.layout)
                .map_err(|error| format!("wide admission stage: {error}"))?;
            let output = encoder
                .encode(input.force_idr)
                .map_err(|error| format!("wide admission encode: {error}"))?;
            Ok(crate::admission_probe::ProbeEncodeResult {
                encode_latency: started.elapsed(),
                delivered: output.is_some(),
            })
        });
    drop(encoder);
    drop(capture);
    drop(cuda_ctx);
    match result {
        Ok(()) => 0,
        Err(error) => {
            log_error(&error);
            5
        }
    }
}

pub fn run_with_args(args: Vec<String>, requested_encoder: RequestedEncoder) -> ! {
    let cursor_mode = match crate::cursor_mode_from_args(&args) {
        Ok(mode) => mode,
        Err(error) => {
            log_error(error);
            std::process::exit(1);
        }
    };
    let output_index: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let codec = args.get(2).cloned().unwrap_or_else(|| "h264".to_string());
    let fps_arg = args.get(3).filter(|value| value.as_str() != "selftest");
    let fps = fps_arg
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(60)
        .clamp(1, 240);
    // "yuv444" may appear anywhere after the codec (position-independent so
    // the fps arg stays optional): 4:4:4 capture (NvFBC YUV444P) + encode.
    let yuv444_token = args.iter().any(|value| value == "yuv444");
    // The single resolved colour contract for this run: an explicit
    // `variant=<id>` wins over the legacy `yuv444_token`, and it is this one
    // value — never a separately re-derived `ColorSpec::legacy(...)` — that
    // must reach the encoder init below, `EncodeOptions`, and
    // `resolved_media_plan`'s READY line. See `crate::requested_color`.
    let color = match crate::requested_color(&args, yuv444_token) {
        Ok(color) => color,
        Err(error) => {
            log_error(&error);
            std::process::exit(2);
        }
    };
    let intent = match crate::requested_intent(&args) {
        Ok(intent) => intent,
        Err(error) => {
            log_error(&format!("invalid intent: {error}"));
            std::process::exit(2);
        }
    };
    let qp_map_policy = match crate::requested_qp_map(&args) {
        Ok(policy) => policy,
        Err(error) => {
            log_error(&format!("invalid qp-map: {error}"));
            std::process::exit(2);
        }
    };
    // NvFBC/CUDA buffer geometry and the encoder's own `chroma`/`bit_depth`
    // gate (`nvenc_cuda::Encoder::new`) must agree on this flag, or a variant
    // requesting 4:4:4 while the raw positional token still says 4:2:0 (or
    // vice versa) would feed the encoder a buffer laid out for the wrong
    // chroma — silent corruption, not merely a mislabelled stream. Deriving
    // it here, once, from `color` and `codec` (H.264 cannot carry 10-bit —
    // see `ColorSpecRejection::H264RequiresEightBit`) and rejecting up
    // front, before any capture resource is allocated, anything this Linux
    // path cannot honour at all — currently 4:2:2 — closes that gap.
    //
    // `yuv444` is *not* simply "chroma == Yuv444" any more now that bit
    // depth is negotiable: 10-bit 4:4:4 still needs NvFBC to hand this file
    // raw BGRA (see `PixelFormat::nvfbc_capture_is_yuv444`'s doc) so it can
    // perform its own MSB-aligned conversion, exactly like nvenc.rs's D3D11
    // path — asking NvFBC for its own YUV444P conversion at 10-bit would
    // silently discard the extra precision the format exists for and feed
    // the encoder a differently-shaped buffer than its surface expects.
    //
    // The codec token is parsed once into `NvencCodec` here (see its doc):
    // an unrecognised token is a named, typed failure rather than silently
    // being treated as H.264.
    let nvenc_codec = match crate::nvenc_cuda::NvencCodec::parse(&codec) {
        Some(nvenc_codec) => nvenc_codec,
        None => {
            log_error(&format!(
                "unrecognized codec {codec:?}; NVENC handles \"h264\", \"h265\" or \"av1\""
            ));
            std::process::exit(2);
        }
    };
    let yuv444 = match crate::nvenc_cuda::resolve_pixel_format(nvenc_codec, color) {
        Ok(format) => format.nvfbc_capture_is_yuv444(),
        Err(rejection) => {
            log_error(&rejection.to_string());
            std::process::exit(2);
        }
    };
    let capture_backend = linux_capture_backend(color.bit_depth);
    if capture_backend == LinuxCaptureBackend::XShm && cursor_mode.include_cursor() {
        log_error("Linux depth-30 XShm capture cannot include the host cursor; use cursor=local");
        std::process::exit(2);
    }
    let framed = crate::framed_output_from_args(&args);
    let admission_probe = match crate::admission_probe::options_from_args(&args) {
        Ok(options) => options,
        Err(error) => {
            log_error(&error);
            std::process::exit(2);
        }
    };
    let selftest_index = args.iter().position(|value| value == "selftest");
    let selftest = selftest_index.is_some();
    let (st_w, st_h) = selftest_index
        .and_then(|index| args.get(index + 1))
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((3840u32, 2160u32));

    unsafe {
        let cuda_ctx = match cuda::init_from_env() {
            Ok(ctx) => ctx,
            Err(error) => {
                fallback_or_exit(args, requested_encoder, error);
            }
        };

        if let Some(options) = admission_probe.as_ref() {
            let code = match capture_backend {
                LinuxCaptureBackend::NvFbc => {
                    let capture = match nvfbc::Capture::new(output_index, yuv444, cursor_mode) {
                        Ok(capture) => capture,
                        Err(error) => {
                            drop(cuda_ctx);
                            probe_startup_exit(error);
                        }
                    };
                    run_admission_probe(
                        cuda_ctx,
                        capture,
                        &codec,
                        color,
                        intent,
                        qp_map_policy,
                        yuv444,
                        options,
                    )
                }
                LinuxCaptureBackend::XShm => {
                    let capture = match crate::linux_x11::X11Capture::connect_wide(output_index) {
                        Ok(capture) => capture,
                        Err(error) => {
                            drop(cuda_ctx);
                            log_unavailable(BackendUnavailableReason::UnsupportedDisplay, &error);
                            std::process::exit(2);
                        }
                    };
                    run_wide_admission_probe(
                        cuda_ctx,
                        capture,
                        &codec,
                        color,
                        intent,
                        qp_map_policy,
                        options,
                    )
                }
            };
            std::process::exit(code);
        }

        if selftest {
            let code = run_selftest(
                cuda_ctx,
                &codec,
                st_w,
                st_h,
                color,
                intent,
                qp_map_policy,
                yuv444,
                framed,
            );
            std::process::exit(code);
        }

        let options = EncodeOptions {
            codec: &codec,
            fps,
            yuv444,
            framed,
            cursor_mode,
            color,
        };
        let code = match capture_backend {
            LinuxCaptureBackend::NvFbc => {
                let capture = match nvfbc::Capture::new(output_index, yuv444, cursor_mode) {
                    Ok(capture) => capture,
                    Err(error) => {
                        drop(cuda_ctx);
                        fallback_or_exit(args, requested_encoder, error);
                    }
                };
                let encoder = match crate::nvenc_cuda::Encoder::new(
                    cuda_ctx.as_raw(),
                    capture.width,
                    capture.height,
                    &codec,
                    color,
                    intent,
                    qp_map_policy,
                ) {
                    Ok(mut encoder) => {
                        if qp_map_policy.submits_map() {
                            let engaged = encoder.enable_qp_map(
                                qp_map_policy,
                                arcen_media::video::QpBias::default(),
                                arcen_media::VideoCodec::from_token(&codec)
                                    .unwrap_or(arcen_media::VideoCodec::H264),
                            );
                            crate::log(&format!(
                                "QP map policy={} engaged={engaged}",
                                qp_map_policy.token()
                            ));
                        }
                        encoder
                    }
                    Err(error) => {
                        drop(capture);
                        drop(cuda_ctx);
                        fallback_or_exit(args, requested_encoder, error);
                    }
                };
                run_encode(cuda_ctx, capture, encoder, options)
            }
            LinuxCaptureBackend::XShm => {
                let capture = match crate::linux_x11::X11Capture::connect_wide(output_index) {
                    Ok(capture) => capture,
                    Err(error) => {
                        drop(cuda_ctx);
                        log_unavailable(BackendUnavailableReason::UnsupportedDisplay, &error);
                        std::process::exit(2);
                    }
                };
                let encoder = match crate::nvenc_cuda::Encoder::new(
                    cuda_ctx.as_raw(),
                    capture.width(),
                    capture.height(),
                    &codec,
                    color,
                    intent,
                    qp_map_policy,
                ) {
                    Ok(mut encoder) => {
                        if qp_map_policy.submits_map() {
                            let engaged = encoder.enable_qp_map(
                                qp_map_policy,
                                arcen_media::video::QpBias::default(),
                                arcen_media::VideoCodec::from_token(&codec)
                                    .unwrap_or(arcen_media::VideoCodec::H264),
                            );
                            crate::log(&format!(
                                "wide QP map policy={} engaged={engaged}",
                                qp_map_policy.token()
                            ));
                        }
                        encoder
                    }
                    Err(error) => {
                        drop(capture);
                        drop(cuda_ctx);
                        match error {
                            NativeStartupError::Unavailable { reason, detail } => {
                                log_unavailable(reason, &detail);
                                std::process::exit(2);
                            }
                            NativeStartupError::Fatal(detail) => {
                                log_error(&format!(
                                    "wide native NVENC startup failed closed: {detail}"
                                ));
                                std::process::exit(5);
                            }
                        }
                    }
                };
                run_wide_encode(cuda_ctx, capture, encoder, options)
            }
        };
        std::process::exit(code);
    }
}

pub(crate) fn probe_with_args(args: Vec<String>) -> ! {
    let cursor_mode = match crate::cursor_mode_from_args(&args) {
        Ok(mode) => mode,
        Err(error) => {
            log_error(error);
            std::process::exit(2);
        }
    };
    let output_index = args
        .get(1)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let codec = args.get(2).map_or("h264", String::as_str);
    let yuv444_token = args.iter().any(|value| value == "yuv444");
    // See `run_with_args`'s identical resolution: the single colour contract
    // this probe must actually attempt, and the capture/encoder-consistent
    // `yuv444` flag derived from it, never two independently-derived values.
    let color = match crate::requested_color(&args, yuv444_token) {
        Ok(color) => color,
        Err(error) => {
            log_error(&error);
            std::process::exit(2);
        }
    };
    let intent = match crate::requested_intent(&args) {
        Ok(intent) => intent,
        Err(error) => {
            log_error(&format!("invalid intent: {error}"));
            std::process::exit(2);
        }
    };
    let qp_map_policy = match crate::requested_qp_map(&args) {
        Ok(policy) => policy,
        Err(error) => {
            log_error(&format!("invalid qp-map: {error}"));
            std::process::exit(2);
        }
    };
    // See `run_with_args`'s identical `NvencCodec::parse` resolution: an
    // unrecognised codec token is a named, typed failure rather than
    // silently being treated as H.264.
    let nvenc_codec = match crate::nvenc_cuda::NvencCodec::parse(codec) {
        Some(nvenc_codec) => nvenc_codec,
        None => {
            log_error(&format!(
                "unrecognized codec {codec:?}; NVENC handles \"h264\", \"h265\" or \"av1\""
            ));
            std::process::exit(2);
        }
    };
    let yuv444 = match crate::nvenc_cuda::resolve_pixel_format(nvenc_codec, color) {
        Ok(format) => format.nvfbc_capture_is_yuv444(),
        Err(rejection) => {
            log_error(&rejection.to_string());
            std::process::exit(2);
        }
    };
    let capture_backend = linux_capture_backend(color.bit_depth);
    if capture_backend == LinuxCaptureBackend::XShm && cursor_mode.include_cursor() {
        log_error("Linux depth-30 XShm capture cannot include the host cursor; use cursor=local");
        std::process::exit(2);
    }

    // SAFETY: the backend wrappers validate all dynamically loaded entry points,
    // own every returned handle, and release them in reverse dependency order.
    // The probe does not grab a frame, connect to X11, or mutate display state.
    unsafe {
        let cuda_context = match cuda::init_from_env() {
            Ok(context) => context,
            Err(error) => {
                probe_startup_exit(error);
            }
        };
        match capture_backend {
            LinuxCaptureBackend::NvFbc => {
                let capture = match nvfbc::Capture::new(output_index, yuv444, cursor_mode) {
                    Ok(capture) => capture,
                    Err(error) => {
                        drop(cuda_context);
                        probe_startup_exit(error);
                    }
                };
                let encoder = match crate::nvenc_cuda::Encoder::new(
                    cuda_context.as_raw(),
                    capture.width,
                    capture.height,
                    codec,
                    color,
                    intent,
                    qp_map_policy,
                ) {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        drop(capture);
                        drop(cuda_context);
                        probe_startup_exit(error);
                    }
                };
                drop(encoder);
                drop(capture);
            }
            LinuxCaptureBackend::XShm => {
                let mut capture = match crate::linux_x11::X11Capture::connect_wide(output_index) {
                    Ok(capture) => capture,
                    Err(error) => {
                        drop(cuda_context);
                        log_unavailable(BackendUnavailableReason::UnsupportedDisplay, &error);
                        std::process::exit(2);
                    }
                };
                let mut encoder = match crate::nvenc_cuda::Encoder::new(
                    cuda_context.as_raw(),
                    capture.width(),
                    capture.height(),
                    codec,
                    color,
                    intent,
                    qp_map_policy,
                ) {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        drop(capture);
                        drop(cuda_context);
                        probe_startup_exit(error);
                    }
                };
                let frame = match capture.capture_wide() {
                    Ok(frame) => frame,
                    Err(error) => {
                        drop(encoder);
                        drop(capture);
                        drop(cuda_context);
                        log_unavailable(BackendUnavailableReason::UnsupportedDisplay, &error);
                        std::process::exit(2);
                    }
                };
                if let Err(error) = encoder.stage_wide_host(frame.bytes, frame.stride, frame.layout)
                {
                    drop(frame);
                    drop(encoder);
                    drop(capture);
                    drop(cuda_context);
                    log_error(&format!("wide native probe staging failed: {error}"));
                    std::process::exit(5);
                }
                drop(frame);
                drop(encoder);
                drop(capture);
            }
        }
        drop(cuda_context);
        log(&format!(
            "native probe capture backend={}",
            match capture_backend {
                LinuxCaptureBackend::NvFbc => "nvfbc",
                LinuxCaptureBackend::XShm => "xshm",
            }
        ));
        log("PROBE version=1 backend=native-nvenc available=true");
    }
    std::process::exit(0);
}

fn fallback_or_exit(
    args: Vec<String>,
    requested: RequestedEncoder,
    error: NativeStartupError,
) -> ! {
    #[cfg(not(feature = "software-h264"))]
    let _ = &args;
    match error {
        NativeStartupError::Unavailable { reason, detail } => {
            if requested == RequestedEncoder::Auto {
                #[cfg(feature = "software-h264")]
                {
                    log(&format!(
                        "native NVENC probe unavailable: {detail}; trying OpenH264"
                    ));
                    let intent = match crate::requested_intent(&args) {
                        Ok(intent) => intent,
                        Err(error) => {
                            log_error(&format!("invalid intent: {error}"));
                            std::process::exit(2);
                        }
                    };
                    let qp_map_policy = match crate::requested_qp_map(&args) {
                        Ok(policy) => policy,
                        Err(error) => {
                            log_error(&format!("invalid qp-map: {error}"));
                            std::process::exit(2);
                        }
                    };
                    if !crate::linux_software_policy_supported(intent, qp_map_policy) {
                        log_error(
                            "software-h264 supports only intent=interactive and qp-map=off; \
                             quality and QP delta maps require native NVENC",
                        );
                        std::process::exit(2);
                    }
                    crate::linux_x11::run_with_args(args);
                }
            }
            log_unavailable(reason, &detail);
            std::process::exit(2);
        }
        NativeStartupError::Fatal(detail) => {
            log_error(&format!("native NVENC startup failed closed: {detail}"));
            std::process::exit(5);
        }
    }
}

fn probe_startup_exit(error: NativeStartupError) -> ! {
    match error {
        NativeStartupError::Unavailable { reason, detail } => {
            log_unavailable(reason, &detail);
            std::process::exit(2);
        }
        NativeStartupError::Fatal(detail) => {
            log_error(&format!("native NVENC probe failed closed: {detail}"));
            std::process::exit(5);
        }
    }
}

/// `capenc probe-matrix [--output <path>]` on Linux: mirrors `win.rs`'s
/// `run_probe_matrix_subcommand` (see that function's doc and
/// `crate::probe_matrix`'s module doc for the full output contract and the
/// reason a failing row is a finding, not an error). Only the NVENC (CUDA)
/// backend is attempted here — there is no Media Foundation equivalent on
/// Linux, and the `software-h264` (OpenH264) backend is 8-bit 4:2:0 only,
/// exactly like Windows' MF backend, so it would report an identical
/// supported/unsupported verdict for every row; wiring it in alongside NVENC
/// is a reasonable follow-up, not duplicated here.
///
/// UNVERIFIED AT RUNTIME: written and only ever compiled on a Windows
/// machine with no CUDA/NvFBC runtime and no Linux machine available to
/// build or run the resulting binary. It is checked with `cargo check
/// --target x86_64-unknown-linux-gnu -p arcen-capenc --features nvenc`,
/// which does type-check cleanly against this exact source tree — that is
/// compile-time correctness, not a runtime proof, and the trial itself
/// (`nvenc_cuda::Encoder::new`) has never actually run.
#[cfg(feature = "nvenc")]
pub(crate) fn probe_matrix_with_args(args: &[String]) -> i32 {
    let host = crate::probe_matrix::HostInfo {
        os: std::env::consts::OS.to_string(),
        // No cheap, dependency-free GPU-name query is wired into this
        // module (see `win.rs`'s `first_adapter_description` for the DXGI
        // equivalent) -- left for a human to fill in, like
        // `driver_version`/`nvenc_generation` already are.
        gpu: String::new(),
        driver_version: String::new(),
        nvenc_generation: String::new(),
    };
    let environment = crate::probe_matrix::EnvironmentInfo::new(host);

    // SAFETY: `cuda::init_from_env` validates every dynamically loaded entry
    // point and owns the returned context; it touches no NvFBC, X11, or
    // display state at all, matching `probe_with_args`'s own safety comment.
    let cuda_context = unsafe { cuda::init_from_env() };
    if let Err(error) = &cuda_context {
        log(&format!(
            "probe-matrix: CUDA unavailable for NVENC trials: {error}"
        ));
    }

    let report = crate::probe_matrix::build_report(environment, |row| {
        vec![("NVENC", nvenc_attempt_for_row(&cuda_context, row))]
    });

    let json = report.render();
    let exit_code = match crate::probe_matrix::output_path_from_args(args) {
        Some(path) => match std::fs::write(&path, &json) {
            Ok(()) => {
                println!("probe-matrix: wrote {}", path.display());
                0
            }
            Err(error) => {
                log(&format!(
                    "probe-matrix: failed to write {}: {error}",
                    path.display()
                ));
                2
            }
        },
        None => {
            println!("{json}");
            0
        }
    };
    drop(cuda_context);
    exit_code
}

/// One row's real NVENC (CUDA) trial. Classified the same way `win.rs`'s
/// `nvenc_attempt_for_row` classifies its D3D11 counterpart:
/// `BackendUnavailableReason::UnsupportedConfiguration` is a typed refusal
/// (`Unsupported`); anything else `cuda::init_from_env`/`Encoder::new`
/// reported is a real failure (`Failed`). Now reached for 4:2:0 AV1 rows too
/// -- `crate::probe_matrix::probe_one_row` routes those into the same
/// `attempt_backends` sweep as H.264/HEVC, since NVENC's AV1 Main profile is
/// 4:2:0 (Ada onward); only 4:4:4 AV1 rows are routed to rav1e instead and
/// never reach this function. `nvenc_cuda::Encoder::new`'s codec-GUID
/// selection (`NvencCodec`) recognises `"av1"` as its own codec rather than
/// defaulting an unrecognised token to H.264's GUID, which is what makes
/// that routing safe.
#[cfg(feature = "nvenc")]
fn nvenc_attempt_for_row(
    cuda_context: &Result<cuda::Context, NativeStartupError>,
    row: arcen_media::video::VideoVariant,
) -> crate::probe_matrix::EncoderAttemptOutcome {
    use crate::probe_matrix::EncoderAttemptOutcome;

    let context = match cuda_context {
        Ok(context) => context,
        Err(error) => {
            return EncoderAttemptOutcome::Failed {
                detail: format!("no CUDA context available for a trial: {error}"),
            };
        }
    };
    let codec_token = row.video.codec.token();
    // The exact same `crate::requested_color` every real Linux entry point
    // now calls (see `run_with_args`/`probe_with_args`) — not
    // `ColorSpec::from_variant(row)` directly — via a synthetic single-token
    // argv, so a bug in that shared resolution function shows up as a
    // probe-matrix finding too. `row.id()` always round-trips (enforced by
    // `arcen_media::video::variant`'s own tests), so this cannot fail.
    let color = crate::requested_color(&[format!("variant={}", row.id())], false)
        .expect("a PROBE_MATRIX row's own id always round-trips");
    // Fixed, modest probe geometry -- see `win.rs`'s `PROBE_WIDTH`/
    // `PROBE_HEIGHT` doc: the matrix probes whether a combination
    // initialises, not performance at a particular resolution.
    match unsafe {
        crate::nvenc_cuda::Encoder::new(
            context.as_raw(),
            1920,
            1080,
            codec_token,
            color,
            arcen_media::EncodeIntent::Interactive,
            crate::qp_map::QpMapPolicy::Off,
        )
    } {
        Ok(encoder) => {
            drop(encoder);
            EncoderAttemptOutcome::Ok {
                sustained_fps: None,
                bitrate_mbps: None,
                note: "NVENC (CUDA) initialised; no measurement burst attempted on this \
                       platform (see win.rs's run_probe_burst for the Windows equivalent)"
                    .to_string(),
            }
        }
        Err(error) => {
            let detail = error.to_string();
            match &error {
                NativeStartupError::Unavailable {
                    reason: BackendUnavailableReason::UnsupportedConfiguration,
                    ..
                } => EncoderAttemptOutcome::Unsupported { detail },
                _ => EncoderAttemptOutcome::Failed { detail },
            }
        }
    }
}
