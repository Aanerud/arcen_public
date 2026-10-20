//! DMA-BUF layout for EGL import and producer synchronisation.
//!
//! [`egl_import_attributes_as`] turns a validated DMA-BUF surface into the
//! `EGL_EXT_image_dma_buf_import(_modifiers)` attribute list, as the
//! surface's own format or the swizzled format the `gpu-convert` route
//! imports ([`swizzled_import_format`]); it is pure and tested everywhere.
//! [`wait_for_producer`] waits for the producer's implicit write fence before
//! a GPU reads a buffer that carries no explicit-sync points (Linux only).

use arcen_media::video::{CaptureMemory, CapturePixelFormat, CaptureSurface, ModifierClass};

/// `EGL_NONE`.
pub const EGL_NONE: i32 = 0x3038;
/// `EGL_WIDTH`.
pub const EGL_WIDTH: i32 = 0x3057;
/// `EGL_HEIGHT`.
pub const EGL_HEIGHT: i32 = 0x3056;
/// `EGL_LINUX_DRM_FOURCC_EXT`.
pub const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
/// `EGL_IMAGE_PRESERVED_KHR`.
pub const EGL_IMAGE_PRESERVED_KHR: i32 = 0x30D2;
/// `EGL_TRUE`.
pub const EGL_TRUE: i32 = 1;
/// Per-plane `(fd, offset, pitch, modifier_lo, modifier_hi)` attribute names.
pub const EGL_PLANE_ATTRIBUTES: [[i32; 5]; 4] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
];

const fn fourcc(code: &[u8; 4]) -> u32 {
    (code[0] as u32) | ((code[1] as u32) << 8) | ((code[2] as u32) << 16) | ((code[3] as u32) << 24)
}

/// The DRM fourcc for a capture format, in memory-order terms matching
/// `CapturePixelFormat` (BGRx bytes are `XRGB8888`).
#[must_use]
pub const fn drm_fourcc(format: CapturePixelFormat) -> u32 {
    match format {
        CapturePixelFormat::Bgrx8 => fourcc(b"XR24"),
        CapturePixelFormat::Bgra8 => fourcc(b"AR24"),
        CapturePixelFormat::Rgbx8 => fourcc(b"XB24"),
        CapturePixelFormat::Rgba8 => fourcc(b"AB24"),
        CapturePixelFormat::Nv12 => fourcc(b"NV12"),
        CapturePixelFormat::Xrgb2101010 => fourcc(b"XR30"),
        CapturePixelFormat::Xbgr2101010 => fourcc(b"XB30"),
        CapturePixelFormat::RgbaF16 => fourcc(b"AB4H"),
    }
}

/// Why a surface cannot be described to EGL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EglAttributeError {
    /// The surface is not a DMA-BUF.
    NotDmaBuf,
    /// The implicit modifier is never imported.
    ImplicitModifier,
    /// A plane's file descriptor is missing.
    MissingFd(usize),
    /// A value does not fit an `EGLint`.
    OutOfRange,
    /// More planes than EGL can describe.
    TooManyPlanes,
    /// The import format does not share the surface format's layout.
    FormatLayout,
}

/// Builds the `eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)` attribute list.
/// `fds[i]` is the file descriptor of buffer object `i`.
///
/// # Errors
///
/// [`EglAttributeError`] for non-DMA-BUF memory, the implicit modifier,
/// missing descriptors or values outside `EGLint`.
#[cfg(test)]
pub fn egl_import_attributes(
    surface: &CaptureSurface,
    fds: &[Option<i32>],
) -> Result<Vec<i32>, EglAttributeError> {
    egl_import_attributes_as(surface, fds, surface.format)
}

/// The format the `gpu-convert` route imports a producer format as: the
/// same 32-bit layout with red and blue exchanged, so that a GL copy of the
/// image into an RGBA8 texture stores the producer's B, G, R, X byte order
/// (what NVENC's ARGB input reads). `None` for formats the route does not
/// import.
#[must_use]
pub const fn swizzled_import_format(format: CapturePixelFormat) -> Option<CapturePixelFormat> {
    match format {
        CapturePixelFormat::Bgrx8 => Some(CapturePixelFormat::Rgbx8),
        CapturePixelFormat::Bgra8 => Some(CapturePixelFormat::Rgba8),
        _ => None,
    }
}

/// Builds the `eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)` attribute list,
/// describing the buffer to EGL as `import_as`: the surface's own format or
/// its [`swizzled_import_format`] (the same single-plane 32-bit layout).
/// `fds[i]` is the file descriptor of buffer object `i`.
///
/// # Errors
///
/// [`EglAttributeError`] for non-DMA-BUF memory, the implicit modifier,
/// missing descriptors, values outside `EGLint`, or an `import_as` that does
/// not share the surface format's layout.
pub fn egl_import_attributes_as(
    surface: &CaptureSurface,
    fds: &[Option<i32>],
    import_as: CapturePixelFormat,
) -> Result<Vec<i32>, EglAttributeError> {
    if import_as != surface.format && swizzled_import_format(surface.format) != Some(import_as) {
        return Err(EglAttributeError::FormatLayout);
    }
    let CaptureMemory::DmaBuf { modifier } = surface.memory else {
        return Err(EglAttributeError::NotDmaBuf);
    };
    if modifier.class() == ModifierClass::Implicit {
        return Err(EglAttributeError::ImplicitModifier);
    }
    if surface.planes.len() > EGL_PLANE_ATTRIBUTES.len() {
        return Err(EglAttributeError::TooManyPlanes);
    }
    let int = |value: u64| i32::try_from(value).map_err(|_| EglAttributeError::OutOfRange);
    let mut attributes = vec![
        EGL_WIDTH,
        int(u64::from(surface.geometry.coded_width))?,
        EGL_HEIGHT,
        int(u64::from(surface.geometry.coded_height))?,
        EGL_LINUX_DRM_FOURCC_EXT,
        // Reinterpreting the fourcc's bits is EGL's own convention.
        i32::from_ne_bytes(drm_fourcc(import_as).to_ne_bytes()),
    ];
    let low = i32::from_ne_bytes(((modifier.0 & 0xffff_ffff) as u32).to_ne_bytes());
    let high = i32::from_ne_bytes(((modifier.0 >> 32) as u32).to_ne_bytes());
    for (index, (plane, names)) in surface
        .planes
        .iter()
        .zip(EGL_PLANE_ATTRIBUTES.iter())
        .enumerate()
    {
        let fd = fds
            .get(usize::from(plane.buffer))
            .copied()
            .flatten()
            .ok_or(EglAttributeError::MissingFd(index))?;
        attributes.extend_from_slice(&[
            names[0],
            fd,
            names[1],
            int(plane.offset)?,
            names[2],
            int(u64::from(plane.stride))?,
            names[3],
            low,
            names[4],
            high,
        ]);
    }
    attributes.extend_from_slice(&[EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE]);
    Ok(attributes)
}

/// Outcome of waiting for the producer's implicit fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerSync {
    /// The producer's writes are complete.
    Signalled,
    /// The fence did not signal within the bound; do not read the buffer.
    TimedOut,
    /// The kernel cannot export the fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`
    /// unsupported, or not a DMA-BUF); synchronisation is unknown.
    Unsupported,
}

impl ProducerSync {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Signalled => "signalled",
            Self::TimedOut => "timed_out",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Waits up to `timeout` for every pending write to the DMA-BUF `fd`,
/// using an exported read sync file. Runs in the import worker, never in a
/// `PipeWire` callback.
#[cfg(target_os = "linux")]
#[must_use]
pub fn wait_for_producer(
    fd: std::os::fd::BorrowedFd<'_>,
    timeout: std::time::Duration,
) -> ProducerSync {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[repr(C)]
    struct ExportSyncFile {
        flags: u32,
        fd: i32,
    }
    // _IOWR('b', 2, struct dma_buf_export_sync_file); DMA_BUF_SYNC_READ = 1.
    const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = 0xC008_6202;
    const DMA_BUF_SYNC_READ: u32 = 1;

    let mut request = ExportSyncFile {
        flags: DMA_BUF_SYNC_READ,
        fd: -1,
    };
    // SAFETY: `fd` is a live descriptor borrowed for this call; `request` is
    // a properly sized and aligned `dma_buf_export_sync_file` that the kernel
    // only writes within. Any failure is reported through the return value.
    let status = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            DMA_BUF_IOCTL_EXPORT_SYNC_FILE,
            &raw mut request,
        )
    };
    if status != 0 || request.fd < 0 {
        return ProducerSync::Unsupported;
    }
    // SAFETY: on success the kernel returned a new descriptor that this
    // process now exclusively owns.
    let sync_file = unsafe { OwnedFd::from_raw_fd(request.fd) };
    let mut poll = libc::pollfd {
        fd: sync_file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `poll` points at one initialised `pollfd` for the duration of
    // the call.
    let ready = unsafe { libc::poll(&raw mut poll, 1, millis) };
    if ready == 1 && poll.revents & libc::POLLIN != 0 {
        ProducerSync::Signalled
    } else {
        ProducerSync::TimedOut
    }
}

#[cfg(test)]
mod tests {
    use arcen_media::video::{CopyPath, FormatModifier, GpuIdentity};

    use super::*;
    use crate::wayland::layout::tests::{context, host_buffer};
    use crate::wayland::layout::{classify, Classified, DataKind};

    fn dmabuf_surface(modifier: Option<FormatModifier>) -> CaptureSurface {
        let mut context = context(1920, 1080);
        context.negotiated.modifier = modifier;
        context.source_gpu = Some(GpuIdentity::default());
        context.dmabuf_copy_path = Some(CopyPath::GpuConvert);
        let mut buffer = host_buffer(7680, 1080);
        buffer.planes[0].kind = DataKind::DmaBuf;
        buffer.planes[0].chunk_offset = 0;
        match classify(&buffer, &context, 1, 0) {
            Classified::Frame { surface, .. } => *surface,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn fourcc_codes_match_drm() {
        assert_eq!(drm_fourcc(CapturePixelFormat::Bgrx8), 0x3432_5258);
        assert_eq!(drm_fourcc(CapturePixelFormat::Xrgb2101010), 0x3033_5258);
        assert_eq!(drm_fourcc(CapturePixelFormat::Nv12), 0x3231_564e);
    }

    #[test]
    fn single_plane_attributes_carry_fd_offset_pitch_and_modifier() {
        let modifier = FormatModifier(0x0300_0000_004f_e010);
        let attributes =
            egl_import_attributes(&dmabuf_surface(Some(modifier)), &[Some(17)]).unwrap();
        assert_eq!(
            attributes,
            vec![
                EGL_WIDTH,
                1920,
                EGL_HEIGHT,
                1080,
                EGL_LINUX_DRM_FOURCC_EXT,
                0x3432_5258,
                0x3272,
                17,
                0x3273,
                0,
                0x3274,
                7680,
                0x3443,
                0x004f_e010,
                0x3444,
                0x0300_0000,
                EGL_IMAGE_PRESERVED_KHR,
                EGL_TRUE,
                EGL_NONE,
            ]
        );
    }

    #[test]
    fn implicit_missing_fd_and_host_memory_are_refused() {
        assert_eq!(
            egl_import_attributes(&dmabuf_surface(None), &[Some(3)]),
            Err(EglAttributeError::ImplicitModifier)
        );
        assert_eq!(
            egl_import_attributes(&dmabuf_surface(Some(FormatModifier::LINEAR)), &[None]),
            Err(EglAttributeError::MissingFd(0))
        );
        let host = match classify(&host_buffer(256, 64), &context(64, 64), 1, 0) {
            Classified::Frame { surface, .. } => *surface,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            egl_import_attributes(&host, &[Some(3)]),
            Err(EglAttributeError::NotDmaBuf)
        );
    }

    #[test]
    fn the_swizzled_import_exchanges_red_and_blue_only() {
        assert_eq!(
            swizzled_import_format(CapturePixelFormat::Bgrx8),
            Some(CapturePixelFormat::Rgbx8)
        );
        assert_eq!(
            swizzled_import_format(CapturePixelFormat::Bgra8),
            Some(CapturePixelFormat::Rgba8)
        );
        assert_eq!(swizzled_import_format(CapturePixelFormat::Nv12), None);
        assert_eq!(
            swizzled_import_format(CapturePixelFormat::Xrgb2101010),
            None
        );
        let surface = dmabuf_surface(Some(FormatModifier(0x0300_0000_00cd_b014)));
        let attributes =
            egl_import_attributes_as(&surface, &[Some(9)], CapturePixelFormat::Rgbx8).unwrap();
        // XB24 instead of XR24; everything else as the producer described.
        assert_eq!(attributes[5], 0x3432_4258);
        assert_eq!(
            attributes[6..],
            egl_import_attributes(&surface, &[Some(9)]).unwrap()[6..]
        );
        assert_eq!(
            egl_import_attributes_as(&surface, &[Some(9)], CapturePixelFormat::Nv12),
            Err(EglAttributeError::FormatLayout)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_dmabuf_descriptor_reports_unsupported_sync() {
        use std::os::fd::AsFd;

        let file = std::fs::File::open("/proc/self/stat").unwrap();
        assert_eq!(
            wait_for_producer(file.as_fd(), std::time::Duration::from_millis(1)),
            ProducerSync::Unsupported
        );
    }
}
