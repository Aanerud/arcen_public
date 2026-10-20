//! The wide read-back route of the Wayland Grading and HDR compositions:
//! a compositor's ten-bit or half-float DMA-BUF read back into host memory
//! for the shared wide conversion.
//!
//! ```text
//! producer DMA-BUF (XB30 / XR30 / AB4H, NVIDIA block-linear modifier)
//!   -> wait: explicit-sync acquire point, else the buffer's implicit fence
//!   -> eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT) in the producer's own format
//!   -> GL_TEXTURE_2D (glEGLImageTargetTexture2DOES) on a read framebuffer
//!   -> glReadPixels: GL_RGBA / GL_UNSIGNED_INT_2_10_10_10_REV (packed
//!      XBGR2101010 words) or GL_RGBA / GL_HALF_FLOAT (RGBA half floats)
//!   -> the host frame the shared conversion turns into NVENC's P16 input
//! ```
//!
//! This is the same cost class as Xorg Grading (`XShm` read into host
//! memory, shared conversion, one upload), and it is reported as
//! `host-copy`: pixels transit host memory. It exists because the
//! compositor's shared-memory path is not trustworthy for wide formats
//! (Mutter 50 and 51 paint eight-bit pixels into a ten-bit or half-float
//! shared-memory buffer), while its DMA-BUF path renders at the negotiated
//! precision, and CUDA cannot register block-linear EGL images
//! (`gpu_convert`'s module notes).
//!
//! A start-up self-test uploads a known ten-bit and half-float pattern,
//! reads both back and compares every value, so the GL half of the route is
//! proven before any producer buffer is trusted to it. EGL and GL are
//! `dlopen`ed and never `dlclose`d, as in [`super::gpu_convert`].

use std::ffi::{c_char, c_void, CStr, CString};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

use arcen_media::video::{CapturePixelFormat, CaptureSurface, FormatModifier};

use super::dmabuf::{drm_fourcc, egl_import_attributes_as, wait_for_producer, ProducerSync};
use super::explicit_sync::SyncError;
use super::gpu_convert::{AcquireSync, GpuConvertError, ImportableModifier, SYNC_TIMEOUT};
use crate::linux::dl;

type EglBoolean = u32;
type EglDisplay = *mut c_void;
type EglDeviceExt = *mut c_void;
type EglContext = *mut c_void;
type EglImage = *mut c_void;

const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
const EGL_DRM_RENDER_NODE_FILE_EXT: i32 = 0x3377;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_NONE: i32 = 0x3038;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA: u32 = 0x1908;
const GL_RGB10_A2: i32 = 0x8059;
const GL_RGBA16F: i32 = 0x881A;
const GL_UNSIGNED_INT_2_10_10_10_REV: u32 = 0x8368;
const GL_HALF_FLOAT: u32 = 0x140B;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_NEAREST: i32 = 0x2600;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_PACK_ALIGNMENT: u32 = 0x0D05;
const GL_PACK_ROW_LENGTH: u32 = 0x0D02;
const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
const GL_RENDERER: u32 = 0x1F01;
const GL_VERSION: u32 = 0x1F02;
const GL_NO_ERROR: u32 = 0;

type EglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type EglInitialize = unsafe extern "C" fn(EglDisplay, *mut i32, *mut i32) -> EglBoolean;
type EglQueryString = unsafe extern "C" fn(EglDisplay, i32) -> *const c_char;
type EglQueryDevices = unsafe extern "C" fn(i32, *mut EglDeviceExt, *mut i32) -> EglBoolean;
type EglQueryDeviceString = unsafe extern "C" fn(EglDeviceExt, i32) -> *const c_char;
type EglGetPlatformDisplay = unsafe extern "C" fn(u32, *mut c_void, *const i32) -> EglDisplay;
type EglBindApi = unsafe extern "C" fn(u32) -> EglBoolean;
type EglCreateContext =
    unsafe extern "C" fn(EglDisplay, *mut c_void, EglContext, *const i32) -> EglContext;

struct EglFns {
    terminate: unsafe extern "C" fn(EglDisplay) -> EglBoolean,
    create_image:
        unsafe extern "C" fn(EglDisplay, *mut c_void, u32, *mut c_void, *const i32) -> EglImage,
    destroy_image: unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean,
    query_modifiers: unsafe extern "C" fn(
        EglDisplay,
        i32,
        i32,
        *mut u64,
        *mut EglBoolean,
        *mut i32,
    ) -> EglBoolean,
    destroy_context: unsafe extern "C" fn(EglDisplay, EglContext) -> EglBoolean,
    make_current:
        unsafe extern "C" fn(EglDisplay, *mut c_void, *mut c_void, EglContext) -> EglBoolean,
    get_error: unsafe extern "C" fn() -> i32,
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
    read_pixels: unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void),
    pixel_storei: unsafe extern "C" fn(u32, i32),
    get_error: unsafe extern "C" fn() -> u32,
    get_string: unsafe extern "C" fn(u32) -> *const u8,
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

/// What a wide read-back writes into host memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WideHostLayout {
    /// Packed little-endian `XBGR2101010` words (red in the low bits),
    /// whatever the producer's ten-bit channel order: GL reads channels by
    /// name.
    Xbgr2101010,
    /// R, G, B, A little-endian half floats.
    RgbaF16,
}

impl WideHostLayout {
    /// The layout a producer format is read back as.
    #[must_use]
    pub const fn for_format(format: CapturePixelFormat) -> Option<Self> {
        match format {
            CapturePixelFormat::Xrgb2101010 | CapturePixelFormat::Xbgr2101010 => {
                Some(Self::Xbgr2101010)
            }
            CapturePixelFormat::RgbaF16 => Some(Self::RgbaF16),
            _ => None,
        }
    }

    /// Bytes per pixel.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Xbgr2101010 => 4,
            Self::RgbaF16 => 8,
        }
    }

    const fn gl_type(self) -> u32 {
        match self {
            Self::Xbgr2101010 => GL_UNSIGNED_INT_2_10_10_10_REV,
            Self::RgbaF16 => GL_HALF_FLOAT,
        }
    }

    const fn gl_internal_format(self) -> i32 {
        match self {
            Self::Xbgr2101010 => GL_RGB10_A2,
            Self::RgbaF16 => GL_RGBA16F,
        }
    }
}

/// What one wide read-back cost.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WideReadbackReport {
    /// Waiting for the producer.
    pub sync_wait: Duration,
    /// Image import and read-back (the producer's buffer is free once this
    /// ends).
    pub readback: Duration,
}

/// The EGL device display on the encoder's GPU and a surfaceless GL context
/// current on the import thread.
///
/// Not `Send`: the GL context is current on the thread that opened it.
pub struct WideReadbackImporter {
    display: EglDisplay,
    context: EglContext,
    egl: EglFns,
    gl: GlFns,
    renderer: String,
    width: u32,
    height: u32,
    read_fbo: u32,
}

impl WideReadbackImporter {
    /// Opens the route on the GPU whose DRM render node is `render_node` for
    /// `width`x`height` frames and proves the GL read-back of both wide
    /// layouts with a self-test.
    ///
    /// # Errors
    ///
    /// [`GpuConvertError`]; nothing is left current or allocated.
    ///
    /// # Safety
    ///
    /// The importer must only be used and dropped on the opening thread, and
    /// no other GL context may be current there.
    pub unsafe fn open(
        render_node: &Path,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuConvertError> {
        let egl_lib = dl::open("libEGL.so.1").map_err(GpuConvertError::Library)?;
        let mut last = None;
        for (api, attributes) in [
            (EGL_OPENGL_API, vec![EGL_NONE]),
            (
                EGL_OPENGL_ES_API,
                vec![EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE],
            ),
        ] {
            match Self::open_with(egl_lib, render_node, width, height, api, &attributes) {
                Ok(importer) => return Ok(importer),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| GpuConvertError::Display("no client API".into())))
    }

    #[allow(clippy::too_many_lines)] // One linear bring-up with paired teardown.
    unsafe fn open_with(
        egl_lib: *mut c_void,
        render_node: &Path,
        width: u32,
        height: u32,
        api: u32,
        context_attributes: &[i32],
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
        let display = platform_display(EGL_PLATFORM_DEVICE_EXT, device, [EGL_NONE].as_ptr());
        if display.is_null() {
            return Err(GpuConvertError::Display(
                "eglGetPlatformDisplayEXT failed".into(),
            ));
        }
        let (mut major, mut minor) = (0, 0);
        if initialize(display, &raw mut major, &raw mut minor) == 0 {
            return Err(GpuConvertError::Display("eglInitialize failed".into()));
        }
        let extensions = {
            let value = query_string(display, EGL_EXTENSIONS);
            if value.is_null() {
                String::new()
            } else {
                CStr::from_ptr(value).to_string_lossy().into_owned()
            }
        };
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
                "EGL {major}.{minor} lacks {missing:?}"
            )));
        }
        if bind_api(api) == 0 {
            (egl.terminate)(display);
            return Err(GpuConvertError::Display(format!(
                "eglBindAPI({api:#x}) refused"
            )));
        }
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
                "eglCreateContext({api:#x}) failed (EGL error {error:#x})"
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
            renderer,
            width,
            height,
            read_fbo: 0,
        };
        (importer.gl.gen_framebuffers)(1, &raw mut importer.read_fbo);
        importer.gl_check("read framebuffer")?;
        importer.self_test(WideHostLayout::Xbgr2101010)?;
        importer.self_test(WideHostLayout::RgbaF16)?;
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
            read_pixels: proc_address(get, "glReadPixels")?,
            pixel_storei: proc_address(get, "glPixelStorei")?,
            get_error: proc_address(get, "glGetError")?,
            get_string: proc_address(get, "glGetString")?,
        })
    }

    unsafe fn gl_check(&self, what: &str) -> Result<(), GpuConvertError> {
        match (self.gl.get_error)() {
            GL_NO_ERROR => Ok(()),
            error => Err(GpuConvertError::Gl(format!("{what}: GL error {error:#x}"))),
        }
    }

    /// Reads `texture` (already holding `width`x`height` pixels) back into
    /// `dst` as `layout`, rows `dst_stride` bytes apart.
    unsafe fn read_texture(
        &mut self,
        texture: u32,
        width: u32,
        height: u32,
        layout: WideHostLayout,
        dst: &mut [u8],
        dst_stride: usize,
    ) -> Result<(), GpuConvertError> {
        let bpp = layout.bytes_per_pixel();
        let row_bytes = width as usize * bpp;
        if dst_stride < row_bytes
            || !dst_stride.is_multiple_of(bpp)
            || dst.len() < dst_stride * (height as usize).saturating_sub(1) + row_bytes
        {
            return Err(GpuConvertError::Surface(format!(
                "host frame of {} bytes, stride {dst_stride}, cannot hold {width}x{height} {layout:?}",
                dst.len()
            )));
        }
        let int = |value: usize| {
            i32::try_from(value)
                .map_err(|_| GpuConvertError::Surface(format!("dimension {value} too large")))
        };
        (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, self.read_fbo);
        (self.gl.framebuffer_texture_2d)(
            GL_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_2D,
            texture,
            0,
        );
        let result = (|| {
            let status = (self.gl.check_framebuffer_status)(GL_FRAMEBUFFER);
            if status != GL_FRAMEBUFFER_COMPLETE {
                return Err(GpuConvertError::Gl(format!(
                    "read framebuffer incomplete ({status:#x})"
                )));
            }
            (self.gl.pixel_storei)(GL_PACK_ALIGNMENT, 4);
            (self.gl.pixel_storei)(GL_PACK_ROW_LENGTH, int(dst_stride / bpp)?);
            // Row 0 of the image is read into row 0 of `dst`: no flip, as in
            // the blit route.
            (self.gl.read_pixels)(
                0,
                0,
                int(width as usize)?,
                int(height as usize)?,
                GL_RGBA,
                layout.gl_type(),
                dst.as_mut_ptr().cast(),
            );
            (self.gl.pixel_storei)(GL_PACK_ROW_LENGTH, 0);
            self.gl_check("glReadPixels")
        })();
        (self.gl.framebuffer_texture_2d)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0);
        (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, 0);
        result
    }

    /// Uploads a pattern in `layout`, reads it back and compares every
    /// value.
    unsafe fn self_test(&mut self, layout: WideHostLayout) -> Result<(), GpuConvertError> {
        const SIDE: u32 = 32;
        let bpp = layout.bytes_per_pixel();
        let pattern = self_test_pattern(layout, SIDE);
        let mut texture = 0u32;
        (self.gl.gen_textures)(1, &raw mut texture);
        (self.gl.bind_texture)(GL_TEXTURE_2D, texture);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        (self.gl.pixel_storei)(GL_UNPACK_ALIGNMENT, 4);
        (self.gl.tex_image_2d)(
            GL_TEXTURE_2D,
            0,
            layout.gl_internal_format(),
            SIDE as i32,
            SIDE as i32,
            0,
            GL_RGBA,
            layout.gl_type(),
            pattern.as_ptr().cast(),
        );
        (self.gl.bind_texture)(GL_TEXTURE_2D, 0);
        let mut read = vec![0u8; pattern.len()];
        let result = self.gl_check("self-test texture").and_then(|()| {
            self.read_texture(texture, SIDE, SIDE, layout, &mut read, SIDE as usize * bpp)
        });
        (self.gl.delete_textures)(1, &raw const texture);
        result?;
        // Alpha is never used; compare colour only.
        let colour = |bytes: &[u8]| -> Vec<u8> {
            bytes
                .chunks_exact(bpp)
                .flat_map(|pixel| match layout {
                    WideHostLayout::Xbgr2101010 => {
                        let word = u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]])
                            & 0x3fff_ffff;
                        word.to_le_bytes().to_vec()
                    }
                    WideHostLayout::RgbaF16 => pixel[..6].to_vec(),
                })
                .collect()
        };
        if colour(&read) == colour(&pattern) {
            Ok(())
        } else {
            Err(GpuConvertError::SelfTest(format!(
                "{layout:?} read-back differs from the uploaded pattern"
            )))
        }
    }

    /// The GL renderer string, for logs.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("renderer={:?}", self.renderer)
    }

    /// Modifiers EGL can import for `format` as `GL_TEXTURE_2D`, implicit
    /// and external-only excluded.
    #[must_use]
    pub fn offerable_modifiers(&self, format: CapturePixelFormat) -> Vec<FormatModifier> {
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
            .filter(|entry| entry.modifier != FormatModifier::IMPLICIT && !entry.external_only)
            .map(|entry| entry.modifier)
            .collect()
    }

    /// Reads one validated wide DMA-BUF surface back into `dst` (rows
    /// `dst_stride` apart) as [`WideHostLayout::for_format`].
    ///
    /// When this returns, successfully or not, the GPU no longer reads the
    /// producer's buffer (`glReadPixels` into client memory completes before
    /// it returns), so the caller may release the lease at once.
    ///
    /// # Errors
    ///
    /// [`GpuConvertError`]; on an error `dst` must not be encoded.
    ///
    /// # Safety
    ///
    /// On the opening thread; `fds` stay open for the call (the lease is
    /// held).
    pub unsafe fn import(
        &mut self,
        surface: &CaptureSurface,
        fds: &[BorrowedFd<'_>],
        acquire: AcquireSync<'_>,
        dst: &mut [u8],
        dst_stride: usize,
    ) -> Result<WideReadbackReport, GpuConvertError> {
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
        let layout = WideHostLayout::for_format(surface.format).ok_or_else(|| {
            GpuConvertError::Surface(format!("no wide read-back for {}", surface.format.token()))
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
        let attributes = egl_import_attributes_as(surface, &raw_fds, surface.format)
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
        while (self.gl.get_error)() != GL_NO_ERROR {}
        let mut texture = 0u32;
        (self.gl.gen_textures)(1, &raw mut texture);
        (self.gl.bind_texture)(GL_TEXTURE_2D, texture);
        (self.gl.egl_image_target_texture_2d)(GL_TEXTURE_2D, image);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        (self.gl.bind_texture)(GL_TEXTURE_2D, 0);
        let result = self
            .gl_check("glEGLImageTargetTexture2DOES")
            .and_then(|()| {
                self.read_texture(texture, self.width, self.height, layout, dst, dst_stride)
            });
        (self.gl.delete_textures)(1, &raw const texture);
        (self.egl.destroy_image)(self.display, image);
        result?;
        Ok(WideReadbackReport {
            sync_wait: synced - started,
            readback: synced.elapsed(),
        })
    }
}

impl Drop for WideReadbackImporter {
    fn drop(&mut self) {
        // SAFETY: everything below was created by this importer on its
        // owning thread with its context current; each object is released
        // exactly once.
        unsafe {
            if self.read_fbo != 0 {
                (self.gl.delete_framebuffers)(1, &raw const self.read_fbo);
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

/// A `side`x`side` pattern in `layout` whose channels walk every ten-bit
/// code (or a spread of half floats in `0.0..=1.0`), alpha opaque.
fn self_test_pattern(layout: WideHostLayout, side: u32) -> Vec<u8> {
    let pixels = (side * side) as usize;
    let mut bytes = Vec::with_capacity(pixels * layout.bytes_per_pixel());
    for index in 0..pixels {
        #[allow(clippy::cast_possible_truncation)]
        let code = |offset: usize| ((index * 3 + offset) % 1024) as u32;
        match layout {
            WideHostLayout::Xbgr2101010 => {
                let word = code(0) | (code(1) << 10) | (code(2) << 20) | (3 << 30);
                bytes.extend_from_slice(&word.to_le_bytes());
            }
            WideHostLayout::RgbaF16 => {
                // Exact binary16 values k/1024 for k < 1024, and 1.0.
                for channel in 0..4 {
                    let bits: u16 = if channel == 3 {
                        0x3c00
                    } else {
                        half_of_ten_bit_fraction(code(channel))
                    };
                    bytes.extend_from_slice(&bits.to_le_bytes());
                }
            }
        }
    }
    bytes
}

/// The binary16 encoding of `code / 1024` (exact for every `code < 1024`).
const fn half_of_ten_bit_fraction(code: u32) -> u16 {
    if code == 0 {
        return 0;
    }
    // code / 1024 = 1.m * 2^e with e = floor(log2(code)) - 10.
    let leading = 31 - code.leading_zeros();
    let exponent = leading as i32 - 10 + 15;
    let mantissa = (code << (10 - leading)) & 0x3ff;
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    {
        ((exponent as u32) << 10 | mantissa) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_formats_read_back_as_one_layout_per_precision() {
        assert_eq!(
            WideHostLayout::for_format(CapturePixelFormat::Xrgb2101010),
            Some(WideHostLayout::Xbgr2101010)
        );
        assert_eq!(
            WideHostLayout::for_format(CapturePixelFormat::Xbgr2101010),
            Some(WideHostLayout::Xbgr2101010)
        );
        assert_eq!(
            WideHostLayout::for_format(CapturePixelFormat::RgbaF16),
            Some(WideHostLayout::RgbaF16)
        );
        assert_eq!(WideHostLayout::for_format(CapturePixelFormat::Bgrx8), None);
    }

    #[test]
    fn the_self_test_half_floats_are_exact_ten_bit_fractions() {
        for code in 0..1024u32 {
            let bits = half_of_ten_bit_fraction(code);
            let value = arcen_media::video::half_to_f32(bits);
            #[allow(clippy::cast_precision_loss)]
            let expected = code as f32 / 1024.0;
            assert!((value - expected).abs() < f32::EPSILON, "{code}: {value}");
        }
    }

    #[test]
    fn the_self_test_patterns_cover_every_ten_bit_code() {
        let pattern = self_test_pattern(WideHostLayout::Xbgr2101010, 32);
        let reds: std::collections::BTreeSet<u32> = pattern
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]) & 0x3ff)
            .collect();
        assert_eq!(reds.len(), 1024);
        assert_eq!(
            self_test_pattern(WideHostLayout::RgbaF16, 32).len(),
            32 * 32 * 8
        );
    }
}
