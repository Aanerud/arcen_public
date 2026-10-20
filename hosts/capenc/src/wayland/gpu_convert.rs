//! The `gpu-convert` DMA-BUF route: EGL image import, a GL blit into a
//! CUDA-registered texture, and a CUDA copy into the encoder's retained frame.
//!
//! ```text
//! producer DMA-BUF (XR24/AR24, NVIDIA block-linear modifier)
//!   -> wait: explicit-sync acquire point, else the buffer's implicit fence
//!   -> eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT) imported as XB24/AB24
//!   -> GL_TEXTURE_2D (glEGLImageTargetTexture2DOES) on a read framebuffer
//!   -> glBlitFramebuffer into a GL_RGBA8 texture registered with CUDA
//!   -> GL fence wait: the producer's buffer is no longer read
//!   -> cuGraphicsMapResources -> cuMemcpy2D (array -> device) -> unmap
//!   -> retained BGRA frame -> NVENC (ARGB input) as on the host-copy route
//! ```
//!
//! Why this shape (measured on NVIDIA 580 with `nvidia-drm modeset=1`,
//! `docs/architecture/wayland-qualification/rocky10-gnome49-nvidia-grid-nodrm.md`):
//! `cuGraphicsEGLRegisterImage` refuses block-linear EGL images
//! (`CUDA_ERROR_INVALID_VALUE`), and so does `cuGraphicsGLRegisterImage` on a
//! texture backed by the image; a GL blit from it into an ordinary texture
//! that CUDA registered reads the buffer correctly.
//!
//! The channel order is resolved by the import itself: the producer's
//! `XRGB8888` bytes (B, G, R, X) are imported as `XBGR8888`, so GL reads byte
//! 0 as red; the blit stores the texture's red first, and the CUDA copy of
//! the `GL_RGBA8` array therefore yields B, G, R, X again, the BGRA input
//! NVENC takes. Both formats share one 32-bit layout and the modifier
//! describes only tiling, so the reinterpretation copies bits, never values.
//!
//! Truthful classification: this is `gpu-convert`, never zero-copy. There are
//! two GPU copies (the blit and the array copy) and no CPU read-back.
//!
//! EGL, GL (through `eglGetProcAddress`) and CUDA are `dlopen`ed like the rest
//! of capenc's NVIDIA entry points; nothing here links at build time. The
//! libraries are deliberately never `dlclose`d: vendor EGL/GL libraries
//! install thread-exit and process-exit hooks that must not outlive their
//! code.

use std::ffi::{c_char, c_void, CStr, CString};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};

use arcen_media::video::{CapturePixelFormat, CaptureSurface, FormatModifier};

use super::dmabuf::{
    drm_fourcc, egl_import_attributes_as, swizzled_import_format, wait_for_producer, ProducerSync,
};
use super::explicit_sync::{SyncError, SyncobjDevice};
use crate::linux::cuda::{self, CUdeviceptr};
use crate::linux::dl;

type EglBoolean = u32;
type EglDisplay = *mut c_void;
type EglDeviceExt = *mut c_void;
type EglContext = *mut c_void;
type EglImage = *mut c_void;
type GlSync = *mut c_void;

const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_VENDOR: i32 = 0x3053;
const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
const EGL_DRM_RENDER_NODE_FILE_EXT: i32 = 0x3377;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_NONE: i32 = 0x3038;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA8: i32 = 0x8058;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_NEAREST: i32 = 0x2600;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_SYNC_GPU_COMMANDS_COMPLETE: u32 = 0x9117;
const GL_SYNC_FLUSH_COMMANDS_BIT: u32 = 0x1;
const GL_ALREADY_SIGNALED: u32 = 0x911A;
const GL_CONDITION_SATISFIED: u32 = 0x911C;
const GL_RENDERER: u32 = 0x1F01;
const GL_VERSION: u32 = 0x1F02;
const GL_NO_ERROR: u32 = 0;

type CuResult = i32;
type CuGraphicsResource = *mut c_void;
type CuArray = *mut c_void;
const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: u32 = 1;
const CU_MEMORYTYPE_DEVICE: u32 = 2;
const CU_MEMORYTYPE_ARRAY: u32 = 3;

/// How long the import waits for the producer's acquire point or fence, and
/// for its own blit.
pub const SYNC_TIMEOUT: Duration = Duration::from_millis(100);

/// The colour the start-up self-test clears the CUDA-registered texture to,
/// as RGBA8 bytes (rounded as GL converts 0.25/0.5/0.75).
const SELF_TEST_RGBA: [u8; 4] = [0x40, 0x80, 0xBF, 0xFF];

/// `CUDA_MEMCPY2D`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CuMemcpy2DParams {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: CUdeviceptr,
    src_array: CuArray,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: CUdeviceptr,
    dst_array: CuArray,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

/// A modifier the EGL device can import for a format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportableModifier {
    /// Modifier.
    pub modifier: FormatModifier,
    /// Importable only as `GL_TEXTURE_EXTERNAL_OES`; the blit needs a
    /// `GL_TEXTURE_2D`, so such a modifier is never offered.
    pub external_only: bool,
}

/// Why the route is unavailable or an import failed. Every failure releases
/// what it created; the caller falls back explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuConvertError {
    /// `libEGL.so.1`, `libcuda.so.1` or an entry point is missing.
    Library(String),
    /// No EGL device drives the encoder's render node.
    NoMatchingDevice(String),
    /// EGL refused initialisation, an extension or the GL context.
    Display(String),
    /// GL refused an object or reported an error.
    Gl(String),
    /// The self-test read back the wrong colour through CUDA.
    SelfTest(String),
    /// The surface cannot be imported.
    Surface(String),
    /// The producer's acquire point or fence did not signal.
    Sync(String),
    /// `eglCreateImageKHR` failed.
    CreateImage(i32),
    /// A CUDA interop call failed.
    Cuda(&'static str, i32),
}

impl GpuConvertError {
    /// Stable token for logs and fallback reasons.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Library(_) => "library",
            Self::NoMatchingDevice(_) => "no_matching_egl_device",
            Self::Display(_) => "egl_display",
            Self::Gl(_) => "gl",
            Self::SelfTest(_) => "self_test",
            Self::Surface(_) => "surface",
            Self::Sync(_) => "producer_sync",
            Self::CreateImage(_) => "egl_create_image",
            Self::Cuda(..) => "cuda_interop",
        }
    }
}

impl std::fmt::Display for GpuConvertError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Library(detail) => write!(formatter, "EGL/GL/CUDA library: {detail}"),
            Self::NoMatchingDevice(detail) => write!(formatter, "no EGL device: {detail}"),
            Self::Display(detail) => write!(formatter, "EGL: {detail}"),
            Self::Gl(detail) => write!(formatter, "GL: {detail}"),
            Self::SelfTest(detail) => write!(formatter, "GL/CUDA self-test: {detail}"),
            Self::Surface(detail) => write!(formatter, "surface: {detail}"),
            Self::Sync(detail) => write!(formatter, "producer sync: {detail}"),
            Self::CreateImage(error) => write!(
                formatter,
                "eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT) failed (EGL error {error:#x})"
            ),
            Self::Cuda(call, status) => write!(formatter, "{call} -> CUDA status {status}"),
        }
    }
}

impl std::error::Error for GpuConvertError {}

/// How the producer's writes are known to be complete before the blit.
#[derive(Debug, Clone, Copy)]
pub enum AcquireSync<'sync> {
    /// Explicit sync: wait for `point` on the syncobj `fd`.
    Explicit {
        /// Device the syncobj is imported into.
        device: &'sync SyncobjDevice,
        /// Acquire syncobj descriptor (valid while the lease is held).
        fd: i32,
        /// Acquire point.
        point: u64,
    },
    /// Implicit sync: wait for the DMA-BUF's exported write fence.
    Implicit,
}

/// What one import cost.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpuImportReport {
    /// Waiting for the producer.
    pub sync_wait: Duration,
    /// Image import, blit and the wait for the blit (the producer's buffer
    /// is free once this ends).
    pub blit: Duration,
    /// CUDA map, array copy and unmap.
    pub cuda_copy: Duration,
}

type EglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type EglInitialize = unsafe extern "C" fn(EglDisplay, *mut i32, *mut i32) -> EglBoolean;
type EglTerminate = unsafe extern "C" fn(EglDisplay) -> EglBoolean;
type EglQueryString = unsafe extern "C" fn(EglDisplay, i32) -> *const c_char;
type EglQueryDevices = unsafe extern "C" fn(i32, *mut EglDeviceExt, *mut i32) -> EglBoolean;
type EglQueryDeviceString = unsafe extern "C" fn(EglDeviceExt, i32) -> *const c_char;
type EglGetPlatformDisplay = unsafe extern "C" fn(u32, *mut c_void, *const i32) -> EglDisplay;
type EglQueryDmaBufModifiers =
    unsafe extern "C" fn(EglDisplay, i32, i32, *mut u64, *mut EglBoolean, *mut i32) -> EglBoolean;
type EglCreateImage =
    unsafe extern "C" fn(EglDisplay, *mut c_void, u32, *mut c_void, *const i32) -> EglImage;
type EglDestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean;
type EglBindApi = unsafe extern "C" fn(u32) -> EglBoolean;
type EglCreateContext =
    unsafe extern "C" fn(EglDisplay, *mut c_void, EglContext, *const i32) -> EglContext;
type EglDestroyContext = unsafe extern "C" fn(EglDisplay, EglContext) -> EglBoolean;
type EglMakeCurrent =
    unsafe extern "C" fn(EglDisplay, *mut c_void, *mut c_void, EglContext) -> EglBoolean;
type EglGetError = unsafe extern "C" fn() -> i32;

struct EglFns {
    terminate: EglTerminate,
    create_image: EglCreateImage,
    destroy_image: EglDestroyImage,
    query_modifiers: EglQueryDmaBufModifiers,
    destroy_context: EglDestroyContext,
    make_current: EglMakeCurrent,
    get_error: EglGetError,
}

struct GlFns {
    gen_textures: unsafe extern "C" fn(i32, *mut u32),
    delete_textures: unsafe extern "C" fn(i32, *const u32),
    bind_texture: unsafe extern "C" fn(u32, u32),
    tex_parameteri: unsafe extern "C" fn(u32, u32, i32),
    tex_image_2d: unsafe extern "C" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    egl_image_target_texture_2d: unsafe extern "C" fn(u32, EglImage),
    gen_framebuffers: unsafe extern "C" fn(i32, *mut u32),
    delete_framebuffers: unsafe extern "C" fn(i32, *const u32),
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
    framebuffer_texture_2d: unsafe extern "C" fn(u32, u32, u32, u32, i32),
    check_framebuffer_status: unsafe extern "C" fn(u32) -> u32,
    blit_framebuffer: unsafe extern "C" fn(i32, i32, i32, i32, i32, i32, i32, i32, u32, u32),
    fence_sync: unsafe extern "C" fn(u32, u32) -> GlSync,
    client_wait_sync: unsafe extern "C" fn(GlSync, u32, u64) -> u32,
    delete_sync: unsafe extern "C" fn(GlSync),
    get_error: unsafe extern "C" fn() -> u32,
    get_string: unsafe extern "C" fn(u32) -> *const u8,
    clear_color: unsafe extern "C" fn(f32, f32, f32, f32),
    clear: unsafe extern "C" fn(u32),
    finish: unsafe extern "C" fn(),
}

struct CudaFns {
    register_image: unsafe extern "C" fn(*mut CuGraphicsResource, u32, u32, u32) -> CuResult,
    unregister: unsafe extern "C" fn(CuGraphicsResource) -> CuResult,
    map: unsafe extern "C" fn(u32, *mut CuGraphicsResource, *mut c_void) -> CuResult,
    unmap: unsafe extern "C" fn(u32, *mut CuGraphicsResource, *mut c_void) -> CuResult,
    mapped_array: unsafe extern "C" fn(*mut CuArray, CuGraphicsResource, u32, u32) -> CuResult,
    memcpy_2d: unsafe extern "C" fn(*const CuMemcpy2DParams) -> CuResult,
}

/// Loads a symbol as a function pointer of type `T`.
///
/// # Safety
///
/// `T` must be the correct `extern "C"` function pointer type for `name`.
unsafe fn symbol<T: Copy>(lib: *mut c_void, name: &str) -> Result<T, GpuConvertError> {
    let pointer = dl::sym(lib, name).map_err(GpuConvertError::Library)?;
    Ok(std::mem::transmute_copy(&pointer))
}

/// Loads an EGL or GL entry point of type `T` through `eglGetProcAddress`.
///
/// # Safety
///
/// As for [`symbol`].
unsafe fn proc_address<T: Copy>(get: EglGetProcAddress, name: &str) -> Result<T, GpuConvertError> {
    let cname = CString::new(name).map_err(|_| GpuConvertError::Library(name.to_string()))?;
    let pointer = get(cname.as_ptr());
    if pointer.is_null() {
        return Err(GpuConvertError::Library(format!("{name} unavailable")));
    }
    Ok(std::mem::transmute_copy(&pointer))
}

/// The client API the context was created for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientApi {
    /// Desktop OpenGL.
    OpenGl,
    /// OpenGL ES 3.
    OpenGlEs3,
}

impl ClientApi {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::OpenGl => "gl",
            Self::OpenGlEs3 => "gles3",
        }
    }
}

/// The EGL device display on the encoder's GPU, a surfaceless GL context
/// current on the import thread, and the CUDA-registered target texture.
///
/// Not `Send`: the GL and CUDA contexts are current on the thread that
/// opened it, and every call must come from that thread.
pub struct GpuConvertImporter {
    display: EglDisplay,
    context: EglContext,
    egl: EglFns,
    gl: GlFns,
    cuda: CudaFns,
    render_node: PathBuf,
    api: ClientApi,
    renderer: String,
    width: u32,
    height: u32,
    target: u32,
    draw_fbo: u32,
    read_fbo: u32,
    resource: CuGraphicsResource,
}

impl GpuConvertImporter {
    /// Opens the route on the GPU whose DRM render node is `render_node`
    /// for `width`x`height` frames, and proves the GL-to-CUDA half with a
    /// self-test (a cleared texture read back through CUDA).
    ///
    /// # Errors
    ///
    /// [`GpuConvertError`]; nothing is left current or allocated.
    ///
    /// # Safety
    ///
    /// Must run on the thread whose CUDA context is current (the encoder's),
    /// and the importer must only be used and dropped on that thread, before
    /// that context is destroyed. No other GL context may be current there.
    pub unsafe fn open(
        render_node: &Path,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuConvertError> {
        let egl_lib = dl::open("libEGL.so.1").map_err(GpuConvertError::Library)?;
        let cuda_lib = dl::open("libcuda.so.1").map_err(GpuConvertError::Library)?;
        let mut last = None;
        for api in [ClientApi::OpenGl, ClientApi::OpenGlEs3] {
            match Self::open_with(egl_lib, cuda_lib, render_node, width, height, api) {
                Ok(importer) => return Ok(importer),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| GpuConvertError::Display("no client API".into())))
    }

    #[allow(clippy::too_many_lines)] // One linear bring-up with paired teardown.
    unsafe fn open_with(
        egl_lib: *mut c_void,
        cuda_lib: *mut c_void,
        render_node: &Path,
        width: u32,
        height: u32,
        api: ClientApi,
    ) -> Result<Self, GpuConvertError> {
        let get: EglGetProcAddress = symbol(egl_lib, "eglGetProcAddress")?;
        let initialize: EglInitialize = symbol(egl_lib, "eglInitialize")?;
        let query_string: EglQueryString = symbol(egl_lib, "eglQueryString")?;
        let bind_api: EglBindApi = symbol(egl_lib, "eglBindAPI")?;
        let create_context: EglCreateContext = symbol(egl_lib, "eglCreateContext")?;
        let query_devices: EglQueryDevices = proc_address(get, "eglQueryDevicesEXT")?;
        let device_string: EglQueryDeviceString = proc_address(get, "eglQueryDeviceStringEXT")?;
        let platform_display: EglGetPlatformDisplay =
            proc_address(get, "eglGetPlatformDisplayEXT")?;
        let egl = EglFns {
            terminate: symbol(egl_lib, "eglTerminate")?,
            create_image: proc_address(get, "eglCreateImageKHR")?,
            destroy_image: proc_address(get, "eglDestroyImageKHR")?,
            query_modifiers: proc_address(get, "eglQueryDmaBufModifiersEXT")?,
            destroy_context: symbol(egl_lib, "eglDestroyContext")?,
            make_current: symbol(egl_lib, "eglMakeCurrent")?,
            get_error: symbol(egl_lib, "eglGetError")?,
        };
        let cuda = CudaFns {
            register_image: symbol(cuda_lib, "cuGraphicsGLRegisterImage")?,
            unregister: symbol(cuda_lib, "cuGraphicsUnregisterResource")?,
            map: symbol(cuda_lib, "cuGraphicsMapResources")?,
            unmap: symbol(cuda_lib, "cuGraphicsUnmapResources")?,
            mapped_array: symbol(cuda_lib, "cuGraphicsSubResourceGetMappedArray")?,
            memcpy_2d: symbol(cuda_lib, "cuMemcpy2D_v2")?,
        };

        let mut devices = [ptr::null_mut(); 16];
        let mut count = 0i32;
        if query_devices(16, devices.as_mut_ptr(), &raw mut count) == 0 {
            return Err(GpuConvertError::NoMatchingDevice(
                "eglQueryDevicesEXT failed".into(),
            ));
        }
        let wanted = render_node.as_os_str().as_encoded_bytes();
        let device = devices
            .iter()
            .take(usize::try_from(count).unwrap_or(0))
            .copied()
            .find(|device| {
                let name = device_string(*device, EGL_DRM_RENDER_NODE_FILE_EXT);
                // SAFETY (closure): EGL returns a NUL-terminated string owned
                // by the implementation, or null.
                !name.is_null() && CStr::from_ptr(name).to_bytes() == wanted
            })
            .ok_or_else(|| {
                GpuConvertError::NoMatchingDevice(format!(
                    "{count} EGL devices, none with render node {}",
                    render_node.display()
                ))
            })?;
        let no_attributes = [EGL_NONE];
        let display = platform_display(EGL_PLATFORM_DEVICE_EXT, device, no_attributes.as_ptr());
        if display.is_null() {
            return Err(GpuConvertError::Display(
                "eglGetPlatformDisplayEXT failed".into(),
            ));
        }
        let (mut major, mut minor) = (0, 0);
        if initialize(display, &raw mut major, &raw mut minor) == 0 {
            return Err(GpuConvertError::Display("eglInitialize failed".into()));
        }
        let text = |value: *const c_char| {
            if value.is_null() {
                String::new()
            } else {
                CStr::from_ptr(value).to_string_lossy().into_owned()
            }
        };
        let extensions = text(query_string(display, EGL_EXTENSIONS));
        let vendor = text(query_string(display, EGL_VENDOR));
        let missing: Vec<&str> = [
            "EGL_EXT_image_dma_buf_import",
            "EGL_EXT_image_dma_buf_import_modifiers",
            "EGL_KHR_surfaceless_context",
            "EGL_KHR_no_config_context",
        ]
        .into_iter()
        .filter(|required| {
            !extensions
                .split_ascii_whitespace()
                .any(|name| name == *required)
        })
        .collect();
        if !missing.is_empty() {
            (egl.terminate)(display);
            return Err(GpuConvertError::Display(format!(
                "EGL {major}.{minor} ({vendor}) lacks {missing:?}"
            )));
        }
        let (api_enum, context_attributes) = match api {
            ClientApi::OpenGl => (EGL_OPENGL_API, vec![EGL_NONE]),
            ClientApi::OpenGlEs3 => (
                EGL_OPENGL_ES_API,
                vec![EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE],
            ),
        };
        if bind_api(api_enum) == 0 {
            (egl.terminate)(display);
            return Err(GpuConvertError::Display(format!(
                "eglBindAPI({}) refused",
                api.token()
            )));
        }
        // EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT: a surfaceless context.
        let context = create_context(
            display,
            ptr::null_mut(),
            ptr::null_mut(),
            context_attributes.as_ptr(),
        );
        if context.is_null() {
            let error = (egl.get_error)();
            (egl.terminate)(display);
            return Err(GpuConvertError::Display(format!(
                "eglCreateContext({}) failed (EGL error {error:#x})",
                api.token()
            )));
        }
        if (egl.make_current)(display, ptr::null_mut(), ptr::null_mut(), context) == 0 {
            let error = (egl.get_error)();
            (egl.destroy_context)(display, context);
            (egl.terminate)(display);
            return Err(GpuConvertError::Display(format!(
                "eglMakeCurrent(surfaceless) failed (EGL error {error:#x})"
            )));
        }
        // From here on, `importer`'s Drop releases whatever was created.
        let gl = match Self::load_gl(get) {
            Ok(gl) => gl,
            Err(error) => {
                (egl.make_current)(display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
                (egl.destroy_context)(display, context);
                (egl.terminate)(display);
                return Err(error);
            }
        };
        let renderer = {
            let read = |name| {
                let value = (gl.get_string)(name);
                if value.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(value.cast()).to_string_lossy().into_owned()
                }
            };
            format!("{} / {}", read(GL_RENDERER), read(GL_VERSION))
        };
        let mut importer = Self {
            display,
            context,
            egl,
            gl,
            cuda,
            render_node: render_node.to_path_buf(),
            api,
            renderer,
            width,
            height,
            target: 0,
            draw_fbo: 0,
            read_fbo: 0,
            resource: ptr::null_mut(),
        };
        importer.create_target()?;
        importer.self_test()?;
        Ok(importer)
    }

    unsafe fn load_gl(get: EglGetProcAddress) -> Result<GlFns, GpuConvertError> {
        Ok(GlFns {
            gen_textures: proc_address(get, "glGenTextures")?,
            delete_textures: proc_address(get, "glDeleteTextures")?,
            bind_texture: proc_address(get, "glBindTexture")?,
            tex_parameteri: proc_address(get, "glTexParameteri")?,
            tex_image_2d: proc_address(get, "glTexImage2D")?,
            egl_image_target_texture_2d: proc_address(get, "glEGLImageTargetTexture2DOES")?,
            gen_framebuffers: proc_address(get, "glGenFramebuffers")?,
            delete_framebuffers: proc_address(get, "glDeleteFramebuffers")?,
            bind_framebuffer: proc_address(get, "glBindFramebuffer")?,
            framebuffer_texture_2d: proc_address(get, "glFramebufferTexture2D")?,
            check_framebuffer_status: proc_address(get, "glCheckFramebufferStatus")?,
            blit_framebuffer: proc_address(get, "glBlitFramebuffer")?,
            fence_sync: proc_address(get, "glFenceSync")?,
            client_wait_sync: proc_address(get, "glClientWaitSync")?,
            delete_sync: proc_address(get, "glDeleteSync")?,
            get_error: proc_address(get, "glGetError")?,
            get_string: proc_address(get, "glGetString")?,
            clear_color: proc_address(get, "glClearColor")?,
            clear: proc_address(get, "glClear")?,
            finish: proc_address(get, "glFinish")?,
        })
    }

    unsafe fn gl_check(&self, what: &str) -> Result<(), GpuConvertError> {
        match (self.gl.get_error)() {
            GL_NO_ERROR => Ok(()),
            error => Err(GpuConvertError::Gl(format!("{what}: GL error {error:#x}"))),
        }
    }

    fn dimensions(&self) -> Result<(i32, i32), GpuConvertError> {
        let int = |value: u32| {
            i32::try_from(value)
                .map_err(|_| GpuConvertError::Surface(format!("dimension {value} too large")))
        };
        Ok((int(self.width)?, int(self.height)?))
    }

    /// The `GL_RGBA8` texture CUDA reads, its draw framebuffer, the read
    /// framebuffer each import attaches to, and the CUDA registration.
    unsafe fn create_target(&mut self) -> Result<(), GpuConvertError> {
        let (width, height) = self.dimensions()?;
        (self.gl.gen_textures)(1, &raw mut self.target);
        (self.gl.bind_texture)(GL_TEXTURE_2D, self.target);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        (self.gl.tex_image_2d)(
            GL_TEXTURE_2D,
            0,
            GL_RGBA8,
            width,
            height,
            0,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            ptr::null(),
        );
        (self.gl.bind_texture)(GL_TEXTURE_2D, 0);
        self.gl_check("target texture")?;
        (self.gl.gen_framebuffers)(1, &raw mut self.draw_fbo);
        (self.gl.gen_framebuffers)(1, &raw mut self.read_fbo);
        (self.gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, self.draw_fbo);
        (self.gl.framebuffer_texture_2d)(
            GL_DRAW_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_2D,
            self.target,
            0,
        );
        let status = (self.gl.check_framebuffer_status)(GL_DRAW_FRAMEBUFFER);
        if status != GL_FRAMEBUFFER_COMPLETE {
            return Err(GpuConvertError::Gl(format!(
                "target framebuffer incomplete ({status:#x})"
            )));
        }
        self.gl_check("target framebuffer")?;
        let status = (self.cuda.register_image)(
            &raw mut self.resource,
            self.target,
            GL_TEXTURE_2D,
            CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY,
        );
        if status != 0 {
            self.resource = ptr::null_mut();
            return Err(GpuConvertError::Cuda("cuGraphicsGLRegisterImage", status));
        }
        Ok(())
    }

    /// Clears the target, copies it through CUDA and checks the colour: the
    /// half of the route that does not depend on a producer buffer.
    unsafe fn self_test(&mut self) -> Result<(), GpuConvertError> {
        (self.gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, self.draw_fbo);
        (self.gl.clear_color)(0.25, 0.5, 0.75, 1.0);
        (self.gl.clear)(GL_COLOR_BUFFER_BIT);
        (self.gl.finish)();
        self.gl_check("self-test clear")?;
        let row_bytes = self.width as usize * 4;
        let rows = self.height as usize;
        let device = cuda::mem_alloc(row_bytes * rows)
            .map_err(|error| GpuConvertError::SelfTest(format!("allocation: {error}")))?;
        let copied = self.copy_target(device, row_bytes);
        let mut first_row = vec![0u8; row_bytes];
        let read = copied.and_then(|()| {
            cuda::memcpy_dtoh(first_row.as_mut_ptr().cast(), device, row_bytes)
                .map_err(GpuConvertError::SelfTest)
        });
        let _ = cuda::mem_free(device);
        read?;
        let near = |a: u8, b: u8| a.abs_diff(b) <= 1;
        let wrong = first_row
            .chunks_exact(4)
            .position(|pixel| !pixel.iter().zip(SELF_TEST_RGBA).all(|(a, b)| near(*a, b)));
        match wrong {
            None => Ok(()),
            Some(index) => Err(GpuConvertError::SelfTest(format!(
                "pixel {index} read {:02x?}, expected {SELF_TEST_RGBA:02x?}",
                &first_row[index * 4..index * 4 + 4]
            ))),
        }
    }

    /// Maps the target, copies it into `dst` (rows `dst_pitch` apart) and
    /// unmaps it. The copy is ordered on the null stream before any later
    /// CUDA work (the encoder's staging) and, through the unmap, before any
    /// later GL work on the target (the next blit).
    unsafe fn copy_target(
        &mut self,
        dst: CUdeviceptr,
        dst_pitch: usize,
    ) -> Result<(), GpuConvertError> {
        let status = (self.cuda.map)(1, &raw mut self.resource, ptr::null_mut());
        if status != 0 {
            return Err(GpuConvertError::Cuda("cuGraphicsMapResources", status));
        }
        let mut array: CuArray = ptr::null_mut();
        let status = (self.cuda.mapped_array)(&raw mut array, self.resource, 0, 0);
        let copied = if status == 0 {
            let copy = CuMemcpy2DParams {
                src_x_in_bytes: 0,
                src_y: 0,
                src_memory_type: CU_MEMORYTYPE_ARRAY,
                src_host: ptr::null(),
                src_device: 0,
                src_array: array,
                src_pitch: 0,
                dst_x_in_bytes: 0,
                dst_y: 0,
                dst_memory_type: CU_MEMORYTYPE_DEVICE,
                dst_host: ptr::null_mut(),
                dst_device: dst,
                dst_array: ptr::null_mut(),
                dst_pitch,
                width_in_bytes: self.width as usize * 4,
                height: self.height as usize,
            };
            match (self.cuda.memcpy_2d)(&raw const copy) {
                0 => Ok(()),
                status => Err(GpuConvertError::Cuda("cuMemcpy2D_v2 (GL array)", status)),
            }
        } else {
            Err(GpuConvertError::Cuda(
                "cuGraphicsSubResourceGetMappedArray",
                status,
            ))
        };
        let unmapped = (self.cuda.unmap)(1, &raw mut self.resource, ptr::null_mut());
        copied?;
        if unmapped != 0 {
            return Err(GpuConvertError::Cuda("cuGraphicsUnmapResources", unmapped));
        }
        Ok(())
    }

    /// The render node this importer drives.
    #[must_use]
    pub fn render_node(&self) -> &Path {
        &self.render_node
    }

    /// The client API and GL renderer string, for logs.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("api={} renderer={:?}", self.api.token(), self.renderer)
    }

    /// Modifiers EGL can import for `format` as `GL_TEXTURE_2D`, implicit
    /// excluded.
    #[must_use]
    pub fn modifiers(&self, format: CapturePixelFormat) -> Vec<ImportableModifier> {
        let fourcc = i32::from_ne_bytes(drm_fourcc(format).to_ne_bytes());
        let mut count = 0i32;
        // SAFETY: `display` is initialised; a zero-capacity query only
        // writes the count.
        let ok = unsafe {
            (self.egl.query_modifiers)(
                self.display,
                fourcc,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &raw mut count,
            )
        };
        let Ok(capacity) = usize::try_from(count) else {
            return Vec::new();
        };
        if ok == 0 || capacity == 0 {
            return Vec::new();
        }
        let mut modifiers = vec![0u64; capacity];
        let mut external = vec![0u32; capacity];
        // SAFETY: both arrays hold `count` elements, as the call requires.
        let ok = unsafe {
            (self.egl.query_modifiers)(
                self.display,
                fourcc,
                count,
                modifiers.as_mut_ptr(),
                external.as_mut_ptr(),
                &raw mut count,
            )
        };
        if ok == 0 {
            return Vec::new();
        }
        modifiers
            .into_iter()
            .zip(external)
            .take(usize::try_from(count).unwrap_or(0))
            .map(|(modifier, external_only)| ImportableModifier {
                modifier: FormatModifier(modifier),
                external_only: external_only != 0,
            })
            .filter(|entry| entry.modifier != FormatModifier::IMPLICIT)
            .collect()
    }

    /// Modifiers to offer the producer for `format`: importable as a
    /// `GL_TEXTURE_2D` both in the producer's format and in the swizzled
    /// format the import uses. Empty when `format` has no swizzled import.
    #[must_use]
    pub fn offerable_modifiers(&self, format: CapturePixelFormat) -> Vec<FormatModifier> {
        let Some(import_as) = swizzled_import_format(format) else {
            return Vec::new();
        };
        let producer = self.modifiers(format);
        self.modifiers(import_as)
            .into_iter()
            .filter(|entry| !entry.external_only)
            .map(|entry| entry.modifier)
            .filter(|modifier| {
                producer
                    .iter()
                    .any(|entry| entry.modifier == *modifier && !entry.external_only)
            })
            .collect()
    }

    /// Imports one validated DMA-BUF surface into `dst` (BGRA rows
    /// `dst_pitch` apart, the retained frame NVENC stages from).
    ///
    /// When this returns, successfully or not, the GPU no longer reads the
    /// producer's buffer (the blit's fence was waited for), so the caller
    /// may release the lease at once; on an error `dst` may hold a partial
    /// frame only if the CUDA copy itself failed, and the caller must not
    /// encode it.
    ///
    /// # Errors
    ///
    /// [`GpuConvertError`]; everything created for this import is released.
    ///
    /// # Safety
    ///
    /// On the opening thread; `fds` stay open for the call (the lease is
    /// held); `dst` is a device allocation of at least `dst_pitch * height`
    /// bytes with `dst_pitch >= width * 4`.
    pub unsafe fn import(
        &mut self,
        surface: &CaptureSurface,
        fds: &[BorrowedFd<'_>],
        acquire: AcquireSync<'_>,
        dst: CUdeviceptr,
        dst_pitch: usize,
    ) -> Result<GpuImportReport, GpuConvertError> {
        if (surface.geometry.coded_width, surface.geometry.coded_height)
            != (self.width, self.height)
        {
            return Err(GpuConvertError::Surface(format!(
                "{}x{} frame for a {}x{} importer",
                surface.geometry.coded_width,
                surface.geometry.coded_height,
                self.width,
                self.height
            )));
        }
        let import_as = swizzled_import_format(surface.format).ok_or_else(|| {
            GpuConvertError::Surface(format!("no swizzled import for {}", surface.format.token()))
        })?;
        let started = Instant::now();
        match acquire {
            AcquireSync::Explicit { device, fd, point } => device
                .wait_point(fd, point, SYNC_TIMEOUT)
                .map_err(|error: SyncError| GpuConvertError::Sync(error.to_string()))?,
            AcquireSync::Implicit => {
                for fd in fds {
                    match wait_for_producer(*fd, SYNC_TIMEOUT) {
                        ProducerSync::Signalled => {}
                        other => {
                            return Err(GpuConvertError::Sync(format!(
                                "implicit fence {}",
                                other.token()
                            )))
                        }
                    }
                }
            }
        }
        let synced = Instant::now();
        let raw_fds: Vec<Option<i32>> = fds.iter().map(|fd| Some(fd.as_raw_fd())).collect();
        let attributes = egl_import_attributes_as(surface, &raw_fds, import_as)
            .map_err(|error| GpuConvertError::Surface(format!("{error:?}")))?;
        let image = (self.egl.create_image)(
            self.display,
            ptr::null_mut(),
            EGL_LINUX_DMA_BUF_EXT,
            ptr::null_mut(),
            attributes.as_ptr(),
        );
        if image.is_null() {
            return Err(GpuConvertError::CreateImage((self.egl.get_error)()));
        }
        let blitted = self.blit_image(image);
        (self.egl.destroy_image)(self.display, image);
        blitted?;
        let blit_done = Instant::now();
        self.copy_target(dst, dst_pitch)?;
        Ok(GpuImportReport {
            sync_wait: synced - started,
            blit: blit_done - synced,
            cuda_copy: blit_done.elapsed(),
        })
    }

    /// Blits `image` into the target and waits until the GPU has finished,
    /// deleting the temporary texture in every case.
    unsafe fn blit_image(&mut self, image: EglImage) -> Result<(), GpuConvertError> {
        let (width, height) = self.dimensions()?;
        // Clear any stale error so the checks below describe this import.
        while (self.gl.get_error)() != GL_NO_ERROR {}
        let mut source = 0u32;
        (self.gl.gen_textures)(1, &raw mut source);
        (self.gl.bind_texture)(GL_TEXTURE_2D, source);
        (self.gl.egl_image_target_texture_2d)(GL_TEXTURE_2D, image);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        (self.gl.bind_texture)(GL_TEXTURE_2D, 0);
        let result = (|| {
            self.gl_check("glEGLImageTargetTexture2DOES")?;
            (self.gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, self.read_fbo);
            (self.gl.framebuffer_texture_2d)(
                GL_READ_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                source,
                0,
            );
            let status = (self.gl.check_framebuffer_status)(GL_READ_FRAMEBUFFER);
            if status != GL_FRAMEBUFFER_COMPLETE {
                return Err(GpuConvertError::Gl(format!(
                    "imported framebuffer incomplete ({status:#x})"
                )));
            }
            (self.gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, self.draw_fbo);
            // Row 0 of the image lands in row 0 of the target: no flip.
            (self.gl.blit_framebuffer)(
                0,
                0,
                width,
                height,
                0,
                0,
                width,
                height,
                GL_COLOR_BUFFER_BIT,
                GL_NEAREST as u32,
            );
            self.gl_check("glBlitFramebuffer")?;
            let fence = (self.gl.fence_sync)(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
            if fence.is_null() {
                return Err(GpuConvertError::Gl("glFenceSync failed".into()));
            }
            let timeout = u64::try_from(SYNC_TIMEOUT.as_nanos()).unwrap_or(u64::MAX);
            let waited = (self.gl.client_wait_sync)(fence, GL_SYNC_FLUSH_COMMANDS_BIT, timeout);
            (self.gl.delete_sync)(fence);
            if waited != GL_ALREADY_SIGNALED && waited != GL_CONDITION_SATISFIED {
                // A blit still running would read a buffer about to be
                // released: wait it out before reporting the failure.
                (self.gl.finish)();
                return Err(GpuConvertError::Gl(format!(
                    "blit fence wait returned {waited:#x}"
                )));
            }
            Ok(())
        })();
        (self.gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, self.read_fbo);
        (self.gl.framebuffer_texture_2d)(
            GL_READ_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_2D,
            0,
            0,
        );
        (self.gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, 0);
        (self.gl.delete_textures)(1, &raw const source);
        result
    }
}

impl Drop for GpuConvertImporter {
    fn drop(&mut self) {
        // SAFETY: everything below was created by this importer on its
        // owning thread, with its GL context still current and the CUDA
        // context alive (the caller drops the importer before the context);
        // each object is released exactly once.
        unsafe {
            if !self.resource.is_null() {
                let _ = (self.cuda.unregister)(self.resource);
            }
            if self.read_fbo != 0 {
                (self.gl.delete_framebuffers)(1, &raw const self.read_fbo);
            }
            if self.draw_fbo != 0 {
                (self.gl.delete_framebuffers)(1, &raw const self.draw_fbo);
            }
            if self.target != 0 {
                (self.gl.delete_textures)(1, &raw const self.target);
            }
            (self.egl.make_current)(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            (self.egl.destroy_context)(self.display, self.context);
            (self.egl.terminate)(self.display);
        }
    }
}
